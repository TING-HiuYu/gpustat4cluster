use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use std::convert::TryFrom;

use rkyv::util::AlignedVec;
use rkyv::{
    rancor::Error as RkyvError, Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize,
};

/// 协议 v1 版本号。
pub const PROTOCOL_VERSION: u8 = 1;
/// Binary frame magic: ASCII `G4C1`.
pub const FRAME_MAGIC: [u8; 4] = *b"G4C1";
/// Binary frame header length: magic(4) + version(1) + frame_type(1) + request_id(8) + payload_len(4).
pub const FRAME_HEADER_LEN: usize = 18;
/// Deprecated: snapshot payloads no longer carry an external timestamp prefix.
#[deprecated(
    since = "0.1.0",
    note = "timestamp_ms is part of ServerGpuSnapshot; use encode_snapshot_payload instead"
)]
pub const PAYLOAD_TIMESTAMP_LEN: usize = 8;
/// v1 握手中 `payload_len` 的最大可表达长度。
pub const MAX_PAYLOAD_LEN: usize = u16::MAX as usize;

/// 固定 payload 长度计算错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadLenError {
    TooLarge { len: usize, max: usize },
}

/// rkyv snapshot payload encoding error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadEncodeError {
    PayloadTooLarge { len: usize, max: usize },
    SerializeFailed(String),
}

/// rkyv snapshot payload decoding error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadDecodeError {
    Empty,
    DeserializeFailed(String),
}

/// Binary frame type identifiers for target KCP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    DiscoveryQuery = 1,
    DiscoveryAnnounce = 2,
    HandshakeRequest = 3,
    HandshakeInfo = 4,
    QueryRequest = 5,
    QueryResponse = 6,
    DataPayload = 7,
    Heartbeat = 8,
    Disconnect = 9,
}

impl TryFrom<u8> for FrameType {
    type Error = FrameHeaderError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::DiscoveryQuery),
            2 => Ok(Self::DiscoveryAnnounce),
            3 => Ok(Self::HandshakeRequest),
            4 => Ok(Self::HandshakeInfo),
            5 => Ok(Self::QueryRequest),
            6 => Ok(Self::QueryResponse),
            7 => Ok(Self::DataPayload),
            8 => Ok(Self::Heartbeat),
            9 => Ok(Self::Disconnect),
            other => Err(FrameHeaderError::UnknownFrameType(other)),
        }
    }
}

/// Fixed binary frame header used by the target KCP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub frame_type: FrameType,
    pub request_id: u64,
    pub payload_len: u32,
}

impl FrameHeader {
    pub fn new(frame_type: FrameType, request_id: u64, payload_len: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            frame_type,
            request_id,
            payload_len,
        }
    }

    pub fn encode(self) -> [u8; FRAME_HEADER_LEN] {
        let mut out = [0u8; FRAME_HEADER_LEN];
        out[0..4].copy_from_slice(&FRAME_MAGIC);
        out[4] = self.version;
        out[5] = self.frame_type as u8;
        out[6..14].copy_from_slice(&self.request_id.to_be_bytes());
        out[14..18].copy_from_slice(&self.payload_len.to_be_bytes());
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, FrameHeaderError> {
        if input.len() < FRAME_HEADER_LEN {
            return Err(FrameHeaderError::TooShort {
                got: input.len(),
                min: FRAME_HEADER_LEN,
            });
        }

        if input[0..4] != FRAME_MAGIC {
            return Err(FrameHeaderError::BadMagic);
        }

        let version = input[4];
        if version != PROTOCOL_VERSION {
            return Err(FrameHeaderError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: version,
            });
        }

        let frame_type = FrameType::try_from(input[5])?;
        let mut request_id = [0u8; 8];
        request_id.copy_from_slice(&input[6..14]);
        let mut payload_len = [0u8; 4];
        payload_len.copy_from_slice(&input[14..18]);

        Ok(Self {
            version,
            frame_type,
            request_id: u64::from_be_bytes(request_id),
            payload_len: u32::from_be_bytes(payload_len),
        })
    }
}

