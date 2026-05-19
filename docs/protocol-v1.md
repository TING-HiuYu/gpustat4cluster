# gpustat4cluster Protocol v1

## 1. Version

- `PROTOCOL_VERSION = 1` (`u8`).
- All protocol frames include `version`.
- Receiver MUST call version check:
  - matched => continue
  - mismatch => respond `ErrorCode::ProtocolVersionMismatch (1010)` and close/ignore frame.

## 2. Frame Types (JSON for current bootstrap)

## 2.1 Discovery

### DiscoveryQuery
- `version: u8`

### DiscoveryAnnounce
- `version: u8`
- `hostname: string`
- `ip: string`
- `port: u16`
- `ttl: u16?` optional
- `load: u8?` optional (0-100)
- `degraded: bool?` optional

## 2.2 Handshake

### HandshakeRequest
- `version: u8`

### HandshakeInfo
- `version: u8`
- `hostname: string`
- `gpu_num: u8`
- `payload_len: u16` (fixed-width; max 65535)

## 2.3 Query

### QueryRequest
- `version: u8`
- `request_id: u64`

### QueryResponse
- `version: u8`
- `request_id: u64`
- `status: u8 enum`
  - `0 => Ok`
  - `1 => Error`
- `error: ErrorCode?` optional

## 3. Error Code Mapping (stable `u16`)

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

## 4. State Machine (v1)

1. Discovery phase:
   - client sends `DiscoveryQuery`
   - server periodically sends `DiscoveryAnnounce`
2. Connect phase:
   - client sends `HandshakeRequest`
   - server replies `HandshakeInfo`
3. Query phase:
   - client sends `QueryRequest(request_id)`
   - server replies `QueryResponse(request_id, status, error?)`
4. Any version mismatch -> protocol error path (`1010`).

## 5. Example Frames

### DiscoveryQuery
```json
{"version":1}
```

### DiscoveryAnnounce
```json
{"version":1,"hostname":"node-a","ip":"10.0.0.1","port":30001,"ttl":5,"load":24,"degraded":false}
```

### HandshakeInfo
```json
{"version":1,"hostname":"node-a","gpu_num":8,"payload_len":4096}
```

### QueryResponse Error
```json
{"version":1,"request_id":42,"status":"Error","error":"QueryTimeout"}
```
