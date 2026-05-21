# Local multinode smoke

[中文](#local-multinode-smoke) | [English](#local-multinode-smoke-english)

本页说明本地多 server / 多 backend / 多 CLI integration 验证。脚本不依赖真实 NVML，不需要 sudo/systemd，不写 `/etc`。

## 当前实现模式

`scripts/smoke-multinode-local.sh` 和 `scripts/stress-multinode-local.sh` 现在使用真实进程链路：

- 构建 `server --features "kcp-transport mock-nvml"`，让本地测试显式使用 mock NVML provider。
- 构建 `gpustat4cluster-client-backend --features kcp-transport` 和 CLI。
- 启动 3 个真实 `target/debug/server`，每个 server 使用独立临时 config、不同 `GPUSTAT4CLUSTER_QUERY_ADDR`、不同 KCP port 和不同 mock hostname。
- 每个 server 通过 `GPUSTAT4CLUSTER_COLLECTOR=mock`、`GPUSTAT4CLUSTER_FORCE_MOCK=1`、`GPUSTAT4CLUSTER_MOCK_HOSTNAME=...`、`GPUSTAT4CLUSTER_MOCK_GPU_COUNT=...` 产生确定性 NVML 形状数据。
- 启动 2 个真实 `target/debug/gpustat4cluster-client-backend`，分别设置不同 `GPUSTAT4CLUSTER_BACKEND_ADDR`，并通过 `GPUSTAT4CLUSTER_STATIC_NODES` 连接不同 server 集合。
- CLI 使用 `--backend-addr` 分别查询两个 backend，不再需要 fixture backend 或 fixed-port proxy。

mock provider 只在 `test` 或 `mock-nvml` feature 下编译；不带 `mock-nvml` 的生产构建不会因为这些 env 自动切到 mock。

## Smoke 命令

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
bash -n scripts/smoke-multinode-local.sh
scripts/smoke-multinode-local.sh
```

默认启动：

- 3 个 server：`mn-node-a`、`mn-node-b`、`mn-node-c`。
- 2 个真实 client-backend：一个查询全量 3 nodes，一个查询 subset 2 nodes。
- CLI 对两个 backend 分别执行 JSON 和表格查询。

验证字段：

- JSON `meta.node_count`。
- JSON hostnames：`mn-node-a`、`mn-node-b`、`mn-node-c`。
- GPU rows：`87% 1234/16384`、`80% 1746/17408`、`73% 2258/18432`。
- Process fields：`mock-user-*`、`mock-helper-*`、`pid`、`used_memory_mb`。
- 表格输出包含 `HOSTNAME`、hostname、GPU utilization/memory 和 `proc mock-user-*` 行。

## Stress 命令

```bash
bash -n scripts/stress-multinode-local.sh
scripts/stress-multinode-local.sh
```

默认行为：

- 启动 3 个真实 server。
- 启动 2 个真实 client-backend。
- 并发执行 CLI `--json --backend-addr ...` 查询，交替命中两个 backend。
- 每次断言 JSON 至少包含 `mn-node-a`、`mn-node-b` 和 mock process 字段。
- 输出 requests/concurrency/success/failure/elapsed_ms。

可覆盖变量：

```bash
GPUSTAT4CLUSTER_MULTINODE_STRESS_REQUESTS=80 scripts/stress-multinode-local.sh
GPUSTAT4CLUSTER_MULTINODE_STRESS_CONCURRENCY=16 scripts/stress-multinode-local.sh
GPUSTAT4CLUSTER_MULTINODE_BACKEND_A_ADDR=127.0.0.1:4543 scripts/smoke-multinode-local.sh
GPUSTAT4CLUSTER_MULTINODE_BACKEND_B_ADDR=127.0.0.1:4544 scripts/smoke-multinode-local.sh
```

## Cleanup

两个脚本都会记录 server 和 client-backend PID，并在退出时逐个 TERM/KILL。cleanup 后会等待以下端口释放：

- backend local API addr：默认 `127.0.0.1:4523`、`127.0.0.1:4524`
- server query addr：默认 `127.0.0.1:4922`、`127.0.0.1:4923`、`127.0.0.1:4924`
- server KCP UDP ports：默认 `39800`、`39820`、`39840`

失败时脚本会输出 server/backend log 尾部，便于定位是启动失败、端口占用、KCP/static nodes 回归、JSON schema 回归还是 CLI 渲染回归。

## 覆盖边界

这些脚本覆盖本地真实 KCP socket、真实 client-backend local API、多个 backend 地址覆盖、CLI JSON/table 查询、多节点 payload 聚合和 mock NVML 数据格式。仍不覆盖 NVML 真机读取、systemd 部署、跨机器网络或 GitHub Release workflow 真跑。

---

# Local multinode smoke (English)

This page describes the local integration checks for multiple servers, multiple client backends, and multiple CLI callers. The scripts do not require real NVML, sudo, systemd, or writes under `/etc`.

Current model:

- `scripts/smoke-multinode-local.sh` and `scripts/stress-multinode-local.sh` start real local server and client-backend processes.
- Servers are built with `kcp-transport mock-nvml` so the mock NVML provider is explicit and deterministic.
- Three local servers use isolated temporary configs, distinct query addresses, distinct KCP ports, and distinct mock hostnames.
- Each server uses mock environment variables such as `GPUSTAT4CLUSTER_COLLECTOR=mock`, `GPUSTAT4CLUSTER_FORCE_MOCK=1`, `GPUSTAT4CLUSTER_MOCK_HOSTNAME`, and `GPUSTAT4CLUSTER_MOCK_GPU_COUNT`.
- Two real client backends use different frontend API addresses and connect to different server sets through `GPUSTAT4CLUSTER_STATIC_NODES`.
- The CLI queries both backends in JSON and table modes; no fixture backend or fixed-port proxy is required.

Smoke command:

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
bash -n scripts/smoke-multinode-local.sh
scripts/smoke-multinode-local.sh
```

Stress command:

```bash
bash -n scripts/stress-multinode-local.sh
scripts/stress-multinode-local.sh
```

The scripts validate JSON node counts, mock hostnames, GPU rows, process fields, and table output. On failure they print the tail of server/backend logs to help identify startup failures, port conflicts, KCP/static-node regressions, JSON schema regressions, or CLI rendering regressions.

Coverage boundary:

These scripts cover local KCP sockets, the real client-backend local API, multiple backend addresses, CLI JSON/table queries, multi-node payload aggregation, and mock NVML data shape. They do not cover real NVML, systemd deployment, cross-machine networking, or a real GitHub Release workflow run.
