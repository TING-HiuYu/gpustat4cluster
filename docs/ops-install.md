# gpustat4cluster 运维安装指南

[中文](#gpustat4cluster-运维安装指南) | [English](#gpustat4cluster-operations-install-guide-english)

## 在线安装

```bash
curl -fsSL https://raw.githubusercontent.com/TING-HiuYu/gpustat4cluster/main/scripts/install.sh -o install.sh
bash install.sh --role server --version latest --libc gnu
```

支持 `--role server|client|both`，默认从 `TING-HiuYu/gpustat4cluster` 下载 release，安装到 `/usr/local/bin`，并初始化 `/etc/gpustat4cluster/config.toml`。如使用 fork 或镜像仓库，可通过 `REPO=owner/name` 覆盖下载源。

安装脚本会创建 `gpustat4cluster` 系统用户/组，服务默认以非 root 身份运行。可通过环境变量覆盖路径：

```bash
PREFIX=/opt/gpustat4cluster/bin \
ETC_DIR=/etc/gpustat4cluster \
SYSTEMD_DIR=/etc/systemd/system \
bash install.sh --role client --version v0.1.0 --libc musl
```

## dry-run

发布前或批量部署前建议先执行 dry-run，确认角色、版本、架构、libc 和下载 URL：

```bash
bash scripts/install.sh --role server --version v0.0.0 --dry-run
REPO=TING-HiuYu/gpustat4cluster bash scripts/install.sh --role both --version v0.1.0 --libc musl --dry-run
```

dry-run 只打印计划操作，不创建用户、不写配置、不解压 tarball、不调用 `systemctl`。如果 `--version latest`，脚本仍需要访问 GitHub API 解析最新 tag；完全离线检查请显式传入版本号。

## Release 产物命名

GitHub Actions 使用 crate package `server` 构建服务端，但发布 tarball 内会将二进制安装为 `/usr/local/bin/gpustat4cluster-server`，以匹配 systemd 单元和用户可见命名。客户端包包含：

- `/usr/local/bin/gpustat4cluster-client-backend`
- `/usr/local/bin/gpustat4cluster-client`

Release artifacts are built with `kcp-transport` enabled for `gpustat4cluster-server` and `gpustat4cluster-client-backend`. KCP remains disabled at runtime unless enabled through the optional env file.

Release asset 命名格式：

```text
gpustat4cluster-<server|client>-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz
```

## 离线安装

1. 在联网环境下载对应包：
   - `gpustat4cluster-server-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz`
   - `gpustat4cluster-client-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz`
2. 拷贝到目标机后复用安装脚本完成用户、配置和 systemd 初始化：

```bash
LOCAL_TARBALL_DIR=/path/to/tarballs bash install.sh --role server --version <tag> --libc gnu
```

如需手动解压，必须同时创建运行用户并初始化配置：

```bash
sudo groupadd --system gpustat4cluster || true
sudo useradd --system --gid gpustat4cluster --home-dir /var/lib/gpustat4cluster --create-home --shell /usr/sbin/nologin gpustat4cluster || true
sudo tar -xzf gpustat4cluster-server-<...>.tar.gz -C /
sudo tar -xzf gpustat4cluster-client-<...>.tar.gz -C /
sudo install -d -o gpustat4cluster -g gpustat4cluster -m 0755 /etc/gpustat4cluster
sudo test -f /etc/gpustat4cluster/config.toml || sudo tee /etc/gpustat4cluster/config.toml >/dev/null <<'EOF'
[connecting]
port_range = [30000, 40000]
multicast_addr = "239.0.0.1:4000"
protocol = "kcp" # or "tcp"
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
discover_wait_secs = 5
multicast_retry_limit = 5
# Optional: one or more local IPv4 addresses used as multicast outbound interfaces.
# multicast_outbound_ip = ["192.0.2.10"]

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
# Optional: UDS path for client frontend <-> client-backend.
[runtime]
# Leave unset to use libnvidia-ml.so from the dynamic loader.
# If needed, point at the real NVIDIA driver runtime library, not the CUDA stubs path.
# nvml_lib_path = "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1"
EOF
sudo test -f /etc/gpustat4cluster/gpustat4cluster.env || sudo tee /etc/gpustat4cluster/gpustat4cluster.env >/dev/null <<'EOF'
# Optional gpustat4cluster runtime environment.
# GPUSTAT4CLUSTER_ENABLE_KCP=1
# GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:30000
# GPUSTAT4CLUSTER_COLLECTOR=mock
# GPUSTAT4CLUSTER_FORCE_MOCK=1
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now gpustat4cluster-server.service
sudo systemctl enable --now gpustat4cluster-client.service
```

## systemd 行为

服务单元包含 `Restart=on-failure`、`RestartSec=5`、`StartLimitIntervalSec=60` 和 `StartLimitBurst=6`，用于异常退出后的自动恢复，并避免配置错误或环境故障造成启动风暴。systemd 会创建 `/var/lib/gpustat4cluster`、`/var/log/gpustat4cluster` 和 `/run/gpustat4cluster`，目录归属为 `gpustat4cluster` 用户。

当前 server/backend 通过环境变量读取配置路径，systemd 单元使用：

```ini
Environment=GPUSTAT4CLUSTER_CONFIG=/etc/gpustat4cluster/config.toml
EnvironmentFile=-/etc/gpustat4cluster/gpustat4cluster.env
```

`/etc/gpustat4cluster/gpustat4cluster.env` 是可选文件，安装脚本会初始化注释示例。Release 二进制已包含 KCP transport 支持；如需启用，请取消注释：

```bash
GPUSTAT4CLUSTER_ENABLE_KCP=1
GPUSTAT4CLUSTER_STATIC_NODES=server-a:30000,server-b:30000
```

`GPUSTAT4CLUSTER_COLLECTOR=mock` 和 `GPUSTAT4CLUSTER_FORCE_MOCK=1` 仅用于 smoke/stress 验证，不建议在生产 GPU 节点启用。

常用检查命令：

```bash
systemctl status gpustat4cluster-server.service
journalctl -u gpustat4cluster-server.service -n 100 --no-pager
systemctl status gpustat4cluster-client.service
journalctl -u gpustat4cluster-client.service -n 100 --no-pager
```

服务端启动时会校验配置、端口范围和多播地址；生产 NVML 初始化失败时会以 FATAL 日志退出。若系统只有 `libnvidia-ml.so.1` 或 `libnvidia-ml.so.<driver-version>`，请在 `[runtime] nvml_lib_path` 中配置真实 NVIDIA driver runtime library 路径，不要使用 CUDA `stubs/libnvidia-ml.so`。

客户端 backend 启动时会输出结构化启动信息，包含配置路径、KCP 开关状态、discovery multicast 地址、静态节点数量、TCP local API 地址和 UDS 路径。生产建议用 UDS 连接 CLI/frontend，TCP 仍保留为开发 fallback。可用以下环境变量增强发现体验：

```bash
GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:30000,127.0.0.1:30001
GPUSTAT4CLUSTER_ENABLE_KCP=1
GPUSTAT4CLUSTER_BACKEND_SOCKET=/tmp/gpustat4cluster.sock
```

`GPUSTAT4CLUSTER_STATIC_NODES` 会自动 trim 空白并按地址去重；非法地址只会产生 warning 并被跳过，不会阻止 backend 启动。multicast discovery 无结果但 static nodes 有值时，backend 会继续使用 static nodes；两者都为空时 backend 仍会启动 local API，CLI 表格为空，`gpustat4cluster-client --json` 返回 `{"nodes":[]}`。

自动化验证可使用 JSON 输出：

```bash
gpustat4cluster-client --backend-socket /tmp/gpustat4cluster.sock --json
```

JSON 顶层为 `nodes`，每个 GPU 行包含 `index`、`util`、`mem_used_mb`、`mem_total_mb` 和可选 `processes` 字段，便于 smoke/stress 脚本解析。

### 服务端配置校验

server 启动时会拒绝以下配置：

- `connecting.port_range` 任一端为 `0`，或起始端口大于结束端口。
- `connecting.multicast_addr` 无法解析，或 IP 不是 multicast 地址。
- `connecting.connection_idle_timeout = 0`。
- `connecting.discover_wait_secs = 0`。
- `services.cache_ttl_ms = 0`。
- `log.max_size` 为空、为 `0`、数字不可解析或单位不支持。

`log.max_size` 当前支持 `b`、`kb`/`kib`、`mb`/`mib`、`gb`/`gib`，也支持不写单位表示 bytes，例如 `5mb`、`512kb`、`1048576`。本轮 server 已提供 parser 和配置校验；真正文件滚动写入会在后续轮次接入。

server 日志使用结构化 JSON。启动首行事件为 `startup`，包含 `version`、`protocol_version`、`bind_port`、`kcp_enabled`、`collector_mode`、`cache_ttl_ms` 和初始 cache metrics。启动前 NVML 初始化失败会输出 FATAL；运行期 query 错误、多播错误和 KCP session accept/close/error 也会带 `event` 字段输出。

## 本地验证

发布或安装前可先运行本地 smoke，验证当前 TCP/JSON bootstrap、server query、client-backend 和 CLI 渲染链路没有被打断：

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
bash -n scripts/smoke-local.sh
scripts/smoke-local.sh
```

详细说明见 [smoke-test.md](smoke-test.md)。发布前检查清单见 [release-checklist.md](release-checklist.md)，打包产物检查见 [packaging-smoke.md](packaging-smoke.md)。该 smoke 不代表真实 NVML 已完成验证。

## 回滚

1. 停止服务：

```bash
sudo systemctl disable --now gpustat4cluster-server.service gpustat4cluster-client.service
```

2. 恢复上一版本二进制（建议保留旧 tar 包或通过包管理系统版本锁定）。
3. `sudo systemctl daemon-reload` 后重新启动对应服务。

## 卸载

```bash
sudo systemctl disable --now gpustat4cluster-server.service gpustat4cluster-client.service || true
sudo rm -f /usr/local/bin/gpustat4cluster-server
sudo rm -f /usr/local/bin/gpustat4cluster-client-backend
sudo rm -f /usr/local/bin/gpustat4cluster-client
sudo rm -f /etc/systemd/system/gpustat4cluster-server.service
sudo rm -f /etc/systemd/system/gpustat4cluster-client.service
sudo systemctl daemon-reload
```

保留配置：`/etc/gpustat4cluster/config.toml`（如需彻底删除可手动移除）。
保留运行用户：`gpustat4cluster`（如需彻底删除，请先确认没有其他版本或手动部署仍在使用）。

---

# gpustat4cluster Operations Install Guide (English)

This guide covers installing gpustat4cluster from release artifacts and preparing the runtime layout.

Online install:

```bash
curl -fsSL https://raw.githubusercontent.com/TING-HiuYu/gpustat4cluster/main/scripts/install.sh -o install.sh
bash install.sh --role server --version latest --libc gnu
```

Supported roles are `server`, `client`, and `both`. By default the script downloads from `TING-HiuYu/gpustat4cluster`, installs binaries into `/usr/local/bin`, initializes `/etc/gpustat4cluster`, creates the `gpustat4cluster` system user/group, and enables the relevant systemd services. Use `REPO=owner/name` for forks or mirrors.

Dry run:

```bash
bash scripts/install.sh --role server --version v0.1.0 --dry-run
REPO=TING-HiuYu/gpustat4cluster bash scripts/install.sh --role both --version v0.1.0 --libc gnu --dry-run
```

Release asset naming:

```text
gpustat4cluster-<server|client>-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz
```

Offline install:

1. Download the matching server/client artifacts on an online machine.
2. Copy them to the target host.
3. Run the installer with `LOCAL_TARBALL_DIR=/path/to/tarballs` and the desired role/version.

Runtime notes:

- Client packages provide `gpustat4cluster-client-backend` and `gpustat4cluster-client`.
- If the host does not already provide `gpustat`, the package may create a convenience symlink from `gpustat` to `gpustat4cluster-client`.
- KCP is the preferred low-latency transport. If UDP is blocked, set the client config protocol to `tcp` for compatibility.
- The client frontend talks to the backend through a Unix domain socket under `/run/gpustat4cluster` by default.
