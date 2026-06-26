use std::{fmt, sync::Arc};

use common::{
    encode_metadata_payload, encode_runtime_payload, ErrorCode, FrameDecodeError, FrameHeader,
    FrameType, HandshakeRequest, HostMetadata, QueryRequest, QueryResponse, RuntimeSnapshot,
};

use crate::cache::GresCache;
use crate::collector::GresCollector;

pub struct TransportContext {
    collector: Arc<dyn GresCollector>,
    cache: Arc<GresCache>,
    ttl_ms: u64,
}

impl TransportContext {
    pub fn new(
        _hostname: impl Into<String>,
        collector: Arc<dyn GresCollector>,
        cache: Arc<GresCache>,
        ttl_ms: u64,
    ) -> Self {
        Self {
            collector,
            cache,
            ttl_ms,
        }
    }

    #[allow(dead_code)]
    pub fn handle_frame(&self, frame: &[u8]) -> Result<Vec<u8>, TransportError> {
        let decoded = decode_transport_frame(frame)?;
        self.handle_decoded_frame(decoded)
    }

    pub fn handle_decoded_frame(&self, decoded: DecodedFrame) -> Result<Vec<u8>, TransportError> {
        match decoded.header.frame_type {
            FrameType::HandshakeRequest => {
                self.handle_handshake(decoded.header.request_id, &decoded.payload)
            }
            FrameType::QueryRequest => {
                self.handle_query(decoded.header.request_id, &decoded.payload)
            }
            frame_type => Err(TransportError::UnexpectedFrameType(frame_type)),
        }
    }

