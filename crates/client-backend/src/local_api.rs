#[cfg(feature = "kcp-transport")]
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
#[cfg(feature = "kcp-transport")]
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{adapter, cache::SharedCache, discovery, logger};

pub const DEFAULT_BACKEND_SOCKET: &str = "/run/gpustat4cluster/client.sock";
pub const BACKEND_SOCKET_ENV: &str = "GPUSTAT4CLUSTER_BACKEND_SOCKET";
#[cfg(feature = "kcp-transport")]
const KCP_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct LocalApiState {
    cache: SharedCache,
    #[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
    cache_ttl_ms: u64,
    kcp_enabled: bool,
    discovery_multicast_addr: String,
    discover_wait: Duration,
    #[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
    heartbeat_interval: Duration,
    connection_idle_timeout: Duration,
    #[cfg(feature = "kcp-transport")]
    max_connections: usize,
    #[cfg(feature = "kcp-transport")]
    kcp_retry_limit: usize,
    multicast_outbound_ip: Vec<String>,
    #[cfg(feature = "kcp-transport")]
    kcp_runtime: Arc<tokio::runtime::Runtime>,
    #[cfg(feature = "kcp-transport")]
    kcp_sessions: Arc<Mutex<HashMap<SocketAddr, crate::kcp_client::ConnectedKcpNode>>>,
    #[cfg(feature = "kcp-transport")]
    kcp_connecting: Arc<Mutex<HashSet<SocketAddr>>>,
}

