#![cfg(feature = "kcp-transport")]

use std::{
    fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use common::{FrameType, HandshakeInfo, ServerGpuSnapshot};
use kcp2::{KcpConfig, KcpConnection, KcpConnector, KcpSession};
use tokio::{sync::Mutex as TokioMutex, time};

use crate::{cache::KcpConnectionCacheEntry, logger, transport};

const KCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const KCP_CONTROL_POLL_TIMEOUT: Duration = Duration::from_millis(20);
const KCP_RECV_BUFFER_SIZE: usize = 128 * 1024;
const KCP_NODELAY_INTERVAL_MS: u32 = 1;
static NEXT_CONV: AtomicU32 = AtomicU32::new(0);

#[derive(Debug)]
pub enum KcpClientError {
    Io(String),
    Timeout(&'static str),
    Transport(transport::TransportError),
    ServerError(String),
}

#[derive(Clone)]
pub struct ConnectedKcpNode {
    pub addr: SocketAddr,
    pub info: HandshakeInfo,
    #[allow(dead_code)]
    session: Arc<KcpSession>,
    conn: Arc<KcpConnection>,
    io: Arc<TokioMutex<()>>,
}

impl ConnectedKcpNode {
    pub fn connection_count(&self) -> usize {
        1
    }
}

impl fmt::Display for KcpClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "I/O error: {message}"),
            Self::Timeout(operation) => write!(f, "timeout while waiting for {operation}"),
            Self::Transport(err) => write!(f, "transport error: {err:?}"),
            Self::ServerError(message) => write!(f, "server error: {message}"),
        }
    }
}

impl From<transport::TransportError> for KcpClientError {
    fn from(value: transport::TransportError) -> Self {
        Self::Transport(value)
    }
}

pub async fn connect_node(addr: SocketAddr) -> Result<ConnectedKcpNode, KcpClientError> {
    connect_node_with_timeout(addr, KCP_REQUEST_TIMEOUT).await
}

pub async fn connect_node_with_timeout(
    addr: SocketAddr,
    connection_idle_timeout: Duration,
) -> Result<ConnectedKcpNode, KcpClientError> {
    let remote = addr.to_string();
    let config = KcpConfig::default()
        .nodelay(true, KCP_NODELAY_INTERVAL_MS, 2, true)
        .timeout(connection_idle_timeout);
    let conv = next_conv(addr);
    let connector = KcpConnector::new(&remote)
        .map_err(|e| KcpClientError::Io(format!("create connector failed: {}", e)))?
        .with_config(config)
        .conv(conv);
    let session = connector
        .connect()
        .await
        .map_err(|e| KcpClientError::Io(format!("connect {} failed: {}", remote, e)))?;
    let session = Arc::new(session);
    let conn = Arc::clone(session.connection());

    let handshake_frame = transport::build_handshake_request_frame(1)?;
    conn.send(&handshake_frame)
        .await
        .map_err(|e| KcpClientError::Io(format!("send handshake failed: {}", e)))?;
    let handshake_resp = match recv_frame(&conn).await {
        Ok(frame) => frame,
        Err(err) => {
            conn.close();
            return Err(err);
        }
    };
    let info = match transport::parse_handshake_info_frame(&handshake_resp) {
        Ok(info) => info,
        Err(err) => {
            conn.close();
            return Err(err.into());
        }
    };

    Ok(ConnectedKcpNode {
        addr,
        info,
        session,
        conn,
        io: Arc::new(TokioMutex::new(())),
    })
}

pub async fn heartbeat_connected(node: &ConnectedKcpNode) -> Result<(), KcpClientError> {
    let frame = transport::build_heartbeat_frame(0)?;
    let Ok(_guard) = node.io.try_lock() else {
        return Ok(());
    };
    node.conn
        .send(&frame)
        .await
        .map_err(|e| KcpClientError::Io(format!("send heartbeat failed: {}", e)))?;
    poll_control_frame(&node.conn).await?;
    Ok(())
}

pub async fn disconnect_connected(
    node: &ConnectedKcpNode,
    reason: &str,
) -> Result<(), KcpClientError> {
    let frame = transport::build_disconnect_frame(0, reason)?;
    let _guard = node.io.lock().await;
    let result = node
        .conn
        .send(&frame)
        .await
        .map(|_| ())
        .map_err(|e| KcpClientError::Io(format!("send disconnect failed: {}", e)));
    node.conn.close();
    result
}

