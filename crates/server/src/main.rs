use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    path::{Path, PathBuf},
    process,
    sync::{Arc, RwLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use common::{Config, ErrorCode};
use serde_json::json;

const DEFAULT_CONFIG_PATH: &str = "/etc/gpustat4cluster/config.toml";
const CONFIG_PATH_ENV: &str = "GPUSTAT4CLUSTER_CONFIG";
const NVML_MISSING_ENV: &str = "GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING";
const QUERY_LISTEN_ENV: &str = "GPUSTAT4CLUSTER_QUERY_ADDR";
const DEFAULT_QUERY_ADDR: &str = "127.0.0.1:4522";

#[derive(Debug)]
struct StartupError {
    code: ErrorCode,
    message: String,
}

impl StartupError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    timestamp_ms: i64,
    payload: Arc<Vec<u8>>,
    collected_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self, ttl_ms: u64, now: Instant) -> bool {
        now.duration_since(self.collected_at).as_millis() as u64 >= ttl_ms
    }
}

#[derive(Debug, Clone)]
struct GpuSample {
    gpu_num: u8,
    avg_utilization: u8,
}

trait GpuCollector: Send + Sync {
    fn collect(&self) -> Result<GpuSample, ErrorCode>;
}

struct MockCollector;

impl GpuCollector for MockCollector {
    fn collect(&self) -> Result<GpuSample, ErrorCode> {
        Ok(GpuSample {
            gpu_num: 0,
            avg_utilization: 0,
        })
    }
}

struct NvmlCollector;

impl NvmlCollector {
    fn new() -> Result<Self, ErrorCode> {
        if std::env::var(NVML_MISSING_ENV)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return Err(ErrorCode::NvmlUnavailable);
        }
        Ok(Self)
    }
}

impl GpuCollector for NvmlCollector {
    fn collect(&self) -> Result<GpuSample, ErrorCode> {
        // Round1: 最小可查询占位实现（后续接入真实 nvml_wrapper）
        Ok(GpuSample {
            gpu_num: 1,
            avg_utilization: 42,
        })
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!(
            "{}",
            json!({"level":"FATAL","code":err.code.to_string(),"message":err.message})
        );
        process::exit(1);
    }
}

fn run() -> Result<(), StartupError> {
    let config_path = get_config_path();
    let config = load_config(&config_path)?;

    validate_port_range(config.connecting.port_range)?;
    let multicast_addr = validate_multicast_addr(&config.connecting.multicast_addr)?;
    let bind_port = pick_bind_port(config.connecting.port_range)?;

    let (collector, degraded) = match NvmlCollector::new() {
        Ok(c) => (Arc::new(c) as Arc<dyn GpuCollector>, false),
        Err(code) => {
            eprintln!(
                "{}",
                json!({"level":"WARN","code":code.to_string(),"message":"collector init failed; entering degraded mode"})
            );
            (Arc::new(MockCollector) as Arc<dyn GpuCollector>, true)
        }
    };

    let cache: Arc<RwLock<Option<CacheEntry>>> = Arc::new(RwLock::new(None));
    let ttl_ms = config.services.cache_ttl_ms.max(1) as u64;
    let query_addr = std::env::var(QUERY_LISTEN_ENV).unwrap_or_else(|_| DEFAULT_QUERY_ADDR.to_string());

    println!(
        "{}",
        json!({
            "level":"INFO",
            "event":"bootstrap",
            "config":config_path.display().to_string(),
            "bind_port":bind_port,
            "multicast":multicast_addr.to_string(),
            "degraded":degraded,
            "query_addr":query_addr,
        })
    );

    start_runtime_loops(bind_port, multicast_addr, collector, cache, ttl_ms, query_addr)
}

fn get_config_path() -> PathBuf {
    std::env::var(CONFIG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH))
}

fn load_config(path: &Path) -> Result<Config, StartupError> {
    let raw = fs::read_to_string(path).map_err(|e| {
        StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("read config failed at {}: {}", path.display(), e),
        )
    })?;

    toml::from_str(&raw).map_err(|e| {
        StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("parse config failed at {}: {}", path.display(), e),
        )
    })
}

fn validate_port_range([start, end]: [u16; 2]) -> Result<(), StartupError> {
    if start == 0 || end == 0 || start > end {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid port_range [{}, {}]", start, end),
        ));
    }

    Ok(())
}

fn validate_multicast_addr(raw: &str) -> Result<SocketAddr, StartupError> {
    let mut addrs = raw.to_socket_addrs().map_err(|e| {
        StartupError::new(
            ErrorCode::MulticastFailed,
            format!("invalid multicast_addr '{}': {}", raw, e),
        )
    })?;

    let addr = addrs.next().ok_or_else(|| {
        StartupError::new(
            ErrorCode::MulticastFailed,
            format!("invalid multicast_addr '{}': no resolved address", raw),
        )
    })?;

    if !addr.ip().is_multicast() {
        return Err(StartupError::new(
            ErrorCode::MulticastFailed,
            format!("invalid multicast_addr '{}': IP is not multicast", raw),
        ));
    }

    Ok(addr)
}

fn pick_bind_port([start, end]: [u16; 2]) -> Result<u16, StartupError> {
    for port in start..=end {
        if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }

    Err(StartupError::new(
        ErrorCode::PortExhausted,
        format!("all ports exhausted in range [{}, {}]", start, end),
    ))
}

