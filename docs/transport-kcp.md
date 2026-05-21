# KCP Transport Decision

[English](#kcp-transport-decision) | [中文摘要](#中文摘要)

## Decision

Use `kcp2 = "0.2.2"` for the first KCP transport implementation, behind a crate-local feature named `kcp-transport` in both runtime crates.

Do not add any KCP dependency to `crates/common`. `common` remains a pure protocol/frame/payload crate.

Recommended dependency placement:

```toml
# crates/server/Cargo.toml
[features]
default = []
kcp-transport = ["dep:kcp2", "dep:tokio"]

[dependencies]
kcp2 = { version = "0.2.2", optional = true }
tokio = { version = "1", features = ["net", "rt", "macros", "time", "io-util", "sync"], optional = true }
```

```toml
# crates/client-backend/Cargo.toml
[features]
default = []
kcp-transport = ["dep:kcp2", "dep:tokio"]

[dependencies]
kcp2 = { version = "0.2.2", optional = true }
tokio = { version = "1", features = ["net", "rt", "macros", "time", "io-util", "sync"], optional = true }
```

## Why `kcp2`

Selection criteria:

- Maintainable and documented enough for runtime integration.
- Non-blocking async support.
- Can connect to UDP sockets or a pluggable transport.
- Works without contaminating common protocol types.

Observed options:

- `kcp = "0.6.0"`: low-level Rust KCP translation. It has an optional `tokio` feature but is closer to core protocol machinery, so runtime crates would need more session/UDP driving code.
- `tokio_kcp = "0.9.8"`: practical Tokio listener/stream API, supports `KcpListener::from_socket` and `KcpStream::connect_with_socket`. It is a viable fallback when a TCP-like stream wrapper is preferred.
- `kcp-tokio = "0.5.0"`: polished async-first README and high-level `KcpStream`/`KcpListener`, but the published package layout and README version examples look less straightforward to verify locally.
- `kcp2 = "0.2.2"`: documented three-layer architecture, Tokio async support, listener/connector abstractions, `KcpTransport` trait, default UDP transport, optional encryption features, and explicit pluggable transport boundary.

`kcp2` is recommended because it has the cleanest transport abstraction for this project: common owns bytes and frame rules, while server/client own async UDP/KCP session lifecycle.

## Feature Gate

Use feature name: `kcp-transport`.

Rationale:

- Keeps current TCP/JSON bootstrap fallback buildable.
- Keeps KCP behind a controlled opt-in path.
- Avoids adding Tokio/KCP requirements to commands or tests that only exercise JSON/local APIs.

Expected shape:

- `server --features kcp-transport`: bind UDP/KCP listener on the selected service port.
- `client-backend --features kcp-transport`: connect KCP sessions to discovered server addresses.
- `common`: no KCP feature, no KCP dependency, no runtime socket/session code.
- Without the feature: keep existing fallback transport for smoke tests and incremental rollout.

Build examples:

```bash
cargo build -p server --features kcp-transport
cargo build -p gpustat4cluster-client-backend --features kcp-transport
```

The feature is intentionally crate-local. Enabling it for server should not force client-backend to build KCP, and enabling it for client-backend should not affect common.

## KCP Transport Status

Current status:

- Loopback KCP smoke path is wired through the common frame contract.
- Degraded responses are represented through `QueryResponse::error` and can travel over the same frame path.
- The protocol boundary for snapshot payloads is stable: `ServerGpuSnapshot` rkyv bytes inside `FrameType::DataPayload`.

Not complete:

- True GPU rows still depend on validating the NVML/mock collector data path end-to-end.
- Multi-node discovery plus multi-session fan-out is not complete.
- Each client/server pair keeps one KCP session; max_connections caps the number of peers, not sessions per peer.
- Disconnect/reconnect semantics are not complete.
- Heartbeat, idle timeout, and connection liveness policy are not complete.
- Backpressure and bounded queue behavior for slow clients are not complete.
- Soak/packet-loss/reorder testing is still pending.

Failure drill procedures for restart, reconnect, packet loss, protocol mismatch, corrupted frames, and static-node fallback are documented in `docs/failure-drills.md`.

## Discovery And Fallback Order

Runtime discovery and fallback are layered:

1. Multicast discovery is the preferred cluster discovery path.
2. `GPUSTAT4CLUSTER_STATIC_NODES` is the deterministic fallback for KCP loopback, CI, restricted networks, and multicast-disabled environments.
3. TCP/JSON bootstrap fallback remains available for local smoke tests and CLI/backend development while KCP is gated behind `kcp-transport`.

Static nodes should not disable multicast permanently. Treat them as an override/fallback input that the client backend can merge with multicast results, de-duplicate by address, and use when multicast yields no rows.

## KCP Troubleshooting

| Symptom | Possible causes | Log keywords | Suggested recovery |
| --- | --- | --- | --- |
| Handshake timeout | Server KCP feature not enabled; wrong host/port; UDP blocked; session read loop waiting for partial frame; server not started | `handshake timeout`, `HeartbeatTimeout`, `KcpInitFailed`, `ConnectionClosed`, `timed out` | Confirm both binaries were built with `kcp-transport`; verify server listen port; test local loopback first; check firewall/security group UDP rules; fall back to `GPUSTAT4CLUSTER_STATIC_NODES` for deterministic target selection. |
| Version mismatch | Client/server built from incompatible protocol versions; stale binary running after upgrade; frame header version corrupted | `ProtocolVersionMismatch`, `version mismatch`, `expected`, `got` | Restart both sides from the same build; verify `PROTOCOL_VERSION` in logs/build metadata; reject session and reconnect after upgrade. |
| Bad frame magic | Peer is speaking TCP/JSON fallback to KCP port; wrong port; non-gpustat UDP traffic; buffer starts mid-frame due to stream parser bug | `BadMagic`, `bad frame magic`, `FRAME_MAGIC`, `G4C1` | Confirm client is connecting to the KCP service port; separate fallback TCP/JSON endpoint from KCP endpoint; inspect session framing loop and ensure it reads from frame boundary. |
| Payload length mismatch | Sticky packets passed as one frame; partial frame passed too early; corrupted header length; write/read loop not respecting `payload_len` | `PayloadLengthMismatch`, `payload_len`, `expected`, `actual` | In stream-like APIs, read exactly `FRAME_HEADER_LEN`, decode header, then read exactly `payload_len`; split concatenated frames before calling `decode_frame`; add debug logging for header/request_id/frame_type. |
| Degraded response | NVML unavailable; collector failed; mock/force-mock path intentionally simulating degraded; permission issue reading GPU data | `NvmlUnavailable`, `degraded`, `QueryResponse`, `ResponseStatus::Error`, `collector` | Check NVIDIA driver/NVML install and permissions; verify `GPUSTAT4CLUSTER_FORCE_MOCK` or mock env flags; surface the error to CLI while keeping session alive; use mock collector for transport-only validation. |
| Multicast finds no nodes but static nodes work | Multicast disabled by network/CI/container; wrong multicast address/interface; IGMP/firewall issue; server announce not on expected interface | `discovery failed`, `multicast`, `0 nodes`, `static nodes`, `GPUSTAT4CLUSTER_STATIC_NODES` | Use `GPUSTAT4CLUSTER_STATIC_NODES=host:port,...` for CI and restricted networks; verify multicast address and interface config; check firewall/IGMP; keep static nodes as fallback until multicast is validated on target cluster. |

## Minimum Send/Recv Driver

KCP should carry complete `common` frames as the KCP stream payload. Do not expose KCP segment details above the transport module.

### Server side

1. Bind KCP listener on the chosen service port.
2. For each accepted KCP session, spawn one task per session.
3. Read a complete application frame into a `Vec<u8>`.
4. Call `common::decode_frame(&bytes)`.
5. Dispatch by `FrameType`:
   - `HandshakeRequest`: decode JSON `HandshakeRequest`, validate version, compute current snapshot payload, reply `HandshakeInfo` frame.
   - `QueryRequest`: decode JSON `QueryRequest`, refresh/cache snapshot, reply `DataPayload` frame.
   - Errors: reply `QueryResponse::error(request_id, code)` as `FrameType::QueryResponse` where possible.
6. Write complete frames produced by `common::encode_frame`.

### Client side

1. Connect KCP session to discovered server address.
2. Send `HandshakeRequest` frame.
3. Read and decode `HandshakeInfo` frame; validate version and `payload_len`.
4. Send `QueryRequest` frames with client-generated `request_id`.
5. For `DataPayload`, pass payload bytes to `common::decode_snapshot_payload`.
6. For `QueryResponse`, decode JSON and surface the error code.

## Framing Boundary

`common::decode_frame` currently expects exactly one complete frame in the provided byte slice.

Implications:

- If the selected KCP API is message-oriented and returns one send as one recv, pass that buffer directly to `decode_frame`.
- If the selected KCP API is stream-like (`AsyncRead`/`AsyncWrite`), the transport layer must add a small read loop:
  - read exactly `FRAME_HEADER_LEN` bytes;
  - decode `FrameHeader`;
  - read exactly `header.payload_len` bytes;
  - concatenate header + payload and pass that single frame to `decode_frame`, or dispatch using the decoded header and payload without calling `decode_frame` again.
- Do not pass multiple concatenated frames to `decode_frame`; it will return `PayloadLengthMismatch` by design.
- For sticky packets, the session layer must split frames before calling `decode_frame`.
- For partial frames, the session layer must buffer until the full header and declared payload are present.
- `decode_frame` is a frame validator, not a stream parser.

## Payload Size Rules

- `FrameHeader.payload_len` is `u32`, so the frame layer can represent larger control/data buffers.
- `HandshakeInfo.payload_len` is `u16`; GPU snapshot payloads advertised during handshake must pass `common::payload_len_for_handshake(&payload)`.
- For GPU data, if `encode_snapshot_payload` returns `PayloadEncodeError::PayloadTooLarge`, server should return `QueryResponse::error(request_id, ErrorCode::Internal)` until the protocol moves beyond u16 handshake length.

## Common API Used By Transport

Frame helpers:

- `common::encode_frame(header, payload)`
- `common::decode_frame(bytes)`
- `common::FrameHeader::new(frame_type, request_id, payload_len)`
- `common::FrameType`
- `common::FrameDecodeError`

Payload helpers:

- `common::encode_snapshot_payload(&snapshot)`
- `common::decode_snapshot_payload(payload)`
- `common::payload_len_for_handshake(payload)`

JSON control structs:

- `common::HandshakeRequest`
- `common::HandshakeInfo`
- `common::QueryRequest`
- `common::QueryResponse`

## Compatibility Boundaries

Transport work must preserve the protocol compatibility rules in `docs/protocol-v1.md`.

- KCP session code may add new runtime behaviors, but it must not reinterpret existing `FrameType` numeric values.
- Unknown frame types should be surfaced as decode errors and must not be guessed or coerced.
- Version mismatch should terminate or reject the session before processing payload bytes.
- Error codes carried by `QueryResponse` use stable `u16` mappings from `common::ErrorCode`; do not remap them in transport code.
- `common` remains free of KCP crate dependencies so protocol tests can validate compatibility without runtime transport features.

---

## 中文摘要

本文记录 KCP transport 的早期选型和集成边界。当前策略是在 server 和 client-backend 中通过 `kcp-transport` feature 引入 KCP，`common` crate 保持纯协议、frame 和 payload 逻辑，不引入运行时 socket/session 依赖。

设计重点：KCP 用于低延迟 UDP 传输，TCP 作为 UDP 不可用时的兼容 fallback；server/client-backend 分别拥有自己的 socket、session lifecycle、重连和心跳逻辑；common 只定义可验证的 frame、版本、payload 和错误模型。生产配置里协议通过 `[connecting].protocol = "kcp"` 或 `"tcp"` 选择。
