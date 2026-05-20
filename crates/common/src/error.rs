use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 通用错误码（稳定编号，可用于协议和日志对齐）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Error)]
#[repr(u16)]
pub enum ErrorCode {
    #[error("nvml unavailable")]
    NvmlUnavailable = 1001,
    #[error("configuration invalid")]
    ConfigInvalid = 1002,
    #[error("port range exhausted")]
    PortExhausted = 1003,
    #[error("multicast setup failed")]
    MulticastFailed = 1004,
    #[error("kcp initialization failed")]
    KcpInitFailed = 1005,
    #[error("heartbeat timeout")]
    HeartbeatTimeout = 1006,
    #[error("connection closed")]
    ConnectionClosed = 1007,
    #[error("query timeout")]
    QueryTimeout = 1008,
    #[error("invalid filter")]
    InvalidFilter = 1009,
    #[error("protocol version mismatch")]
    ProtocolVersionMismatch = 1010,
    #[error("internal error")]
    Internal = 1999,
}

impl ErrorCode {
    pub const fn code(self) -> u16 {
        self as u16
    }

    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1001 => Some(Self::NvmlUnavailable),
            1002 => Some(Self::ConfigInvalid),
            1003 => Some(Self::PortExhausted),
            1004 => Some(Self::MulticastFailed),
            1005 => Some(Self::KcpInitFailed),
            1006 => Some(Self::HeartbeatTimeout),
            1007 => Some(Self::ConnectionClosed),
            1008 => Some(Self::QueryTimeout),
            1009 => Some(Self::InvalidFilter),
            1010 => Some(Self::ProtocolVersionMismatch),
            1999 => Some(Self::Internal),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    #[test]
    fn stable_numeric_mapping_roundtrip() {
        let code = ErrorCode::QueryTimeout.code();
        assert_eq!(code, 1008);
        assert_eq!(ErrorCode::from_code(code), Some(ErrorCode::QueryTimeout));
    }
}
