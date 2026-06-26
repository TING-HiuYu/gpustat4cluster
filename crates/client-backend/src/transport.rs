use common::{
    protocol::{
        decode_frame, decode_metadata_payload, decode_runtime_payload, encode_frame,
        FrameDecodeError, FrameHeader, FrameType,
    },
    ErrorCode, HandshakeRequest, HostMetadata, QueryRequest as WireQueryRequest,
    QueryResponse as WireQueryResponse, ResponseStatus, RuntimeSnapshot, PROTOCOL_VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Frame(FrameDecodeError),
    UnexpectedFrameType { expected: FrameType, got: FrameType },
    Json(String),
    Payload(String),
    QueryError(Option<ErrorCode>),
}

pub fn build_handshake_request_frame(request_id: u64) -> Result<Vec<u8>, TransportError> {
    encode_json_frame(
        FrameType::HandshakeRequest,
        request_id,
        &HandshakeRequest::current(),
    )
}

pub fn build_query_request_frame(request_id: u64) -> Result<Vec<u8>, TransportError> {
    encode_json_frame(
        FrameType::QueryRequest,
        request_id,
        &WireQueryRequest {
            version: PROTOCOL_VERSION,
            request_id,
        },
    )
}

pub fn build_disconnect_frame(request_id: u64, reason: &str) -> Result<Vec<u8>, TransportError> {
    encode_bytes_frame(FrameType::Disconnect, request_id, reason.as_bytes())
}

pub fn parse_disconnect_reason_frame(frame: &[u8]) -> Result<String, TransportError> {
    let (_, payload) = split_frame(frame, FrameType::Disconnect)?;
    Ok(String::from_utf8_lossy(payload).trim().to_string())
}

pub fn parse_metadata_payload_frame(frame: &[u8]) -> Result<HostMetadata, TransportError> {
    let (_, payload) = split_frame(frame, FrameType::MetadataPayload)?;
    let aligned_payload = payload.to_vec();
    decode_metadata_payload(&aligned_payload)
        .map_err(|e| TransportError::Payload(format!("decode metadata payload failed: {:?}", e)))
}

pub fn parse_runtime_payload_frame(frame: &[u8]) -> Result<RuntimeSnapshot, TransportError> {
    let (_, payload) = split_frame(frame, FrameType::RuntimePayload)?;
    let aligned_payload = payload.to_vec();
    decode_runtime_payload(&aligned_payload)
        .map_err(|e| TransportError::Payload(format!("decode runtime payload failed: {:?}", e)))
}

pub fn parse_query_response_error_frame(frame: &[u8]) -> Result<Option<ErrorCode>, TransportError> {
    let (_, payload) = split_frame(frame, FrameType::QueryResponse)?;
    let resp: WireQueryResponse = serde_json::from_slice(payload)
        .map_err(|e| TransportError::Json(format!("decode QueryResponse failed: {}", e)))?;

    if resp.version != PROTOCOL_VERSION {
        return Err(TransportError::QueryError(Some(
            ErrorCode::ProtocolVersionMismatch,
        )));
    }

    match resp.status {
        ResponseStatus::Ok => Ok(None),
        ResponseStatus::Error => Ok(resp.error),
    }
}

#[cfg(test)]
pub fn build_metadata_payload_frame(
    request_id: u64,
    payload: &[u8],
) -> Result<Vec<u8>, TransportError> {
    encode_bytes_frame(FrameType::MetadataPayload, request_id, payload)
}

#[cfg(test)]
pub fn build_runtime_payload_frame(
    request_id: u64,
    payload: &[u8],
) -> Result<Vec<u8>, TransportError> {
    encode_bytes_frame(FrameType::RuntimePayload, request_id, payload)
}

#[cfg(test)]
pub fn build_query_response_frame(
    request_id: u64,
    response: &WireQueryResponse,
) -> Result<Vec<u8>, TransportError> {
    encode_json_frame(FrameType::QueryResponse, request_id, response)
}

fn encode_json_frame<T: serde::Serialize>(
    frame_type: FrameType,
    request_id: u64,
    value: &T,
) -> Result<Vec<u8>, TransportError> {
    let payload = serde_json::to_vec(value)
        .map_err(|e| TransportError::Json(format!("encode frame payload failed: {}", e)))?;
    encode_bytes_frame(frame_type, request_id, &payload)
}

