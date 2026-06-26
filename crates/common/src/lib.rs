//! Team A（协议/公共层）公共定义。

pub mod config;
pub mod error;
pub mod protocol;
pub mod udp;

pub use config::{Config, ConnectingConfig, LogConfig, RuntimeConfig, ServicesConfig};
pub use error::ErrorCode;
pub use protocol::{
    archived_len_from_payload_len, check_version, decode_frame, decode_metadata_payload,
    decode_runtime_payload, decode_snapshot_payload, encode_frame, encode_metadata_payload,
    encode_runtime_payload, encode_snapshot_payload, payload_len_from_archived_len,
    validate_protocol_version, DiscoveryAnnounce, DiscoveryQuery, FrameDecodeError, FrameHeader,
    FrameHeaderError, FrameType, GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization, GresInfo,
    GresMemory, GresProcessInfo, GresProcessRuntimeInfo, GresRuntimeInfo, GresStaticInfo,
    GresUtilization, HandshakeInfo, HandshakeRequest, HostMetadata, PayloadDecodeError,
    PayloadEncodeError, PayloadLenError, QueryRequest, QueryResponse, ResponseStatus,
    RuntimeSnapshot, ServerGpuSnapshot, ServerGresSnapshot, VersionCheck, FRAME_HEADER_LEN,
    FRAME_MAGIC, MAX_PAYLOAD_LEN, PROTOCOL_VERSION,
};
