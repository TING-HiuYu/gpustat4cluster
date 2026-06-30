use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{FrameHeader, FrameType, HostMetadata, ServerGresSnapshot, FRAME_HEADER_LEN};

use crate::{connection::ServerConnection, transport};

#[derive(Debug)]
pub enum TcpClientError {
    Io(String),
    Server(String),
    Decode(String),
    Transport(transport::TransportError),
}

impl std::fmt::Display for TcpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "I/O error: {message}"),
            Self::Server(message) => write!(f, "server error: {message}"),
            Self::Decode(message) => write!(f, "decode error: {message}"),
            Self::Transport(err) => write!(f, "transport error: {err:?}"),
        }
    }
}

impl From<transport::TransportError> for TcpClientError {
    fn from(value: transport::TransportError) -> Self {
        Self::Transport(value)
    }
}

#[derive(Clone)]
pub struct ConnectedTcpNode {
    addr: SocketAddr,
    io: Arc<Mutex<TcpStream>>,
    metadata: Arc<Mutex<HostMetadata>>,
}

impl ServerConnection for ConnectedTcpNode {
    fn protocol(&self) -> &'static str {
        "tcp"
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn hostname(&self) -> String {
        self.metadata
            .lock()
            .map(|metadata| metadata.hostname.clone())
            .unwrap_or_default()
    }

    fn gres_num(&self) -> u8 {
        self.metadata
            .lock()
            .map(|metadata| metadata.gres.len().min(u8::MAX as usize) as u8)
            .unwrap_or(0)
    }

    fn query(&self, timeout: Duration) -> Result<ServerGresSnapshot, String> {
        query_connected(self, timeout).map_err(|e| e.to_string())
    }

    fn disconnect(&self, reason: &str) -> Result<(), String> {
        let frame = transport::build_disconnect_frame(0, reason).map_err(|e| format!("{e:?}"))?;
        let mut stream = self
            .io
            .lock()
            .map_err(|_| "tcp connection lock poisoned".to_string())?;
        stream
            .write_all(&frame)
            .map_err(|e| format!("send disconnect failed: {e}"))?;
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
        Ok(())
    }

    fn close(&self) {
        close_connected(self);
    }
}

