use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use common::{
    udp::{
        decode_udp_single_chunk, decode_udp_single_chunk_ref, effective_udp_payload_capacity,
        encode_udp_chunks, encode_udp_single_chunk, UdpFrameReassembler,
    },
    FrameType, HostMetadata, ServerGresSnapshot,
};

use crate::{connection::ServerConnection, transport};

#[derive(Debug)]
pub enum UdpClientError {
    Io(String),
    Server(String),
    Decode(String),
    Transport(transport::TransportError),
}

impl std::fmt::Display for UdpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "I/O error: {message}"),
            Self::Server(message) => write!(f, "server error: {message}"),
            Self::Decode(message) => write!(f, "decode error: {message}"),
            Self::Transport(err) => write!(f, "transport error: {err:?}"),
        }
    }
}

impl From<transport::TransportError> for UdpClientError {
    fn from(value: transport::TransportError) -> Self {
        Self::Transport(value)
    }
}

pub struct ConnectedUdpNode {
    addr: SocketAddr,
    io: Arc<Mutex<UdpIo>>,
    metadata: Arc<Mutex<HostMetadata>>,
    max_payload: usize,
    next_request_id: AtomicU64,
}

struct UdpIo {
    socket: UdpSocket,
    recv_buf: Vec<u8>,
}

impl ServerConnection for ConnectedUdpNode {
    fn protocol(&self) -> &'static str {
        "udp"
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
        let request_id = self.next_id();
        let frame =
            transport::build_disconnect_frame(request_id, reason).map_err(|e| format!("{e:?}"))?;
        let chunks = encode_udp_chunks(&frame, self.max_payload).map_err(|e| format!("{e:?}"))?;
        let io = self
            .io
            .lock()
            .map_err(|_| "udp connection lock poisoned".to_string())?;
        for chunk in chunks {
            io.socket
                .send(&chunk)
                .map_err(|e| format!("send disconnect failed: {e}"))?;
        }
        Ok(())
    }

    fn close(&self) {}
}

impl ConnectedUdpNode {
    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }
}

pub fn connect_node(
    addr: SocketAddr,
    connection_idle_timeout: Duration,
    configured_mtu: u16,
) -> Result<ConnectedUdpNode, UdpClientError> {
    let bind_addr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr)
        .map_err(|e| UdpClientError::Io(format!("bind local UDP socket failed: {e}")))?;
    socket
        .connect(addr)
        .map_err(|e| UdpClientError::Io(format!("connect {addr} failed: {e}")))?;
    let max_payload = effective_udp_payload_capacity(addr, configured_mtu);
    set_timeout(&socket, connection_idle_timeout);

    let request_id = 1;
    let handshake_frame = transport::build_handshake_request_frame(request_id)?;
    let mut io = UdpIo {
        socket,
        recv_buf: vec![0u8; 65_535],
    };
    send_frame(&io.socket, &handshake_frame, max_payload)?;
    let handshake_resp = recv_frame(&mut io, request_id, connection_idle_timeout)?;
    let metadata = parse_handshake_reply(&handshake_resp)?;

    Ok(ConnectedUdpNode {
        addr,
        io: Arc::new(Mutex::new(io)),
        metadata: Arc::new(Mutex::new(metadata)),
        max_payload,
        next_request_id: AtomicU64::new(2),
    })
}

pub fn query_connected(
    node: &ConnectedUdpNode,
    connection_idle_timeout: Duration,
) -> Result<ServerGresSnapshot, UdpClientError> {
    let request_id = node.next_id();
    let mut io = node
        .io
        .lock()
        .map_err(|_| UdpClientError::Io("udp connection lock poisoned".to_string()))?;
    send_empty_query(&io.socket, request_id)?;
    let metadata = node
        .metadata
        .lock()
        .map_err(|_| UdpClientError::Io("udp metadata lock poisoned".to_string()))?
        .clone();
    recv_query_reply(&mut io, request_id, connection_idle_timeout, &metadata)
}

fn send_empty_query(socket: &UdpSocket, request_id: u64) -> Result<(), UdpClientError> {
    let datagram =
        common::udp::encode_udp_single_chunk_from_parts(FrameType::QueryRequest, request_id, &[])
            .map_err(|e| UdpClientError::Decode(format!("encode UDP query failed: {e:?}")))?;
    socket
        .send(&datagram)
        .map_err(|e| UdpClientError::Io(format!("send UDP query failed: {e}")))?;
    Ok(())
}

fn send_frame(socket: &UdpSocket, frame: &[u8], max_payload: usize) -> Result<(), UdpClientError> {
    if frame.len() <= max_payload {
        let datagram = encode_udp_single_chunk(frame)
            .map_err(|e| UdpClientError::Decode(format!("encode UDP chunk failed: {e:?}")))?;
        socket
            .send(&datagram)
            .map_err(|e| UdpClientError::Io(format!("send UDP chunk failed: {e}")))?;
        return Ok(());
    }
    let chunks = encode_udp_chunks(frame, max_payload)
        .map_err(|e| UdpClientError::Decode(format!("encode UDP chunks failed: {e:?}")))?;
    for chunk in chunks {
        socket
            .send(&chunk)
            .map_err(|e| UdpClientError::Io(format!("send UDP chunk failed: {e}")))?;
    }
    Ok(())
}

