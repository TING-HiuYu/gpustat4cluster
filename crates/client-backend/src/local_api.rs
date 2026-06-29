use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{adapter, cache::SharedCache, connection::SharedServerConnection, discovery, logger};

pub const DEFAULT_BACKEND_SOCKET: &str = "/run/gpustat4cluster/client.sock";
pub const BACKEND_SOCKET_ENV: &str = "GPUSTAT4CLUSTER_BACKEND_SOCKET";

#[derive(Clone)]
pub struct LocalApiState {
    cache: SharedCache,
    cache_ttl_ms: u64,
    transport_protocol: String,
    udp_mtu: u16,
    discovery_multicast_addr: String,
    discover_wait: Duration,
    heartbeat_interval: Duration,
    connection_idle_timeout: Duration,
    max_connections: usize,
    multicast_outbound_ip: Vec<String>,
    connections: Arc<Mutex<HashMap<SocketAddr, SharedServerConnection>>>,
    connecting: Arc<Mutex<HashSet<SocketAddr>>>,
}

impl LocalApiState {
    pub fn new(
        cache: SharedCache,
        cache_ttl_ms: u64,
        transport_protocol: String,
        udp_mtu: u16,
        discovery_multicast_addr: String,
        discover_wait: Duration,
        heartbeat_interval: Duration,
        connection_idle_timeout: Duration,
        max_connections: usize,
        multicast_outbound_ip: Vec<String>,
    ) -> Self {
        Self {
            cache,
            cache_ttl_ms,
            transport_protocol,
            udp_mtu,
            discovery_multicast_addr,
            discover_wait,
            heartbeat_interval,
            connection_idle_timeout,
            max_connections,
            multicast_outbound_ip,
            connections: Arc::new(Mutex::new(HashMap::new())),
            connecting: Arc::new(Mutex::new(HashSet::new())),
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
            if rows.values().any(|entry| entry.hostname == node.hostname) {
                continue;
            }
            let key = format!("{}-{}", node.addr.ip(), node.addr.port());
            rows.entry(key)
                .or_insert_with(|| crate::cache::ConnectionCacheEntry {
                    connection_id: format!("conn-{:03}", index_base + idx + 1),
                    hostname: node.hostname.clone(),
                    num: 0,
                    server_gres: Vec::new(),
                    record_timestamp: node.ts_ms,
                    addr: node.addr,
                    last_snapshot: None,
                    last_error: None,
                    last_query_latency_us: None,
                });
        }
    }

    pub fn establish_tcp_connections(&self, nodes: &[discovery::DiscoveredNode]) {
        if self.protocol() != "tcp" {
            return;
        }
        self.establish_connections(nodes);
    }

    pub fn establish_udp_connections(&self, nodes: &[discovery::DiscoveredNode]) {
        if self.protocol() != "udp" {
            return;
        }
        self.establish_connections(nodes);
    }

    fn establish_connections(&self, nodes: &[discovery::DiscoveredNode]) {
        let index_base = self.cache.lock().map(|rows| rows.len()).unwrap_or(0);
        for (idx, node) in nodes.iter().enumerate() {
            self.establish_one_connection(index_base + idx + 1, node.addr);
        }
    }