impl LocalApiState {
    pub fn new(
        cache: SharedCache,
        cache_ttl_ms: u64,
        kcp_enabled: bool,
        discovery_multicast_addr: String,
        discover_wait: Duration,
        heartbeat_interval: Duration,
        connection_idle_timeout: Duration,
        #[cfg_attr(not(feature = "kcp-transport"), allow(unused_variables))] max_connections: usize,
        #[cfg_attr(not(feature = "kcp-transport"), allow(unused_variables))] kcp_retry_limit: usize,
        multicast_outbound_ip: Vec<String>,
    ) -> Self {
        Self {
            cache,
            cache_ttl_ms,
            kcp_enabled,
            discovery_multicast_addr,
            discover_wait,
            heartbeat_interval,
            connection_idle_timeout,
            #[cfg(feature = "kcp-transport")]
            max_connections,
            #[cfg(feature = "kcp-transport")]
            kcp_retry_limit,
            multicast_outbound_ip,
            #[cfg(feature = "kcp-transport")]
            kcp_runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_io()
                    .enable_time()
                    .worker_threads(max_connections.max(1))
                    .thread_name("gpustat4cluster-client-kcp")
                    .build()
                    .expect("create client KCP runtime"),
            ),
            #[cfg(feature = "kcp-transport")]
            kcp_sessions: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "kcp-transport")]
            kcp_connecting: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn add_discovered_nodes(&self, nodes: &[discovery::DiscoveredNode]) {
        if nodes.is_empty() {
            return;
        }
        let Ok(mut rows) = self.cache.lock() else {
            logger::warn("cache lock poisoned");
            return;
        };
        let index_base = rows.len();
        for (idx, node) in nodes.iter().enumerate() {
            let key = format!("{}-{}", node.addr.ip(), node.addr.port());
            rows.entry(key)
                .or_insert_with(|| crate::cache::ConnectionCacheEntry {
                    connection_id: format!("conn-{:03}", index_base + idx + 1),
                    hostname: node.hostname.clone(),
                    num: 0,
                    server_gpus: Vec::new(),
                    record_timestamp: node.ts_ms,
                    addr: node.addr,
                    last_snapshot: None,
                    last_error: None,
                    last_query_latency_us: None,
                });
        }
    }

    #[cfg(feature = "kcp-transport")]
    pub fn establish_kcp_connections(&self, nodes: &[discovery::DiscoveredNode]) {
        if !self.kcp_enabled {
            return;
        }
        let index_base = self.cache.lock().map(|rows| rows.len()).unwrap_or(0);
        for (idx, node) in nodes.iter().enumerate() {
            self.establish_one_kcp_connection(index_base + idx + 1, node.addr);
        }
    }

    #[cfg(feature = "kcp-transport")]
    fn establish_one_kcp_connection(&self, index: usize, addr: SocketAddr) {
        let current_sessions = match self.kcp_sessions.lock() {
            Ok(sessions) => {
                if sessions.contains_key(&addr) {
                    return;
                }
                sessions.len()
            }
            Err(_) => {
                logger::warn("kcp session lock poisoned");
                return;
            }
        };
        if current_sessions >= self.max_connections {
            logger::transport_warn(
                "kcp",
                format!(
                    "event=max_connections_reached addr={} max_connections={}",
                    addr, self.max_connections
                ),
            );
            return;
        }
        match self.kcp_connecting.lock() {
            Ok(mut connecting) => {
                if !connecting.insert(addr) {
                    return;
                }
            }
            Err(_) => {
                logger::warn("kcp connecting lock poisoned");
                return;
            }
        }

        let result = self.connect_with_retry(addr);
        if let Ok(mut connecting) = self.kcp_connecting.lock() {
            connecting.remove(&addr);
        }

        match result {
            Ok(node) => {
                if let Ok(mut sessions) = self.kcp_sessions.lock() {
                    if sessions.contains_key(&addr) {
                        if let Err(e) =
                            self.kcp_runtime
                                .block_on(crate::kcp_client::disconnect_connected(
                                    &node,
                                    "client duplicate session",
                                ))
                        {
                            logger::transport_warn(
                                "kcp",
                                format!("event=disconnect_failed addr={} error={}", addr, e),
                            );
                        }
                        return;
                    }
                    if sessions.len() >= self.max_connections {
                        if let Err(e) =
                            self.kcp_runtime
                                .block_on(crate::kcp_client::disconnect_connected(
                                    &node,
                                    "client max connections reached",
                                ))
                        {
                            logger::transport_warn(
                                "kcp",
                                format!("event=disconnect_failed addr={} error={}", addr, e),
                            );
                        }
                        logger::transport_warn(
                            "kcp",
                            format!(
                                "event=max_connections_reached addr={} max_connections={}",
                                addr, self.max_connections
                            ),
                        );
                        return;
                    }
                    sessions.insert(addr, node.clone());
                } else {
                    logger::warn("kcp session lock poisoned");
                    if let Err(e) =
                        self.kcp_runtime
                            .block_on(crate::kcp_client::disconnect_connected(
                                &node,
                                "client session lock poisoned",
                            ))
                    {
                        logger::transport_warn(
                            "kcp",
                            format!("event=disconnect_failed addr={} error={}", addr, e),
                        );
                    }
                    return;
                }
                logger::transport_info(
                    "kcp",
                    format!(
                        "event=connected addr={} hostname={} gpu_num={} connection_count={}",
                        addr,
                        node.info.hostname,
                        node.info.gpu_num,
                        node.connection_count()
                    ),
                );
                if let Ok(mut rows) = self.cache.lock() {
                    crate::cache::upsert_handshake(
                        &mut rows,
                        format!("conn-{:03}", index),
                        addr,
                        &node.info,
                    );
                }
                self.spawn_heartbeat(node);
            }
            Err(e) => logger::transport_warn(
                "kcp",
                format!("event=connect_failed addr={} error={}", addr, e),
            ),
        }
    }

    #[cfg(feature = "kcp-transport")]
    fn connect_with_retry(
        &self,
        addr: SocketAddr,
    ) -> Result<crate::kcp_client::ConnectedKcpNode, crate::kcp_client::KcpClientError> {
        let mut last_error = None;
        for attempt in 1..=self.kcp_retry_limit {
            match self
                .kcp_runtime
                .block_on(crate::kcp_client::connect_node_with_timeout(
                    addr,
                    self.connection_idle_timeout,
                )) {
                Ok(node) => return Ok(node),
                Err(e) => {
                    if attempt < self.kcp_retry_limit {
                        logger::transport_warn(
                            "kcp",
                            format!(
                                "event=connect_retry addr={} attempt={} error={}",
                                addr, attempt, e
                            ),
                        );
                        std::thread::sleep(KCP_CONNECT_RETRY_DELAY);
                    }
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.expect("at least one KCP connect attempt"))
    }

    #[cfg(feature = "kcp-transport")]
    fn spawn_heartbeat(&self, node: crate::kcp_client::ConnectedKcpNode) {
        let interval = self.heartbeat_interval;
        if interval.is_zero() {
            return;
        }
        let cache = Arc::clone(&self.cache);
        let sessions = Arc::clone(&self.kcp_sessions);
        self.kcp_runtime.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = crate::kcp_client::heartbeat_connected(&node).await {
                    crate::kcp_client::close_connected(&node);
                    logger::transport_warn(
                        "kcp",
                        format!(
                            "event=disconnected addr={} hostname={} error={}",
                            node.addr, node.info.hostname, e
                        ),
                    );
                    if let Ok(mut sessions) = sessions.lock() {
                        sessions.remove(&node.addr);
                    }
                    if let Ok(mut rows) = cache.lock() {
                        let connection_id = rows
                            .values()
                            .find(|entry| entry.addr == node.addr)
                            .map(|entry| entry.connection_id.clone())
                            .unwrap_or_else(|| "conn-unknown".to_string());
                        crate::cache::mark_stale(
                            &mut rows,
                            connection_id,
                            node.addr,
                            node.info.hostname.clone(),
                            e.to_string(),
                        );
                    }
                    break;
                }
            }
        });
    }

    #[cfg(feature = "kcp-transport")]
    pub fn shutdown(&self, reason: &str) {
        let sessions = self
            .kcp_sessions
            .lock()
            .map(|sessions| sessions.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        for node in sessions {
            logger::transport_info(
                "kcp",
                format!(
                    "event=disconnect_send addr={} hostname={} reason={}",
                    node.addr, node.info.hostname, reason
                ),
            );
            if let Err(e) = self
                .kcp_runtime
                .block_on(crate::kcp_client::disconnect_connected(&node, reason))
            {
                logger::transport_warn(
                    "kcp",
                    format!(
                        "event=disconnect_failed addr={} hostname={} error={}",
                        node.addr, node.info.hostname, e
                    ),
                );
            }
        }
    }
}

