# RC status

[中文](#rc-status) | [English](#rc-status-english)

本页记录 RC 发布验证闭环状态，便于维护者判断哪些项目已经自动化覆盖，哪些仍需要真实环境签核。

## 已完成

- Bootstrap 基线：`scripts/smoke-local.sh` 覆盖临时 config、server TCP/JSON query、client-backend local API 和 CLI mock row 渲染。
- Common payload 接入基线：bootstrap smoke 与 KCP smoke 均断言 CLI 输出 `HOSTNAME`、mock hostname、`87%`、`1234/16384`。
- KCP 回归入口：`scripts/smoke-kcp-loopback.sh` 覆盖 KCP loopback mock row，`scripts/stress-kcp-loopback.sh` 提供并发查询 baseline。
- 发布工程化：release workflow 生成 server/client x amd64/arm64 x gnu/musl tarball，server/client-backend release 构建启用 `kcp-transport`，systemd unit 使用可选 env file。
- 自动化验证：CI 覆盖 workspace tests、KCP feature tests、KCP+NVML check、locked build 和 package artifact smoke。

## RC 自动化 Gate

RC 候选发布前应完成以下自动化命令：

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
cargo test --workspace
cargo test -p server --features kcp-transport
cargo test -p gpustat4cluster-client-backend --features kcp-transport
cargo check -p server --features "kcp-transport nvml"
bash -n scripts/install.sh
bash -n scripts/smoke-local.sh
bash -n scripts/smoke-kcp-loopback.sh
bash -n scripts/stress-kcp-loopback.sh
bash -n scripts/smoke-package-artifact.sh
scripts/smoke-local.sh
scripts/smoke-kcp-loopback.sh
scripts/stress-kcp-loopback.sh
scripts/smoke-package-artifact.sh
```

通过含义：

- bootstrap chain 可启动并渲染 mock GPU row。
- KCP loopback chain 可启动并渲染 mock GPU row。
- stress baseline 在本地 mock collector 下 `failure=0`。
- 本地 release tarball 布局包含二进制、systemd unit 和 `gpustat4cluster.env.example`。

## 仍需真实环境验证

- NVML 真机：在有 NVIDIA GPU/driver 的节点上确认真实 collector 返回 GPU 数量、利用率和显存字段，并与 `nvidia-smi` 基本一致。
- systemd 真机：用 release tarball 和 `scripts/install.sh` 验证 system user/group、配置文件、env file、daemon-reload、enable/start、restart policy 和 journal 日志。
- GitHub Release 真跑：打 RC tag 后确认 8 个 release tarball 均构建并上传，命名与 `scripts/install.sh` 下载 URL 完全一致。
- 7-day soak：在真实或准真实节点上运行 server/client-backend，观察内存、fd、日志增长、重连和错误率。
- 多节点集群：至少两台节点验证 `GPUSTAT4CLUSTER_STATIC_NODES` 逗号列表、KCP 跨节点连通、CLI 多节点表格和 degraded 节点展示。

## 非阻塞观察项

- 默认/feature 构建如仍出现 transport dead-code warnings，应在 release note 或看板记录；RC 前建议执行 `RUSTFLAGS="-D warnings" cargo check --workspace` 作为清理目标。
- 本地 stress 暂无性能门槛，当前只作为本地趋势 baseline；后续可基于稳定测试机补 p95/p99 或吞吐门槛。

---

# RC status (English)

This page tracks the release-candidate validation loop so the maintainers can see what is automated and what still requires real-environment sign-off.

Completed automation includes:

- Bootstrap smoke for temporary config, server TCP/JSON query, client-backend local API, and CLI mock rendering.
- Common payload integration checks.
- KCP loopback smoke and stress baselines.
- Release packaging for server/client artifacts.
- CI coverage for workspace tests, KCP feature tests, KCP+NVML checks, locked builds, and packaging smoke.

Required RC gate:

Run workspace tests, KCP feature tests, KCP+NVML checks, shell syntax checks, local smoke, KCP loopback smoke, KCP stress, and packaging smoke before an RC candidate.

Still requiring real-environment validation:

- Real NVML on NVIDIA GPU hosts.
- systemd install/start/restart behavior on target hosts.
- Real GitHub Release or prerelease attachment flow.
- 7-day soak for memory, file descriptors, log growth, reconnect behavior, and error rate.
- Multi-node cluster validation across at least two machines.
