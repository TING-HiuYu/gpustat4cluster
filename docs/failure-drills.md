# Failure Drills

[English](#failure-drills) | [中文摘要](#中文摘要)

Failure drills validate operational behavior before GA. These drills are intentionally written as manual or semi-manual procedures until the automation scripts are added.

## Protocol And Transport Drills

### Drill 1: KCP Server Restart

Preconditions:

- Server and client-backend are built with `kcp-transport`.
- Use mock collector or a known-good GPU node.
- Prefer deterministic target selection with `GPUSTAT4CLUSTER_STATIC_NODES=127.0.0.1:<server-port>` for loopback.

Steps:

1. Start `gpustat4cluster-server` with KCP enabled.
2. Start `gpustat4cluster-client-backend` with KCP enabled and static nodes pointing to the server.
3. Run one CLI query and confirm rows are returned.
4. Restart the server process.
5. Run repeated CLI queries for 30-60 seconds.

Expected logs/output:

- Before restart: `HandshakeInfo`, `QueryRequest`, or equivalent KCP session success logs.
- During restart: transient `ConnectionClosed`, `HeartbeatTimeout`, `handshake timeout`, or reconnect logs are acceptable.
- After restart: rows return again without restarting CLI.

Pass criteria:

- Client-backend recovers after server restart.
- No panic or process crash in client-backend.
- Final CLI query returns a valid snapshot or an explicit degraded/query error.

Fail criteria:

- Client-backend wedges permanently.
- Reconnect requires manual client-backend restart.
- Logs show repeated `BadMagic` or `PayloadLengthMismatch` after server is healthy.

### Drill 2: Client Reconnect

Preconditions:

- Server is running with KCP enabled.
- Static nodes or multicast discovery can locate the server.

Steps:

1. Start client-backend and confirm one successful query.
2. Stop client-backend.
3. Start client-backend again with the same config/static nodes.
4. Run CLI query immediately and again after the discovery/reconnect window.

Expected logs/output:

- New KCP session or handshake logs after restart.
- `HandshakeRequest` followed by `HandshakeInfo`.
- CLI rows return after reconnect.

Pass criteria:

- Restarted client-backend establishes a fresh session.
- Server does not require restart.
- Query `request_id` handling remains sane; stale response from old session is not rendered as fresh data.

Fail criteria:

- Server keeps stale state and rejects the fresh client indefinitely.
- Client reconnect loops without surfacing a clear error.

### Drill 3: Packet Loss, Jitter, And Timeout

Preconditions:

- Linux host where traffic control can be changed, or an equivalent network emulator.
- KCP server/client-backend with static nodes.
- Root/sudo access if using `tc netem`.

Manual steps with `tc netem` example:

1. Identify the loopback/test interface, for example `lo` for local loopback or `eth0` for two-node test.
2. Add loss/jitter: `sudo tc qdisc add dev <iface> root netem loss 5% delay 50ms 20ms`.
3. Run KCP smoke and several CLI queries.
4. Increase severity: `sudo tc qdisc change dev <iface> root netem loss 20% delay 150ms 50ms`.
5. Remove rules: `sudo tc qdisc del dev <iface> root`.

Expected logs/output:

- Mild impairment may increase latency but should not corrupt frames.
- Severe impairment may produce `HeartbeatTimeout`, `QueryTimeout`, or reconnect logs.
- No `BadMagic` or `PayloadLengthMismatch` should appear solely because of packet loss; KCP should deliver ordered bytes or fail the session.

Pass criteria:

- Mild loss/jitter still returns rows or explicit degraded/query errors.
- Severe loss times out cleanly and recovers after netem is removed.
- No process crash.

Fail criteria:

- Corrupted snapshots are rendered as valid data.
- Session hangs forever without timeout.
- Netem removal does not restore service.

### Drill 4: Protocol Version Mismatch

Preconditions:

- Ability to run mismatched binaries or inject a frame with a non-v1 header.
- KCP transport path enabled.

Manual steps:

1. Run server built from current `PROTOCOL_VERSION = 1`.
2. Run a test client or frame injector that sends a KCP frame with `version != 1` in the frame header or JSON control payload.
3. Attempt handshake and query.

Expected logs/output:

- `ProtocolVersionMismatch`.
- `version mismatch`, `expected`, and `got` details if debug logging is enabled.
- Session is rejected or closed before processing payload bytes.

Pass criteria:

- Mismatch is detected deterministically.
- No snapshot payload is accepted from the mismatched peer.
- Error is visible enough for operator triage.

Fail criteria:

- Mismatched frame is processed as valid.
- Peer hangs without a visible error.

### Drill 5: Corrupted Frame Or Bad Magic

Preconditions:

- Ability to send arbitrary bytes to the KCP endpoint, or a test harness that mutates the first four frame bytes.
- Server/client logs visible.

Manual steps:

1. Establish a normal KCP session and confirm query success.
2. Send a frame whose first four bytes are not `G4C1`.
3. Send a frame whose `payload_len` does not match actual payload length.
4. Repeat a valid query after the corrupted input.

Expected logs/output:

- Bad magic path: `BadMagic`, `bad frame magic`, `G4C1`, or equivalent transport decode error.
- Length mismatch path: `PayloadLengthMismatch`, `expected`, `actual`.
- Valid follow-up query either succeeds on the same session if the transport keeps it open, or succeeds after reconnect.

Pass criteria:

- Corrupted frame is rejected.
- No panic or memory safety issue.
- Valid traffic can recover via same session or reconnect.

Fail criteria:

- Corrupted frame is interpreted as a valid protocol message.
- Session parser loses synchronization permanently without closing/reconnecting.

### Drill 6: Static Nodes Fallback

Preconditions:

- Multicast can be disabled or blocked, or test in an environment where multicast is known unavailable.
- Known server address and port.

Steps:

1. Start server with KCP enabled.
2. Start client-backend with multicast discovery only and confirm no nodes are found.
3. Restart or reconfigure client-backend with `GPUSTAT4CLUSTER_STATIC_NODES=<host>:<port>`.
4. Run CLI query.

Expected logs/output:

- Without static nodes: `discovery failed`, `0 nodes`, or multicast-related warning.
- With static nodes: static target is loaded, KCP handshake succeeds, CLI returns rows or explicit degraded response.

Pass criteria:

- Static nodes allow KCP connection when multicast discovery fails.
- Operator can distinguish discovery failure from KCP frame/payload failure.

Fail criteria:

- Static nodes are ignored.
- Client reports only generic failure without indicating discovery/static-node status.

## Mock NVML Drill Data Contract

Use mock GPU data only when the drill explicitly targets transport behavior or degraded/mock collector behavior. Real NVML validation drills should unset mock-only switches before starting the server.

Mock drill data must follow the protocol data model rather than a simplified fixture:

- One `ServerGpuSnapshot` represents one server/node. Multi-node drills should provide one independent snapshot per node.
- `hostname` is stable per mock node and `timestamp_ms` is set by the mock server/provider at collection time.
- `GpuInfo.index` is a per-node GPU index. It must not be reused as a global cluster GPU ID.
- `GpuInfo.uuid` is optional in protocol terms, but mock fixtures should provide stable UUID-like values when practical.
- Memory fields are MiB and utilization fields remain in `0..=100`.
- Process fixtures are best-effort, but `pid`, `username`, and `used_memory_mb` should be stable so assertions do not depend on host state.

Mock provider implementation must be `#[cfg(test)]`, feature-gated, or enabled only by an explicitly documented test environment switch. Production default behavior should use real NVML or the degraded response path, not mock rows.

## Current Implementation Limits

- Multi-connection and multi-node behavior still requires real cluster validation.
- Network jitter/loss testing does not yet have an automated `netem` script; the drill above is manual.
- Reconnect and heartbeat policies are not final and should be treated as transport hardening work.
- Zero-copy checked archived view is not complete; `decode_snapshot_payload` currently returns an owned `ServerGpuSnapshot` and may copy into an aligned buffer before rkyv decode.
- True GPU row validation depends on the NVML/mock collector data path and should be verified separately from transport-only loopback; mock rows are not a substitute for the real NVML validation runbook.

---

## 中文摘要

本页是 GA 前的故障演练清单，重点验证 KCP/TCP 传输、重连、超时、坏包、版本不匹配和降级路径。每个 drill 都给出前置条件、执行步骤、预期日志、通过标准和失败标准。

建议在发布前至少覆盖这些场景：服务端重启、客户端重连、丢包/抖动/超时、协议版本不匹配、损坏帧、静态节点兜底、NVML 不可用、systemd restart policy、日志轮转和长时间 soak。真实集群环境里不要中断其他用户计算任务，只验证 gpustat4cluster 自身进程。