pub fn close_connected(node: &ConnectedKcpNode) {
    node.conn.close();
}

pub async fn query_connected(node: &ConnectedKcpNode) -> Result<ServerGpuSnapshot, KcpClientError> {
    let mut kcp_cache = KcpConnectionCacheEntry::from_handshake("conn-001", node.addr, &node.info);

    let _guard = node.io.lock().await;
    let query_frame = transport::build_query_request_frame(2)?;
    node.conn
        .send(&query_frame)
        .await
        .map_err(|e| KcpClientError::Io(format!("send query failed: {}", e)))?;
    let query_resp = recv_frame(&node.conn).await?;
    let snapshot = parse_query_reply(&query_resp)?;
    if node.info.payload_len != 0
        && snapshot.gpus.len().min(u8::MAX as usize) as u8 != node.info.gpu_num
    {
        logger::warn(format!(
            "KCP gpu_num changed after handshake: {} -> {}",
            node.info.gpu_num,
            snapshot.gpus.len().min(u8::MAX as usize)
        ));
    }
    kcp_cache.update_snapshot(snapshot.clone());
    Ok(snapshot)
}

#[allow(dead_code)]
pub async fn query_node(addr: SocketAddr) -> Result<ServerGpuSnapshot, KcpClientError> {
    let node = connect_node(addr).await?;
    query_connected(&node).await
}

async fn recv_frame(conn: &std::sync::Arc<kcp2::KcpConnection>) -> Result<Vec<u8>, KcpClientError> {
    recv_frame_with_timeout(conn, KCP_REQUEST_TIMEOUT).await
}

async fn recv_frame_with_timeout(
    conn: &std::sync::Arc<kcp2::KcpConnection>,
    timeout_duration: Duration,
) -> Result<Vec<u8>, KcpClientError> {
    let mut buf = vec![0u8; KCP_RECV_BUFFER_SIZE];
    let n = time::timeout(timeout_duration, conn.recv(&mut buf))
        .await
        .map_err(|_| KcpClientError::Timeout("recv frame"))?
        .map_err(|e| KcpClientError::Io(format!("recv frame failed: {}", e)))?;
    buf.truncate(n);
    Ok(buf)
}

async fn poll_control_frame(
    conn: &std::sync::Arc<kcp2::KcpConnection>,
) -> Result<(), KcpClientError> {
    match recv_frame_with_timeout(conn, KCP_CONTROL_POLL_TIMEOUT).await {
        Ok(frame) => match common::protocol::decode_frame(&frame) {
            Ok((header, payload)) if header.frame_type == FrameType::Disconnect => {
                let reason = String::from_utf8_lossy(payload).trim().to_string();
                if reason.is_empty() {
                    Err(KcpClientError::ServerError("peer disconnected".to_string()))
                } else {
                    Err(KcpClientError::ServerError(format!(
                        "peer disconnected: {}",
                        reason
                    )))
                }
            }
            Ok(_) => Ok(()),
            Err(err) => Err(KcpClientError::Transport(transport::TransportError::Frame(
                err,
            ))),
        },
        Err(KcpClientError::Timeout(_)) => Ok(()),
        Err(err) => Err(err),
    }
}

fn next_conv(addr: SocketAddr) -> u32 {
    let seed = NEXT_CONV.load(Ordering::Relaxed);
    if seed == 0 {
        let time_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
            .unwrap_or(1);
        let addr_seed = match addr {
            SocketAddr::V4(v4) => u32::from(*v4.ip()) ^ u32::from(v4.port()),
            SocketAddr::V6(v6) => {
                let octets = v6.ip().octets();
                let mut folded = u32::from(v6.port());
                for chunk in octets.chunks_exact(4) {
                    folded ^= u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                folded
            }
        };
        let process_seed = std::process::id();
        let initial = (time_seed ^ addr_seed ^ process_seed).max(1);
        let _ = NEXT_CONV.compare_exchange(0, initial, Ordering::Relaxed, Ordering::Relaxed);
    }

    NEXT_CONV
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.wrapping_add(1).max(1))
        })
        .unwrap_or(1)
}