fn recv_frame(
    io: &mut UdpIo,
    request_id: u64,
    timeout: Duration,
) -> Result<Vec<u8>, UdpClientError> {
    let deadline = Instant::now() + timeout;
    let mut reassembler = None;
    while Instant::now() < deadline
        && reassembler
            .as_ref()
            .is_none_or(|reassembler: &UdpFrameReassembler| !reassembler.is_expired())
    {
        match io.socket.recv(&mut io.recv_buf) {
            Ok(n) => {
                if let Some(frame) = decode_udp_single_chunk(&io.recv_buf[..n])
                    .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?
                {
                    let (header, _) = common::decode_frame(&frame)
                        .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?;
                    if header.request_id == request_id {
                        return Ok(frame);
                    }
                    continue;
                }
                let reassembler =
                    reassembler.get_or_insert_with(|| UdpFrameReassembler::new(timeout));
                match reassembler.insert(&io.recv_buf[..n]) {
                    Ok(Some(frame)) => {
                        let (header, _) = common::decode_frame(&frame)
                            .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?;
                        if header.request_id == request_id {
                            return Ok(frame);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Err(UdpClientError::Decode(format!("{e:?}"))),
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(UdpClientError::Io(
                    "timeout while waiting for UDP frame".to_string(),
                ));
            }
            Err(e) => return Err(UdpClientError::Io(format!("recv UDP chunk failed: {e}"))),
        }
    }
    Err(UdpClientError::Io(
        "timeout while reassembling UDP frame".to_string(),
    ))
}

fn recv_query_reply(
    io: &mut UdpIo,
    request_id: u64,
    timeout: Duration,
    metadata: &HostMetadata,
) -> Result<ServerGresSnapshot, UdpClientError> {
    let deadline = Instant::now() + timeout;
    let mut reassembler = None;
    while Instant::now() < deadline
        && reassembler
            .as_ref()
            .is_none_or(|reassembler: &UdpFrameReassembler| !reassembler.is_expired())
    {
        match io.socket.recv(&mut io.recv_buf) {
            Ok(n) => {
                if let Some(frame) = decode_udp_single_chunk_ref(&io.recv_buf[..n])
                    .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?
                {
                    let (header, _) = common::decode_frame(frame)
                        .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?;
                    if header.request_id == request_id {
                        return parse_query_reply(frame, metadata);
                    }
                    continue;
                }
                let reassembler =
                    reassembler.get_or_insert_with(|| UdpFrameReassembler::new(timeout));
                match reassembler.insert(&io.recv_buf[..n]) {
                    Ok(Some(frame)) => {
                        let (header, _) = common::decode_frame(&frame)
                            .map_err(|e| UdpClientError::Decode(format!("{e:?}")))?;
                        if header.request_id == request_id {
                            return parse_query_reply(&frame, metadata);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return Err(UdpClientError::Decode(format!("{e:?}"))),
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(UdpClientError::Io(
                    "timeout while waiting for UDP frame".to_string(),
                ));
            }
            Err(e) => return Err(UdpClientError::Io(format!("recv UDP chunk failed: {e}"))),
        }
    }
    Err(UdpClientError::Io(
        "timeout while reassembling UDP frame".to_string(),
    ))
}

fn set_timeout(socket: &UdpSocket, timeout: Duration) {
    let _ = socket.set_read_timeout(Some(timeout));
    let _ = socket.set_write_timeout(Some(timeout));
}

fn parse_handshake_reply(frame: &[u8]) -> Result<HostMetadata, UdpClientError> {
    match transport::parse_metadata_payload_frame(frame) {
        Ok(metadata) => Ok(metadata),
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::MetadataPayload,
            got: FrameType::QueryResponse,
        }) => match transport::parse_query_response_error_frame(frame)? {
            Some(code) => Err(UdpClientError::Server(code.to_string())),
            None => Err(UdpClientError::Server(
                "server returned empty QueryResponse instead of MetadataPayload".to_string(),
            )),
        },
        Err(err) => Err(UdpClientError::Transport(err)),
    }
}

fn parse_query_reply(
    frame: &[u8],
    metadata: &HostMetadata,
) -> Result<ServerGresSnapshot, UdpClientError> {
    match transport::parse_runtime_payload_frame(frame) {
        Ok(runtime) => {
            return Ok(runtime.to_snapshot(metadata));
        }
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::RuntimePayload,
            got: FrameType::QueryResponse,
        }) => match transport::parse_query_response_error_frame(frame)? {
            Some(code) => return Err(UdpClientError::Server(code.to_string())),
            None => {
                return Err(UdpClientError::Server(
                    "server returned empty QueryResponse instead of RuntimePayload".to_string(),
                ))
            }
        },
        Err(transport::TransportError::UnexpectedFrameType {
            expected: FrameType::RuntimePayload,
            got: FrameType::Disconnect,
        }) => {
            let reason = transport::parse_disconnect_reason_frame(frame).unwrap_or_default();
            if reason.is_empty() {
                return Err(UdpClientError::Server("peer disconnected".to_string()));
            }
            return Err(UdpClientError::Server(format!(
                "peer disconnected: {}",
                reason
            )));
        }
        Err(err) => return Err(UdpClientError::Transport(err)),
    }
}
