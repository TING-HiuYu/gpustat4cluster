# NVML validation runbook

[English](#nvml-validation-runbook) | [中文摘要](#中文摘要)

This checklist is for validating the server collector on a real NVIDIA GPU host. It does not require client changes.

## Build the server with NVML

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
cargo build --locked -p server --features "kcp-transport nvml"
```

Use a valid server config. For a one-node manual run, keep multicast in the valid multicast range and choose a free query port:

```toml
[connecting]
port_range = [30000, 30010]
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
# Default is libnvidia-ml.so. Set this if the host only exposes a versioned runtime library.
# nvml_lib_path = "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1"
```

## Start with real NVML

Do not set mock or simulated-missing switches:

```bash
unset GPUSTAT4CLUSTER_COLLECTOR
unset GPUSTAT4CLUSTER_FORCE_MOCK
unset GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING
export GPUSTAT4CLUSTER_CONFIG=/path/to/config.toml
export GPUSTAT4CLUSTER_QUERY_ADDR=127.0.0.1:4522
./target/debug/server
```

The first structured log line should contain:

- `event: "startup"`
- `collector_mode: "nvml"`
- `degraded: false`
- `protocol_version`
- `bind_port`
- `cache_ttl_ms`
- initial `metrics`

Production server startup is fail-fast: if NVML cannot initialize, the process exits with a FATAL log instead of continuing in degraded mode. If your host only has `libnvidia-ml.so.1` or `libnvidia-ml.so.<driver-version>`, set `[runtime].nvml_lib_path` to that real driver library path. Do not use CUDA `stubs/libnvidia-ml.so` for runtime validation; it is a link-time stub and cannot replace the NVIDIA driver library.

## Query and inspect payload

Use the TCP/JSON query endpoint to trigger collection:

```bash
python3 - <<'PY'
import base64
import json
import socket

with socket.create_connection(("127.0.0.1", 4522), timeout=3) as sock:
    sock.sendall(b"PING\n")
    response = sock.recv(1024 * 1024).decode("utf-8", "replace")
print(response)
body = json.loads(response)
assert body["ok"] is True
print("gpu_num=", body["gpu_num"])
print("payload_len=", len(base64.b64decode(body["payload_b64"])))
PY
```

For full field inspection, decode `payload_b64` with `common::decode_snapshot_payload()` from a small Rust helper or an existing server test harness. Confirm the common `ServerGpuSnapshot` fields:

- `hostname` matches the server host.
- `gpus.len()` matches `nvidia-smi --query-gpu=index --format=csv,noheader | wc -l`.
- For each GPU, `index`, `name`, and `uuid` are populated or explainably unavailable.
- `memory.used_mb` and `memory.total_mb` are close to `nvidia-smi --query-gpu=memory.used,memory.total --format=csv,noheader,nounits`.
- `utilization.gpu_percent` and `utilization.memory_percent` are in `0..=100` and broadly match `nvidia-smi` at the same moment.
- `processes` field exists. Current server NVML collection may return an empty process list; if process collection is enabled in a later build, verify `pid`, `username`, `command`, and `used_memory_mb` against `nvidia-smi pmon` or `nvidia-smi --query-compute-apps`.

Because utilization and memory are live values, compare within a short time window and allow small drift.

## Mock NVML provider contract

Mock collector data is valid for tests and failure drills only when it preserves the same contract as real NVML-backed snapshots:

- Mock snapshots use `ServerGpuSnapshot` as the payload root and are encoded with `common::encode_snapshot_payload()`.
- Each mock snapshot represents one server/node with its own `hostname`; multi-node mock tests should create one snapshot per node.
- `timestamp_ms` is set by the mock server/provider at collection time, not by the client.
- `GpuInfo.index` is a per-node GPU index in collector order, not a cluster-wide index.
- `uuid` may be absent in the protocol, but mock providers should emit stable UUID-like strings when practical so tests can assert identity.
- `memory.used_mb`, `memory.total_mb`, and `GpuProcessInfo.used_memory_mb` are MiB.
- `utilization.gpu_percent` and `utilization.memory_percent` stay in `0..=100`.
- Process data is best-effort, but mock providers should keep `pid`, `username`, and `used_memory_mb` stable for deterministic assertions.

Implementation constraints:

- A mock provider may be kept in code only behind `#[cfg(test)]`, a dedicated test feature, or an explicitly documented test environment switch.
- Production default startup must not silently choose mock data. For real validation, unset `GPUSTAT4CLUSTER_COLLECTOR`, `GPUSTAT4CLUSTER_FORCE_MOCK`, and other mock-only switches.
- If a test or drill enables mock mode, structured logs should identify it, for example `collector_mode: "mock"`, so the run cannot be mistaken for real NVML validation.

## Test-only mock NVML provider

For local integration runs that need realistic GPU rows without NVIDIA hardware, build the server with the explicit test feature `mock-nvml`:

```bash
source /opt/shell_related/z00_lmod.sh
module load compiler/rust
cargo build --locked -p server --features "kcp-transport mock-nvml"
```

Then opt in at runtime:

```bash
export GPUSTAT4CLUSTER_COLLECTOR=mock
# or: export GPUSTAT4CLUSTER_FORCE_MOCK=1
export GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-a
export GPUSTAT4CLUSTER_MOCK_GPU_COUNT=2
export GPUSTAT4CLUSTER_CONFIG=/path/to/node-a.toml
export GPUSTAT4CLUSTER_QUERY_ADDR=127.0.0.1:4522
./target/debug/server
```

The first startup line should show `collector_mode: "mock-nvml"`. The mock snapshot is shaped like real NVML output: each GPU has `index`, `name`, `uuid`, `memory.used_mb`, `memory.total_mb`, `utilization.gpu_percent`, `utilization.memory_percent`, and two process rows with `pid`, `username`, `command`, and `used_memory_mb`.

Example with `GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-a` and `GPUSTAT4CLUSTER_MOCK_GPU_COUNT=2`:

- GPU 0: `name=NVIDIA Mock GPU 0`, `uuid=GPU-MOCK-node-a-0000`, memory `1234/16384`, util `87/8`, processes `mock-user-0` and `mock-helper-0`.
- GPU 1: `name=NVIDIA Mock GPU 1`, `uuid=GPU-MOCK-node-a-0001`, memory `1746/17408`, util `80/9`, processes `mock-user-1` and `mock-helper-1`.

Production safety: without the `mock-nvml` feature, these mock env vars do not select the provider. A normal production build still tries real NVML and fails startup if NVML is unavailable or `[runtime].nvml_lib_path` points to an unusable library.

For multiple local servers, use a separate config file per server so each `port_range` is independent, and set a distinct TCP query address per process:

```bash
GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-a \
GPUSTAT4CLUSTER_MOCK_GPU_COUNT=2 \
GPUSTAT4CLUSTER_CONFIG=/tmp/gpustat-node-a.toml \
GPUSTAT4CLUSTER_QUERY_ADDR=127.0.0.1:4522 \
GPUSTAT4CLUSTER_COLLECTOR=mock \
./target/debug/server

GPUSTAT4CLUSTER_MOCK_HOSTNAME=node-b \
GPUSTAT4CLUSTER_MOCK_GPU_COUNT=4 \
GPUSTAT4CLUSTER_CONFIG=/tmp/gpustat-node-b.toml \
GPUSTAT4CLUSTER_QUERY_ADDR=127.0.0.1:4523 \
GPUSTAT4CLUSTER_COLLECTOR=mock \
./target/debug/server
```

Startup logs include `hostname`, `query_addr`, and selected `bind_port` to make multi-server local runs easy to verify.

## Expected startup failure behavior

The server should not panic, but it should fail startup for these cases:

- No NVIDIA GPU present.
- NVIDIA driver missing or NVML library unavailable.
- Running user lacks permission to access NVML.
- `GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING=1` is set for test.

Expected logs:

- Startup logs include `event: "nvml_error"` with the underlying NVML wrapper error.
- The process exits with a FATAL log carrying `code: "nvml unavailable"`.
- TCP/KCP listeners are not started until the NVML configuration is fixed.

## KCP notes

KCP remains feature gated. To validate KCP on the same host:

```bash
export GPUSTAT4CLUSTER_ENABLE_KCP=1
./target/debug/server
```

Confirm logs for `kcp_start`, `kcp_listen`, `kcp_session_accept`, and either `kcp_session_close` or `kcp_session_error`. Reconnect behavior should be checked manually by connecting, closing, and reconnecting a KCP client. Current automated coverage includes a loopback ignored test and malformed frame rejection; cross-node KCP reconnect soak remains a manual release-preflight item.

---

## 中文摘要

本 runbook 用于在真实 NVIDIA GPU 节点上验证服务端 NVML collector。核心流程是：使用 `nvml` feature 构建 server，准备包含 `[runtime].nvml_lib_path` 可选项的 server 配置，启动真实 NVML collector，然后通过 query endpoint 检查 GPU 数量、显存、利用率和进程字段。

注意事项：生产启动采用 fail-fast 策略，NVML 初始化失败会输出 FATAL 日志并退出，不再进入 degraded 模式。如果系统只有 `libnvidia-ml.so.1` 或版本化库，需要在配置中设置 `[runtime].nvml_lib_path`。不要使用 CUDA `stubs/libnvidia-ml.so` 做运行时验证。