    pub fn handle_empty_query_udp_datagram(
        &self,
        request_id: u64,
        max_payload: usize,
    ) -> Result<Option<Vec<u8>>, TransportError> {
        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => {
                let runtime = RuntimeSnapshot::from_snapshot(entry.snapshot.as_ref());
                let payload = encode_runtime_payload(&runtime)
                    .map_err(|err| TransportError::Payload(format!("{err:?}")))?;
                let frame_len = common::FRAME_HEADER_LEN + payload.len();
                if frame_len > max_payload {
                    return Ok(None);
                }
                common::udp::encode_udp_single_chunk_from_parts(
                    FrameType::RuntimePayload,
                    request_id,
                    &payload,
                )
                .map(Some)
                .map_err(|err| TransportError::Udp(format!("{err:?}")))
            }
            Err(code) => {
                let response = QueryResponse::error(request_id, code);
                let response_payload = serde_json::to_vec(&response)?;
                let frame_len = common::FRAME_HEADER_LEN + response_payload.len();
                if frame_len > max_payload {
                    return Ok(None);
                }
                common::udp::encode_udp_single_chunk_from_parts(
                    FrameType::QueryResponse,
                    request_id,
                    &response_payload,
                )
                .map(Some)
                .map_err(|err| TransportError::Udp(format!("{err:?}")))
            }
        }
    }

    pub fn handle_udp_metadata_datagram(
        &self,
        request_id: u64,
        payload: &[u8],
        max_payload: usize,
    ) -> Result<Option<Vec<u8>>, TransportError> {
        let request: HandshakeRequest = serde_json::from_slice(payload)?;
        request.validate().map_err(TransportError::Protocol)?;

        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => {
                let metadata = HostMetadata::from_snapshot(entry.snapshot.as_ref());
                let payload = encode_metadata_payload(&metadata)
                    .map_err(|err| TransportError::Payload(format!("{err:?}")))?;
                let frame_len = common::FRAME_HEADER_LEN + payload.len();
                if frame_len > max_payload {
                    return Ok(None);
                }
                common::udp::encode_udp_single_chunk_from_parts(
                    FrameType::MetadataPayload,
                    request_id,
                    &payload,
                )
                .map(Some)
                .map_err(|err| TransportError::Udp(format!("{err:?}")))
            }
            Err(code) => {
                let response = QueryResponse::error(request_id, code);
                let response_payload = serde_json::to_vec(&response)?;
                let frame_len = common::FRAME_HEADER_LEN + response_payload.len();
                if frame_len > max_payload {
                    return Ok(None);
                }
                common::udp::encode_udp_single_chunk_from_parts(
                    FrameType::QueryResponse,
                    request_id,
                    &response_payload,
                )
                .map(Some)
                .map_err(|err| TransportError::Udp(format!("{err:?}")))
            }
        }
    }

    fn handle_handshake(&self, request_id: u64, payload: &[u8]) -> Result<Vec<u8>, TransportError> {
        let request: HandshakeRequest = serde_json::from_slice(payload)?;
        request.validate().map_err(TransportError::Protocol)?;

        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => {
                let metadata = HostMetadata::from_snapshot(entry.snapshot.as_ref());
                let response_payload = encode_metadata_payload(&metadata)
                    .map_err(|err| TransportError::Payload(format!("{err:?}")))?;
                Ok(encode_transport_frame(
                    FrameType::MetadataPayload,
                    request_id,
                    &response_payload,
                ))
            }
            Err(code) => {
                let response = QueryResponse::error(request_id, code);
                let response_payload = serde_json::to_vec(&response)?;
                Ok(encode_transport_frame(
                    FrameType::QueryResponse,
                    request_id,
                    &response_payload,
                ))
            }
        }
    }

    fn handle_query(&self, request_id: u64, payload: &[u8]) -> Result<Vec<u8>, TransportError> {
        let request_id = if payload.is_empty() {
            request_id
        } else {
            let request: QueryRequest = serde_json::from_slice(payload)?;
            common::validate_protocol_version(request.version).map_err(TransportError::Protocol)?;
            request.request_id
        };

        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => {
                let runtime = RuntimeSnapshot::from_snapshot(entry.snapshot.as_ref());
                let payload = encode_runtime_payload(&runtime)
                    .map_err(|err| TransportError::Payload(format!("{err:?}")))?;
                Ok(encode_transport_frame(
                    FrameType::RuntimePayload,
                    request_id,
                    &payload,
                ))
            }
            Err(code) => {
                let response = QueryResponse::error(request_id, code);
                let response_payload = serde_json::to_vec(&response)?;
                Ok(encode_transport_frame(
                    FrameType::QueryResponse,
                    request_id,
                    &response_payload,
                ))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum TransportError {
    FrameDecode(FrameDecodeError),
    UnexpectedFrameType(FrameType),
    Protocol(ErrorCode),
    Json(serde_json::Error),
    Payload(String),
    Udp(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameDecode(err) => write!(formatter, "frame decode failed: {err:?}"),
            Self::UnexpectedFrameType(frame_type) => {
                write!(formatter, "unexpected frame type: {frame_type:?}")
            }
            Self::Protocol(code) => write!(formatter, "protocol error: {code:?}"),
            Self::Json(err) => write!(formatter, "json error: {err}"),
            Self::Payload(err) => write!(formatter, "payload error: {err}"),
            Self::Udp(err) => write!(formatter, "udp encode error: {err}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<FrameDecodeError> for TransportError {
    fn from(value: FrameDecodeError) -> Self {
        Self::FrameDecode(value)
    }
}

impl From<serde_json::Error> for TransportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub fn encode_transport_frame(frame_type: FrameType, request_id: u64, payload: &[u8]) -> Vec<u8> {
    common::encode_frame(
        FrameHeader::new(frame_type, request_id, payload.len() as u32),
        payload,
    )
}

pub fn decode_transport_frame(frame: &[u8]) -> Result<DecodedFrame, TransportError> {
    let (header, payload) = common::decode_frame(frame)?;
    Ok(DecodedFrame {
        header,
        payload: payload.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        GresInfo, GresMemory, GresProcessInfo, GresUtilization, ResponseStatus, ServerGresSnapshot,
        PROTOCOL_VERSION,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::collector::TestGresCollector;

    struct StaticCollector {
        calls: AtomicUsize,
        fail: Option<ErrorCode>,
    }

    impl StaticCollector {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: None,
            }
        }

        fn failing(code: ErrorCode) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: Some(code),
            }
        }
    }

    impl GresCollector for StaticCollector {
        fn collect_gres(&self) -> Result<crate::collector::GresNodeSnapshot, ErrorCode> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = self.fail {
                return Err(code);
            }

            Ok(crate::collector::GresNodeSnapshot::from_gres_snapshot(
                ServerGresSnapshot {
                    hostname: "node-a".to_string(),
                    driver_version: None,
                    gres: vec![GresInfo {
                        index: 0,
                        name: "test-gres".to_string(),
                        temperature_c: None,
                        uuid: Some("GRES-0".to_string()),
                        memory: GresMemory {
                            used_mb: 1024,
                            total_mb: 81920,
                        },
                        utilization: GresUtilization {
                            gres_percent: 75,
                            memory_percent: 10,
                        },
                        processes: vec![GresProcessInfo {
                            pid: 42,
                            uid: 1000,
                            command: Some("python train.py".to_string()),
                            used_memory_mb: 512,
                        }],
                    }],
                },
            ))
        }
    }

    fn context(collector: Arc<dyn GresCollector>) -> TransportContext {
        TransportContext::new("node-a", collector, Arc::new(GresCache::new()), 1_000)
    }

    fn request_frame(frame_type: FrameType, request_id: u64, payload: &[u8]) -> Vec<u8> {
        encode_transport_frame(frame_type, request_id, payload)
    }

    #[test]
    fn handshake_frame_returns_hostname_gres_num_and_payload_len() {
        let ctx = context(Arc::new(StaticCollector::new()));
        let request = request_frame(
            FrameType::HandshakeRequest,
            7,
            &serde_json::to_vec(&HandshakeRequest {
                version: PROTOCOL_VERSION,
            })
            .expect("serialize handshake request"),
        );

        let response = ctx.handle_frame(&request).expect("handle handshake");
        let decoded = decode_transport_frame(&response).expect("decode response");
        assert_eq!(decoded.header.frame_type, FrameType::MetadataPayload);
        assert_eq!(decoded.header.request_id, 7);

        let metadata = common::decode_metadata_payload(&decoded.payload).expect("metadata payload");
        assert_eq!(metadata.hostname, "node-a");
        assert_eq!(metadata.gres.len(), 1);
    }

    #[test]
    fn query_frame_returns_decodable_snapshot_payload() {
        let ctx = context(Arc::new(StaticCollector::new()));
        let request = request_frame(
            FrameType::QueryRequest,
            11,
            &serde_json::to_vec(&QueryRequest {
                version: PROTOCOL_VERSION,
                request_id: 99,
            })
            .expect("serialize query request"),
        );

        let response = ctx.handle_frame(&request).expect("handle query");
        let decoded = decode_transport_frame(&response).expect("decode response");
        assert_eq!(decoded.header.frame_type, FrameType::RuntimePayload);
        assert_eq!(decoded.header.request_id, 99);

        let runtime = common::decode_runtime_payload(&decoded.payload).expect("runtime payload");
        assert_eq!(runtime.gres.len(), 1);
        assert_eq!(runtime.gres[0].utilization.gres_percent, 75);
    }

    #[test]
    fn query_frame_with_test_collector_returns_runtime_payload() {
        let ctx = context(Arc::new(TestGresCollector::new("node-a")));
        let request = request_frame(
            FrameType::QueryRequest,
            21,
            &serde_json::to_vec(&QueryRequest {
                version: PROTOCOL_VERSION,
                request_id: 101,
            })
            .expect("serialize query request"),
        );

        let response = ctx.handle_frame(&request).expect("handle query");
        let decoded = decode_transport_frame(&response).expect("decode response");
        assert_eq!(decoded.header.frame_type, FrameType::RuntimePayload);

        let runtime = common::decode_runtime_payload(&decoded.payload).expect("runtime payload");
        assert_eq!(runtime.gres.len(), 1);
        assert_eq!(runtime.gres[0].utilization.gres_percent, 87);
        assert_eq!(runtime.gres[0].memory_used_mb, 1234);
    }

    #[test]
    fn degraded_collector_returns_explainable_error_frame() {
        let ctx = context(Arc::new(StaticCollector::failing(
            ErrorCode::NvmlUnavailable,
        )));
        let request = request_frame(
            FrameType::QueryRequest,
            12,
            &serde_json::to_vec(&QueryRequest {
                version: PROTOCOL_VERSION,
                request_id: 100,
            })
            .expect("serialize query request"),
        );

        let response = ctx.handle_frame(&request).expect("handle query error");
        let decoded = decode_transport_frame(&response).expect("decode response");
        assert_eq!(decoded.header.frame_type, FrameType::QueryResponse);
        assert_eq!(decoded.header.request_id, 100);

        let response: QueryResponse =
            serde_json::from_slice(&decoded.payload).expect("query response");
        assert_eq!(response.status, ResponseStatus::Error);
        assert_eq!(response.error, Some(ErrorCode::NvmlUnavailable));
    }
}