fn encode_bytes_frame(
    frame_type: FrameType,
    request_id: u64,
    payload: &[u8],
) -> Result<Vec<u8>, TransportError> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        TransportError::Payload(format!(
            "payload too large for frame: {} bytes",
            payload.len()
        ))
    })?;
    let header = FrameHeader::new(frame_type, request_id, payload_len);
    Ok(encode_frame(header, payload))
}

fn split_frame(frame: &[u8], expected: FrameType) -> Result<(FrameHeader, &[u8]), TransportError> {
    let (header, payload) = decode_frame(frame).map_err(TransportError::Frame)?;
    if header.frame_type != expected {
        return Err(TransportError::UnexpectedFrameType {
            expected,
            got: header.frame_type,
        });
    }

    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        encode_metadata_payload, encode_runtime_payload, GresInfo, GresMemory, GresProcessInfo,
        GresUtilization, HostMetadata, RuntimeSnapshot, ServerGresSnapshot,
    };

    fn snapshot() -> ServerGresSnapshot {
        ServerGresSnapshot {
            hostname: "node-a".to_string(),
            driver_version: None,
            gres: vec![GresInfo {
                index: 0,
                name: "NVIDIA A100".to_string(),
                temperature_c: None,
                uuid: None,
                memory: GresMemory {
                    used_mb: 1024,
                    total_mb: 81920,
                },
                utilization: GresUtilization {
                    gres_percent: 88,
                    memory_percent: 10,
                },
                processes: vec![GresProcessInfo {
                    pid: 42,
                    uid: 1000,
                    command: Some("python".to_string()),
                    used_memory_mb: 512,
                }],
            }],
        }
    }

    #[test]
    fn builds_handshake_request_frame() {
        let frame = build_handshake_request_frame(7).expect("frame");
        let (header, payload) = split_frame(&frame, FrameType::HandshakeRequest).expect("split");
        assert_eq!(header.request_id, 7);
        let req: HandshakeRequest = serde_json::from_slice(payload).expect("handshake json");
        assert_eq!(req.version, PROTOCOL_VERSION);
    }

    #[test]
    fn parses_metadata_payload_frame() {
        let metadata = HostMetadata::from_snapshot(&snapshot());
        let payload = encode_metadata_payload(&metadata).expect("metadata payload");
        let frame = build_metadata_payload_frame(9, &payload).expect("frame");
        let parsed = parse_metadata_payload_frame(&frame).expect("parse metadata");
        assert_eq!(parsed.hostname, "node-a");
        assert_eq!(parsed.gres.len(), 1);
    }

    #[test]
    fn builds_query_request_frame() {
        let frame = build_query_request_frame(11).expect("frame");
        let (header, payload) = split_frame(&frame, FrameType::QueryRequest).expect("split");
        assert_eq!(header.request_id, 11);
        let req: WireQueryRequest = serde_json::from_slice(payload).expect("query json");
        assert_eq!(req.request_id, 11);
        assert_eq!(req.version, PROTOCOL_VERSION);
    }

    #[test]
    fn parses_runtime_payload_frame() {
        let runtime = RuntimeSnapshot::from_snapshot(&snapshot());
        let payload = encode_runtime_payload(&runtime).expect("runtime payload");
        let frame = build_runtime_payload_frame(12, &payload).expect("frame");
        let parsed = parse_runtime_payload_frame(&frame).expect("runtime");
        assert_eq!(parsed.gres[0].utilization.gres_percent, 88);
    }

    #[test]
    fn parses_query_response_error_frame() {
        let response = WireQueryResponse::error(13, ErrorCode::QueryTimeout);
        let frame = build_query_response_frame(13, &response).expect("frame");
        let error = parse_query_response_error_frame(&frame).expect("query response");
        assert_eq!(error, Some(ErrorCode::QueryTimeout));
    }

    #[test]
    fn rejects_unexpected_frame_type() {
        let frame = build_query_request_frame(14).expect("frame");
        let err = parse_runtime_payload_frame(&frame).unwrap_err();
        assert_eq!(
            err,
            TransportError::UnexpectedFrameType {
                expected: FrameType::RuntimePayload,
                got: FrameType::QueryRequest,
            }
        );
    }
}
