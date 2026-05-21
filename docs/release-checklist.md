# Release preflight checklist

[中文](#release-preflight-checklist) | [English](#release-preflight-checklist-english)

发布前按本清单逐项确认。除特别说明外，Rust 命令在当前集群环境中需要先加载 module：

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
```

## 代码与测试

- `cargo test -p common`
- `cargo test --workspace`
- `cargo check --workspace`
- `cargo check -p server --features nvml`
- `cargo test -p server --features kcp-transport`
- `cargo test -p gpustat4cluster-client-backend --features kcp-transport`
- `cargo check -p server --features "kcp-transport nvml"`
- `cargo build --locked -p server`
- `cargo build --locked -p gpustat4cluster-client-backend`
- `bash -n scripts/install.sh`
- `bash -n scripts/smoke-local.sh`
- `scripts/smoke-local.sh`
- `bash -n scripts/smoke-kcp-loopback.sh`
- `scripts/smoke-kcp-loopback.sh`
- `bash -n scripts/stress-kcp-loopback.sh`
- `scripts/stress-kcp-loopback.sh`
- `bash -n scripts/smoke-multinode-local.sh`
- `scripts/smoke-multinode-local.sh`
- `bash -n scripts/stress-multinode-local.sh`
- `scripts/stress-multinode-local.sh`
- `bash -n scripts/smoke-package-artifact.sh`
- `scripts/smoke-package-artifact.sh`

## RC gate

RC 候选发布前必须满足：

- all tests：workspace tests、KCP feature tests、KCP+NVML check 均通过。
- `scripts/smoke-local.sh` 通过，CLI 能渲染 mock row。
- `scripts/smoke-kcp-loopback.sh` 通过，KCP loopback 能渲染 mock row。
- `scripts/stress-kcp-loopback.sh` 通过，`failure=0`，并记录 requests/concurrency/success/failure/elapsed_ms。
- `scripts/smoke-multinode-local.sh` 通过，3 个真实 server + 2 个真实 client-backend 的 JSON/table 多节点断言通过。
- `scripts/stress-multinode-local.sh` 通过，多节点真实 backend 并发 CLI query `failure=0`。
- `scripts/smoke-package-artifact.sh` 通过，本地 tarball 布局包含 binary、systemd service 和 env example。
- manual NVML 真机验证完成或在 RC 状态页明确标为待验证。
- manual systemd 真机验证完成或在 RC 状态页明确标为待验证。
- GitHub Release tag workflow 真跑成功，8 个 tarball 均上传，或在 RC 状态页明确标为待验证。

协议/传输检查需要确认：

- `cargo test -p common` 覆盖 version mismatch、unknown frame type、payload length mismatch、empty snapshot、process list roundtrip。
- server/client-backend 的 `kcp-transport` feature tests 均通过。
- `scripts/smoke-kcp-loopback.sh` 覆盖 KCP loopback 或在 KCP/static/mock 开关未接入时明确 skip 且状态码为 0。
- `scripts/stress-kcp-loopback.sh` 作为本地趋势基线执行，记录 requests/concurrency/success/failure/elapsed_ms。
- 如 multicast 发现无节点，使用 `GPUSTAT4CLUSTER_STATIC_NODES` 重跑 KCP smoke 以区分网络发现问题和 KCP frame/payload 问题。
- TCP/JSON bootstrap fallback smoke 仍通过，用于隔离 CLI/local API 回归。
- 执行或人工复核 `docs/failure-drills.md` 中协议/传输故障演练：server restart、client reconnect、packet loss/jitter/timeout、version mismatch、bad magic/corrupted frame、static nodes fallback。

smoke 需要确认输出包含：

- `smoke passed`
- `mock-smoke-node`
- 至少一行 mock GPU utilization/memory row，例如 `87%` 和 `1234/16384`

KCP loopback smoke 需要确认：

- KCP/mock/force mock/static nodes 任一开关尚未接入时，脚本输出 `skip` 且状态码为 0。
- 开关已接入时，脚本在 `GPUSTAT4CLUSTER_ENABLE_KCP=1`、`GPUSTAT4CLUSTER_COLLECTOR=mock`、`GPUSTAT4CLUSTER_FORCE_MOCK=1`、`GPUSTAT4CLUSTER_STATIC_NODES=...` 下启动 server/client-backend。
- CLI 输出 `mock-smoke-node`。
- CLI 输出 mock GPU row，例如 `87%` 和 `1234/16384`。
- mock process username 当前为 `mock-user`，可用于人工排障；preflight 自动断言聚焦 hostname/util/memory。
- degraded response 只作为 secondary troubleshooting signal，不作为 KCP smoke 主验收。

本地 stress baseline 需要确认：

- `scripts/stress-kcp-loopback.sh` 使用 mock collector 与 static nodes，不依赖真实 NVML。
- 输出 `requests`、`concurrency`、`success`、`failure`、`elapsed_ms`。
- `failure=0`。
- 暂不设置性能门槛，先作为本地趋势基线。

本地 multinode baseline 需要确认：

- `scripts/smoke-multinode-local.sh` 启动 3 个真实 server，分别使用不同 config port、query addr、mock hostname 和 mock GPU 数量。
- 脚本启动 2 个真实 client-backend，分别设置 `GPUSTAT4CLUSTER_BACKEND_ADDR` 和不同 `GPUSTAT4CLUSTER_STATIC_NODES`；CLI 使用 `--backend-addr` 查询对应 backend。
- JSON 断言覆盖 `meta.node_count`、hostnames、GPU utilization/memory、process username/pid/memory。
- 表格断言覆盖 `HOSTNAME`、hostname、GPU rows 和 `proc <user>` 行。
- `scripts/stress-multinode-local.sh` 输出 requests/concurrency/success/failure/elapsed_ms，且 `failure=0`。
- cleanup 后 backend local API ports、server query ports 和 server KCP UDP ports 均释放。

本地 packaged artifact smoke 需要确认：

- `scripts/smoke-package-artifact.sh` 不使用 sudo，不写 `/etc`。
- release 版 server/client-backend 构建时启用 `kcp-transport`。
- server artifact 内 `/usr/local/bin/gpustat4cluster-server` 可执行。
- client artifact 内 `/usr/local/bin/gpustat4cluster-client-backend` 和 `/usr/local/bin/gpustat4cluster-client` 可执行。
- artifact 内包含两份 systemd unit 和 `etc/gpustat4cluster/gpustat4cluster.env.example`。
- env example 文档化 `GPUSTAT4CLUSTER_ENABLE_KCP` 与 `GPUSTAT4CLUSTER_STATIC_NODES`，但默认不强制启用 KCP。

Failure drill 需要确认：

- `docs/failure-drills.md` 中每个协议/传输 drill 都记录执行日期、环境、通过/失败结论。
- 暂无自动化 netem 脚本时，packet loss/jitter drill 可以记录为 manual run 或 deferred，并说明原因。
- 多节点/多连接 drill 若无真实集群环境，应标记为 pending cluster validation，而不是通过。

warnings-free build 检查：

- 发布前建议执行 `RUSTFLAGS="-D warnings" cargo check --workspace`。
- KCP/NVML 组合建议执行 `RUSTFLAGS="-D warnings" cargo check -p server --features "kcp-transport nvml"`。
- 当前如仍有 dead-code warnings，应在 release notes 记录为非阻塞/待清理项。

## CI

新增 `.github/workflows/ci.yml` 用于 push/PR 快速反馈。选择新增 CI，而不是改 release workflow，是为了把常规测试与 tag 产物发布解耦：CI 覆盖每次代码变更，release workflow 继续专注构建和上传 release artifacts。

CI 必跑项：

- `cargo test --workspace`
- `cargo build --locked -p server`
- `cargo build --locked -p gpustat4cluster-client-backend`
- `cargo test -p server --features kcp-transport`
- `cargo test -p gpustat4cluster-client-backend --features kcp-transport`
- `cargo check -p server --features "kcp-transport nvml"`
- `bash scripts/smoke-package-artifact.sh`

CI 暂不运行 `scripts/stress-kcp-loopback.sh`，避免端口占用、调度抖动和机器负载导致 flaky；stress 作为本地 release preflight 项执行。

## 安装脚本 dry-run

确认 release asset URL 和角色选择正确：

```bash
bash scripts/install.sh --role server --version v0.0.0 --dry-run
bash scripts/install.sh --role client --version v0.0.0 --dry-run
bash scripts/install.sh --role both --version v0.0.0 --dry-run
bash scripts/install.sh --role server --version v0.0.0 --libc musl --dry-run
bash scripts/install.sh --role client --version v0.0.0 --libc musl --dry-run
```

需要在 amd64 与 arm64 环境各确认一次自动架构识别；没有 arm64 机器时，可至少在 GitHub Actions release 矩阵中确认 arm64 artifact 命名。

## GitHub Actions tag release

打 tag 后检查：

- release workflow 使用 `--features kcp-transport` 构建 server 和 client-backend。
- `release` workflow 的 `server/client x amd64/arm64 x gnu/musl` 8 个 build job 全部成功。
- GitHub Release 页面存在 8 个 tarball。
- tarball 命名符合 `gpustat4cluster-<server|client>-<tag>-linux-<amd64|arm64>-<gnu|musl>.tar.gz`。
- server tarball 内包含 `/usr/local/bin/gpustat4cluster-server`。
- client tarball 内包含 `/usr/local/bin/gpustat4cluster-client-backend` 和 `/usr/local/bin/gpustat4cluster-client`。
- tarball 内 systemd unit 安装路径为 `/etc/systemd/system/`。
- tarball 内包含 `/etc/gpustat4cluster/gpustat4cluster.env.example`。

本地 packaged artifact smoke 见 [packaging-smoke.md](packaging-smoke.md)。

## systemd 真机验证

在 systemd 主机上验证：

- `install.sh --role server --version <tag>` 创建 `gpustat4cluster` system user/group。
- `/etc/gpustat4cluster/config.toml` 初始化且权限合理。
- `systemctl daemon-reload` 成功。
- `systemctl enable --now gpustat4cluster-server.service` 成功。
- `systemctl enable --now gpustat4cluster-client.service` 成功。
- `systemctl status` 中服务使用 `GPUSTAT4CLUSTER_CONFIG=/etc/gpustat4cluster/config.toml`。
- `systemctl show gpustat4cluster-server.service -p EnvironmentFiles` 或 unit 内容显示 `EnvironmentFile=-/etc/gpustat4cluster/gpustat4cluster.env`。
- `/etc/gpustat4cluster/gpustat4cluster.env` 存在且默认只包含注释示例，不强制启用 KCP。
- 异常退出后 `Restart=on-failure` 生效，且 `StartLimit*` 避免启动风暴。
- `journalctl -u gpustat4cluster-server.service` 和 `journalctl -u gpustat4cluster-client.service` 有可读启动日志。

## NVML 真机验证

详细步骤见 [nvml-validation.md](nvml-validation.md)。

在有 NVIDIA 驱动和 GPU 的 server 节点上验证：

- 不设置 `GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING` 时 server 正常启动。
- server query 返回 `ok:true` 或等价的成功 GPU payload。
- GPU 数量、利用率、显存字段与 `nvidia-smi` 基本一致。
- 无 GPU 或 NVML 不可用机器返回可解释 degraded 错误，而不是崩溃。
- 非 root 用户 `gpustat4cluster` 可读取所需 NVML 信息。

## 已知边界

- `scripts/smoke-local.sh` 验证 bootstrap TCP/JSON 与 CLI 渲染，不验证 KCP 真 socket。
- `scripts/smoke-kcp-loopback.sh` 是 KCP 真 socket loopback 验证；KCP/mock/force mock/static nodes 开关未全部接入时允许 skip。
- KCP 跨节点验证需要单独补充。
- 本地 multicast 在部分 CI/容器环境中可能无 rows，smoke 使用 fake backend fixture 保证 CLI row 断言稳定。

---

# Release preflight checklist (English)

Use this checklist before publishing a release or prerelease. In the current cluster environment, load the Rust module first:

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
```

Core checks:

- Run `cargo test --workspace` and targeted package tests.
- Run KCP feature tests for server and client-backend.
- Run NVML compile checks with the `nvml` feature.
- Run shell syntax checks for install and smoke scripts.
- Run local smoke, KCP loopback smoke, KCP stress, multinode smoke, multinode stress, and packaging smoke.

RC requirements:

- All automated tests pass.
- Local bootstrap smoke renders a mock GPU row.
- KCP loopback smoke renders a mock GPU row.
- Stress baselines report `failure=0`.
- Multinode smoke/stress pass with real local server and client-backend processes.
- Packaging smoke confirms binaries, service files, and config examples.
- Real NVML, real systemd, and GitHub release attachment validation are completed or explicitly marked as pending in RC status.

Manual review should also cover failure drills from `docs/failure-drills.md`, including server restart, client reconnect, packet loss, version mismatch, corrupted frames, and static-node fallback.