    fn establish_one_connection(&self, index: usize, addr: SocketAddr) {
        let current_connections = match self.connections.lock() {
            Ok(connections) => {
                if connections.contains_key(&addr) {
                    return;
                }
                connections.len()
            }
            Err(_) => {
                logger::warn("connection pool lock poisoned");
                return;
            }
        };
        if current_connections >= self.max_connections {
            logger::transport_warn(
                self.protocol(),
                format!(
                    "event=max_connections_reached addr={} max_connections={}",
                    addr, self.max_connections
                ),
            );
            return;
        }

        match self.connecting.lock() {
            Ok(mut connecting) => {
                if !connecting.insert(addr) {
                    return;
                }
            }
            Err(_) => {
                logger::warn("connecting set lock poisoned");
                return;
            }
        }

        let result = self.connect_with_retry(addr);
        if let Ok(mut connecting) = self.connecting.lock() {
            connecting.remove(&addr);
        }

        match result {
            Ok(connection) => {
                if let Ok(mut connections) = self.connections.lock() {
                    if connections.contains_key(&addr) {
                        let _ = connection.disconnect("client duplicate session");
                        return;
                    }
                    if let Some(existing) = connections
                        .values()
                        .find(|existing| existing.hostname() == connection.hostname())
                    {
                        logger::transport_info(
                            connection.protocol(),
                            format!(
                                "event=duplicate_host_ignored addr={} hostname={} existing_addr={}",
                                connection.addr(),
                                connection.hostname(),
                                existing.addr()
                            ),
                        );
                        let _ = connection.disconnect("client duplicate hostname");
                        if let Ok(mut rows) = self.cache.lock() {
                            rows.remove(&format!(
                                "{}-{}",
                                connection.addr().ip(),
                                connection.addr().port()
                            ));
                        }
                        return;
                    }
                    if connections.len() >= self.max_connections {
                        let _ = connection.disconnect("client max connections reached");
                        logger::transport_warn(
                            connection.protocol(),
                            format!(
                                "event=max_connections_reached addr={} max_connections={}",
                                addr, self.max_connections
                            ),
                        );
                        return;
                    }
                    connections.insert(addr, Arc::clone(&connection));
                } else {
                    logger::warn("connection pool lock poisoned");
                    let _ = connection.disconnect("client connection pool lock poisoned");
                    return;
                }
                logger::transport_info(
                    connection.protocol(),
                    format!(
                        "event=connected addr={} hostname={} gres_num={} connection_count={}",
                        connection.addr(),
                        connection.hostname(),
                        connection.gres_num(),
                        connection.connection_count()
                    ),
                );
                self.upsert_connection_placeholder(index, &connection);
                self.spawn_heartbeat(connection);
            }
            Err(e) => logger::transport_warn(
                self.protocol(),
                format!("event=connect_failed addr={} error={}", addr, e),
            ),
        }
    }

    fn connect_with_retry(&self, addr: SocketAddr) -> Result<SharedServerConnection, String> {
        if self.protocol() == "udp" {
            return crate::udp_client::connect_node(
                addr,
                self.connection_idle_timeout,
                self.udp_mtu,
            )
            .map(|node| Arc::new(node) as SharedServerConnection)
            .map_err(|e| e.to_string());
        }

        if self.protocol() == "tcp" {
            return crate::tcp_client::connect_node(addr, self.connection_idle_timeout)
                .map(|node| Arc::new(node) as SharedServerConnection)
                .map_err(|e| e.to_string());
        }

        Err(format!("unsupported protocol: {}", self.protocol()))
    }

