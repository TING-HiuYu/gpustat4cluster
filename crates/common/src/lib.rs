//! Team A（协议/公共层）公共定义。

pub mod config;
pub mod error;
pub mod protocol;

pub use config::{Config, ConnectingConfig, LogConfig, RuntimeConfig, ServicesConfig};
pub use error::ErrorCode;
pub use protocol::{
    archived_len_from_payload_len, check_version, decode_frame, decode_snapshot_payload,
    encode_frame, encode_snapshot_payload, payload_len_for_handshake,
    payload_len_from_archived_len, validate_protocol_version, DiscoveryAnnounce, DiscoveryQuery,
    FrameDecodeError, FrameHeader, FrameHeaderError, FrameType, GpuInfo, GpuMemory, GpuProcessInfo,
    GpuUtilization, HandshakeInfo, HandshakeRequest, PayloadDecodeError, PayloadEncodeError,
    PayloadLenError, QueryRequest, QueryResponse, ResponseStatus, ServerGpuSnapshot, VersionCheck,
    FRAME_HEADER_LEN, FRAME_MAGIC, MAX_PAYLOAD_LEN, PROTOCOL_VERSION,
};