fn start_runtime_loops(
    bind_port: u16,
    multicast: SocketAddr,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<RwLock<Option<CacheEntry>>>,
    ttl_ms: u64,
    query_addr: String,
) -> Result<(), StartupError> {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string());
    let local_ip = infer_local_ip();

    let hb = thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        println!("{}", json!({"level":"INFO","event":"heartbeat","status":"alive"}));
    });

    let ann = thread::spawn(move || announce_loop(multicast, hostname, local_ip, bind_port));

    let q_collector = Arc::clone(&collector);
    let q_cache = Arc::clone(&cache);
    let query = thread::spawn(move || query_server_loop(&query_addr, q_collector, q_cache, ttl_ms));

    let _ = hb.join();
    let _ = ann.join();
    let _ = query.join();
    Ok(())
}

fn announce_loop(multicast: SocketAddr, hostname: String, ip: String, port: u16) {
    let sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "{}",
                json!({"level":"WARN","code":ErrorCode::MulticastFailed.to_string(),"message":format!("announce bind failed: {}",e)})
            );
            return;
        }
    };

    loop {
        let msg = json!({
            "hostname": hostname,
            "ip": ip,
            "port": port,
            "ts": now_unix_ms()
        })
        .to_string();

        if let Err(e) = sock.send_to(msg.as_bytes(), multicast) {
            eprintln!(
                "{}",
                json!({"level":"WARN","code":ErrorCode::MulticastFailed.to_string(),"message":format!("announce send failed: {}",e)})
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn query_server_loop(
    listen_addr: &str,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<RwLock<Option<CacheEntry>>>,
    ttl_ms: u64,
) {
    let listener = match TcpListener::bind(listen_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}", json!({"level":"WARN","code":ErrorCode::KcpInitFailed.to_string(),"message":format!("query bind failed on {}: {}", listen_addr, e)}));
            return;
        }
    };

    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => handle_query_stream(&mut stream, &collector, &cache, ttl_ms),
            Err(e) => eprintln!("{}", json!({"level":"WARN","code":ErrorCode::Internal.to_string(),"message":format!("accept failed: {}", e)})),
        }
    }
}

fn handle_query_stream(
    stream: &mut TcpStream,
    collector: &Arc<dyn GpuCollector>,
    cache: &Arc<RwLock<Option<CacheEntry>>>,
    ttl_ms: u64,
) {
    let mut buf = [0u8; 16];
    let _ = stream.read(&mut buf);

    match get_or_refresh_cache(cache, collector.as_ref(), ttl_ms) {
        Ok(entry) => {
            let body = json!({
                "ok": true,
                "timestamp_ms": entry.timestamp_ms,
                "payload": String::from_utf8_lossy(entry.payload.as_slice()),
            })
            .to_string();
            let _ = stream.write_all(body.as_bytes());
        }
        Err(code) => {
            let body = json!({"ok":false,"error_code":code.to_string(),"message":"collector unavailable in degraded mode"}).to_string();
            let _ = stream.write_all(body.as_bytes());
        }
    }
}

fn get_or_refresh_cache(
    cache: &Arc<RwLock<Option<CacheEntry>>>,
    collector: &dyn GpuCollector,
    ttl_ms: u64,
) -> Result<CacheEntry, ErrorCode> {
    let now = Instant::now();
    if let Some(hit) = cache.read().ok().and_then(|g| (*g).clone()) {
        if !hit.is_expired(ttl_ms, now) {
            return Ok(hit);
        }
    }

    let sample = collector.collect()?;
    let ts = now_unix_ms();
    let payload = json!({"gpu_num":sample.gpu_num,"avg_utilization":sample.avg_utilization}).to_string().into_bytes();
    let fresh = CacheEntry {
        timestamp_ms: ts,
        payload: Arc::new(payload),
        collected_at: Instant::now(),
    };

    if let Ok(mut w) = cache.write() {
        *w = Some(fresh.clone());
    }
    Ok(fresh)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn infer_local_ip() -> String {
    let sock = match UdpSocket::bind(("0.0.0.0", 0)) {
        Ok(s) => s,
        Err(_) => return "127.0.0.1".to_string(),
    };

    if sock.connect("8.8.8.8:80").is_ok() {
        if let Ok(addr) = sock.local_addr() {
            return addr.ip().to_string();
        }
    }

    "127.0.0.1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysFailCollector;
    impl GpuCollector for AlwaysFailCollector {
        fn collect(&self) -> Result<GpuSample, ErrorCode> {
            Err(ErrorCode::NvmlUnavailable)
        }
    }

    #[test]
    fn ttl_cache_hit_then_refresh() {
        let cache: Arc<RwLock<Option<CacheEntry>>> = Arc::new(RwLock::new(None));
        let collector = MockCollector;

        let first = get_or_refresh_cache(&cache, &collector, 50).unwrap();
        let second = get_or_refresh_cache(&cache, &collector, 50).unwrap();
        assert_eq!(first.timestamp_ms, second.timestamp_ms);

        thread::sleep(Duration::from_millis(60));
        let third = get_or_refresh_cache(&cache, &collector, 50).unwrap();
        assert!(third.timestamp_ms >= second.timestamp_ms);
    }

    #[test]
    fn degraded_collector_returns_explainable_error() {
        let cache: Arc<RwLock<Option<CacheEntry>>> = Arc::new(RwLock::new(None));
        let collector = AlwaysFailCollector;
        let err = get_or_refresh_cache(&cache, &collector, 10).unwrap_err();
        assert_eq!(err, ErrorCode::NvmlUnavailable);
    }
}
