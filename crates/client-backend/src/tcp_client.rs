use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use common::{decode_snapshot_payload, ServerGpuSnapshot};
use serde::Deserialize;

#[derive(Debug)]
pub enum TcpClientError {
    Io(String),
    Server(String),
    Decode(String),
}

impl std::fmt::Display for TcpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "I/O error: {message}"),
            Self::Server(message) => write!(f, "server error: {message}"),
            Self::Decode(message) => write!(f, "decode error: {message}"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TcpQueryResponse {
    ok: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    payload_b64: Option<String>,
}

pub fn query_node(
    addr: SocketAddr,
    connection_idle_timeout: Duration,
) -> Result<ServerGpuSnapshot, TcpClientError> {
    let started = std::time::Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, connection_idle_timeout)
        .map_err(|e| TcpClientError::Io(format!("connect {addr} failed: {e}")))?;
    let remaining = connection_idle_timeout
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::from_millis(1));
    let _ = stream.set_read_timeout(Some(remaining));
    let _ = stream.set_write_timeout(Some(remaining));
    stream
        .write_all(b"QUERY\n")
        .map_err(|e| TcpClientError::Io(format!("send query failed: {e}")))?;

    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .map_err(|e| TcpClientError::Io(format!("read query response failed: {e}")))?;
    let response: TcpQueryResponse = serde_json::from_slice(&body)
        .map_err(|e| TcpClientError::Decode(format!("invalid JSON response: {e}")))?;
    if !response.ok {
        return Err(TcpClientError::Server(
            response
                .message
                .or(response.error_code)
                .unwrap_or_else(|| "query failed".to_string()),
        ));
    }
    let payload = response
        .payload_b64
        .ok_or_else(|| TcpClientError::Decode("missing payload_b64".to_string()))
        .and_then(|raw| {
            BASE64_STANDARD
                .decode(raw)
                .map_err(|e| TcpClientError::Decode(format!("invalid payload_b64: {e}")))
        })?;
    decode_snapshot_payload(&payload)
        .map_err(|e| TcpClientError::Decode(format!("snapshot payload failed: {e:?}")))
}