pub fn connect_node(
    addr: SocketAddr,
    connection_idle_timeout: Duration,
) -> Result<ConnectedTcpNode, TcpClientError> {
    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, connection_idle_timeout)
        .map_err(|e| TcpClientError::Io(format!("connect {addr} failed: {e}")))?;
    let _ = stream.set_nodelay(true);
    let remaining = connection_idle_timeout
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::from_millis(1));
    set_timeouts(&stream, remaining);

    let handshake_frame = transport::build_handshake_request_frame(1)?;
    stream
        .write_all(&handshake_frame)
        .map_err(|e| TcpClientError::Io(format!("send handshake failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| TcpClientError::Io(format!("flush handshake failed: {e}")))?;
    let handshake_resp = read_frame(&mut stream)?;
    let metadata = parse_handshake_reply(&handshake_resp)?;

    Ok(ConnectedTcpNode {
        addr,
        io: Arc::new(Mutex::new(stream)),
        metadata: Arc::new(Mutex::new(metadata)),
    })
}

pub fn query_connected(
    node: &ConnectedTcpNode,
    connection_idle_timeout: Duration,
) -> Result<ServerGresSnapshot, TcpClientError> {
    let mut stream = node
        .io
        .lock()
        .map_err(|_| TcpClientError::Io("tcp connection lock poisoned".to_string()))?;
    set_timeouts(&stream, connection_idle_timeout);

    let query_frame = transport::build_query_request_frame(2)?;
    stream
        .write_all(&query_frame)
        .map_err(|e| TcpClientError::Io(format!("send query failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| TcpClientError::Io(format!("flush query failed: {e}")))?;

    let query_resp = read_frame(&mut stream)?;
    let runtime = match transport::parse_runtime_payload_frame(&query_resp) {
        Ok(runtime) => runtime,
        Err(_) => {
            let metadata = node
                .metadata
                .lock()
                .map_err(|_| TcpClientError::Io("tcp metadata lock poisoned".to_string()))?
                .clone();
            return parse_query_reply(&query_resp, &metadata);
        }
    };

    let mut metadata = node
        .metadata
        .lock()
        .map_err(|_| TcpClientError::Io("tcp metadata lock poisoned".to_string()))?
        .clone();
    if runtime.metadata_hash != 0 && runtime.metadata_hash != metadata.metadata_hash() {
        metadata = refresh_metadata(&mut stream)?;
        *node
            .metadata
            .lock()
            .map_err(|_| TcpClientError::Io("tcp metadata lock poisoned".to_string()))? =
            metadata.clone();
    }
    Ok(runtime.to_snapshot(&metadata))
}

pub fn close_connected(node: &ConnectedTcpNode) {
    if let Ok(stream) = node.io.lock() {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn set_timeouts(stream: &TcpStream, timeout: Duration) {
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
}

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, TcpClientError> {
    let mut header_bytes = [0u8; FRAME_HEADER_LEN];
    stream
        .read_exact(&mut header_bytes)
        .map_err(|e| TcpClientError::Io(format!("read frame header failed: {e}")))?;
    let header = FrameHeader::decode(&header_bytes)
        .map_err(|e| TcpClientError::Decode(format!("invalid frame header: {e:?}")))?;
    let payload_len = header.payload_len as usize;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload_len);
    frame.extend_from_slice(&header_bytes);
    if payload_len > 0 {
        let mut payload = vec![0u8; payload_len];
        stream
            .read_exact(&mut payload)
            .map_err(|e| TcpClientError::Io(format!("read frame payload failed: {e}")))?;
        frame.extend_from_slice(&payload);
    }
    Ok(frame)
}

fn refresh_metadata(stream: &mut TcpStream) -> Result<HostMetadata, TcpClientError> {
    let frame = transport::build_handshake_request_frame(3)?;
    stream
        .write_all(&frame)
        .map_err(|e| TcpClientError::Io(format!("send metadata refresh failed: {e}")))?;
    stream
        .flush()
        .map_err(|e| TcpClientError::Io(format!("flush metadata refresh failed: {e}")))?;
    let response = read_frame(stream)?;
    parse_handshake_reply(&response)
}

fn parse_handshake_reply(frame: &[u8]) -> Result<HostMetadata, TcpClientError> {
    match transport::parse_metadata_payload_frame(frame) {
        Ok(metadata) => Ok(metadata),
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::MetadataPayload,
            got: FrameType::QueryResponse,
        }) => match transport::parse_query_response_error_frame(frame)? {
            Some(code) => Err(TcpClientError::Server(code.to_string())),
            None => Err(TcpClientError::Server(
                "server returned empty QueryResponse instead of MetadataPayload".to_string(),
            )),
        },
        Err(err) => Err(TcpClientError::Transport(err)),
    }
}

fn parse_query_reply(
    frame: &[u8],
    metadata: &HostMetadata,
) -> Result<ServerGresSnapshot, TcpClientError> {
    match transport::parse_runtime_payload_frame(frame) {
        Ok(runtime) => Ok(runtime.to_snapshot(metadata)),
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::RuntimePayload,
            got: FrameType::QueryResponse,
        }) => match transport::parse_query_response_error_frame(frame)? {
            Some(code) => Err(TcpClientError::Server(code.to_string())),
            None => Err(TcpClientError::Server(
                "server returned empty QueryResponse instead of RuntimePayload".to_string(),
            )),
        },
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::RuntimePayload,
            got: FrameType::Disconnect,
        }) => {
            let reason = transport::parse_disconnect_reason_frame(frame).unwrap_or_default();
            if reason.is_empty() {
                Err(TcpClientError::Server("peer disconnected".to_string()))
            } else {
                Err(TcpClientError::Server(format!(
                    "peer disconnected: {}",
                    reason
                )))
            }
        }
        Err(err) => Err(TcpClientError::Transport(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        encode_frame, encode_metadata_payload, encode_runtime_payload, FrameHeader, GresInfo,
        GresMemory, GresUtilization, HostMetadata, RuntimeSnapshot, ServerGresSnapshot,
    };
    use std::{net::TcpListener, thread};

    fn snapshot(hostname: &str) -> ServerGresSnapshot {
        ServerGresSnapshot {
            hostname: hostname.to_string(),
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
                    gres_percent: 66,
                    memory_percent: 10,
                },
                processes: Vec::new(),
            }],
        }
    }

    #[test]
    fn persistent_tcp_connection_can_query_twice() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _handshake = read_frame(&mut stream).expect("read handshake");
            let metadata = HostMetadata::from_snapshot(&snapshot("tcp-node"));
            let payload = encode_metadata_payload(&metadata).expect("metadata");
            let frame = encode_frame(FrameHeader::new(FrameType::MetadataPayload, 1, 0), &payload);
            stream.write_all(&frame).expect("write handshake");

            for _ in 0..2 {
                let _query = read_frame(&mut stream).expect("read query");
                let runtime = RuntimeSnapshot::from_snapshot(&snapshot("tcp-node"));
                let payload = encode_runtime_payload(&runtime).expect("runtime");
                let frame =
                    encode_frame(FrameHeader::new(FrameType::RuntimePayload, 2, 0), &payload);
                stream.write_all(&frame).expect("write payload");
                stream.flush().expect("flush");
            }
        });

        let node = connect_node(addr, Duration::from_secs(2)).expect("connect");
        let first = query_connected(&node, Duration::from_secs(2)).expect("first query");
        let second = query_connected(&node, Duration::from_secs(2)).expect("second query");

        assert_eq!(first.hostname, "tcp-node");
        assert_eq!(second.hostname, "tcp-node");
        assert_eq!(node.connection_count(), 1);
        assert_eq!(node.gres_num(), 1);
    }
}
