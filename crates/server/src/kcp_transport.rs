#![cfg(feature = "kcp-transport")]

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use chrono::Local;
use kcp2::{KcpConfig, KcpConnection, KcpListener};
use serde_json::{json, Value};
use tokio::runtime::Builder;
use tokio::time::{timeout, Duration};

use crate::transport::{decode_transport_frame, encode_transport_frame, TransportContext};

const KCP_RECV_BUF_SIZE: usize = 1024 * 1024;
const KCP_NODELAY_INTERVAL_MS: u32 = 1;
static ACTIVE_CONNECTIONS: OnceLock<Mutex<HashMap<u32, (Arc<KcpConnection>, SocketAddr)>>> =
    OnceLock::new();

fn active_connections() -> &'static Mutex<HashMap<u32, (Arc<KcpConnection>, SocketAddr)>> {
    ACTIVE_CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn disconnect_all_active_blocking(reason: &str) {
    let connections = active_connections()
        .lock()
        .map(|active| active.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    if connections.is_empty() {
        return;
    }

    let frame = encode_transport_frame(common::FrameType::Disconnect, 0, reason.as_bytes());
    let runtime = match Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            log_json_stderr(
                json!({"level":"WARN","event":"kcp_disconnect_error","message":format!("runtime init failed: {err}")}),
            );
            return;
        }
    };

    runtime.block_on(async {
        for (connection, peer) in connections {
            let conv = connection.conv();
            let _ = connection.send(&frame).await;
            connection.close();
            log_json_stderr(
                json!({"level":"INFO","event":"kcp_disconnect_send","peer":peer.to_string(),"conv":conv,"reason":reason}),
            );
        }
    });
}

fn log_json_stderr(mut value: Value) {
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "time".to_string(),
            Value::String(Local::now().format("%Y-%m-%d %H:%M:%S").to_string()),
        );
    }
    eprintln!("{}", value);
}

pub fn spawn_kcp_server(
    bind_addr: SocketAddr,
    context: Arc<TransportContext>,
    idle_timeout: Duration,
    max_connections: usize,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("gpustat4cluster-kcp".to_string())
        .spawn(move || {
            let runtime = match Builder::new_multi_thread()
                .enable_io()
                .enable_time()
                .worker_threads(max_connections.max(1))
                .thread_name("gpustat4cluster-server-kcp")
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log_json_stderr(json!({"level":"ERROR","event":"kcp_error","message":format!("kcp runtime init failed: {err}")}));
                    return;
                }
            };

            if let Err(err) =
                runtime.block_on(run_kcp_server(bind_addr, context, idle_timeout, max_connections))
            {
                log_json_stderr(json!({"level":"WARN","event":"kcp_error","message":format!("kcp server stopped: {err}")}));
            }
        })
}

pub async fn run_kcp_server(
    bind_addr: SocketAddr,
    context: Arc<TransportContext>,
    idle_timeout: Duration,
    max_connections: usize,
) -> io::Result<()> {
    let listener = Arc::new(
        KcpListener::bind_with_config(&bind_addr.to_string(), kcp_config(idle_timeout)).await?,
    );
    log_json_stderr(
        json!({"level":"INFO","event":"kcp_listen","addr":listener.local_addr()?.to_string()}),
    );

    loop {
        let (connection, peer) = listener.accept().await?;
        let conv = connection.conv();
        let accepted = active_connections()
            .lock()
            .map(|mut active| {
                if active.len() >= max_connections {
                    false
                } else {
                    active.insert(conv, (Arc::clone(&connection), peer));
                    true
                }
            })
            .unwrap_or(false);
        if !accepted {
            let frame = encode_transport_frame(
                common::FrameType::Disconnect,
                0,
                b"server max connections reached",
            );
            let _ = connection.send(&frame).await;
            log_json_stderr(
                json!({"level":"WARN","event":"kcp_session_reject","peer":peer.to_string(),"conv":conv,"max_connections":max_connections}),
            );
            connection.close();
            listener.remove_connection(conv);
            continue;
        }
        log_json_stderr(
            json!({"level":"INFO","event":"kcp_session_accept","peer":peer.to_string(),"conv":conv}),
        );
        let context = Arc::clone(&context);
        let listener = Arc::clone(&listener);
        tokio::spawn(async move {
            let result = run_session(connection, context, idle_timeout, peer).await;
            if let Ok(mut active) = active_connections().lock() {
                active.remove(&conv);
            }
            listener.remove_connection(conv);
            match result {
                Ok(()) => log_json_stderr(
                    json!({"level":"INFO","event":"kcp_session_close","peer":peer.to_string(),"conv":conv}),
                ),
                Err(err) => log_json_stderr(
                    json!({"level":"WARN","event":"kcp_session_error","peer":peer.to_string(),"conv":conv,"message":err.to_string()}),
                ),
            }
        });
    }
}

async fn run_session(
    connection: Arc<KcpConnection>,
    context: Arc<TransportContext>,
    idle_timeout: Duration,
    peer: SocketAddr,
) -> io::Result<()> {
    let result = run_session_loop(Arc::clone(&connection), context, idle_timeout, peer).await;
    connection.close();
    result
}