pub fn serve(state: LocalApiState, configured_uds_path: Option<&str>) -> Result<(), String> {
    serve_on_uds(state, uds_path_from_config_or_env(configured_uds_path))
}

pub fn uds_path_from_config_or_env(configured_uds_path: Option<&str>) -> String {
    std::env::var(BACKEND_SOCKET_ENV)
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .or_else(|| {
            configured_uds_path
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_BACKEND_SOCKET.to_string())
}

fn serve_on_uds(state: LocalApiState, socket_path: String) -> Result<(), String> {
    let path = Path::new(&socket_path);
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|e| format!("remove stale UDS {} failed: {}", socket_path, e))?;
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create UDS parent {} failed: {}", parent.display(), e))?;
    }

    let listener =
        UnixListener::bind(path).map_err(|e| format!("bind UDS {} failed: {}", socket_path, e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("chmod UDS {} failed: {}", socket_path, e))?;

    logger::info(format!("frontend UDS listening on {}", socket_path));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, state) {
                        logger::warn(e);
                    }
                });
            }
            Err(e) => logger::warn(format!("UDS accept failed: {}", e)),
        }
    }

    Ok(())
}

fn handle_client<S>(stream: S, state: LocalApiState) -> Result<(), String>
where
    S: Read + Write,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read request failed: {}", e))?;
        if n == 0 {
            return Ok(());
        }

        let cmd = line.trim();
        let response = handle_command(cmd, &state)?;
        let stream = reader.get_mut();
        stream
            .write_all(response.as_bytes())
            .map_err(|e| format!("write response failed: {}", e))?;
        stream
            .flush()
            .map_err(|e| format!("flush response failed: {}", e))?;
    }
}

pub(crate) fn handle_command(cmd: &str, state: &LocalApiState) -> Result<String, String> {
    if cmd == "LIST" {
        let rows = state
            .cache
            .lock()
            .map_err(|_| "cache lock poisoned".to_string())?;
        let mut entries: Vec<_> = rows.values().collect();
        entries.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        let mut out = String::new();
        for n in entries {
            out.push_str(&format!(
                "{} {} {} {}",
                n.connection_id, n.hostname, n.addr, n.record_timestamp
            ));
            out.push('\n');
        }
        return Ok(out);
    }

    if let Some(payload) = cmd.strip_prefix("QUERY") {
        let req = adapter::parse_query_request(payload.trim())?;
        ensure_nodes_available_for_query(state)?;
        refresh_stale_cache_for_query(state)?;
        let rows = state
            .cache
            .lock()
            .map_err(|_| "cache lock poisoned".to_string())?;
        let resp = adapter::build_query_response(&rows, req.filter.as_deref(), req.user.as_deref());
        let json =
            serde_json::to_string(&resp).map_err(|e| format!("encode response failed: {}", e))?;
        return Ok(format!("{}\n", json));
    }

    Ok(format!("ERR unsupported command: {}\n", cmd))
}

