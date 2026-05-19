//! Team A（协议/公共层）公共定义。

pub mod config;
pub mod error;
pub mod protocol;

pub use config::{Config, ConnectingConfig, LogConfig, ServicesConfig};
pub use error::ErrorCode;
pub use protocol::{
    check_version, DiscoveryAnnounce, DiscoveryQuery, HandshakeInfo, HandshakeRequest,
    QueryRequest, QueryResponse, ResponseStatus, VersionCheck, PROTOCOL_VERSION,
};
