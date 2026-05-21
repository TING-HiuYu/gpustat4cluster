# 本地 smoke 验证

[中文](#本地-smoke-验证) | [English](#local-smoke-validation-english)

本 smoke 用于验证当前 TCP/JSON bootstrap、临时配置加载、server query 端口、client-backend 本地 API 和 CLI 渲染没有被打断。它不依赖真实 NVML，默认通过 `GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING=1` 走 degraded 友好路径，并用临时 fake backend 提供确定性的 mock GPU row。

## 前置环境

在当前集群环境中，编译或测试 Rust 前需要先加载 Rust module：

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
```

建议先跑 workspace 测试：

```bash
cargo test --workspace
```

## Bootstrap smoke

```bash
bash -n scripts/smoke-local.sh
scripts/smoke-local.sh
```

脚本会自动：

- 创建临时 `config.toml`，不读写 `/etc/gpustat4cluster`
- 设置 `GPUSTAT4CLUSTER_CONFIG` 指向临时配置
- 设置 `GPUSTAT4CLUSTER_QUERY_ADDR=127.0.0.1:4622`，避免 server 默认 query 端口冲突
- 构建 debug 版 `server`、`gpustat4cluster-client-backend` 和 `gpustat4cluster`
- 启动 server 并等待 query 端口可用
- 直接查询 server TCP/JSON bootstrap 响应
- 启动真实 client-backend 并确认本地 API 端口可用
- 关闭真实 client-backend，启动临时 fake backend
- 调用 CLI 并断言 mock GPU row 已渲染
- 退出时清理后台进程和临时目录

## 可覆盖变量

```bash
GPUSTAT4CLUSTER_SMOKE_QUERY_ADDR=127.0.0.1:4722 scripts/smoke-local.sh
GPUSTAT4CLUSTER_SMOKE_PORT_START=39300 GPUSTAT4CLUSTER_SMOKE_PORT_END=39310 scripts/smoke-local.sh
GPUSTAT4CLUSTER_SMOKE_MULTICAST_ADDR=239.0.0.1:4500 scripts/smoke-local.sh
GPUSTAT4CLUSTER_SMOKE_MOCK_HOSTNAME=mock-node-a scripts/smoke-local.sh
```

注意：单节点 `scripts/smoke-local.sh` 默认仍使用 `127.0.0.1:4521`，如果该端口被占用，smoke 会在启动前失败并提示端口冲突。需要并行启动多个 client-backend 时，可用 `GPUSTAT4CLUSTER_BACKEND_ADDR` 覆盖 backend 监听地址，并用 CLI `--backend-addr` 指向对应 backend。

## 验收含义

通过条件：

- server query 端口可用
- server TCP/JSON 响应包含 JSON `ok` 字段
- client-backend 本地 API 可用
- CLI 输出包含 `HOSTNAME` 表头
- CLI 输出包含 mock hostname，默认 `mock-smoke-node`
- CLI 输出包含至少一行 mock GPU utilization/memory row，默认 `87%` 和 `1234/16384`

真实 client-backend 启动阶段仍会经过本地 multicast discovery；如果本地 multicast 环境没有把 server announce 送到 client-backend，不影响最终 CLI 断言，因为脚本会切换到 fake backend 提供稳定 fixture。

CLI 也支持机器可读输出：

```bash
gpustat4cluster-client --json
```

JSON schema 当前为：

```json
{
  "meta": {
    "status": "ok|empty|unknown",
    "timestamp_ms": 1700000000000,
    "node_count": 1,
    "errors": []
  },
  "nodes": [
    {
      "hostname": "mock-smoke-node",
      "stale": false,
      "error": null,
      "gpus": [
        {
          "index": 0,
          "util": 87,
          "mem_used_mb": 1234,
          "mem_total_mb": 16384,
          "processes": null
        }
      ]
    }
  ]
}
```

当 discovery/static/KCP 都没有产生节点时，backend 仍启动 local API，表格输出保持表头，`--json` 返回稳定空 schema：`{"meta":{"status":"empty",...},"nodes":[]}`。watch 模式目前只做全量重绘和 ANSI 清屏，没有启用 raw mode；Ctrl-C 退出不会留下需要恢复的 terminal 状态。

本地并行启动多个 client-backend 时，可覆盖 local API 地址：

```bash
GPUSTAT4CLUSTER_BACKEND_ADDR=127.0.0.1:4521 gpustat4cluster-client-backend
GPUSTAT4CLUSTER_BACKEND_ADDR=127.0.0.1:4522 gpustat4cluster-client-backend
gpustat4cluster-client --backend-addr 127.0.0.1:4521 --json
gpustat4cluster-client --backend-addr 127.0.0.1:4522 --json
```

`GPUSTAT4CLUSTER_BACKEND_ADDR` 是 backend 和 CLI 共用的推荐 env 名；兼容旧名 `GPUSTAT4CLUSTER_LOCAL_API_ADDR`。如果一个 backend 需要查询多个 KCP/mock server，可设置：

```bash
GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:39400,127.0.0.1:39401
```

多节点 JSON 的 `meta.node_count` 会等于节点数量，`nodes` 按连接 ID 稳定排序，节点内的 hostname/GPU/process 字段可用于区分不同 backend/server。

## KCP loopback smoke

KCP loopback smoke 是可选验证，脚本路径：

```bash
bash -n scripts/smoke-kcp-loopback.sh
scripts/smoke-kcp-loopback.sh
```

默认使用以下验证开关：

```bash
GPUSTAT4CLUSTER_ENABLE_KCP=1
GPUSTAT4CLUSTER_COLLECTOR=mock
GPUSTAT4CLUSTER_FORCE_MOCK=1
GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:<server-port>
```

如果当前代码尚未接入 KCP、mock collector、force mock 或 static nodes 开关，脚本会输出明确的 `skip` 并以成功状态退出，不阻断 release preflight。可覆盖变量：

```bash
GPUSTAT4CLUSTER_KCP_ENV_NAME=GPUSTAT4CLUSTER_ENABLE_KCP scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_ENV_VALUE=1 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_NAME=GPUSTAT4CLUSTER_COLLECTOR scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_COLLECTOR_ENV_VALUE=mock scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_NAME=GPUSTAT4CLUSTER_FORCE_MOCK scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_FORCE_MOCK_ENV_VALUE=1 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_STATIC_NODES_ENV_NAME=GPUSTAT4CLUSTER_STATIC_NODES scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_STATIC_NODES=127.0.0.1:39400 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_MOCK_HOSTNAME=mock-smoke-node scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_SMOKE_FORCE=1 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_QUERY_ADDR=127.0.0.1:4822 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_PORT_START=39500 GPUSTAT4CLUSTER_KCP_PORT_END=39510 scripts/smoke-kcp-loopback.sh
GPUSTAT4CLUSTER_KCP_MULTICAST_ADDR=239.0.0.1:4700 scripts/smoke-kcp-loopback.sh
```

KCP loopback 通过条件：

- KCP/mock/force mock/static nodes 任一开关未接入时：输出 skip，状态码为 0。
- 开关已接入时：server 与 client-backend 在 `GPUSTAT4CLUSTER_ENABLE_KCP=1` 下启动成功。
- CLI query 成功，并渲染 mock hostname，默认 `mock-smoke-node`。
- CLI 输出包含 mock GPU utilization/memory row，默认 `87%` 与 `1234/16384`。
  其中 hostname 由脚本设置 `HOSTNAME=mock-smoke-node` 稳定化；mock process username 当前为 `mock-user`，不作为本脚本必需断言。

无 NVML 环境的预期结果：bootstrap smoke 通过 fake backend 断言 mock row；KCP loopback 通过 mock collector 断言真 KCP loopback 下的 mock row。degraded response 仅作为排障辅助输出，不再作为 KCP smoke 的主验收条件。

## 当前边界

这个 smoke 只覆盖当前 TCP/JSON bootstrap 和 common payload 接入基线，不代表真实 NVML、跨节点 multicast、rkyv 零拷贝传输或 systemd 安装路径已经完成验证。

KCP transport 当前已纳入真 socket loopback 验证入口；`scripts/smoke-kcp-loopback.sh` 在 KCP/mock/force mock/static nodes 开关未全部接入时会 skip。

## KCP stress baseline

`scripts/stress-kcp-loopback.sh` 是轻量本地压测/回归基线。它不依赖真实 NVML，使用 KCP loopback、mock collector 和 static nodes，启动 server/client-backend 后并发执行多次 CLI query，并输出请求总数、并发度、成功数、失败数和耗时。

```bash
bash -n scripts/stress-kcp-loopback.sh
scripts/stress-kcp-loopback.sh
```

可覆盖变量：

```bash
GPUSTAT4CLUSTER_STRESS_REQUESTS=64 scripts/stress-kcp-loopback.sh
GPUSTAT4CLUSTER_STRESS_CONCURRENCY=16 scripts/stress-kcp-loopback.sh
GPUSTAT4CLUSTER_STRESS_PORT_START=39700 GPUSTAT4CLUSTER_STRESS_PORT_END=39710 scripts/stress-kcp-loopback.sh
GPUSTAT4CLUSTER_STRESS_STATIC_NODES=127.0.0.1:39700 scripts/stress-kcp-loopback.sh
```

当前不设置严格性能门槛；只要求所有 query 成功并渲染 mock row。该脚本暂不放入 CI，避免本地端口、调度抖动或运行时负载造成 flaky。

## Local multinode smoke

多节点本地 integration 入口：

```bash
bash -n scripts/smoke-multinode-local.sh
scripts/smoke-multinode-local.sh
```

当前脚本启动 3 个真实 server，每个 server 使用不同临时 config、不同 `GPUSTAT4CLUSTER_QUERY_ADDR`、不同 KCP port 和不同 mock hostname/GPU 数量。脚本同时启动 2 个真实 client-backend，分别设置 `GPUSTAT4CLUSTER_BACKEND_ADDR` 与不同 `GPUSTAT4CLUSTER_STATIC_NODES`，CLI 通过 `--backend-addr` 验证 JSON 和表格输出，不再使用 fixture backend 或 fixed-port proxy。

验收字段：

- JSON `meta.node_count`。
- Hostnames：`mn-node-a`、`mn-node-b`、`mn-node-c`。
- GPU rows：`87% 1234/16384`、`80% 1746/17408`、`73% 2258/18432`。
- Process fields：`mock-user-*`、`mock-helper-*`、`pid`、`used_memory_mb`。
- 表格输出包含 `HOSTNAME`、hostname、GPU utilization/memory 和 `proc mock-user-*` 行。

多节点 stress baseline：

```bash
bash -n scripts/stress-multinode-local.sh
scripts/stress-multinode-local.sh
```

它会启动 3 个真实 server 与 2 个真实 client-backend，并发执行 CLI `--json --backend-addr ...` 查询，交替命中两个 backend。该脚本暂不放入 CI，避免端口和调度 flake。

详细说明见 [local-multinode.md](local-multinode.md)。

---

# Local smoke validation (English)

This smoke test validates that the current bootstrap path, temporary config loading, server query port, client-backend local API, and CLI rendering still work. It does not require real NVML.

Prepare the Rust environment:

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
```

Run the smoke:

```bash
bash -n scripts/smoke-local.sh
scripts/smoke-local.sh
```

The script creates a temporary config, sets a temporary query address, builds debug binaries, starts the server, checks the server TCP/JSON response, starts the real client-backend, switches to a deterministic fake backend when needed, calls the CLI, and cleans up background processes on exit.

Pass criteria:

- The server query port is reachable.
- The server response contains JSON `ok`.
- The client-backend local API is reachable.
- CLI output contains the `HOSTNAME` header.
- CLI output contains the mock hostname, default `mock-smoke-node`.
- CLI output contains at least one mock utilization/memory row such as `87%` and `1234/16384`.

The CLI also supports machine-readable output with `gpustat4cluster-client --json` for automation.