/// Binary frame header decode/validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameHeaderError {
    TooShort { got: usize, min: usize },
    BadMagic,
    VersionMismatch { expected: u8, got: u8 },
    UnknownFrameType(u8),
}

/// Complete binary frame decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecodeError {
    ShortHeader { got: usize, min: usize },
    BadMagic,
    VersionMismatch { expected: u8, got: u8 },
    UnknownFrameType(u8),
    PayloadLengthMismatch { expected: usize, actual: usize },
}

impl From<FrameHeaderError> for FrameDecodeError {
    fn from(value: FrameHeaderError) -> Self {
        match value {
            FrameHeaderError::TooShort { got, min } => Self::ShortHeader { got, min },
            FrameHeaderError::BadMagic => Self::BadMagic,
            FrameHeaderError::VersionMismatch { expected, got } => {
                Self::VersionMismatch { expected, got }
            }
            FrameHeaderError::UnknownFrameType(frame_type) => Self::UnknownFrameType(frame_type),
        }
    }
}

/// Stable rkyv payload envelope for one server/node snapshot.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    SerdeSerialize,
    SerdeDeserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct ServerGpuSnapshot {
    /// Server hostname as reported in `HandshakeInfo`; independent per node.
    pub hostname: String,
    /// NVIDIA driver version reported by NVML when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// Full GPU list for this server/node; empty means the node currently reports no GPUs.
    pub gpus: Vec<GpuInfo>,
}

/// Stable GPU record shared by JSON bootstrap and rkyv payloads.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    SerdeSerialize,
    SerdeDeserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct GpuInfo {
    /// Zero-based GPU index from this node's collector/NVML device order, not a cluster-wide ID.
    pub index: u8,
    /// Human-readable GPU product name, for example `NVIDIA A100`.
    pub name: String,
    /// GPU temperature in Celsius when available.
    #[serde(default)]
    pub temperature_c: Option<u32>,
    /// Stable GPU UUID when the collector can provide one; mock data should keep this deterministic.
    pub uuid: Option<String>,
    /// Memory counters in MiB.
    pub memory: GpuMemory,
    /// Utilization counters in percent (`0..=100`).
    pub utilization: GpuUtilization,
    /// Best-effort processes attributed to this GPU; empty when unavailable or no processes are present.
    pub processes: Vec<GpuProcessInfo>,
}

/// GPU memory counters in MiB.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    SerdeSerialize,
    SerdeDeserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct GpuMemory {
    /// Used framebuffer memory in MiB.
    pub used_mb: u64,
    /// Total framebuffer memory in MiB.
    pub total_mb: u64,
}

/// GPU utilization counters in percent (`0..=100`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    SerdeSerialize,
    SerdeDeserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct GpuUtilization {
    /// GPU core utilization percent in the inclusive range `0..=100`.
    pub gpu_percent: u8,
    /// Memory controller utilization percent in the inclusive range `0..=100`.
    pub memory_percent: u8,
}

/// Process record attributed to one GPU.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    SerdeSerialize,
    SerdeDeserialize,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct GpuProcessInfo {
    /// OS process ID.
    pub pid: u32,
    /// Linux UID owning the process; client-side renderers may resolve it via NSS.
    pub uid: u32,
    /// Best-effort command or process name; `None` when unavailable.
    pub command: Option<String>,
    /// GPU memory attributed to this process in MiB.
    pub used_memory_mb: u64,
}

/// 查询请求。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct QueryRequest {
    /// 协议版本号。
    pub version: u8,
    /// 请求 ID（客户端生成，服务端原样回传）。
    pub request_id: u64,
}

/// 查询响应状态码。
#[derive(Debug, Clone, Copy, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseStatus {
    Ok = 0,
    Error = 1,
}

/// 查询响应。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct QueryResponse {
    /// 协议版本号。
    pub version: u8,
    /// 与请求对应的 request_id。
    pub request_id: u64,
    /// 响应状态。
    pub status: ResponseStatus,
    /// 可选错误信息（当 status=Error 时使用）。
    #[serde(default)]
    pub error: Option<crate::ErrorCode>,
}

