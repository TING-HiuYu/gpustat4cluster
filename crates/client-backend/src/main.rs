use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use common::{Config, DiscoveryAnnounce, DiscoveryQuery};
use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG_PATH: &str = "/etc/gpustat4cluster/config.toml";
const CONFIG_PATH_ENV: &str = "GPUSTAT4CLUSTER_CONFIG";
const LOCAL_LISTEN_ADDR: &str = "127.0.0.1:4521";
const QUERY_VERSION: u8 = 1;

#[derive(Debug, Clone)]
struct ConnectionCacheEntry {
    connection_id: String,
    hostname: String,
    num: u8,
    server_gpus: Vec<u8>,
    record_timestamp: i64,
    addr: SocketAddr,
}

#[derive(Debug, Clone, Deserialize)]
struct QueryRequest {
    filter: Option<String>,
    user: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct QueryResponse {
    nodes: Vec<NodeView>,
}

#[derive(Debug, Clone, Serialize)]
struct NodeView {
    connection_id: String,
    hostname: String,
    addr: String,
    timestamp_ms: i64,
    num: u8,
    gpus: Vec<GpuView>,
}

#[derive(Debug, Clone, Serialize)]
struct GpuView {
    index: u8,
    util: u8,
    mem_used_mb: u32,
    mem_total_mb: u32,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("[client-backend][fatal] {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cfg = load_config(&get_config_path())?;
    let discover_wait = Duration::from_secs(cfg.connecting.discover_wait_secs);
    let discovered = match discover_nodes(&cfg.connecting.multicast_addr, discover_wait) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[client-backend][warn] discovery failed: {}", e);
            Vec::new()
        }
    };

    let cache_map = build_cache(discovered);
    let shared = Arc::new(Mutex::new(cache_map));
    let listener = TcpListener::bind(LOCAL_LISTEN_ADDR)
        .map_err(|e| format!("bind {} failed: {}", LOCAL_LISTEN_ADDR, e))?;

    println!("client-backend listening on {}", LOCAL_LISTEN_ADDR);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cache = Arc::clone(&shared);
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, cache) {
                        eprintln!("[client-backend][warn] {}", e);
                    }
                });
            }
            Err(e) => eprintln!("[client-backend][warn] accept failed: {}", e),
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, cache: Arc<Mutex<HashMap<String, ConnectionCacheEntry>>>) -> Result<(), String> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .map_err(|e| format!("read request failed: {}", e))?;
    }

    let cmd = line.trim();
    if cmd == "LIST" {
        let rows = cache.lock().map_err(|_| "cache lock poisoned".to_string())?;
        let mut entries: Vec<_> = rows.values().collect();
        entries.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        for n in entries {
            writeln!(stream, "{} {} {} {}", n.connection_id, n.hostname, n.addr, n.record_timestamp)
                .map_err(|e| format!("write response failed: {}", e))?;
        }
        return Ok(());
    }

    if let Some(payload) = cmd.strip_prefix("QUERY") {
        let req = parse_query_request(payload.trim())?;
        let rows = cache.lock().map_err(|_| "cache lock poisoned".to_string())?;
        let resp = build_query_response(&rows, req.filter.as_deref(), req.user.as_deref());
        let json = serde_json::to_string(&resp).map_err(|e| format!("encode response failed: {}", e))?;
        writeln!(stream, "{}", json).map_err(|e| format!("write response failed: {}", e))?;
        return Ok(());
    }

    writeln!(stream, "ERR unsupported command: {}", cmd)
        .map_err(|e| format!("write error failed: {}", e))
}

fn parse_query_request(s: &str) -> Result<QueryRequest, String> {
    if s.is_empty() {
        return Ok(QueryRequest { filter: None, user: None });
    }
    serde_json::from_str::<QueryRequest>(s).map_err(|e| format!("invalid QUERY payload: {}", e))
}