fn ensure_nodes_available_for_query(state: &LocalApiState) -> Result<(), String> {
    let cache_is_empty = state
        .cache
        .lock()
        .map_err(|_| "cache lock poisoned".to_string())?
        .is_empty();
    if !cache_is_empty {
        return Ok(());
    }

    logger::info("cache is empty; running discovery before QUERY");
    let multicast_nodes = match discovery::discover_nodes(
        &state.discovery_multicast_addr,
        state.discover_wait,
        &state.multicast_outbound_ip,
        if state.kcp_enabled { "kcp" } else { "tcp" },
    ) {
        Ok(nodes) => nodes,
        Err(e) => {
            logger::warn(format!("query-triggered discovery failed: {}", e));
            Vec::new()
        }
    };
    let static_nodes = match discovery::static_nodes_from_env() {
        Ok(nodes) => nodes,
        Err(e) => {
            logger::warn(format!("query-triggered static nodes ignored: {}", e));
            Vec::new()
        }
    };
    let discovered = discovery::merge_discovered_nodes(multicast_nodes, static_nodes);
    if discovered.is_empty() {
        logger::warn("query-triggered discovery found no nodes");
        return Ok(());
    }

    let mut rows = state
        .cache
        .lock()
        .map_err(|_| "cache lock poisoned".to_string())?;
    if rows.is_empty() {
        *rows = crate::cache::build_cache(discovered);
        logger::info(format!(
            "query-triggered discovery populated {} node(s)",
            rows.len()
        ));
    }
    Ok(())
}