impl QueryResponse {
    pub fn ok(request_id: u64) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            status: ResponseStatus::Ok,
            error: None,
        }
    }

    pub fn error(request_id: u64, error: crate::ErrorCode) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            status: ResponseStatus::Error,
            error: Some(error),
        }
    }
}

/// 客户端 -> 服务端握手请求。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct HandshakeRequest {
    /// 协议版本号。
    pub version: u8,
}

impl HandshakeRequest {
    pub fn current() -> Self {
        Self {
            version: PROTOCOL_VERSION,
        }
    }

    pub fn validate(&self) -> Result<(), crate::ErrorCode> {
        validate_protocol_version(self.version)
    }
}

/// Server 与 Client 首次建立连接时传递的握手信息。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct HandshakeInfo {
    /// 协议版本号。
    pub version: u8,
    /// 服务端主机名。
    pub hostname: String,
    /// GPU 数量。
    pub gpu_num: u8,
    /// 固定 payload 字节长度。
    pub payload_len: u16,
}

impl HandshakeInfo {
    pub fn new(hostname: impl Into<String>, gpu_num: u8, payload_len: u16) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            hostname: hostname.into(),
            gpu_num,
            payload_len,
        }
    }

    pub fn validate(&self) -> Result<(), crate::ErrorCode> {
        validate_protocol_version(self.version)
    }
}

/// 客户端向多播组发送的发现查询报文。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct DiscoveryQuery {
    /// 协议版本号（用于后续兼容演进）。
    pub version: u8,
}

/// 服务端周期性广播的发现通告报文。
#[derive(Debug, Clone, SerdeSerialize, SerdeDeserialize, PartialEq, Eq)]
pub struct DiscoveryAnnounce {
    /// 协议版本号（用于后续兼容演进）。
    pub version: u8,
    /// 服务端主机名。
    pub hostname: String,
    /// 服务端监听 IP。
    pub ip: String,
    /// 服务端监听端口。
    pub port: u16,
    /// KCP 监听端口；省略时兼容旧客户端使用 `port`。
    #[serde(default)]
    pub kcp_port: Option<u16>,
    /// TCP 监听端口；省略时兼容旧服务端使用 `port`。
    #[serde(default)]
    pub tcp_port: Option<u16>,
    /// 建议客户端缓存该发现记录的秒数。
    #[serde(default)]
    pub ttl: Option<u16>,
    /// 服务端负载百分比（0-100）。
    #[serde(default)]
    pub load: Option<u8>,
    /// 服务端是否处于降级模式。
    #[serde(default)]
    pub degraded: Option<bool>,
}

/// 版本协商/校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    Matched,
    Mismatch { expected: u8, got: u8 },
}

pub fn check_version(version: u8) -> VersionCheck {
    if version == PROTOCOL_VERSION {
        VersionCheck::Matched
    } else {
        VersionCheck::Mismatch {
            expected: PROTOCOL_VERSION,
            got: version,
        }
    }
}

pub fn validate_protocol_version(version: u8) -> Result<(), crate::ErrorCode> {
    match check_version(version) {
        VersionCheck::Matched => Ok(()),
        VersionCheck::Mismatch { .. } => Err(crate::ErrorCode::ProtocolVersionMismatch),
    }
}

/// Encode a complete `ServerGpuSnapshot` as the canonical GPU payload.
///
/// The returned bytes are the full rkyv archive for `ServerGpuSnapshot`.
pub fn encode_snapshot_payload(
    snapshot: &ServerGpuSnapshot,
) -> Result<Vec<u8>, PayloadEncodeError> {
    let bytes = rkyv::to_bytes::<RkyvError>(snapshot)
        .map_err(|err| PayloadEncodeError::SerializeFailed(err.to_string()))?
        .into_vec();

    payload_len_from_archived_len(bytes.len()).map_err(|err| match err {
        PayloadLenError::TooLarge { len, max } => PayloadEncodeError::PayloadTooLarge { len, max },
    })?;

    Ok(bytes)
}

