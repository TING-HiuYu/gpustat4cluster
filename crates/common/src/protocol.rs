use serde::{Deserialize, Serialize};

/// 协议 v1 版本号。
pub const PROTOCOL_VERSION: u8 = 1;

/// 查询请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryRequest {
    /// 协议版本号。
    pub version: u8,
    /// 请求 ID（客户端生成，服务端原样回传）。
    pub request_id: u64,
}

/// 查询响应状态码。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseStatus {
    Ok = 0,
    Error = 1,
}

/// 查询响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// 客户端 -> 服务端握手请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandshakeRequest {
    /// 协议版本号。
    pub version: u8,
}

/// Server 与 Client 首次建立连接时传递的握手信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

/// 客户端向多播组发送的发现查询报文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryQuery {
    /// 协议版本号（用于后续兼容演进）。
    pub version: u8,
}

/// 服务端周期性广播的发现通告报文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryAnnounce {
    /// 协议版本号（用于后续兼容演进）。
    pub version: u8,
    /// 服务端主机名。
    pub hostname: String,
    /// 服务端监听 IP。
    pub ip: String,
    /// 服务端监听端口。
    pub port: u16,
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
    fn discovery_announce_backward_compatible_optional_fields() {
        let json = r#"{"version":1,"hostname":"node-a","ip":"10.0.0.1","port":30001}"#;
        let msg: DiscoveryAnnounce = serde_json::from_str(json).expect("deserialize");
        assert_eq!(msg.ttl, None);
        assert_eq!(msg.load, None);
        assert_eq!(msg.degraded, None);
    }

    #[test]
    fn query_response_roundtrip() {
        let resp = QueryResponse {
            version: PROTOCOL_VERSION,
            request_id: 42,
            status: ResponseStatus::Error,
            error: Some(crate::ErrorCode::QueryTimeout),
        };
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
    }
}