    fn protocol(&self) -> &'static str {
        match self.transport_protocol.as_str() {
            "udp" => "udp",
            "tcp" => "tcp",
            _ => "udp",
        }
    }

    fn upsert_connection_placeholder(&self, index: usize, connection: &SharedServerConnection) {
        if let Ok(mut rows) = self.cache.lock() {
            let addr = connection.addr();
            let key = format!("{}-{}", addr.ip(), addr.port());
            rows.entry(key)
                .and_modify(|entry| {
                    entry.connection_id = format!("conn-{:03}", index);
                    entry.hostname = connection.hostname();
                    entry.num = connection.gres_num();
                })
                .or_insert_with(|| crate::cache::ConnectionCacheEntry {
                    connection_id: format!("conn-{:03}", index),
                    hostname: connection.hostname(),
                    num: connection.gres_num(),
                    server_gres: Vec::new(),
                    record_timestamp: now_ms(),
                    addr,
                    last_snapshot: None,
                    last_error: None,
                    last_query_latency_us: None,
                });
        }
    }

    fn spawn_heartbeat(&self, connection: SharedServerConnection) {
        if !connection.wants_heartbeat() {
            return;
        }
        let interval = self.heartbeat_interval;
        if interval.is_zero() {
            return;
        }
        let cache = Arc::clone(&self.cache);
        let connections = Arc::clone(&self.connections);
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            if let Err(e) = connection.heartbeat() {
                connection.close();
                logger::transport_warn(
                    connection.protocol(),
                    format!(
                        "event=disconnected addr={} hostname={} error={}",
                        connection.addr(),
                        connection.hostname(),
                        e
                    ),
                );
                if let Ok(mut connections) = connections.lock() {
                    connections.remove(&connection.addr());
                }
                if let Ok(mut rows) = cache.lock() {
                    let connection_id = rows
                        .values()
                        .find(|entry| entry.addr == connection.addr())
                        .map(|entry| entry.connection_id.clone())
                        .unwrap_or_else(|| "conn-unknown".to_string());
                    crate::cache::mark_stale(
                        &mut rows,
                        connection_id,
                        connection.addr(),
                        connection.hostname(),
                        e,
                    );
                }
                break;
            }
        });
    }

    #[allow(dead_code)]
    pub fn shutdown(&self, reason: &str) {
        let connections = self
            .connections
            .lock()
            .map(|connections| connections.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        for connection in connections {
            logger::transport_info(
                connection.protocol(),
                format!(
                    "event=disconnect_send addr={} hostname={} reason={}",
                    connection.addr(),
                    connection.hostname(),
                    reason
                ),
            );
            if let Err(e) = connection.disconnect(reason) {
                logger::transport_warn(
                    connection.protocol(),
                    format!(
                        "event=disconnect_failed addr={} hostname={} error={}",
                        connection.addr(),
                        connection.hostname(),
                        e
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
    #[cfg(test)]
    if cmd == "TEST_GRES_SCHEMA" {
        return Ok("OK schema=gres\n".to_string());
    }

    #[cfg(test)]
    if cmd == "TEST_CACHE_KEYS" {
        let rows = state
            .cache
            .lock()
            .map_err(|_| "cache lock poisoned".to_string())?;
        let mut keys: Vec<_> = rows.keys().cloned().collect();
        keys.sort();
        return Ok(format!("{}\n", keys.join(",")));
    }

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
        state.protocol(),
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

fn refresh_stale_cache_for_query(state: &LocalApiState) -> Result<(), String> {
    let targets = stale_targets(&state.cache, state.cache_ttl_ms)?;
    if targets.is_empty() {
        return Ok(());
    }

    for target in targets {
        let connection = state
            .connections
            .lock()
            .map_err(|_| "connection pool lock poisoned".to_string())?
            .get(&target.addr)
            .cloned();
        let connection = match connection {
            Some(connection) => connection,
            None => {
                let node = discovery::DiscoveredNode {
                    hostname: target.hostname.clone(),
                    addr: target.addr,
                    ts_ms: now_ms(),
                };
                state.establish_connections(&[node]);
                state
                    .connections
                    .lock()
                    .map_err(|_| "connection pool lock poisoned".to_string())?
                    .get(&target.addr)
                    .cloned()
                    .ok_or_else(|| {
                        format!("no {} connection for {}", state.protocol(), target.addr)
                    })?
            }
        };

        let query_started = std::time::Instant::now();
        match connection.query(state.connection_idle_timeout) {
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
                connection.close();
                if target.had_snapshot {
                    logger::transport_warn(
                        connection.protocol(),
                        format!(
                            "event=disconnected addr={} hostname={} error={}",
                            target.addr, target.hostname, e
                        ),
                    );
                } else {
                    logger::transport_warn(
                        connection.protocol(),
                        format!(
                            "event=query_failed addr={} hostname={} error={}",
                            target.addr, target.hostname, e
                        ),
                    );
                }
                if let Ok(mut connections) = state.connections.lock() {
                    connections.remove(&target.addr);
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

#[derive(Debug)]
struct RefreshTarget {
    connection_id: String,
    hostname: String,
    addr: SocketAddr,
    had_snapshot: bool,
}

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
        })
        .collect())
}

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
    use crate::connection::ServerConnection;
    use common::{GresInfo, GresMemory, GresProcessInfo, GresUtilization, ServerGresSnapshot};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    fn sample_cache() -> SharedCache {
        let mut rows: CacheMap = HashMap::new();
        upsert_snapshot(
            &mut rows,
            "conn-001",
            "127.0.0.1:30000".parse().unwrap(),
            ServerGresSnapshot {
                hostname: "sample-node".to_string(),
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
                    processes: vec![GresProcessInfo {
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
            "udp".to_string(),
            0,
            "239.0.0.1:4000".to_string(),
            Duration::from_millis(1),
            Duration::from_secs(5),
            Duration::from_secs(10),
            2,
            Vec::new(),
        )
    }

    #[derive(Debug)]
    struct MockConnection {
        addr: SocketAddr,
        snapshot: ServerGresSnapshot,
    }

    impl ServerConnection for MockConnection {
        fn protocol(&self) -> &'static str {
            "tcp"
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }

        fn hostname(&self) -> String {
            self.snapshot.hostname.clone()
        }

        fn gres_num(&self) -> u8 {
            self.snapshot.gres.len() as u8
        }

        fn query(&self, _timeout: Duration) -> Result<ServerGresSnapshot, String> {
            Ok(self.snapshot.clone())
        }

        fn disconnect(&self, _reason: &str) -> Result<(), String> {
            Ok(())
        }

        fn close(&self) {}
    }

    #[test]
    fn query_response_includes_snapshot_node() {
        let response = handle_command("QUERY {}", &state(sample_cache())).expect("query response");
        assert!(response.contains("\"meta\""));
        assert!(response.contains("sample-node"));
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
    fn list_response_includes_snapshot_node() {
        let response = handle_command("LIST", &state(sample_cache())).expect("list response");
        assert!(response.contains("conn-001 sample-node 127.0.0.1:30000 "));
    }

    #[test]
    fn test_endpoint_reports_gres_schema() {
        let response =
            handle_command("TEST_GRES_SCHEMA", &state(sample_cache())).expect("test response");
        assert_eq!(response, "OK schema=gres\n");
    }

    #[test]
    fn test_endpoint_lists_cache_keys() {
        let response =
            handle_command("TEST_CACHE_KEYS", &state(sample_cache())).expect("test response");
        assert_eq!(response, "127.0.0.1-30000\n");
    }

    #[test]
    fn query_refreshes_stale_cache_through_connection_trait() {
        let addr: SocketAddr = "127.0.0.1:39001".parse().unwrap();
        let cache = Arc::new(Mutex::new(CacheMap::new()));
        {
            let mut rows = cache.lock().unwrap();
            rows.insert(
                "127.0.0.1-39001".to_string(),
                crate::cache::ConnectionCacheEntry {
                    connection_id: "conn-001".to_string(),
                    hostname: "mock-node".to_string(),
                    num: 0,
                    server_gres: Vec::new(),
                    record_timestamp: 0,
                    addr,
                    last_snapshot: None,
                    last_error: None,
                    last_query_latency_us: None,
                },
            );
        }
        let state = state(cache.clone());
        state.connections.lock().unwrap().insert(
            addr,
            Arc::new(MockConnection {
                addr,
                snapshot: ServerGresSnapshot {
                    hostname: "mock-node".to_string(),
                    driver_version: Some("test-driver".to_string()),
                    gres: (0..4)
                        .map(|index| GresInfo {
                            index,
                            name: format!("NVIDIA Test GPU {index}"),
                            temperature_c: Some(30 + index as u32),
                            uuid: Some(format!("GRES-MOCK-{index}")),
                            memory: GresMemory {
                                used_mb: 1024 + index as u64,
                                total_mb: 24 * 1024,
                            },
                            utilization: GresUtilization {
                                gres_percent: 50 + index,
                                memory_percent: 10,
                            },
                            processes: Vec::new(),
                        })
                        .collect(),
                },
            }),
        );

        let response = handle_command("QUERY {}", &state).expect("query response");
        let json: serde_json::Value = serde_json::from_str(response.trim()).expect("json");
        assert_eq!(json["nodes"][0]["hostname"], "mock-node");
        assert_eq!(json["nodes"][0]["num"], 4);
        assert_eq!(json["nodes"][0]["gres"].as_array().unwrap().len(), 4);
        assert_eq!(json["nodes"][0]["gres"][3]["mem_total_mb"], 24 * 1024);
    }
}
