use std::{fmt, sync::Arc};

#[cfg(test)]
use common::decode_snapshot_payload;
use common::{
    payload_len_for_handshake, ErrorCode, FrameDecodeError, FrameHeader, FrameType, HandshakeInfo,
    HandshakeRequest, QueryRequest, QueryResponse,
};

use crate::cache::GpuCache;
use crate::collector::GpuCollector;

pub struct TransportContext {
    hostname: String,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<GpuCache>,
    ttl_ms: u64,
}

impl TransportContext {
    pub fn new(
        hostname: impl Into<String>,
        collector: Arc<dyn GpuCollector>,
        cache: Arc<GpuCache>,
        ttl_ms: u64,
    ) -> Self {
        Self {
            hostname: hostname.into(),
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

    fn handle_handshake(&self, request_id: u64, payload: &[u8]) -> Result<Vec<u8>, TransportError> {
        let request: HandshakeRequest = serde_json::from_slice(payload)?;
        request.validate().map_err(TransportError::Protocol)?;

        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => {
                let payload_len = payload_len_for_handshake(&entry.payload)
                    .map_err(|_| TransportError::Protocol(ErrorCode::Internal))?;
                let response =
                    HandshakeInfo::new(self.hostname.clone(), entry.gpu_num(), payload_len);
                let response_payload = serde_json::to_vec(&response)?;
                Ok(encode_transport_frame(
                    FrameType::HandshakeInfo,
                    request_id,
                    &response_payload,
                ))
            }
            Err(code) => {
                let response = HandshakeInfo::new(self.hostname.clone(), 0, 0);
                let response_payload = serde_json::to_vec(&response)?;
                let _ = code;
                Ok(encode_transport_frame(
                    FrameType::HandshakeInfo,
                    request_id,
                    &response_payload,
                ))
            }
        }
    }

    fn handle_query(&self, _request_id: u64, payload: &[u8]) -> Result<Vec<u8>, TransportError> {
        let request: QueryRequest = serde_json::from_slice(payload)?;
        common::validate_protocol_version(request.version).map_err(TransportError::Protocol)?;

        match self
            .cache
            .get_latest_or_refresh(self.collector.as_ref(), self.ttl_ms)
        {
            Ok(entry) => Ok(encode_transport_frame(
                FrameType::DataPayload,
                request.request_id,
                &entry.payload,
            )),
            Err(code) => {
                let response = QueryResponse::error(request.request_id, code);
                let response_payload = serde_json::to_vec(&response)?;
                Ok(encode_transport_frame(
                    FrameType::QueryResponse,
                    request.request_id,
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
        GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization, ResponseStatus, ServerGpuSnapshot,
        PROTOCOL_VERSION,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::collector::MockNvmlCollector;

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

    impl GpuCollector for StaticCollector {
        fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(code) = self.fail {
                return Err(code);
            }

            Ok(ServerGpuSnapshot {
                hostname: "node-a".to_string(),
                driver_version: None,
                gpus: vec![GpuInfo {
                    index: 0,
                    name: "mock-gpu".to_string(),
                    temperature_c: None,
                    uuid: Some("GPU-0".to_string()),
                    memory: GpuMemory {
                        used_mb: 1024,
                        total_mb: 81920,
                    },
                    utilization: GpuUtilization {
                        gpu_percent: 75,
                        memory_percent: 10,
                    },
                    processes: vec![GpuProcessInfo {
                        pid: 42,
                        uid: 1000,
                        command: Some("python train.py".to_string()),
                        used_memory_mb: 512,
                    }],
                }],
            })
        }
    }

    fn context(collector: Arc<dyn GpuCollector>) -> TransportContext {
        TransportContext::new("node-a", collector, Arc::new(GpuCache::new()), 1_000)
    }

    fn request_frame(frame_type: FrameType, request_id: u64, payload: &[u8]) -> Vec<u8> {
        encode_transport_frame(frame_type, request_id, payload)
    }

    #[test]
    fn handshake_frame_returns_hostname_gpu_num_and_payload_len() {
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
        assert_eq!(decoded.header.frame_type, FrameType::HandshakeInfo);
        assert_eq!(decoded.header.request_id, 7);

        let info: HandshakeInfo = serde_json::from_slice(&decoded.payload).expect("handshake info");
        assert_eq!(info.hostname, "node-a");
        assert_eq!(info.gpu_num, 1);
        assert!(info.payload_len > 0);
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
        assert_eq!(decoded.header.frame_type, FrameType::DataPayload);
        assert_eq!(decoded.header.request_id, 99);

        let snapshot = decode_snapshot_payload(&decoded.payload).expect("snapshot payload");
        assert_eq!(snapshot.hostname, "node-a");
        assert_eq!(snapshot.gpus.len(), 1);
        assert_eq!(snapshot.gpus[0].utilization.gpu_percent, 75);
    }

    #[test]
    fn query_frame_with_mock_collector_returns_data_payload() {
        let ctx = context(Arc::new(MockNvmlCollector::new("node-a")));
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
        assert_eq!(decoded.header.frame_type, FrameType::DataPayload);

        let snapshot = decode_snapshot_payload(&decoded.payload).expect("snapshot payload");
        assert_eq!(snapshot.hostname, "node-a");
        assert_eq!(snapshot.gpus.len(), 1);
        assert_eq!(snapshot.gpus[0].utilization.gpu_percent, 87);
        assert_eq!(snapshot.gpus[0].memory.used_mb, 1234);
        assert_eq!(snapshot.gpus[0].memory.total_mb, 16384);
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
