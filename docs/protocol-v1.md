# gpustat4cluster Protocol v1

[English](#gpustat4cluster-protocol-v1) | [中文摘要](#中文摘要)

## 1. Scope And Compatibility

Protocol v1 has two explicitly supported layers:

- TCP/JSON fallback: server and client components use serde JSON frames for compatibility paths, local API checks, and smoke tests.
- KCP binary + rkyv transport: KCP transport frames use a fixed binary header followed by JSON control payloads or rkyv archived GPU payloads.

The TCP/JSON bootstrap is fallback compatibility glue. New protocol work should target the KCP binary frame layout and the stable rkyv GPU data model in `crates/common/src/protocol.rs`.

KCP crate selection and runtime integration guidance live in `docs/transport-kcp.md`.
Operational KCP troubleshooting, static-node fallback, and multicast fallback guidance also live in `docs/transport-kcp.md`.

## 2. Snapshot Payload Contract

Snapshot payloads use these common interfaces:

- TCP/JSON bootstrap remains valid as fallback for UDP discovery and the local backend/CLI API.
- GPU data payloads MUST use `common::encode_snapshot_payload(&ServerGpuSnapshot)`.
- GPU data payload receivers MUST use `common::decode_snapshot_payload(bytes)`.
- The payload bytes are the complete rkyv archive of `ServerGpuSnapshot`.
- `ServerGpuSnapshot.timestamp_ms` is the only timestamp field; there is no external timestamp prefix.
- `HandshakeInfo.payload_len` is the encoded snapshot payload length and MUST fit in `u16`.
- The KCP binary frame header is represented by `FrameHeader` and `FrameType`.
- Runtime KCP is feature-gated in server/client-backend as `kcp-transport`; `common` has no KCP dependency.
- `decode_snapshot_payload` returns an owned `ServerGpuSnapshot` and internally realigns network payload bytes before rkyv decode.

## 3. Version Contract

- `PROTOCOL_VERSION = 1` (`u8`).
- Every JSON control message includes `version`.
- Every binary frame header includes `version`.
- Receiver MUST validate the version before processing the frame body.
- Version mismatch error path:
  - JSON bootstrap: return/record `ErrorCode::ProtocolVersionMismatch (1010)` and close or ignore the frame.
  - Binary KCP target: reject the frame during header decode or reply with a `QueryResponse`/control error carrying `ProtocolVersionMismatch`, then close the session when the frame is part of handshake.

## 4. Current JSON Bootstrap Frames

These types remain serde-compatible for fallback paths, smoke tests, and local diagnostics.

### DiscoveryQuery

- `version: u8`

Example:

```json
{"version":1}
```

### DiscoveryAnnounce

- `version: u8`
- `hostname: string`
- `ip: string`
- `port: u16`
- `ttl: u16?` optional
- `load: u8?` optional (0-100)
- `degraded: bool?` optional

Example:

```json
{"version":1,"hostname":"node-a","ip":"10.0.0.1","port":30001,"ttl":5,"load":24,"degraded":false}
```

### HandshakeRequest

- `version: u8`

Example:

```json
{"version":1}
```

### HandshakeInfo

- `version: u8`
- `hostname: string`
- `gpu_num: u8`
- `payload_len: u16` fixed rkyv data payload width, max 65535

Example:

```json
{"version":1,"hostname":"node-a","gpu_num":8,"payload_len":4096}
```

### QueryRequest

- `version: u8`
- `request_id: u64`

### QueryResponse

- `version: u8`
- `request_id: u64`
- `status: u8 enum`
- `error: ErrorCode?` optional

Status values:

- `0 => Ok`
- `1 => Error`

Error example:

```json
{"version":1,"request_id":42,"status":"Error","error":"QueryTimeout"}
```

## 5. KCP Binary Frame Layout

All multi-byte integers are big-endian. The header is exactly 18 bytes.

```text
0               4 5 6              14             18
+---------------+-+-+---------------+---------------+
| magic "G4C1"  |v|t| request_id u64| payload_len u32|
+---------------+-+-+---------------+---------------+
| payload bytes ...                                  |
+----------------------------------------------------+
```

Header fields:

- `magic: [u8; 4] = "G4C1"`
- `version: u8 = 1`
- `frame_type: u8`
- `request_id: u64`
- `payload_len: u32`
- `payload: [u8; payload_len]`

Frame type values:

- `1 => DiscoveryQuery`
- `2 => DiscoveryAnnounce`
- `3 => HandshakeRequest`
- `4 => HandshakeInfo`
- `5 => QueryRequest`
- `6 => QueryResponse`
- `7 => DataPayload`

Header validation order:

1. Require at least 18 bytes.
2. Validate magic.
3. Validate protocol version.
4. Validate frame type.
5. Read `request_id` and `payload_len`.
6. Wait until exactly `payload_len` bytes are available before decoding payload.

`decode_frame` accepts exactly one complete frame. If a KCP stream read returns multiple frames or a partial frame, the session layer must split/buffer before calling `decode_frame`; common does not provide a stream parser.

Common helpers:

- `encode_frame(header: FrameHeader, payload: &[u8]) -> Vec<u8>`
- `decode_frame(bytes: &[u8]) -> Result<(FrameHeader, &[u8]), FrameDecodeError>`
- `payload_len_for_handshake(payload: &[u8]) -> Result<u16, PayloadLenError>`

`decode_frame` errors:

- `ShortHeader`
- `BadMagic`
- `VersionMismatch`
- `UnknownFrameType`
- `PayloadLengthMismatch`

## 6. Stable GPU Data Model

`ServerGpuSnapshot` is the stable rkyv archive root for GPU data payloads. It also derives serde traits so JSON bootstrap tests and local diagnostics can share the same data model.

### Multi-node Snapshot Contract

- One `ServerGpuSnapshot` describes exactly one server/node; multi-node views are assembled by the client/backend from multiple independent snapshots.
- `hostname` is independent per server and must not be treated as a cluster-global payload wrapper outside the snapshot.
- `timestamp_ms` is set by the server at collection time. Clients should not rewrite it during transport decode.
- `GpuInfo.index` is the single-node GPU index from that server's collector/NVML device order, not a cluster-wide GPU identifier.
- `GpuInfo.uuid` is optional because real collectors may fail to provide it, but mock/test providers should emit stable UUID-like values when practical.
- `GpuMemory` values are MiB.
- `GpuUtilization` values are percentages in the inclusive range `0..=100`.
- `GpuProcessInfo` is best-effort. For deterministic tests, mock providers should keep `pid`, `username`, and `used_memory_mb` stable.

### Mock NVML Data Contract

- Mock NVML providers may live in code, but must be compiled only under `#[cfg(test)]`, a dedicated test feature, or an explicitly documented test environment switch.
- Production default startup must not silently use mock GPU data. If real NVML is unavailable, the server should use the documented degraded response path unless an explicit test-only mock mode is enabled.
- Mock data must preserve the real `ServerGpuSnapshot` shape: per-node `hostname`, server-set `timestamp_ms`, per-node GPU `index`, optional but stable `uuid`, MiB memory fields, `0..=100` utilization, and best-effort process records.
- Mock rows should avoid JSON-only shortcuts. Runtime integration should use `encode_snapshot_payload` and `decode_snapshot_payload` for the same archived snapshot bytes used by real collection.
- Logs or test harness output should make mock mode visible, for example with `collector_mode=mock`, so operators do not confuse mock rows with real NVML validation.

### ServerGpuSnapshot

- `timestamp_ms: i64`
  - Server-side UNIX epoch milliseconds when the snapshot was collected.
- `hostname: string`
  - Server hostname as reported in `HandshakeInfo`.
- `gpus: GpuInfo[]`
  - Full GPU list for this server. Empty means the node currently reports no GPUs.

### GpuInfo

- `index: u8`
  - Zero-based GPU index from collector/NVML device order.
- `name: string`
  - Human-readable GPU product name.
- `uuid: string?`
  - Stable GPU UUID when available.
- `memory: GpuMemory`
- `utilization: GpuUtilization`
- `processes: GpuProcessInfo[]`
  - Per-GPU process list. Empty means no processes or process data unavailable.

### GpuMemory

- `used_mb: u64`
  - Used framebuffer memory in MiB.
- `total_mb: u64`
  - Total framebuffer memory in MiB.

### GpuUtilization

- `gpu_percent: u8`
  - GPU core utilization percent, inclusive range `0..=100`.
- `memory_percent: u8`
  - Memory controller utilization percent, inclusive range `0..=100`.

### GpuProcessInfo

Process records describe best-effort process attribution for one GPU.

- `pid: u32`
  - OS process ID.
- `username: string`
  - Best-effort username owning the process.
- `command: string?`
  - Best-effort command/process name; absent when unavailable.
- `used_memory_mb: u64`
  - GPU memory attributed to this process in MiB.

## 7. Data Payload Envelope

After a successful handshake, `HandshakeInfo.payload_len` is the exact byte length of every GPU data payload on that KCP connection.

Payload layout:

```text
Archived<ServerGpuSnapshot> bytes
```

Length rules:

- `payload_len = encode_snapshot_payload(snapshot)?.len()`.
- `payload_len` MUST fit in `u16` (`0..=65535`) because it is advertised in `HandshakeInfo`.
- `ServerGpuSnapshot.timestamp_ms` carries the server-side collection timestamp.
- There is no external timestamp prefix.
- The entire payload remains an opaque byte slice for rkyv validation and decoding on the receiver.
- The decode helper returns an owned snapshot and may copy into an aligned buffer before decode.

Example:

```text
<Archived<ServerGpuSnapshot> bytes...>
```

Helper functions:

- `encode_snapshot_payload(snapshot: &ServerGpuSnapshot) -> Result<Vec<u8>, PayloadEncodeError>`
- `decode_snapshot_payload(bytes: &[u8]) -> Result<ServerGpuSnapshot, PayloadDecodeError>`
- `payload_len_from_archived_len(payload.len()) -> Result<u16, PayloadLenError>`

Deprecated compatibility note:

- `PAYLOAD_TIMESTAMP_LEN`, `encode_payload`, and `decode_payload` are timestamp-prefix compatibility helpers and are not exported from `common::lib`.
- New transport code should not call them.

## 8. KCP Transport Flow

1. Client opens KCP session to discovered server.
2. Client sends a `FrameType::HandshakeRequest` frame.
3. Server validates `HandshakeRequest.version`.
4. Server replies with a `FrameType::HandshakeInfo` frame containing hostname, `gpu_num`, and fixed `payload_len`.
5. Client validates `HandshakeInfo.version` and may preallocate a receive buffer of `payload_len` bytes.
6. Client sends a `FrameType::QueryRequest` frame with a client-generated `request_id`.
7. Server replies with a `FrameType::DataPayload` frame using the same `request_id`; the payload is `encode_snapshot_payload(&ServerGpuSnapshot)`.
8. Error path uses a `FrameType::QueryResponse` frame with `ResponseStatus::Error` and the same `request_id`.
9. If handshake version mismatches, server returns or logs `ErrorCode::ProtocolVersionMismatch (1010)` and closes the KCP session.

Current limitations:

- Loopback and degraded response paths are the current validation target.
- Real GPU rows still depend on NVML/mock collector data path validation.
- Multi-node, multi-connection, reconnect, heartbeat, and liveness policy are not complete.
- See `docs/transport-kcp.md` for transport feature strategy and rollout boundaries.

## 9. Error Code Mapping (stable `u16`)

- 1001 `NvmlUnavailable`
- 1002 `ConfigInvalid`
- 1003 `PortExhausted`
- 1004 `MulticastFailed`
- 1005 `KcpInitFailed`
- 1006 `HeartbeatTimeout`
- 1007 `ConnectionClosed`
- 1008 `QueryTimeout`
- 1009 `InvalidFilter`
- 1010 `ProtocolVersionMismatch`
- 1999 `Internal`

## 10. V1 Schema Evolution Rules

The v1 protocol should evolve conservatively so runtime changes do not fragment the wire contract.

- Additive fields are preferred. New JSON fields MUST use serde `default` or be optional.
- New rkyv snapshot fields should be optional/default-compatible where possible, and must be documented before runtime code depends on them.
- Existing field names, meanings, units, and numeric ranges are stable for v1.
- Frame type numeric values are append-only and MUST NOT be reused.
- Removing a frame type or changing an existing frame type payload shape requires a `PROTOCOL_VERSION` bump.
- Error code numeric values are stable and MUST NOT change once published.
- New error codes must use new numeric IDs and be added to `ErrorCode::from_code`.
- `PROTOCOL_VERSION` must be bumped when a receiver cannot safely parse or ignore a change under v1 rules.
- `PROTOCOL_VERSION` does not need to bump for optional/default JSON fields, documentation-only clarifications, or new frame types that old peers can reject as `UnknownFrameType`.
- `HandshakeInfo.payload_len` remains `u16` for v1. Moving GPU payload length beyond that limit requires either a compatible extension frame or a version bump.

## 11. Integration Notes

- Server should build `ServerGpuSnapshot`, call `encode_snapshot_payload`, then validate `payload_len_for_handshake(&payload)` before publishing the value through `HandshakeInfo`.
- Client should call `decode_frame`, validate `FrameType::DataPayload`, then call `decode_snapshot_payload` on the returned payload slice.
- TCP/JSON discovery/control paths may remain as fallback, but new transport code should use `encode_frame`, `decode_frame`, `FrameHeader`, and `FrameType` from `common`.
- KCP runtime dependency decisions are intentionally kept out of `common`; see `docs/transport-kcp.md`.

---

## 中文摘要

本文定义 gpustat4cluster Protocol v1。当前协议同时支持 TCP/JSON bootstrap fallback 和 KCP binary + rkyv 传输层。新的协议工作应优先面向 KCP 二进制 frame 和 `crates/common/src/protocol.rs` 中稳定的 GPU payload model。

关键约束：`PROTOCOL_VERSION = 1`，所有 JSON 控制消息和二进制 frame header 都必须携带并校验版本；GPU payload 使用 common crate 的编码/解码函数；接收端需要处理版本不匹配、未知 frame type、payload length mismatch、坏 magic 和错误 frame。文档也记录了 discovery、handshake、query、error code、frame header 和兼容策略。