fn build_query_response(
    rows: &HashMap<String, ConnectionCacheEntry>,
    filter: Option<&str>,
    _user: Option<&str>,
) -> QueryResponse {
    let mut nodes: Vec<_> = rows
        .values()
        .filter(|entry| match filter {
            Some(f) if !f.is_empty() => {
                entry.hostname.contains(f)
                    || entry.addr.ip().to_string().contains(f)
                    || entry.connection_id.contains(f)
            }
            _ => true,
        })
        .map(|entry| NodeView {
            connection_id: entry.connection_id.clone(),
            hostname: entry.hostname.clone(),
            addr: entry.addr.to_string(),
            timestamp_ms: entry.record_timestamp,
            num: entry.num,
            gpus: parse_gpu_views(&entry.server_gpus),
        })
        .collect();
    nodes.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
    QueryResponse { nodes }
}

fn parse_gpu_views(raw: &[u8]) -> Vec<GpuView> {
    let s = String::from_utf8_lossy(raw);
    s.split(';')
        .filter_map(|item| {
            let parts: Vec<_> = item.split(',').collect();
            if parts.len() != 4 {
                return None;
            }
            Some(GpuView {
                index: parts[0].parse().ok()?,
                util: parts[1].parse().ok()?,
                mem_used_mb: parts[2].parse().ok()?,
                mem_total_mb: parts[3].parse().ok()?,
            })
        })
        .collect()
}

fn build_cache(discovered: Vec<DiscoveredNode>) -> HashMap<String, ConnectionCacheEntry> {
    discovered
        .into_iter()
        .enumerate()
        .map(|(idx, n)| {
            let num = 2u8;
            let server_gpus = b"0,35,2048,8192;1,76,6144,8192".to_vec();
            let key = format!("{}-{}", n.addr.ip(), n.addr.port());
            (
                key.clone(),
                ConnectionCacheEntry {
                    connection_id: format!("conn-{:03}", idx + 1),
                    hostname: n.hostname,
                    num,
                    server_gpus,
                    record_timestamp: n.ts_ms,
                    addr: n.addr,
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
struct DiscoveredNode {
    hostname: String,
    addr: SocketAddr,
    ts_ms: i64,
}

fn discover_nodes(multicast_addr: &str, wait: Duration) -> Result<Vec<DiscoveredNode>, String> {
    let target = resolve_addr(multicast_addr)?;
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind failed: {}", e))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("set read timeout failed: {}", e))?;

    let query = serde_json::to_vec(&DiscoveryQuery { version: QUERY_VERSION })
        .map_err(|e| format!("encode discovery query failed: {}", e))?;
    socket
        .send_to(&query, target)
        .map_err(|e| format!("send query to {} failed: {}", target, e))?;

    let deadline = std::time::Instant::now() + wait;
    let mut buf = [0u8; 2048];
    let mut map: HashMap<String, DiscoveredNode> = HashMap::new();

    while std::time::Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let msg = String::from_utf8_lossy(&buf[..n]);
                if let Some(node) = parse_announce(&msg, from) {
                    map.insert(node.addr.to_string(), node);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("recv announce failed: {}", e)),
        }
    }

    let mut items: Vec<_> = map.into_values().collect();
    items.sort_by(|a, b| a.addr.to_string().cmp(&b.addr.to_string()));
    Ok(items)
}

fn parse_announce(msg: &str, src: SocketAddr) -> Option<DiscoveredNode> {
    let ann: DiscoveryAnnounce = serde_json::from_str(msg).ok()?;
    if ann.version != QUERY_VERSION {
        return None;
    }

    Some(DiscoveredNode {
        hostname: ann.hostname,
        addr: SocketAddr::new(src.ip(), ann.port),
        ts_ms: now_ms(),
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn resolve_addr(raw: &str) -> Result<SocketAddr, String> {
    raw.to_socket_addrs()
        .map_err(|e| format!("resolve multicast_addr '{}' failed: {}", raw, e))?
        .next()
        .ok_or_else(|| format!("no resolved address for '{}'", raw))
}

fn get_config_path() -> PathBuf {
    std::env::var(CONFIG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH))
}

fn load_config(path: &Path) -> Result<Config, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("read config failed at {}: {}", path.display(), e))?;
    toml::from_str(&raw).map_err(|e| format!("parse config failed at {}: {}", path.display(), e))
}