#[cfg(feature = "kcp-transport")]
fn refresh_stale_cache_for_query(state: &LocalApiState) -> Result<(), String> {
    if !state.kcp_enabled {
        return refresh_stale_cache_for_query_tcp(state);
    }

    let targets = stale_targets(&state.cache, state.cache_ttl_ms)?;
    if targets.is_empty() {
        return Ok(());
    }

    for target in targets {
        let session = state
            .kcp_sessions
            .lock()
            .map_err(|_| "kcp session lock poisoned".to_string())?
            .get(&target.addr)
            .cloned();
        let session = match session {
            Some(session) => session,
            None => {
                let node = discovery::DiscoveredNode {
                    hostname: target.hostname.clone(),
                    addr: target.addr,
                    ts_ms: now_ms(),
                };
                state.establish_kcp_connections(&[node]);
                state
                    .kcp_sessions
                    .lock()
                    .map_err(|_| "kcp session lock poisoned".to_string())?
                    .get(&target.addr)
                    .cloned()
                    .ok_or_else(|| format!("no KCP session for {}", target.addr))?
            }
        };

        let query_started = std::time::Instant::now();
        match state
            .kcp_runtime
            .block_on(crate::kcp_client::query_connected(&session))
        {
            Ok(snapshot) => {
                let latency_us = query_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                let mut rows = state
                    .cache
                    .lock()
                    .map_err(|_| "cache lock poisoned".to_string())?;
                crate::cache::upsert_snapshot(
                    &mut rows,
                    target.connection_id,
                    target.addr,
                    snapshot,
                    Some(latency_us),
                );
            }
            Err(e) => {
                crate::kcp_client::close_connected(&session);
                if target.had_snapshot {
                    logger::transport_warn(
                        "kcp",
                        format!(
                            "event=disconnected addr={} hostname={} error={}",
                            target.addr, target.hostname, e
                        ),
                    );
                } else {
                    logger::transport_warn(
                        "kcp",
                        format!(
                            "event=query_failed addr={} hostname={} error={}",
                            target.addr, target.hostname, e
                        ),
                    );
                }
                if let Ok(mut sessions) = state.kcp_sessions.lock() {
                    sessions.remove(&target.addr);
                }
                let mut rows = state
                    .cache
                    .lock()
                    .map_err(|_| "cache lock poisoned".to_string())?;
                crate::cache::mark_stale(
                    &mut rows,
                    target.connection_id,
                    target.addr,
                    target.hostname,
                    e.to_string(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "kcp-transport"))]
fn refresh_stale_cache_for_query(state: &LocalApiState) -> Result<(), String> {
    if state.kcp_enabled {
        logger::warn("KCP requested but this binary was built without kcp-transport");
        return Ok(());
    }
    refresh_stale_cache_for_query_tcp(state)
}

fn refresh_stale_cache_for_query_tcp(state: &LocalApiState) -> Result<(), String> {
    let targets = stale_targets(&state.cache, state.cache_ttl_ms)?;
    if targets.is_empty() {
        return Ok(());
    }

    for target in targets {
        let query_started = std::time::Instant::now();
        match crate::tcp_client::query_node(target.addr, state.connection_idle_timeout) {
            Ok(snapshot) => {
                let latency_us = query_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                let hostname = snapshot.hostname.clone();
                let gpu_num = snapshot.gpus.len();
                if !target.had_snapshot {
                    logger::transport_info(
                        "tcp",
                        format!(
                            "event=connected addr={} hostname={} gpu_num={} latency_us={}",
                            target.addr, hostname, gpu_num, latency_us
                        ),
                    );
                } else if target.had_error {
                    logger::transport_info(
                        "tcp",
                        format!(
                            "event=reconnected addr={} hostname={} gpu_num={} latency_us={}",
                            target.addr, hostname, gpu_num, latency_us
                        ),
                    );
                } else {
                    logger::transport_info(
                        "tcp",
                        format!(
                            "event=query_ok addr={} hostname={} gpu_num={} latency_us={}",
                            target.addr, hostname, gpu_num, latency_us
                        ),
                    );
                }
                let mut rows = state
                    .cache
                    .lock()
                    .map_err(|_| "cache lock poisoned".to_string())?;
                crate::cache::upsert_snapshot(
                    &mut rows,
                    target.connection_id,
                    target.addr,
                    snapshot,
                    Some(latency_us),
                );
            }
            Err(e) => {
                logger::transport_warn(
                    "tcp",
                    format!(
                        "event=query_failed addr={} hostname={} error={}",
                        target.addr, target.hostname, e
                    ),
                );
                let mut rows = state
                    .cache
                    .lock()
                    .map_err(|_| "cache lock poisoned".to_string())?;
                crate::cache::mark_stale(
                    &mut rows,
                    target.connection_id,
                    target.addr,
                    target.hostname,
                    e.to_string(),
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
#[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
struct RefreshTarget {
    connection_id: String,
    hostname: String,
    addr: SocketAddr,
    had_snapshot: bool,
    had_error: bool,
}

#[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
fn stale_targets(cache: &SharedCache, cache_ttl_ms: u64) -> Result<Vec<RefreshTarget>, String> {
    let now = now_ms();
    let rows = cache
        .lock()
        .map_err(|_| "cache lock poisoned".to_string())?;
    Ok(rows
        .values()
        .filter(|entry| {
            entry.last_snapshot.is_none()
                || now.saturating_sub(entry.record_timestamp) > cache_ttl_ms as i64
        })
        .map(|entry| RefreshTarget {
            connection_id: entry.connection_id.clone(),
            hostname: entry.hostname.clone(),
            addr: entry.addr,
            had_snapshot: entry.last_snapshot.is_some(),
            had_error: entry.last_error.is_some(),
        })
        .collect())
}

#[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{upsert_snapshot, CacheMap};
    use common::{GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization, ServerGpuSnapshot};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn kcp_cache() -> SharedCache {
        let mut rows: CacheMap = HashMap::new();
        upsert_snapshot(
            &mut rows,
            "conn-001",
            "127.0.0.1:30000".parse().unwrap(),
            ServerGpuSnapshot {
                hostname: "kcp-node".to_string(),
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
            },
            Some(123),
        );
        Arc::new(Mutex::new(rows))
    }

    fn state(cache: SharedCache) -> LocalApiState {
        LocalApiState::new(
            cache,
            40,
            false,
            "239.0.0.1:4000".to_string(),
            Duration::from_millis(1),
            Duration::from_secs(5),
            Duration::from_secs(10),
            2,
            3,
            Vec::new(),
        )
    }

    #[test]
    fn query_response_includes_kcp_snapshot_node() {
        let response = handle_command("QUERY {}", &state(kcp_cache())).expect("query response");
        assert!(response.contains("\"meta\""));
        assert!(response.contains("kcp-node"));
        assert!(response.contains("\"util\":66"));
        assert!(response.contains("\"uid\":1000"));
    }

    #[test]
    fn query_response_empty_cache_keeps_json_schema() {
        let cache = Arc::new(Mutex::new(CacheMap::new()));
        let response = handle_command("QUERY {}", &state(cache)).expect("query response");
        let json: serde_json::Value = serde_json::from_str(response.trim()).expect("json");
        assert_eq!(json["meta"]["status"], "empty");
        assert_eq!(json["meta"]["node_count"], 0);
        assert_eq!(json["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_response_includes_kcp_snapshot_node() {
        let response = handle_command("LIST", &state(kcp_cache())).expect("list response");
        assert!(response.contains("conn-001 kcp-node 127.0.0.1:30000 "));
    }
}
