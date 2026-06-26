#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

use crate::args::CliOptions;

#[cfg_attr(not(unix), allow(dead_code))]
pub const BACKEND_SOCKET_ENV: &str = "GPUSTAT4CLUSTER_BACKEND_SOCKET";
pub const CONFIG_PATH_ENV: &str = "GPUSTAT4CLUSTER_CONFIG";
pub const DEFAULT_CONFIG_PATH: &str = "/etc/gpustat4cluster/client.toml";
#[cfg_attr(not(unix), allow(dead_code))]
pub const DEFAULT_BACKEND_SOCKET: &str = "/run/gpustat4cluster/client.sock";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QueryResponse {
    #[serde(default)]
    pub meta: QueryMeta,
    pub nodes: Vec<NodeView>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
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

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NodeView {
    #[serde(default)]
    pub connection_id: String,
    pub hostname: String,
    #[serde(default)]
    pub addr: String,
    #[serde(default)]
    pub timestamp_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    #[serde(default)]
    pub num: u8,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_us: Option<u64>,
    pub gres: Vec<GresView>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GresView {
    pub index: u8,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub temperature_c: Option<u32>,
    pub util: u8,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    #[serde(default)]
    pub processes: Option<Vec<ProcessView>>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProcessView {
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub username: String,
    pub pid: u32,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub used_memory_mb: u32,
}

pub enum BackendConnection {
    #[cfg(unix)]
    Unix(UnixStream),
    #[allow(dead_code)]
    #[cfg(not(unix))]
    Unsupported,
}

impl BackendConnection {
    pub fn query(&mut self, opts: &CliOptions) -> Result<QueryResponse, String> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => query_stream(stream, opts),
            #[cfg(not(unix))]
            Self::Unsupported => {
                let _ = opts;
                unsupported_backend()
            }
        }
    }
}

#[cfg(unix)]
pub fn connect_backend(opts: &CliOptions) -> Result<BackendConnection, String> {
    let socket_path = backend_socket_from_options(opts);
    let stream = UnixStream::connect(&socket_path).map_err(|e| {
        format!(
            "backend UDS 未运行：请先启动 gpustat4cluster-client-backend（{}）。连接失败: {}",
            socket_path, e
        )
    })?;
    Ok(BackendConnection::Unix(stream))
}

#[cfg(not(unix))]
pub fn connect_backend(_opts: &CliOptions) -> Result<BackendConnection, String> {
    unsupported_backend()
}

pub fn query_backend(opts: &CliOptions) -> Result<QueryResponse, String> {
    connect_backend(opts)?.query(opts)
}

#[cfg(unix)]
fn query_stream<S>(stream: &mut S, opts: &CliOptions) -> Result<QueryResponse, String>
where
    S: Write + std::io::Read,
{
    let cmd = build_query_command(opts);
    stream
        .write_all(cmd.as_bytes())
        .map_err(|e| format!("send QUERY failed: {}", e))?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read QUERY response failed: {}", e))?;

    decode_response(line.trim())
}

#[cfg(not(unix))]
fn unsupported_backend<T>() -> Result<T, String> {
    Err(
        "gpustat4cluster client on this platform can render data but cannot connect to the local UDS backend yet; use Linux or macOS for live cluster queries"
            .to_string(),
    )
}

#[cfg_attr(not(unix), allow(dead_code))]
pub fn backend_socket_from_options(opts: &CliOptions) -> String {
    if let Some(path) = opts.backend_socket.as_deref() {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    std::env::var(BACKEND_SOCKET_ENV)
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .or_else(configured_uds_path)
        .unwrap_or_else(|| DEFAULT_BACKEND_SOCKET.to_string())
}

#[cfg_attr(not(unix), allow(dead_code))]
fn configured_uds_path() -> Option<String> {
    let path = std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: common::Config = toml::from_str(&raw).ok()?;
    cfg.services
        .uds_path
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

pub fn latency_display_from_options(opts: &CliOptions) -> bool {
    opts.latency_display
        .or_else(configured_latency_display)
        .unwrap_or(true)
}

fn configured_latency_display() -> Option<bool> {
    let path = std::env::var(CONFIG_PATH_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: common::Config = toml::from_str(&raw).ok()?;
    Some(cfg.services.latency_display)
}

#[cfg_attr(not(unix), allow(dead_code))]
pub fn build_query_command(opts: &CliOptions) -> String {
    let req = serde_json::json!({ "filter": opts.node_filter, "user": opts.user_filter });
    format!("QUERY {}\n", req)
}

#[cfg_attr(not(unix), allow(dead_code))]
pub fn decode_response(raw: &str) -> Result<QueryResponse, String> {
    serde_json::from_str(raw).map_err(|e| format!("invalid backend response: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_response_json_decodes_without_processes() {
        let raw = r#"{"nodes":[{"hostname":"node-a","gres":[{"index":0,"util":42,"mem_used_mb":1024,"mem_total_mb":8192}]}]}"#;
        let resp = decode_response(raw).expect("decode response");
        assert_eq!(resp.meta.status, "unknown");
        assert_eq!(resp.nodes[0].hostname, "node-a");
        assert!(!resp.nodes[0].stale);
        assert_eq!(resp.nodes[0].gres[0].processes, None);
    }

    #[test]
    fn backend_response_json_decodes_with_processes() {
        let raw = r#"{"nodes":[{"hostname":"node-a","gres":[{"index":0,"util":42,"mem_used_mb":1024,"mem_total_mb":8192,"processes":[{"username":"alice","pid":7,"command":"python","used_memory_mb":512}]}]}]}"#;
        let resp = decode_response(raw).expect("decode response");
        let processes = resp.nodes[0].gres[0].processes.as_ref().unwrap();
        assert_eq!(processes[0].username, "alice");
        assert_eq!(processes[0].used_memory_mb, 512);
    }

    #[test]
    fn backend_response_json_decodes_with_meta_and_stale() {
        let raw = r#"{"meta":{"status":"empty","timestamp_ms":123,"node_count":0,"errors":[]},"nodes":[{"hostname":"node-a","stale":true,"error":"transport timeout","gres":[]}]}"#;
        let resp = decode_response(raw).expect("decode response");
        assert_eq!(resp.meta.status, "empty");
        assert_eq!(resp.meta.timestamp_ms, 123);
        assert!(resp.nodes[0].stale);
        assert_eq!(resp.nodes[0].error.as_deref(), Some("transport timeout"));
    }

    #[test]
    fn latency_display_reads_config_default() {
        let opts = CliOptions::default();
        assert!(latency_display_from_options(&opts));
    }

    #[test]
    fn backend_socket_defaults_to_run_path() {
        assert_eq!(
            backend_socket_from_options(&CliOptions::default()),
            DEFAULT_BACKEND_SOCKET
        );
    }

    #[test]
    fn query_command_includes_filters() {
        let opts = CliOptions {
            node_filter: Some("node-a".to_string()),
            user_filter: Some("alice".to_string()),
            ..CliOptions::default()
        };
        let cmd = build_query_command(&opts);
        assert!(cmd.starts_with("QUERY "));
        assert!(cmd.contains("\"filter\":\"node-a\""));
        assert!(cmd.contains("\"user\":\"alice\""));
    }
}
