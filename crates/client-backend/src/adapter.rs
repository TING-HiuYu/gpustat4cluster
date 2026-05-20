use common::{
    protocol::decode_snapshot_payload as common_decode_snapshot_payload, GpuProcessInfo,
    ServerGpuSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    process::Command,
    sync::{Mutex, OnceLock},
};
use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{cache::CacheMap, filter::NodeFilter};

#[derive(Debug, Clone, Deserialize)]
pub struct QueryRequest {
    pub filter: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryResponse {
    #[serde(default)]
    pub meta: QueryMeta,
    pub nodes: Vec<NodeView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryMeta {
    pub status: String,
    pub timestamp_ms: i64,
    pub node_count: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl Default for QueryMeta {
    fn default() -> Self {
        Self {
            status: "unknown".to_string(),
            timestamp_ms: 0,
            node_count: 0,
            errors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeView {
    pub connection_id: String,
    pub hostname: String,
    pub addr: String,
    pub timestamp_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    pub num: u8,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_us: Option<u64>,
    pub gpus: Vec<GpuView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GpuView {
    pub index: u8,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<u32>,
    pub util: u8,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<Vec<ProcessView>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessView {
    #[serde(default)]
    pub uid: u32,
    pub username: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub used_memory_mb: u32,
}

pub fn parse_query_request(s: &str) -> Result<QueryRequest, String> {
    if s.is_empty() {
        return Ok(QueryRequest {
            filter: None,
            user: None,
        });
    }
    serde_json::from_str::<QueryRequest>(s).map_err(|e| format!("invalid QUERY payload: {}", e))
}

pub fn build_query_response(
    rows: &CacheMap,
    filter: Option<&str>,
    user: Option<&str>,
) -> QueryResponse {
    let filter = NodeFilter::parse(filter);
    let mut nodes: Vec<_> = rows
        .values()
        .filter(|entry| entry.matches_filter(&filter))
        .map(|entry| {
            let mut node = node_view_from_entry(entry);
            apply_user_filter(&mut node, user);
            node
        })
        .collect();
    nodes.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
    QueryResponse {
        meta: QueryMeta {
            status: if nodes.is_empty() {
                "empty".to_string()
            } else {
                "ok".to_string()
            },
            timestamp_ms: now_ms(),
            node_count: nodes.len(),
            errors: Vec::new(),
        },
        nodes,
    }
}

fn node_view_from_entry(entry: &crate::cache::ConnectionCacheEntry) -> NodeView {
    if let Some(snapshot) = &entry.last_snapshot {
        return node_view_from_snapshot(
            entry.connection_id.clone(),
            entry.addr,
            entry.record_timestamp,
            snapshot,
            entry.last_query_latency_us,
        );
    }

    let (timestamp_ms, gpus) = parse_payload_view(&entry.server_gpus).unwrap_or_else(|| {
        (
            entry.record_timestamp,
            parse_legacy_gpu_views(&entry.server_gpus),
        )
    });

    NodeView {
        connection_id: entry.connection_id.clone(),
        hostname: entry.hostname.clone(),
        addr: entry.addr.to_string(),
        timestamp_ms,
        driver_version: None,
        num: entry.num,
        stale: entry.last_snapshot.is_none(),
        error: entry.last_error.clone(),
        delay_us: entry.last_query_latency_us,
        gpus,
    }
}

pub fn node_view_from_snapshot(
    connection_id: impl Into<String>,
    addr: SocketAddr,
    timestamp_ms: i64,
    snapshot: &ServerGpuSnapshot,
    delay_us: Option<u64>,
) -> NodeView {
    NodeView {
        connection_id: connection_id.into(),
        hostname: snapshot.hostname.clone(),
        addr: addr.to_string(),
        timestamp_ms,
        driver_version: snapshot.driver_version.clone(),
        num: snapshot.gpus.len().min(u8::MAX as usize) as u8,
        stale: false,
        error: None,
        delay_us,
        gpus: gpu_views_from_snapshot(snapshot),
    }
}

fn parse_payload_view(raw: &[u8]) -> Option<(i64, Vec<GpuView>)> {
    let s = std::str::from_utf8(raw).ok()?;
    let value: Value = serde_json::from_str(s).ok()?;

    if let Some(payload) = payload_bytes_from_server_json(&value) {
        let (timestamp_ms, snapshot) = decode_snapshot_payload(&payload)?;
        return Some((timestamp_ms, gpu_views_from_snapshot(&snapshot)));
    }

    None
}

fn payload_bytes_from_server_json(value: &Value) -> Option<Vec<u8>> {
    if let Some(raw) = value.get("payload_b64").and_then(Value::as_str) {
        return decode_base64(raw);
    }

    let items = value.get("payload")?.as_array()?;
    items
        .iter()
        .map(|item| item.as_u64().and_then(|v| u8::try_from(v).ok()))
        .collect()
}

fn decode_base64(raw: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len() * 3 / 4);
    let mut chunk = [0u8; 4];
    let mut chunk_len = 0usize;

    for byte in raw.bytes().filter(|b| !b.is_ascii_whitespace()) {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 64,
            _ => return None,
        };
        chunk[chunk_len] = value;
        chunk_len += 1;

        if chunk_len == 4 {
            push_base64_chunk(&mut out, chunk)?;
            chunk_len = 0;
        }
    }

    if chunk_len != 0 {
        return None;
    }
    Some(out)
}

fn push_base64_chunk(out: &mut Vec<u8>, chunk: [u8; 4]) -> Option<()> {
    if chunk[0] == 64 || chunk[1] == 64 {
        return None;
    }

    out.push((chunk[0] << 2) | (chunk[1] >> 4));
    if chunk[2] != 64 {
        out.push((chunk[1] << 4) | (chunk[2] >> 2));
    }
    if chunk[3] != 64 {
        if chunk[2] == 64 {
            return None;
        }
        out.push((chunk[2] << 6) | chunk[3]);
    }
    Some(())
}

fn decode_snapshot_payload(payload: &[u8]) -> Option<(i64, ServerGpuSnapshot)> {
    let snapshot = common_decode_snapshot_payload(payload).ok()?;
    Some((now_ms(), snapshot))
}

pub fn gpu_views_from_snapshot(snapshot: &ServerGpuSnapshot) -> Vec<GpuView> {
    snapshot
        .gpus
        .iter()
        .map(|gpu| GpuView {
            index: gpu.index,
            name: gpu.name.clone(),
            temperature_c: gpu.temperature_c,
            util: gpu.utilization.gpu_percent.min(100),
            mem_used_mb: clamp_u64_to_u32(gpu.memory.used_mb),
            mem_total_mb: clamp_u64_to_u32(gpu.memory.total_mb),
            processes: Some(process_views_from_snapshot(&gpu.processes)),
        })
        .collect()
}

fn process_views_from_snapshot(processes: &[GpuProcessInfo]) -> Vec<ProcessView> {
    processes
        .iter()
        .map(|process| ProcessView {
            uid: process.uid,
            username: username_for_uid(process.uid),
            pid: process.pid,
            command: process.command.clone(),
            used_memory_mb: clamp_u64_to_u32(process.used_memory_mb),
        })
        .collect()
}

fn username_for_uid(uid: u32) -> String {
    if uid == u32::MAX {
        return "?".to_string();
    }
    static CACHE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(name) = guard.get(&uid) {
            return name.clone();
        }
    }

    let name = resolve_username_for_uid(uid).unwrap_or_else(|| uid.to_string());
    if let Ok(mut guard) = cache.lock() {
        guard.insert(uid, name.clone());
    }
    name
}

fn resolve_username_for_uid(uid: u32) -> Option<String> {
    let output = Command::new("getent")
        .arg("passwd")
        .arg(uid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8(output.stdout).ok()?;
    line.split(':')
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn apply_user_filter(node: &mut NodeView, user: Option<&str>) {
    let Some(user) = user.filter(|s| !s.is_empty()) else {
        return;
    };

    for gpu in &mut node.gpus {
        if let Some(processes) = &mut gpu.processes {
            processes.retain(|process| process.username == user);
        }
    }
}

fn parse_legacy_gpu_views(raw: &[u8]) -> Vec<GpuView> {
    let s = String::from_utf8_lossy(raw);
    if let Some(view) = parse_round1_server_json(&s) {
        return view;
    }

    parse_round1_csv(&s)
}

fn parse_round1_csv(s: &str) -> Vec<GpuView> {
    s.split(';')
        .filter_map(|item| {
            let parts: Vec<_> = item.split(',').collect();
            if parts.len() != 4 {
                return None;
            }
            Some(GpuView {
                index: parts[0].parse().ok()?,
                name: "GPU".to_string(),
                temperature_c: None,
                util: parts[1].parse().ok()?,
                mem_used_mb: parts[2].parse().ok()?,
                mem_total_mb: parts[3].parse().ok()?,
                processes: None,
            })
        })
        .collect()
}

fn parse_round1_server_json(s: &str) -> Option<Vec<GpuView>> {
    let value: Value = serde_json::from_str(s).ok()?;
    let gpu_num = value.get("gpu_num")?.as_u64()?;
    let util = value.get("avg_utilization")?.as_u64()?.min(100) as u8;

    Some(
        (0..gpu_num.min(u8::MAX as u64))
            .map(|idx| GpuView {
                index: idx as u8,
                name: "GPU".to_string(),
                temperature_c: None,
                util,
                mem_used_mb: 0,
                mem_total_mb: 0,
                processes: None,
            })
            .collect(),
    )
}

fn clamp_u64_to_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        protocol::encode_snapshot_payload, GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization,
    };

    fn snapshot() -> ServerGpuSnapshot {
        ServerGpuSnapshot {
            hostname: "node-a".to_string(),
            driver_version: None,
            gpus: vec![GpuInfo {
                index: 0,
                name: "NVIDIA A100".to_string(),
                temperature_c: None,
                uuid: Some("GPU-123".to_string()),
                memory: GpuMemory {
                    used_mb: 1024,
                    total_mb: 81920,
                },
                utilization: GpuUtilization {
                    gpu_percent: 75,
                    memory_percent: 20,
                },
                processes: vec![
                    GpuProcessInfo {
                        pid: 1234,
                        uid: 1000,
                        command: Some("python train.py".to_string()),
                        used_memory_mb: 512,
                    },
                    GpuProcessInfo {
                        pid: 5678,
                        uid: 1001,
                        command: Some("python eval.py".to_string()),
                        used_memory_mb: 256,
                    },
                ],
            }],
        }
    }

    fn snapshot_payload() -> Vec<u8> {
        let snapshot = snapshot();
        encode_snapshot_payload(&snapshot).expect("encode payload")
    }

    fn encode_base64_for_test(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn backend_response_json_decodes() {
        let raw = r#"{"nodes":[{"connection_id":"conn-001","hostname":"node-a","addr":"10.0.0.1:30000","timestamp_ms":1,"num":1,"gpus":[{"index":0,"util":42,"mem_used_mb":1024,"mem_total_mb":8192}]}]}"#;
        let decoded: QueryResponse = serde_json::from_str(raw).expect("decode response");
        assert_eq!(decoded.meta.status, "unknown");
        assert_eq!(decoded.nodes[0].hostname, "node-a");
        assert_eq!(decoded.nodes[0].gpus[0].util, 42);
    }

    #[test]
    fn empty_query_response_schema_is_stable() {
        let rows = CacheMap::new();
        let response = build_query_response(&rows, None, None);
        let json = serde_json::to_value(&response).expect("response json");
        assert_eq!(json["meta"]["status"], "empty");
        assert_eq!(json["meta"]["node_count"], 0);
        assert!(json["meta"]["timestamp_ms"].as_i64().unwrap_or_default() > 0);
        assert_eq!(json["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn multi_node_query_response_is_sorted_and_counts_nodes() {
        let mut rows = CacheMap::new();
        let addr_b: SocketAddr = "127.0.0.1:39401".parse().unwrap();
        let addr_a: SocketAddr = "127.0.0.1:39400".parse().unwrap();
        let mut snapshot_b = snapshot();
        snapshot_b.hostname = "node-b".to_string();
        snapshot_b.gpus[0].processes[0].uid = 1001;
        let mut snapshot_a = snapshot();
        snapshot_a.hostname = "node-a".to_string();
        snapshot_a.gpus[0].processes[0].uid = 1000;

        crate::cache::upsert_snapshot(&mut rows, "conn-002", addr_b, snapshot_b, None);
        crate::cache::upsert_snapshot(&mut rows, "conn-001", addr_a, snapshot_a, Some(321));

        let response = build_query_response(&rows, None, None);
        assert_eq!(response.meta.status, "ok");
        assert_eq!(response.meta.node_count, 2);
        assert_eq!(response.nodes[0].hostname, "node-a");
        assert_eq!(response.nodes[1].hostname, "node-b");
        assert_eq!(
            response.nodes[0].gpus[0].processes.as_ref().unwrap()[0].uid,
            1000
        );
        assert_eq!(
            response.nodes[1].gpus[0].processes.as_ref().unwrap()[0].uid,
            1001
        );
    }

    #[test]
    fn common_snapshot_maps_to_gpu_view_with_processes() {
        let views = gpu_views_from_snapshot(&snapshot());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].util, 75);
        assert_eq!(views[0].mem_total_mb, 81920);
        let processes = views[0].processes.as_ref().expect("processes");
        assert_eq!(processes[0].uid, 1000);
        assert_eq!(processes[0].used_memory_mb, 512);
    }

    #[test]
    fn common_snapshot_builds_node_view() {
        let addr: SocketAddr = "10.0.0.1:30000".parse().unwrap();
        let node =
            node_view_from_snapshot("conn-001", addr, 1_700_000_000_123, &snapshot(), Some(321));
        assert_eq!(node.connection_id, "conn-001");
        assert_eq!(node.hostname, "node-a");
        assert_eq!(node.timestamp_ms, 1_700_000_000_123);
        assert_eq!(node.num, 1);
        assert!(!node.stale);
        assert_eq!(node.delay_us, Some(321));
        assert_eq!(node.gpus[0].processes.as_ref().unwrap()[0].uid, 1000);
    }

    #[test]
    fn common_snapshot_payload_decodes_from_payload_array() {
        let payload = snapshot_payload();
        let body = serde_json::json!({"ok": true, "payload": payload}).to_string();
        let (timestamp_ms, views) =
            parse_payload_view(body.as_bytes()).expect("decode payload view");
        assert!(timestamp_ms > 0);
        assert_eq!(views[0].util, 75);
        assert_eq!(views[0].processes.as_ref().unwrap()[0].uid, 1000);
    }

    #[test]
    fn common_snapshot_payload_decodes_from_payload_b64() {
        let encoded = encode_base64_for_test(&snapshot_payload());
        let body = serde_json::json!({"ok": true, "payload_b64": encoded}).to_string();
        let (_, views) = parse_payload_view(body.as_bytes()).expect("decode b64 payload view");
        assert_eq!(views[0].mem_used_mb, 1024);
    }

    #[test]
    fn user_filter_trims_processes_without_dropping_gpu_rows() {
        let mut node = NodeView {
            connection_id: "conn-001".to_string(),
            hostname: "node-a".to_string(),
            addr: "10.0.0.1:30000".to_string(),
            timestamp_ms: 1,
            driver_version: None,
            num: 1,
            stale: false,
            error: None,
            delay_us: None,
            gpus: gpu_views_from_snapshot(&snapshot()),
        };

        let user = username_for_uid(1000);
        apply_user_filter(&mut node, Some(&user));
        let processes = node.gpus[0].processes.as_ref().unwrap();
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].uid, 1000);
    }

    #[test]
    fn round1_server_json_payload_maps_to_gpu_view() {
        let views = parse_legacy_gpu_views(br#"{"gpu_num":2,"avg_utilization":42}"#);
        assert_eq!(views.len(), 2);
        assert_eq!(views[1].util, 42);
    }

    #[test]
    fn round1_csv_payload_still_maps_to_gpu_view() {
        let views = parse_legacy_gpu_views(b"0,35,2048,8192;1,76,6144,8192");
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].mem_used_mb, 2048);
        assert_eq!(views[1].util, 76);
    }
}