async fn run_session_loop(
    connection: Arc<KcpConnection>,
    context: Arc<TransportContext>,
    idle_timeout: Duration,
    peer: SocketAddr,
) -> io::Result<()> {
    let mut buf = vec![0u8; KCP_RECV_BUF_SIZE];

    loop {
        let read = match timeout(idle_timeout, connection.recv(&mut buf)).await {
            Ok(result) => result.map_err(|err| io::Error::other(err.to_string()))?,
            Err(_) => {
                let frame = encode_transport_frame(
                    common::FrameType::Disconnect,
                    0,
                    b"server idle timeout",
                );
                let _ = connection.send(&frame).await;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "kcp session idle timeout",
                ));
            }
        };

        if read == 0 {
            return Ok(());
        }

        let decoded = decode_transport_frame(&buf[..read])
            .map_err(|err| io::Error::other(format!("transport frame failed: {err}")))?;
        if decoded.header.frame_type == common::FrameType::Heartbeat {
            log_json_stderr(
                json!({"level":"INFO","event":"kcp_heartbeat","peer":peer.to_string()}),
            );
            continue;
        }
        if decoded.header.frame_type == common::FrameType::Disconnect {
            let reason = String::from_utf8_lossy(&decoded.payload).trim().to_string();
            log_json_stderr(
                json!({"level":"INFO","event":"kcp_disconnect_received","peer":peer.to_string(),"reason":reason}),
            );
            return Ok(());
        }

        let response = context
            .handle_decoded_frame(decoded)
            .map_err(|err| io::Error::other(format!("transport frame failed: {err}")))?;
        connection
            .send(&response)
            .await
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
}

fn kcp_config(idle_timeout: Duration) -> KcpConfig {
    KcpConfig::default()
        .nodelay(true, KCP_NODELAY_INTERVAL_MS, 2, true)
        .timeout(idle_timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        decode_snapshot_payload, FrameType, HandshakeInfo, HandshakeRequest, QueryRequest,
        PROTOCOL_VERSION,
    };
    use kcp2::KcpConnector;
    use tokio::net::UdpSocket;

    use crate::cache::GpuCache;
    use crate::collector::MockNvmlCollector;
    use crate::transport::{decode_transport_frame, encode_transport_frame};

    #[test]
    fn kcp_session_malformed_frame_is_rejected_without_panic() {
        let context = TransportContext::new(
            "loopback-node",
            Arc::new(MockNvmlCollector::new("loopback-node")),
            Arc::new(GpuCache::new()),
            1_000,
        );

        assert!(context.handle_frame(b"not-a-complete-frame").is_err());
    }

    #[tokio::test]
    #[ignore = "loopback KCP smoke test; run with --features kcp-transport -- --ignored"]
    async fn kcp_loopback_handshake_and_query() {
        let bind_socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind probe");
        let bind_addr = bind_socket.local_addr().expect("local addr");
        drop(bind_socket);

        let context = Arc::new(TransportContext::new(
            "loopback-node",
            Arc::new(MockNvmlCollector::new("loopback-node")),
            Arc::new(GpuCache::new()),
            1_000,
        ));
        let server_context = Arc::clone(&context);
        tokio::spawn(async move {
            let _ = run_kcp_server(bind_addr, server_context, Duration::from_secs(10), 64).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let session = KcpConnector::new(&bind_addr.to_string())
            .expect("connector")
            .conv(42)
            .connect()
            .await
            .expect("connect");
        let connection = session.connection().clone();

        let handshake_payload = serde_json::to_vec(&HandshakeRequest {
            version: PROTOCOL_VERSION,
        })
        .expect("handshake json");
        let handshake = encode_transport_frame(FrameType::HandshakeRequest, 1, &handshake_payload);
        connection.send(&handshake).await.expect("send handshake");

        let mut recv = vec![0u8; KCP_RECV_BUF_SIZE];
        let n = timeout(Duration::from_secs(3), connection.recv(&mut recv))
            .await
            .expect("handshake timeout")
            .expect("recv handshake");
        let decoded = decode_transport_frame(&recv[..n]).expect("decode handshake frame");
        assert_eq!(decoded.header.frame_type, FrameType::HandshakeInfo);
        let info: HandshakeInfo = serde_json::from_slice(&decoded.payload).expect("handshake info");
        assert_eq!(info.hostname, "loopback-node");
        assert_eq!(info.gpu_num, 1);

        let query_payload = serde_json::to_vec(&QueryRequest {
            version: PROTOCOL_VERSION,
            request_id: 2,
        })
        .expect("query json");
        let query = encode_transport_frame(FrameType::QueryRequest, 2, &query_payload);
        connection.send(&query).await.expect("send query");

        let n = timeout(Duration::from_secs(3), connection.recv(&mut recv))
            .await
            .expect("query timeout")
            .expect("recv query");
        let decoded = decode_transport_frame(&recv[..n]).expect("decode data frame");
        assert_eq!(decoded.header.frame_type, FrameType::DataPayload);
        let snapshot = decode_snapshot_payload(&decoded.payload).expect("snapshot payload");
        assert_eq!(snapshot.hostname, "loopback-node");
        assert_eq!(snapshot.gpus[0].utilization.gpu_percent, 87);
        assert_eq!(snapshot.gpus[0].memory.used_mb, 1234);
        assert_eq!(snapshot.gpus[0].memory.total_mb, 16384);
    }
}