fn parse_query_reply(frame: &[u8]) -> Result<ServerGpuSnapshot, KcpClientError> {
    match transport::parse_data_payload_frame(frame) {
        Ok(snapshot) => Ok(snapshot),
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::DataPayload,
            got: FrameType::QueryResponse,
        }) => match transport::parse_query_response_error_frame(frame)? {
            Some(code) => Err(KcpClientError::ServerError(code.to_string())),
            None => Err(KcpClientError::ServerError(
                "server returned empty QueryResponse instead of DataPayload".to_string(),
            )),
        },
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::DataPayload,
            got: FrameType::Disconnect,
        }) => {
            let reason = transport::parse_disconnect_reason_frame(frame).unwrap_or_default();
            if reason.is_empty() {
                Err(KcpClientError::ServerError("peer disconnected".to_string()))
            } else {
                Err(KcpClientError::ServerError(format!(
                    "peer disconnected: {}",
                    reason
                )))
            }
        }
        Err(err) => Err(KcpClientError::Transport(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        protocol::encode_snapshot_payload, FrameHeader, GpuInfo, GpuMemory, GpuProcessInfo,
        GpuUtilization, HandshakeInfo, HandshakeRequest, QueryRequest, ServerGpuSnapshot,
    };
    use kcp2::KcpListener;
    use tokio::sync::oneshot;

    fn snapshot() -> ServerGpuSnapshot {
        ServerGpuSnapshot {
            hostname: "node-a".to_string(),
            driver_version: None,
            gpus: vec![GpuInfo {
                index: 0,
                name: "NVIDIA A100".to_string(),
                temperature_c: None,
                uuid: None,
                memory: GpuMemory {
                    used_mb: 1024,
                    total_mb: 81920,
                },
                utilization: GpuUtilization {
                    gpu_percent: 66,
                    memory_percent: 10,
                },
                processes: vec![GpuProcessInfo {
                    pid: 7,
                    uid: 1000,
                    command: Some("python".to_string()),
                    used_memory_mb: 512,
                }],
            }],
        }
    }

    #[tokio::test]
    async fn single_node_loopback_query_returns_snapshot() {
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(async move {
            let listener = KcpListener::bind("127.0.0.1:0").await.expect("listen");
            let addr = listener.local_addr().expect("listener local addr");
            let _ = ready_tx.send(addr);
            let mut buf = vec![0u8; KCP_RECV_BUFFER_SIZE];

            let (n, conn, _) = listener.recv_from(&mut buf).await.expect("recv handshake");
            let (header, payload) = common::decode_frame(&buf[..n]).expect("decode handshake");
            assert_eq!(header.frame_type, FrameType::HandshakeRequest);
            let req: HandshakeRequest = serde_json::from_slice(payload).expect("handshake req");
            req.validate().expect("valid handshake");

            let snapshot_payload = encode_snapshot_payload(&snapshot()).expect("snapshot payload");
            let info = HandshakeInfo::new(
                "node-a",
                1,
                common::payload_len_for_handshake(&snapshot_payload).expect("payload len"),
            );
            let info_payload = serde_json::to_vec(&info).expect("info json");
            let frame = common::encode_frame(
                FrameHeader::new(
                    FrameType::HandshakeInfo,
                    header.request_id,
                    info_payload.len() as u32,
                ),
                &info_payload,
            );
            conn.send(&frame).await.expect("send handshake info");

            let (n, conn, _) = listener.recv_from(&mut buf).await.expect("recv query");
            let (header, payload) = common::decode_frame(&buf[..n]).expect("decode query");
            assert_eq!(header.frame_type, FrameType::QueryRequest);
            let req: QueryRequest = serde_json::from_slice(payload).expect("query req");
            assert_eq!(req.request_id, 2);

            let frame = common::encode_frame(
                FrameHeader::new(
                    FrameType::DataPayload,
                    req.request_id,
                    snapshot_payload.len() as u32,
                ),
                &snapshot_payload,
            );
            conn.send(&frame).await.expect("send data payload");
        });

        let addr = ready_rx.await.expect("server ready");
        let result = query_node(addr).await.expect("query node");
        assert_eq!(result.hostname, "node-a");
        assert_eq!(result.gpus[0].utilization.gpu_percent, 66);
    }
}