/// Decode a GPU payload into an owned `ServerGpuSnapshot`.
///
/// The function copies into an aligned buffer before deserializing so callers can
/// pass payload slices directly from transport frames.
pub fn decode_snapshot_payload(bytes: &[u8]) -> Result<ServerGpuSnapshot, PayloadDecodeError> {
    if bytes.is_empty() {
        return Err(PayloadDecodeError::Empty);
    }

    let mut aligned = AlignedVec::<16>::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);

    rkyv::from_bytes::<ServerGpuSnapshot, RkyvError>(&aligned)
        .map_err(|err| PayloadDecodeError::DeserializeFailed(err.to_string()))
}

/// Encode a complete binary transport frame.
///
/// `header.payload_len` is normalized to `payload.len()` to keep the returned
/// frame self-consistent.
pub fn encode_frame(mut header: FrameHeader, payload: &[u8]) -> Vec<u8> {
    header.payload_len = payload.len() as u32;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out
}

/// Decode a complete binary transport frame and return the header plus payload slice.
pub fn decode_frame(bytes: &[u8]) -> Result<(FrameHeader, &[u8]), FrameDecodeError> {
    let header = FrameHeader::decode(bytes).map_err(FrameDecodeError::from)?;
    let payload = &bytes[FRAME_HEADER_LEN..];
    let expected = header.payload_len as usize;

    if payload.len() != expected {
        return Err(FrameDecodeError::PayloadLengthMismatch {
            expected,
            actual: payload.len(),
        });
    }

    Ok((header, payload))
}

/// Validate that an encoded common payload can be advertised in `HandshakeInfo`.
pub fn payload_len_for_handshake(payload: &[u8]) -> Result<u16, PayloadLenError> {
    payload_len_from_archived_len(payload.len())
}

/// 根据完整 rkyv snapshot payload 字节长度计算握手中的固定 payload 长度。
pub fn payload_len_from_archived_len(archived_len: usize) -> Result<u16, PayloadLenError> {
    if archived_len > MAX_PAYLOAD_LEN {
        return Err(PayloadLenError::TooLarge {
            len: archived_len,
            max: MAX_PAYLOAD_LEN,
        });
    }

    Ok(archived_len as u16)
}

/// 从握手 payload 长度还原 rkyv 归档字节长度。
pub fn archived_len_from_payload_len(payload_len: u16) -> Option<usize> {
    Some(payload_len as usize)
}

/// Deprecated compatibility helper from the timestamp-prefix draft.
#[deprecated(
    since = "0.1.0",
    note = "Current payload is complete ServerGpuSnapshot rkyv bytes; use encode_snapshot_payload"
)]
pub fn encode_payload(
    timestamp_ms: i64,
    archived_server_gpus: &[u8],
) -> Result<Vec<u8>, PayloadLenError> {
    #[allow(deprecated)]
    let payload_len = PAYLOAD_TIMESTAMP_LEN
        .checked_add(archived_server_gpus.len())
        .ok_or(PayloadLenError::TooLarge {
            len: usize::MAX,
            max: MAX_PAYLOAD_LEN,
        })?;
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(PayloadLenError::TooLarge {
            len: payload_len,
            max: MAX_PAYLOAD_LEN,
        });
    }

    let mut out = Vec::with_capacity(payload_len);
    out.extend_from_slice(&timestamp_ms.to_be_bytes());
    out.extend_from_slice(archived_server_gpus);
    Ok(out)
}

