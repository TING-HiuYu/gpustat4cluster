use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket},
    process::Command,
    time::{Duration, Instant},
};

use crate::{FrameDecodeError, FrameType, PROTOCOL_VERSION};

pub const UDP_CHUNK_MAGIC: [u8; 4] = *b"G4U1";
pub const UDP_CHUNK_HEADER_LEN: usize = 32;
pub const UDP_FALLBACK_MTU: usize = 1200;
pub const UDP_MIN_PAYLOAD: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpChunkHeader {
    pub version: u8,
    pub frame_type: FrameType,
    pub request_id: u64,
    pub frame_len: u32,
    pub chunk_id: u16,
    pub total_chunks: u16,
    pub payload_len: u16,
    pub prev_checksum: u32,
    pub next_checksum: u32,
}

impl UdpChunkHeader {
    pub fn encode(self) -> [u8; UDP_CHUNK_HEADER_LEN] {
        let mut out = [0u8; UDP_CHUNK_HEADER_LEN];
        out[0..4].copy_from_slice(&UDP_CHUNK_MAGIC);
        out[4] = self.version;
        out[5] = self.frame_type as u8;
        out[6..14].copy_from_slice(&self.request_id.to_be_bytes());
        out[14..18].copy_from_slice(&self.frame_len.to_be_bytes());
        out[18..20].copy_from_slice(&self.chunk_id.to_be_bytes());
        out[20..22].copy_from_slice(&self.total_chunks.to_be_bytes());
        out[22..24].copy_from_slice(&self.payload_len.to_be_bytes());
        out[24..28].copy_from_slice(&self.prev_checksum.to_be_bytes());
        out[28..32].copy_from_slice(&self.next_checksum.to_be_bytes());
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, UdpChunkError> {
        if input.len() < UDP_CHUNK_HEADER_LEN {
            return Err(UdpChunkError::ShortDatagram {
                got: input.len(),
                min: UDP_CHUNK_HEADER_LEN,
            });
        }
        if input[0..4] != UDP_CHUNK_MAGIC {
            return Err(UdpChunkError::BadMagic);
        }
        let version = input[4];
        if version != PROTOCOL_VERSION {
            return Err(UdpChunkError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: version,
            });
        }
        let frame_type =
            FrameType::try_from(input[5]).map_err(|_| UdpChunkError::UnknownFrameType(input[5]))?;
        let request_id = u64::from_be_bytes(input[6..14].try_into().expect("slice length"));
        let frame_len = u32::from_be_bytes(input[14..18].try_into().expect("slice length"));
        let chunk_id = u16::from_be_bytes(input[18..20].try_into().expect("slice length"));
        let total_chunks = u16::from_be_bytes(input[20..22].try_into().expect("slice length"));
        let payload_len = u16::from_be_bytes(input[22..24].try_into().expect("slice length"));
        let prev_checksum = u32::from_be_bytes(input[24..28].try_into().expect("slice length"));
        let next_checksum = u32::from_be_bytes(input[28..32].try_into().expect("slice length"));

        if total_chunks == 0 {
            return Err(UdpChunkError::InvalidChunkLayout);
        }
        if chunk_id >= total_chunks {
            return Err(UdpChunkError::InvalidChunkLayout);
        }
        if input.len() != UDP_CHUNK_HEADER_LEN + payload_len as usize {
            return Err(UdpChunkError::PayloadLengthMismatch {
                expected: payload_len as usize,
                actual: input.len().saturating_sub(UDP_CHUNK_HEADER_LEN),
            });
        }

        Ok(Self {
            version,
            frame_type,
            request_id,
            frame_len,
            chunk_id,
            total_chunks,
            payload_len,
            prev_checksum,
            next_checksum,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpChunkError {
    EmptyFrame,
    PayloadTooSmall { got: usize, min: usize },
    PayloadTooLarge { len: usize, max: usize },
    TooManyChunks { chunks: usize },
    FrameTooLarge { len: usize },
    ShortDatagram { got: usize, min: usize },
    BadMagic,
    VersionMismatch { expected: u8, got: u8 },
    UnknownFrameType(u8),
    InvalidChunkLayout,
    PayloadLengthMismatch { expected: usize, actual: usize },
    MetadataMismatch,
    ChecksumMismatch,
    Frame(FrameDecodeError),
}

pub fn encode_udp_chunks(frame: &[u8], max_payload: usize) -> Result<Vec<Vec<u8>>, UdpChunkError> {
    if frame.is_empty() {
        return Err(UdpChunkError::EmptyFrame);
    }
    if max_payload < UDP_MIN_PAYLOAD {
        return Err(UdpChunkError::PayloadTooSmall {
            got: max_payload,
            min: UDP_MIN_PAYLOAD,
        });
    }
    let (frame_header, _) = crate::decode_frame(frame).map_err(UdpChunkError::Frame)?;
    let frame_len = u32::try_from(frame.len())
        .map_err(|_| UdpChunkError::FrameTooLarge { len: frame.len() })?;
    let total_chunks = frame.len().div_ceil(max_payload);
    if total_chunks > u16::MAX as usize {
        return Err(UdpChunkError::TooManyChunks {
            chunks: total_chunks,
        });
    }

    let chunks: Vec<&[u8]> = frame.chunks(max_payload).collect();
    let checksums: Vec<u32> = chunks.iter().map(|chunk| checksum(chunk)).collect();
    let total = chunks.len();
    let mut out = Vec::with_capacity(total);
    for (idx, payload) in chunks.iter().enumerate() {
        let prev = if idx == 0 { total - 1 } else { idx - 1 };
        let next = if idx + 1 == total { 0 } else { idx + 1 };
        let header = UdpChunkHeader {
            version: PROTOCOL_VERSION,
            frame_type: frame_header.frame_type,
            request_id: frame_header.request_id,
            frame_len,
            chunk_id: idx as u16,
            total_chunks: total as u16,
            payload_len: payload.len() as u16,
            prev_checksum: checksums[prev],
            next_checksum: checksums[next],
        };
        let mut datagram = Vec::with_capacity(UDP_CHUNK_HEADER_LEN + payload.len());
        datagram.extend_from_slice(&header.encode());
        datagram.extend_from_slice(payload);
        out.push(datagram);
    }
    Ok(out)
}

pub fn encode_udp_single_chunk(frame: &[u8]) -> Result<Vec<u8>, UdpChunkError> {
    if frame.is_empty() {
        return Err(UdpChunkError::EmptyFrame);
    }
    if frame.len() > u16::MAX as usize {
        return Err(UdpChunkError::PayloadTooLarge {
            len: frame.len(),
            max: u16::MAX as usize,
        });
    }
    let (frame_header, _) = crate::decode_frame(frame).map_err(UdpChunkError::Frame)?;
    let frame_len = u32::try_from(frame.len())
        .map_err(|_| UdpChunkError::FrameTooLarge { len: frame.len() })?;
    let checksum = checksum(frame);
    let header = UdpChunkHeader {
        version: PROTOCOL_VERSION,
        frame_type: frame_header.frame_type,
        request_id: frame_header.request_id,
        frame_len,
        chunk_id: 0,
        total_chunks: 1,
        payload_len: frame.len() as u16,
        prev_checksum: checksum,
        next_checksum: checksum,
    };
    let mut datagram = Vec::with_capacity(UDP_CHUNK_HEADER_LEN + frame.len());
    datagram.extend_from_slice(&header.encode());
    datagram.extend_from_slice(frame);
    Ok(datagram)
}

pub fn encode_udp_single_chunk_from_parts(
    frame_type: FrameType,
    request_id: u64,
    payload: &[u8],
) -> Result<Vec<u8>, UdpChunkError> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| UdpChunkError::FrameTooLarge {
        len: payload.len() + crate::FRAME_HEADER_LEN,
    })?;
    let frame_len = crate::FRAME_HEADER_LEN + payload.len();
    if frame_len > u16::MAX as usize {
        return Err(UdpChunkError::PayloadTooLarge {
            len: frame_len,
            max: u16::MAX as usize,
        });
    }
    let frame_header = crate::FrameHeader::new(frame_type, request_id, payload_len).encode();
    let mut frame_checksum = crc32fast::Hasher::new();
    frame_checksum.update(&frame_header);
    frame_checksum.update(payload);
    let checksum = frame_checksum.finalize();
    let udp_header = UdpChunkHeader {
        version: PROTOCOL_VERSION,
        frame_type,
        request_id,
        frame_len: frame_len as u32,
        chunk_id: 0,
        total_chunks: 1,
        payload_len: frame_len as u16,
        prev_checksum: checksum,
        next_checksum: checksum,
    };
    let mut datagram = Vec::with_capacity(UDP_CHUNK_HEADER_LEN + frame_len);
    datagram.extend_from_slice(&udp_header.encode());
    datagram.extend_from_slice(&frame_header);
    datagram.extend_from_slice(payload);
    Ok(datagram)
}

pub fn decode_udp_single_chunk(datagram: &[u8]) -> Result<Option<Vec<u8>>, UdpChunkError> {
    Ok(decode_udp_single_chunk_ref(datagram)?.map(|frame| frame.to_vec()))
}

pub fn decode_udp_single_chunk_ref(datagram: &[u8]) -> Result<Option<&[u8]>, UdpChunkError> {
    let header = UdpChunkHeader::decode(datagram)?;
    if header.total_chunks != 1 {
        return Ok(None);
    }
    let frame = &datagram[UDP_CHUNK_HEADER_LEN..];
    let checksum = checksum(frame);
    if header.prev_checksum != checksum || header.next_checksum != checksum {
        return Err(UdpChunkError::ChecksumMismatch);
    }
    let (decoded, _) = crate::decode_frame(frame).map_err(UdpChunkError::Frame)?;
    if decoded.frame_type != header.frame_type || decoded.request_id != header.request_id {
        return Err(UdpChunkError::MetadataMismatch);
    }
    Ok(Some(frame))
}

pub struct UdpFrameReassembler {
    deadline: Instant,
    groups: HashMap<u64, ChunkGroup>,
}

impl UdpFrameReassembler {
    pub fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            groups: HashMap::new(),
        }
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn insert(&mut self, datagram: &[u8]) -> Result<Option<Vec<u8>>, UdpChunkError> {
        let header = UdpChunkHeader::decode(datagram)?;
        let payload = datagram[UDP_CHUNK_HEADER_LEN..].to_vec();
        let group = self.groups.entry(header.request_id).or_insert_with(|| {
            ChunkGroup::new(
                header.frame_type,
                header.request_id,
                header.frame_len,
                header.total_chunks,
            )
        });
        group.insert(header, payload)
    }
}

struct ChunkGroup {
    frame_type: FrameType,
    request_id: u64,
    frame_len: u32,
    total_chunks: u16,
    chunks: Vec<Option<ChunkSlot>>,
}

#[derive(Clone)]
struct ChunkSlot {
    payload: Vec<u8>,
    prev_checksum: u32,
    next_checksum: u32,
}

impl ChunkGroup {
    fn new(frame_type: FrameType, request_id: u64, frame_len: u32, total_chunks: u16) -> Self {
        Self {
            frame_type,
            request_id,
            frame_len,
            total_chunks,
            chunks: vec![None; total_chunks as usize],
        }
    }

