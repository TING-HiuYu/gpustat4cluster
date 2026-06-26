use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::Duration,
};

use common::{
    udp::{
        decode_udp_single_chunk_ref, effective_udp_payload_capacity, encode_udp_chunks,
        encode_udp_single_chunk, UdpFrameReassembler,
    },
    FrameType,
};
use serde_json::json;

use crate::{
    cache::GresCache,
    collector::GresCollector,
    log_json_stderr, log_json_stdout,
    transport::{decode_transport_frame, TransportContext},
};

pub fn server_loop(
    bind_addr: &str,
    hostname: String,
    collector: Arc<dyn GresCollector>,
    cache: Arc<GresCache>,
    ttl_ms: u64,
    idle_timeout: Duration,
    configured_mtu: u16,
) {
    let socket = match UdpSocket::bind(bind_addr) {
        Ok(socket) => socket,
        Err(e) => {
            log_json_stderr(json!({
                "level":"ERROR",
                "event":"udp_bind_failed",
                "addr":bind_addr,
                "message":e.to_string()
            }));
            return;
        }
    };

    log_json_stdout(json!({
        "level":"INFO",
        "event":"udp_listen",
        "addr":bind_addr,
        "udp_mtu":configured_mtu
    }));

    let context = Arc::new(TransportContext::new(hostname, collector, cache, ttl_ms));
    let mut reassemblers: HashMap<SocketAddr, UdpFrameReassembler> = HashMap::new();
    let mut payload_caps: HashMap<SocketAddr, usize> = HashMap::new();
    let mut buf = vec![0u8; 65_535];

    loop {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(item) => item,
            Err(e) => {
                log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_recv_failed",
                    "message":e.to_string()
                }));
                continue;
            }
        };

        let owned_frame;
        let frame = match decode_udp_single_chunk_ref(&buf[..n]) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                let reassembler = reassemblers
                    .entry(peer)
                    .or_insert_with(|| UdpFrameReassembler::new(idle_timeout));
                if reassembler.is_expired() {
                    *reassembler = UdpFrameReassembler::new(idle_timeout);
                }
                owned_frame = match reassembler.insert(&buf[..n]) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => continue,
                    Err(e) => {
                        log_json_stderr(json!({
                            "level":"WARN",
                            "event":"udp_chunk_error",
                            "peer":peer.to_string(),
                            "message":format!("{e:?}")
                        }));
                        continue;
                    }
                };
                &owned_frame
            }
            Err(e) => {
                log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_chunk_error",
                    "peer":peer.to_string(),
                    "message":format!("{e:?}")
                }));
                continue;
            }
        };

        if let Ok(decoded) = decode_transport_frame(frame) {
            if decoded.header.frame_type == FrameType::Disconnect {
                log_json_stdout(json!({
                    "level":"INFO",
                    "event":"udp_peer_disconnect",
                    "peer":peer.to_string(),
                    "reason":String::from_utf8_lossy(&decoded.payload).trim().to_string()
                }));
                reassemblers.remove(&peer);
                continue;
            }
            if decoded.header.frame_type == FrameType::HandshakeRequest {
                let max_payload = *payload_caps
                    .entry(peer)
                    .or_insert_with(|| effective_udp_payload_capacity(peer, configured_mtu));
                match context.handle_udp_metadata_datagram(
                    decoded.header.request_id,
                    &decoded.payload,
                    max_payload,
                ) {
                    Ok(Some(datagram)) => {
                        if let Err(e) = socket.send_to(&datagram, peer) {
                            log_json_stderr(json!({
                                "level":"WARN",
                                "event":"udp_send_failed",
                                "peer":peer.to_string(),
                                "message":e.to_string()
                            }));
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log_json_stderr(json!({
                            "level":"WARN",
                            "event":"udp_frame_error",
                            "peer":peer.to_string(),
                            "message":e.to_string()
                        }));
                        continue;
                    }
                }
            }
            if decoded.header.frame_type == FrameType::QueryRequest && decoded.payload.is_empty() {
                let max_payload = *payload_caps
                    .entry(peer)
                    .or_insert_with(|| effective_udp_payload_capacity(peer, configured_mtu));
                match context
                    .handle_empty_query_udp_datagram(decoded.header.request_id, max_payload)
                {
                    Ok(Some(datagram)) => {
                        if let Err(e) = socket.send_to(&datagram, peer) {
                            log_json_stderr(json!({
                                "level":"WARN",
                                "event":"udp_send_failed",
                                "peer":peer.to_string(),
                                "message":e.to_string()
                            }));
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log_json_stderr(json!({
                            "level":"WARN",
                            "event":"udp_frame_error",
                            "peer":peer.to_string(),
                            "message":e.to_string()
                        }));
                        continue;
                    }
                }
            }
        }

        let response = match context.handle_frame(frame) {
            Ok(response) => response,
            Err(e) => {
                log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_frame_error",
                    "peer":peer.to_string(),
                    "message":e.to_string()
                }));
                continue;
            }
        };

        let max_payload = *payload_caps
            .entry(peer)
            .or_insert_with(|| effective_udp_payload_capacity(peer, configured_mtu));
        if response.len() <= max_payload {
            match encode_udp_single_chunk(&response) {
                Ok(chunk) => {
                    if let Err(e) = socket.send_to(&chunk, peer) {
                        log_json_stderr(json!({
                            "level":"WARN",
                            "event":"udp_send_failed",
                            "peer":peer.to_string(),
                            "message":e.to_string()
                        }));
                    }
                }
                Err(e) => log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_encode_error",
                    "peer":peer.to_string(),
                    "message":format!("{e:?}")
                })),
            }
            continue;
        }

        let chunks = match encode_udp_chunks(&response, max_payload) {
            Ok(chunks) => chunks,
            Err(e) => {
                log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_encode_error",
                    "peer":peer.to_string(),
                    "message":format!("{e:?}")
                }));
                continue;
            }
        };
        for chunk in chunks {
            if let Err(e) = socket.send_to(&chunk, peer) {
                log_json_stderr(json!({
                    "level":"WARN",
                    "event":"udp_send_failed",
                    "peer":peer.to_string(),
                    "message":e.to_string()
                }));
                break;
            }
        }
    }
}