/// Deprecated compatibility helper from the timestamp-prefix draft.
#[deprecated(
    since = "0.1.0",
    note = "Current payload is complete ServerGpuSnapshot rkyv bytes; use decode_snapshot_payload"
)]
pub fn decode_payload(payload: &[u8]) -> Option<(i64, &[u8])> {
    #[allow(deprecated)]
    if payload.len() < PAYLOAD_TIMESTAMP_LEN {
        return None;
    }

    #[allow(deprecated)]
    let mut ts = [0u8; PAYLOAD_TIMESTAMP_LEN];
    #[allow(deprecated)]
    ts.copy_from_slice(&payload[..PAYLOAD_TIMESTAMP_LEN]);
    #[allow(deprecated)]
    Some((i64::from_be_bytes(ts), &payload[PAYLOAD_TIMESTAMP_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_len_is_u16_type_level_constraint() {
        let info = HandshakeInfo {
            version: PROTOCOL_VERSION,
            hostname: "node-a".to_string(),
            gpu_num: 8,
            payload_len: u16::MAX,
        };
        assert_eq!(info.payload_len, 65_535);
    }

    #[test]
    fn payload_len_matches_complete_snapshot_archive_len() {
        assert_eq!(payload_len_from_archived_len(0), Ok(0));
        assert_eq!(payload_len_from_archived_len(4), Ok(4));
        assert_eq!(archived_len_from_payload_len(12), Some(12));
        assert_eq!(archived_len_from_payload_len(0), Some(0));
    }

    #[test]
    fn payload_len_rejects_values_that_do_not_fit_handshake() {
        let err = payload_len_from_archived_len(MAX_PAYLOAD_LEN + 1).unwrap_err();
        assert_eq!(
            err,
            PayloadLenError::TooLarge {
                len: MAX_PAYLOAD_LEN + 1,
                max: MAX_PAYLOAD_LEN,
            }
        );
    }

    #[test]
    fn snapshot_payload_roundtrip_preserves_processes() {
        let snapshot = sample_snapshot();
        let payload = encode_snapshot_payload(&snapshot).expect("encode");
        let decoded = decode_snapshot_payload(&payload).expect("decode");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.gpus[0].processes[0].uid, 1000);
        assert_eq!(
            decoded.gpus[0].processes[0].command.as_deref(),
            Some("python train.py")
        );
    }

    #[test]
    fn snapshot_payload_roundtrip_allows_empty_gpu_list() {
        let snapshot = ServerGpuSnapshot {
            hostname: "empty-node".to_string(),
            driver_version: None,
            gpus: Vec::new(),
        };

        let payload = encode_snapshot_payload(&snapshot).expect("encode");
        let decoded = decode_snapshot_payload(&payload).expect("decode");

        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn snapshot_payload_roundtrip_preserves_multi_gpu_multi_process_shape() {
        let snapshot = ServerGpuSnapshot {
            hostname: "mock-node-02".to_string(),
            driver_version: None,
            gpus: vec![
                GpuInfo {
                    index: 0,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: None,
                    uuid: Some("GPU-mock-0000".to_string()),
                    memory: GpuMemory {
                        used_mb: 1_234,
                        total_mb: 16_384,
                    },
                    utilization: GpuUtilization {
                        gpu_percent: 87,
                        memory_percent: 42,
                    },
                    processes: vec![
                        GpuProcessInfo {
                            pid: 1001,
                            uid: 1001,
                            command: Some("python train.py".to_string()),
                            used_memory_mb: 512,
                        },
                        GpuProcessInfo {
                            pid: 1002,
                            uid: 1002,
                            command: Some("python eval.py".to_string()),
                            used_memory_mb: 256,
                        },
                    ],
                },
                GpuInfo {
                    index: 1,
                    name: "NVIDIA A100".to_string(),
                    temperature_c: None,
                    uuid: Some("GPU-mock-0001".to_string()),
                    memory: GpuMemory {
                        used_mb: 2_048,
                        total_mb: 16_384,
                    },
                    utilization: GpuUtilization {
                        gpu_percent: 12,
                        memory_percent: 8,
                    },
                    processes: Vec::new(),
                },
            ],
        };

        let payload = encode_snapshot_payload(&snapshot).expect("encode");
        let decoded = decode_snapshot_payload(&payload).expect("decode");

        assert_eq!(decoded.hostname, "mock-node-02");
        assert_eq!(decoded.gpus.len(), 2);
        assert_eq!(decoded.gpus[0].index, 0);
        assert_eq!(decoded.gpus[1].index, 1);
        assert_eq!(decoded.gpus[0].uuid.as_deref(), Some("GPU-mock-0000"));
        assert_eq!(decoded.gpus[1].uuid.as_deref(), Some("GPU-mock-0001"));
        assert_eq!(decoded.gpus[0].processes.len(), 2);
        assert_eq!(decoded.gpus[0].processes[0].uid, 1001);
        assert_eq!(decoded.gpus[0].processes[1].pid, 1002);
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn snapshot_payload_decode_rejects_empty_bytes() {
        assert_eq!(decode_snapshot_payload(&[]), Err(PayloadDecodeError::Empty));
    }

    #[test]
    fn snapshot_payload_encode_rejects_handshake_u16_overflow() {
        let snapshot = ServerGpuSnapshot {
            hostname: "oversized-node".to_string(),
            driver_version: None,
            gpus: vec![GpuInfo {
                index: 0,
                name: "x".repeat(MAX_PAYLOAD_LEN + 1),
                temperature_c: None,
                uuid: None,
                memory: GpuMemory {
                    used_mb: 0,
                    total_mb: 0,
                },
                utilization: GpuUtilization {
                    gpu_percent: 0,
                    memory_percent: 0,
                },
                processes: Vec::new(),
            }],
        };

        let err = encode_snapshot_payload(&snapshot).unwrap_err();
        match err {
            PayloadEncodeError::PayloadTooLarge { len, max } => {
                assert!(len > MAX_PAYLOAD_LEN);
                assert_eq!(max, MAX_PAYLOAD_LEN);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn frame_header_roundtrip() {
        let header = FrameHeader::new(FrameType::DataPayload, 42, 4096);
        let encoded = header.encode();
        assert_eq!(&encoded[0..4], &FRAME_MAGIC);
        assert_eq!(FrameHeader::decode(&encoded), Ok(header));
    }

    #[test]
    fn complete_frame_roundtrip() {
        let header = FrameHeader::new(FrameType::QueryRequest, 42, 0);
        let payload = b"{\"version\":1,\"request_id\":42}";
        let frame = encode_frame(header, payload);
        let (decoded_header, decoded_payload) = decode_frame(&frame).expect("decode frame");

        assert_eq!(decoded_header.frame_type, FrameType::QueryRequest);
        assert_eq!(decoded_header.request_id, 42);
        assert_eq!(decoded_header.payload_len, payload.len() as u32);
        assert_eq!(decoded_payload, payload);
    }

    #[test]
    fn complete_frame_roundtrip_allows_empty_payload() {
        let header = FrameHeader::new(FrameType::HandshakeRequest, 1, 123);
        let frame = encode_frame(header, &[]);
        let (decoded_header, decoded_payload) = decode_frame(&frame).expect("decode frame");

        assert_eq!(decoded_header.frame_type, FrameType::HandshakeRequest);
        assert_eq!(decoded_header.request_id, 1);
        assert_eq!(decoded_header.payload_len, 0);
        assert!(decoded_payload.is_empty());
    }

    #[test]
    fn complete_frame_roundtrip_allows_large_transport_payload() {
        let payload = vec![0xabu8; MAX_PAYLOAD_LEN + 512];
        let frame = encode_frame(FrameHeader::new(FrameType::DataPayload, 5, 0), &payload);
        let (decoded_header, decoded_payload) = decode_frame(&frame).expect("decode frame");

        assert_eq!(decoded_header.frame_type, FrameType::DataPayload);
        assert_eq!(decoded_header.payload_len, payload.len() as u32);
        assert_eq!(decoded_payload, payload);
        assert!(payload_len_for_handshake(decoded_payload).is_err());
    }

    #[test]
    fn complete_frame_rejects_payload_length_mismatch() {
        let header = FrameHeader::new(FrameType::DataPayload, 7, 10);
        let mut frame = Vec::from(header.encode());
        frame.extend_from_slice(b"short");

        assert_eq!(
            decode_frame(&frame),
            Err(FrameDecodeError::PayloadLengthMismatch {
                expected: 10,
                actual: 5,
            })
        );
    }

    #[test]
    fn complete_frame_decode_does_not_accept_concatenated_frames() {
        let first = encode_frame(FrameHeader::new(FrameType::QueryRequest, 1, 0), b"one");
        let second = encode_frame(FrameHeader::new(FrameType::QueryRequest, 2, 0), b"two");
        let mut concatenated = first;
        concatenated.extend_from_slice(&second);

        assert_eq!(
            decode_frame(&concatenated),
            Err(FrameDecodeError::PayloadLengthMismatch {
                expected: 3,
                actual: 3 + FRAME_HEADER_LEN + 3,
            })
        );
    }

    #[test]
    fn frame_header_rejects_invalid_inputs() {
        assert_eq!(
            FrameHeader::decode(&[0u8; 4]),
            Err(FrameHeaderError::TooShort {
                got: 4,
                min: FRAME_HEADER_LEN,
            })
        );

        let mut bad_magic = FrameHeader::new(FrameType::QueryRequest, 1, 0).encode();
        bad_magic[0] = b'X';
        assert_eq!(
            FrameHeader::decode(&bad_magic),
            Err(FrameHeaderError::BadMagic)
        );

        let mut bad_version = FrameHeader::new(FrameType::QueryRequest, 1, 0).encode();
        bad_version[4] = PROTOCOL_VERSION + 1;
        assert_eq!(
            FrameHeader::decode(&bad_version),
            Err(FrameHeaderError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: PROTOCOL_VERSION + 1,
            })
        );

        let mut bad_type = FrameHeader::new(FrameType::QueryRequest, 1, 0).encode();
        bad_type[5] = 255;
        assert_eq!(
            FrameHeader::decode(&bad_type),
            Err(FrameHeaderError::UnknownFrameType(255))
        );
    }

    #[test]
    fn complete_frame_rejects_version_mismatch() {
        let mut frame = encode_frame(FrameHeader::new(FrameType::QueryRequest, 1, 0), b"{}");
        frame[4] = PROTOCOL_VERSION + 1;

        assert_eq!(
            decode_frame(&frame),
            Err(FrameDecodeError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: PROTOCOL_VERSION + 1,
            })
        );
    }

    #[test]
    fn complete_frame_rejects_unknown_frame_type() {
        let mut frame = encode_frame(FrameHeader::new(FrameType::QueryRequest, 1, 0), b"{}");
        frame[5] = 255;

        assert_eq!(
            decode_frame(&frame),
            Err(FrameDecodeError::UnknownFrameType(255))
        );
    }

    #[test]
    fn kcp_handshake_query_data_sequence_encodes_and_decodes() {
        let request_id = 99;

        let handshake_request = HandshakeRequest::current();
        let handshake_request_payload =
            serde_json::to_vec(&handshake_request).expect("encode handshake request json");
        let handshake_request_frame = encode_frame(
            FrameHeader::new(FrameType::HandshakeRequest, 0, 0),
            &handshake_request_payload,
        );
        let (header, payload) =
            decode_frame(&handshake_request_frame).expect("decode handshake request");
        assert_eq!(header.frame_type, FrameType::HandshakeRequest);
        let decoded_request: HandshakeRequest =
            serde_json::from_slice(payload).expect("decode handshake request payload");
        assert_eq!(decoded_request.validate(), Ok(()));

        let snapshot = sample_snapshot();
        let data_payload = encode_snapshot_payload(&snapshot).expect("encode snapshot");
        let handshake_payload_len =
            payload_len_for_handshake(&data_payload).expect("payload fits handshake");
        let handshake_info = HandshakeInfo::new(
            snapshot.hostname.clone(),
            snapshot.gpus.len() as u8,
            handshake_payload_len,
        );
        let handshake_info_payload =
            serde_json::to_vec(&handshake_info).expect("encode handshake info json");
        let handshake_info_frame = encode_frame(
            FrameHeader::new(FrameType::HandshakeInfo, 0, 0),
            &handshake_info_payload,
        );
        let (header, payload) = decode_frame(&handshake_info_frame).expect("decode handshake info");
        assert_eq!(header.frame_type, FrameType::HandshakeInfo);
        let decoded_info: HandshakeInfo =
            serde_json::from_slice(payload).expect("decode handshake info payload");
        assert_eq!(decoded_info.validate(), Ok(()));
        assert_eq!(decoded_info.payload_len, data_payload.len() as u16);

        let query = QueryRequest {
            version: PROTOCOL_VERSION,
            request_id,
        };
        let query_payload = serde_json::to_vec(&query).expect("encode query json");
        let query_frame = encode_frame(
            FrameHeader::new(FrameType::QueryRequest, request_id, 0),
            &query_payload,
        );
        let (header, payload) = decode_frame(&query_frame).expect("decode query");
        assert_eq!(header.frame_type, FrameType::QueryRequest);
        assert_eq!(header.request_id, request_id);
        let decoded_query: QueryRequest =
            serde_json::from_slice(payload).expect("decode query payload");
        assert_eq!(decoded_query, query);

        let data_frame = encode_frame(
            FrameHeader::new(FrameType::DataPayload, request_id, 0),
            &data_payload,
        );
        let (header, payload) = decode_frame(&data_frame).expect("decode data");
        assert_eq!(header.frame_type, FrameType::DataPayload);
        assert_eq!(header.request_id, request_id);
        assert_eq!(header.payload_len, handshake_payload_len as u32);
        let decoded_snapshot = decode_snapshot_payload(payload).expect("decode snapshot payload");
        assert_eq!(decoded_snapshot, snapshot);
    }

    #[test]
    fn stable_gpu_model_json_bootstrap_roundtrip() {
        let snapshot = sample_snapshot();

        let encoded = serde_json::to_vec(&snapshot).expect("serialize");
        let decoded: ServerGpuSnapshot = serde_json::from_slice(&encoded).expect("deserialize");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn handshake_version_mismatch_uses_stable_error_code() {
        let req = HandshakeRequest {
            version: PROTOCOL_VERSION + 1,
        };
        assert_eq!(
            req.validate(),
            Err(crate::ErrorCode::ProtocolVersionMismatch)
        );
        assert_eq!(crate::ErrorCode::ProtocolVersionMismatch.code(), 1010);
    }

    #[test]
    fn discovery_announce_backward_compatible_optional_fields() {
        let json = r#"{"version":1,"hostname":"node-a","ip":"10.0.0.1","port":30001}"#;
        let msg: DiscoveryAnnounce = serde_json::from_str(json).expect("deserialize");
        assert_eq!(msg.ttl, None);
        assert_eq!(msg.load, None);
        assert_eq!(msg.degraded, None);
        assert_eq!(msg.kcp_port, None);
        assert_eq!(msg.tcp_port, None);
    }

    #[test]
    fn query_response_roundtrip() {
        let resp = QueryResponse::error(42, crate::ErrorCode::QueryTimeout);
        let data = serde_json::to_vec(&resp).expect("serialize");
        let decoded: QueryResponse = serde_json::from_slice(&data).expect("deserialize");
        assert_eq!(decoded, resp);
    }

    #[test]
    fn version_mismatch_behavior() {
        let result = check_version(PROTOCOL_VERSION + 1);
        assert_eq!(
            result,
            VersionCheck::Mismatch {
                expected: PROTOCOL_VERSION,
                got: PROTOCOL_VERSION + 1,
            }
        );
        assert_eq!(
            validate_protocol_version(PROTOCOL_VERSION + 1),
            Err(crate::ErrorCode::ProtocolVersionMismatch)
        );
    }

    fn sample_snapshot() -> ServerGpuSnapshot {
        ServerGpuSnapshot {
            hostname: "node-a".to_string(),
            driver_version: None,
            gpus: vec![GpuInfo {
                index: 0,
                name: "NVIDIA A100".to_string(),
                temperature_c: None,
                uuid: Some("GPU-123".to_string()),
                memory: GpuMemory {
                    used_mb: 1024,
                    total_mb: 81920,
                },
                utilization: GpuUtilization {
                    gpu_percent: 75,
                    memory_percent: 20,
                },
                processes: vec![GpuProcessInfo {
                    pid: 1234,
                    uid: 1000,
                    command: Some("python train.py".to_string()),
                    used_memory_mb: 512,
                }],
            }],
        }
    }
}