    fn insert(
        &mut self,
        header: UdpChunkHeader,
        payload: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, UdpChunkError> {
        if header.frame_type != self.frame_type
            || header.request_id != self.request_id
            || header.frame_len != self.frame_len
            || header.total_chunks != self.total_chunks
        {
            return Err(UdpChunkError::MetadataMismatch);
        }

        let idx = header.chunk_id as usize;
        self.chunks[idx] = Some(ChunkSlot {
            payload,
            prev_checksum: header.prev_checksum,
            next_checksum: header.next_checksum,
        });

        if !self.is_complete() {
            return Ok(None);
        }
        self.verify_all()?;
        let mut frame = Vec::with_capacity(self.frame_len as usize);
        for slot in self.chunks.iter().flatten() {
            frame.extend_from_slice(&slot.payload);
        }
        frame.truncate(self.frame_len as usize);
        let (decoded, _) = crate::decode_frame(&frame).map_err(UdpChunkError::Frame)?;
        if decoded.frame_type != self.frame_type || decoded.request_id != self.request_id {
            return Err(UdpChunkError::MetadataMismatch);
        }
        Ok(Some(frame))
    }

    fn is_complete(&self) -> bool {
        self.chunks.iter().all(Option::is_some)
    }

    fn verify_all(&self) -> Result<(), UdpChunkError> {
        let total = self.chunks.len();
        for idx in 0..total {
            let slot = self.chunks[idx].as_ref().expect("complete");
            let prev = if idx == 0 { total - 1 } else { idx - 1 };
            let next = if idx + 1 == total { 0 } else { idx + 1 };
            let prev_slot = self.chunks[prev].as_ref().expect("complete");
            let next_slot = self.chunks[next].as_ref().expect("complete");
            if checksum(&prev_slot.payload) != slot.prev_checksum
                || checksum(&next_slot.payload) != slot.next_checksum
            {
                return Err(UdpChunkError::ChecksumMismatch);
            }
        }
        Ok(())
    }
}

pub fn effective_udp_payload_capacity(remote: SocketAddr, configured_mtu: u16) -> usize {
    let mtu = if configured_mtu > 0 {
        configured_mtu as usize
    } else {
        detect_route_mtu(remote).unwrap_or(UDP_FALLBACK_MTU)
    };
    let ip_header = if remote.is_ipv4() { 20 } else { 40 };
    mtu.saturating_sub(ip_header + 8 + UDP_CHUNK_HEADER_LEN)
        .max(UDP_MIN_PAYLOAD)
}

fn detect_route_mtu(remote: SocketAddr) -> Option<usize> {
    let dev = route_device(remote)?;
    let raw = std::fs::read_to_string(format!("/sys/class/net/{dev}/mtu")).ok()?;
    raw.trim().parse().ok()
}

fn route_device(remote: SocketAddr) -> Option<String> {
    if UdpSocket::bind(if remote.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .and_then(|sock| sock.connect(remote))
    .is_err()
    {
        return None;
    }
    let output = Command::new("ip")
        .args(["route", "get", &remote.ip().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut words = stdout.split_whitespace();
    while let Some(word) = words.next() {
        if word == "dev" {
            return words.next().map(str::to_string);
        }
    }
    None
}

fn checksum(payload: &[u8]) -> u32 {
    crc32fast::hash(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{encode_frame, FrameHeader, FrameType};

    fn frame(payload_len: usize) -> Vec<u8> {
        let payload = vec![7u8; payload_len];
        encode_frame(
            FrameHeader::new(FrameType::RuntimePayload, 42, payload.len() as u32),
            &payload,
        )
    }

    #[test]
    fn single_chunk_roundtrip() {
        let frame = frame(64);
        let chunks = encode_udp_chunks(&frame, 512).expect("chunks");
        assert_eq!(chunks.len(), 1);
        let mut reassembler = UdpFrameReassembler::new(Duration::from_secs(1));
        let out = reassembler
            .insert(&chunks[0])
            .expect("insert")
            .expect("complete");
        assert_eq!(out, frame);
    }

    #[test]
    fn multi_chunk_out_of_order_roundtrip() {
        let frame = frame(1900);
        let chunks = encode_udp_chunks(&frame, 512).expect("chunks");
        assert!(chunks.len() > 2);
        let mut reassembler = UdpFrameReassembler::new(Duration::from_secs(1));
        let mut out = None;
        for idx in (0..chunks.len()).rev() {
            out = reassembler.insert(&chunks[idx]).expect("insert").or(out);
        }
        assert_eq!(out.expect("complete"), frame);
    }

    #[test]
    fn missing_chunk_does_not_complete() {
        let frame = frame(1900);
        let chunks = encode_udp_chunks(&frame, 512).expect("chunks");
        let mut reassembler = UdpFrameReassembler::new(Duration::from_secs(1));
        for chunk in chunks.iter().take(chunks.len() - 1) {
            assert!(reassembler.insert(chunk).expect("insert").is_none());
        }
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let frame = frame(1900);
        let mut chunks = encode_udp_chunks(&frame, 512).expect("chunks");
        let last = chunks.last_mut().expect("last");
        let last_byte = last.last_mut().expect("byte");
        *last_byte ^= 0xff;
        let mut reassembler = UdpFrameReassembler::new(Duration::from_secs(1));
        for chunk in chunks.iter().take(chunks.len() - 1) {
            assert!(reassembler.insert(chunk).expect("insert").is_none());
        }
        assert_eq!(
            reassembler.insert(chunks.last().expect("last")),
            Err(UdpChunkError::ChecksumMismatch)
        );
    }
}
