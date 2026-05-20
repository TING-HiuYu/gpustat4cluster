mod cache;
mod collector;
#[cfg(feature = "kcp-transport")]
mod kcp_transport;
mod model;
#[cfg(any(feature = "kcp-transport", test))]
mod transport;

#[cfg(feature = "kcp-transport")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
    path::{Path, PathBuf},
    process,
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use cache::GpuCache;
use chrono::Local;
#[cfg(feature = "mock-nvml")]
use collector::MockNvmlCollector;
use collector::{mock_nvml_requested_from_env, GpuCollector, NvmlCollector};
use common::{Config, DiscoveryAnnounce, DiscoveryQuery, ErrorCode, PROTOCOL_VERSION};
use serde_json::json;
use serde_json::Value;
use socket2::{Domain, Protocol, Socket, Type};

const DEFAULT_CONFIG_PATH: &str = "/etc/gpustat4cluster/server.toml";
const CONFIG_PATH_ENV: &str = "GPUSTAT4CLUSTER_CONFIG";
const QUERY_LISTEN_ENV: &str = "GPUSTAT4CLUSTER_QUERY_ADDR";
const DEFAULT_QUERY_ADDR: &str = "127.0.0.1:4522";

#[derive(Debug)]
struct StartupError {
    code: ErrorCode,
    message: String,
    hint: Option<String>,
}

impl StartupError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    fn log(&self) {
        let config_path = get_config_path();
        let mut value = json!({
            "level": "FATAL",
            "event": "startup_error",
            "code": self.code.to_string(),
            "code_num": self.code.code(),
            "message": self.message,
            "config": config_path.display().to_string(),
        });
        if let Some(hint) = &self.hint {
            if let Some(map) = value.as_object_mut() {
                map.insert("hint".to_string(), Value::String(hint.clone()));
            }
        }
        log_json_stderr(value);
    }
}

fn main() {
    if let Err(err) = run() {
        err.log();
        process::exit(1);
    }
}

fn log_time() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn add_log_time(value: &mut Value) {
    if let Some(map) = value.as_object_mut() {
        map.insert("time".to_string(), Value::String(log_time()));
    }
}

fn log_json_stdout(mut value: Value) {
    add_log_time(&mut value);
    println!("{}", value);
}

fn log_json_stderr(mut value: Value) {
    add_log_time(&mut value);
    eprintln!("{}", value);
}

fn run() -> Result<(), StartupError> {
    let config_path = get_config_path();
    let config = load_config(&config_path)?;
    let kcp_enabled = kcp_enabled_from_config(&config);

    let multicast_addr = validate_multicast_addr(&config.connecting.multicast_addr)?;
    let multicast_outbound_ip =
        validate_multicast_outbound_ips(&config.connecting.multicast_outbound_ip)?;
    let kcp_port = pick_udp_port(config.connecting.port_range, config.connecting.kcp_port)?;
    let tcp_port = pick_tcp_port(
        config.connecting.port_range,
        config.connecting.tcp_port,
        Some(kcp_port),
    )?;

    let hostname = detect_hostname();

    let (collector, degraded, collector_mode) = build_collector(&hostname, &config)?;

    let cache = Arc::new(GpuCache::new());
    let ttl_ms = config.services.cache_ttl_ms;
    let collector_interval_ms = config.services.collector_interval_ms;
    let _query_addr =
        std::env::var(QUERY_LISTEN_ENV).unwrap_or_else(|_| DEFAULT_QUERY_ADDR.to_string());
    let metrics = cache.metrics();
    #[cfg(feature = "kcp-transport")]
    if kcp_enabled {
        install_signal_handler();
    }

    log_json_stdout(json!({
        "level":"INFO",
        "event":"startup",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        "config":config_path.display().to_string(),
        "hostname": hostname.clone(),
        "kcp_port":kcp_port,
        "tcp_port":tcp_port,
        "protocols": ["kcp", "tcp"],
        "kcp_enabled": kcp_enabled,
        "multicast":multicast_addr.to_string(),
        "multicast_outbound_ip": multicast_outbound_ip.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
        "degraded":degraded,
        "collector_mode":collector_mode,
        "cache_ttl_ms": ttl_ms,
        "collector_interval_ms": collector_interval_ms,
        "heartbeat_interval": config.connecting.heartbeat_interval,
        "connection_idle_timeout": config.connecting.connection_idle_timeout,
        "max_connections": config.connecting.max_connections,
        "kcp_retry_limit": config.connecting.kcp_retry_limit,
        "tcp_addr":format!("0.0.0.0:{tcp_port}"),
        "metrics": {
            "cache_hits": metrics.cache_hits,
            "cache_misses": metrics.cache_misses,
            "merge_count": metrics.merge_count,
            "collect_count": metrics.collect_count,
            "avg_collect_latency_us": metrics.avg_collect_latency_us,
            "collect_latency_p50_us": metrics.collect_latency_p50_us,
            "collect_latency_p95_us": metrics.collect_latency_p95_us,
            "cache_hit_rate_bps": metrics.cache_hit_rate_bps,
            "cache_miss_rate_bps": metrics.cache_miss_rate_bps,
            "merge_ratio_bps": metrics.merge_ratio_bps,
        }
    }));

    start_runtime_loops(
        kcp_port,
        tcp_port,
        multicast_addr,
        hostname,
        collector,
        cache,
        ttl_ms,
        collector_interval_ms,
        config.connecting.multicast_retry_limit,
        config.connecting.connection_idle_timeout,
        config.connecting.max_connections,
        multicast_outbound_ip,
        kcp_enabled,
    )
}

fn build_collector(
    hostname: &str,
    config: &Config,
) -> Result<(Arc<dyn GpuCollector>, bool, &'static str), StartupError> {
    #[cfg(feature = "mock-nvml")]
    if mock_nvml_requested_from_env() {
        return Ok((
            Arc::new(MockNvmlCollector::from_env(hostname.to_string())) as Arc<dyn GpuCollector>,
            false,
            "mock-nvml",
        ));
    }

    #[cfg(not(feature = "mock-nvml"))]
    let _mock_requested_but_not_enabled = mock_nvml_requested_from_env();

    NvmlCollector::new(
        hostname.to_string(),
        config.runtime.nvml_lib_path.as_deref(),
    )
    .map(|c| (Arc::new(c) as Arc<dyn GpuCollector>, false, "nvml"))
    .map_err(|code| {
        StartupError::new(
            code,
            nvml_startup_error_message(config.runtime.nvml_lib_path.as_deref()),
        )
        .with_hint(nvml_startup_hint(config.runtime.nvml_lib_path.as_deref()))
    })
}

fn nvml_startup_error_message(nvml_lib_path: Option<&str>) -> String {
    match nvml_lib_path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => format!(
            "NVML collector init failed while loading configured runtime.nvml_lib_path='{}'",
            path
        ),
        None => {
            "NVML collector init failed while loading default libnvidia-ml.so; configure [runtime].nvml_lib_path if this host only provides a versioned NVML library".to_string()
        }
    }
}

fn nvml_startup_hint(nvml_lib_path: Option<&str>) -> String {
    match nvml_lib_path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(_) => concat!(
            "Verify the configured NVML library path exists, is readable by the gpustat4cluster service user, ",
            "and is a real NVIDIA runtime library, not the CUDA stubs library."
        )
        .to_string(),
        None => concat!(
            "This host may only provide a versioned NVML library such as ",
            "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1. Set [runtime].nvml_lib_path in ",
            "/etc/gpustat4cluster/server.toml, then run: systemctl reset-failed gpustat4cluster-server && ",
            "systemctl restart gpustat4cluster-server. Do not use CUDA stubs/libnvidia-ml.so."
        )
        .to_string(),
    }
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

    let config: Config = toml::from_str(&raw).map_err(|e| {
        StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("parse config failed at {}: {}", path.display(), e),
        )
    })?;

    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<(), StartupError> {
    validate_port_range(config.connecting.port_range)?;
    validate_multicast_addr(&config.connecting.multicast_addr)?;
    validate_protocol(&config.connecting.protocol)?;
    validate_positive("cache_ttl_ms", config.services.cache_ttl_ms)?;
    validate_positive(
        "collector_interval_ms",
        config.services.collector_interval_ms,
    )?;
    validate_positive("heartbeat_interval", config.connecting.heartbeat_interval)?;
    validate_positive(
        "connection_idle_timeout",
        config.connecting.connection_idle_timeout,
    )?;
    validate_positive("max_connections", config.connecting.max_connections as u64)?;
    validate_positive("kcp_retry_limit", config.connecting.kcp_retry_limit as u64)?;
    validate_positive("discover_wait_secs", config.connecting.discover_wait_secs)?;
    validate_positive(
        "multicast_retry_limit",
        config.connecting.multicast_retry_limit as u64,
    )?;
    validate_multicast_outbound_ips(&config.connecting.multicast_outbound_ip)?;
    parse_log_size_bytes(&config.log.max_size)?;
    Ok(())
}

fn validate_protocol(raw: &str) -> Result<(), StartupError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "kcp" | "tcp" => Ok(()),
        other => Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            format!(
                "invalid connecting.protocol '{}': expected 'kcp' or 'tcp'",
                other
            ),
        )),
    }
}

fn validate_positive(name: &str, value: u64) -> Result<(), StartupError> {
    if value == 0 {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid {name}: must be > 0"),
        ));
    }

    Ok(())
}

fn parse_log_size_bytes(raw: &str) -> Result<u64, StartupError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            "invalid log.max_size: empty value",
        ));
    }

    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split_at);
    if number.is_empty() {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid log.max_size '{}': missing number", raw),
        ));
    }

    let value = number.parse::<u64>().map_err(|e| {
        StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid log.max_size '{}': {}", raw, e),
        )
    })?;
    if value == 0 {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            "invalid log.max_size: must be > 0",
        ));
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" | "kib" => 1024,
        "mb" | "mib" => 1024 * 1024,
        "gb" | "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(StartupError::new(
                ErrorCode::ConfigInvalid,
                format!(
                    "invalid log.max_size '{}': unsupported unit '{}'",
                    raw, other
                ),
            ));
        }
    };

    value.checked_mul(multiplier).ok_or_else(|| {
        StartupError::new(
            ErrorCode::ConfigInvalid,
            format!("invalid log.max_size '{}': value too large", raw),
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

fn validate_multicast_outbound_ips(raw: &[String]) -> Result<Vec<Ipv4Addr>, StartupError> {
    raw.iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<Ipv4Addr>().map_err(|e| {
                StartupError::new(
                    ErrorCode::ConfigInvalid,
                    format!(
                        "invalid connecting.multicast_outbound_ip entry '{}': expected IPv4 address, got {}",
                        item, e
                    ),
                )
            })
        })
        .collect()
}

fn pick_udp_port(range: [u16; 2], requested: u16) -> Result<u16, StartupError> {
    if requested != 0 {
        if UdpSocket::bind(("0.0.0.0", requested)).is_ok() {
            return Ok(requested);
        }
        return Err(StartupError::new(
            ErrorCode::PortExhausted,
            format!("configured kcp_port {} is not available", requested),
        ));
    }
    let [start, end] = range;
    for port in port_candidates(range) {
        if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }

    Err(StartupError::new(
        ErrorCode::PortExhausted,
        format!("all UDP ports exhausted in range [{}, {}]", start, end),
    ))
}

fn pick_tcp_port(
    range: [u16; 2],
    requested: u16,
    exclude: Option<u16>,
) -> Result<u16, StartupError> {
    if requested != 0 {
        if Some(requested) == exclude {
            return Err(StartupError::new(
                ErrorCode::PortExhausted,
                format!("configured tcp_port {} conflicts with kcp_port", requested),
            ));
        }
        if TcpListener::bind(("0.0.0.0", requested)).is_ok() {
            return Ok(requested);
        }
        return Err(StartupError::new(
            ErrorCode::PortExhausted,
            format!("configured tcp_port {} is not available", requested),
        ));
    }
    let [start, end] = range;
    for port in port_candidates(range) {
        if Some(port) == exclude {
            continue;
        }
        if TcpListener::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }

    Err(StartupError::new(
        ErrorCode::PortExhausted,
        format!("all TCP ports exhausted in range [{}, {}]", start, end),
    ))
}

fn port_candidates([start, end]: [u16; 2]) -> Vec<u16> {
    let count = u32::from(end - start) + 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let offset = nanos % count;
    (0..count)
        .map(|idx| start + ((offset + idx) % count) as u16)
        .collect()
}

fn start_runtime_loops(
    kcp_port: u16,
    tcp_port: u16,
    multicast: SocketAddr,
    hostname: String,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<GpuCache>,
    ttl_ms: u64,
    collector_interval_ms: u64,
    multicast_retry_limit: u32,
    connection_idle_timeout_secs: u64,
    #[cfg_attr(not(feature = "kcp-transport"), allow(unused_variables))] max_connections: usize,
    multicast_outbound_ip: Vec<Ipv4Addr>,
    #[cfg_attr(not(feature = "kcp-transport"), allow(unused_variables))] kcp_enabled: bool,
) -> Result<(), StartupError> {
    let local_ip = multicast_outbound_ip
        .first()
        .map(|ip| ip.to_string())
        .unwrap_or_else(infer_local_ip);

    #[cfg(feature = "kcp-transport")]
    let kcp = maybe_spawn_kcp_server(
        kcp_enabled,
        kcp_port,
        Arc::clone(&collector),
        Arc::clone(&cache),
        ttl_ms,
        connection_idle_timeout_secs,
        max_connections,
    );

    let refresh_collector = Arc::clone(&collector);
    let refresh_cache = Arc::clone(&cache);
    let refresh = thread::spawn(move || {
        background_refresh_loop(
            refresh_collector,
            refresh_cache,
            ttl_ms,
            collector_interval_ms,
        )
    });

    let discovery_hostname = hostname.clone();
    let discovery_ip = local_ip.clone();
    let discovery_outbound_ip = multicast_outbound_ip.clone();
    let discovery = thread::spawn(move || {
        discovery_query_loop(
            multicast,
            discovery_hostname,
            discovery_ip,
            kcp_port,
            tcp_port,
            discovery_outbound_ip,
        )
    });

    let ann_hostname = hostname;
    let ann_ip = local_ip;
    let ann_outbound_ip = multicast_outbound_ip;
    let ann = thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        announce_startup_once(
            multicast,
            ann_hostname,
            ann_ip,
            kcp_port,
            tcp_port,
            multicast_retry_limit,
            ann_outbound_ip,
        )
    });

    let q_collector = Arc::clone(&collector);
    let q_cache = Arc::clone(&cache);
    let query = thread::spawn(move || {
        query_server_loop(
            &format!("0.0.0.0:{tcp_port}"),
            q_collector,
            q_cache,
            ttl_ms,
            Duration::from_secs(connection_idle_timeout_secs),
        )
    });

    let _ = ann.join();
    let _ = discovery.join();
    #[cfg(feature = "kcp-transport")]
    if let Some(kcp) = kcp {
        let _ = kcp.join();
    }
    let _ = refresh.join();
    let _ = query.join();
    Ok(())
}

fn background_refresh_loop(
    collector: Arc<dyn GpuCollector>,
    cache: Arc<GpuCache>,
    ttl_ms: u64,
    collector_interval_ms: u64,
) {
    let interval = Duration::from_millis(collector_interval_ms.max(1));
    loop {
        let started = std::time::Instant::now();
        if let Err(code) = cache.get_or_refresh(collector.as_ref(), ttl_ms) {
            log_json_stderr(
                json!({"level":"WARN","event":"collector_refresh_error","code":code.to_string(),"message":"background collector refresh failed"}),
            );
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        } else {
            thread::yield_now();
        }
    }
}

#[cfg(feature = "kcp-transport")]
fn kcp_enabled_from_config(_config: &Config) -> bool {
    true
}

#[cfg(not(feature = "kcp-transport"))]
fn kcp_enabled_from_config(_config: &Config) -> bool {
    false
}

#[cfg(feature = "kcp-transport")]
fn maybe_spawn_kcp_server(
    kcp_enabled: bool,
    bind_port: u16,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<GpuCache>,
    ttl_ms: u64,
    connection_idle_timeout_secs: u64,
    max_connections: usize,
) -> Option<thread::JoinHandle<()>> {
    if !kcp_enabled {
        return None;
    }

    let hostname = detect_hostname();
    let context = Arc::new(transport::TransportContext::new(
        hostname, collector, cache, ttl_ms,
    ));
    let bind_addr = std::net::SocketAddr::from(([0, 0, 0, 0], bind_port));

    match kcp_transport::spawn_kcp_server(
        bind_addr,
        context,
        std::time::Duration::from_secs(connection_idle_timeout_secs),
        max_connections,
    ) {
        Ok(handle) => {
            log_json_stdout(
                json!({"level":"INFO","event":"kcp_start","addr":bind_addr.to_string()}),
            );
            Some(handle)
        }
        Err(err) => {
            log_json_stderr(
                json!({"level":"WARN","event":"kcp_error","code":ErrorCode::KcpInitFailed.to_string(),"message":format!("kcp spawn failed: {}", err)}),
            );
            None
        }
    }
}

#[cfg(feature = "kcp-transport")]
fn install_signal_handler() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    if let Err(err) = ctrlc::set_handler(move || {
        kcp_transport::disconnect_all_active_blocking("server graceful shutdown: signal");
        process::exit(0);
    }) {
        log_json_stderr(json!({
            "level":"WARN",
            "event":"signal_handler_error",
            "message": format!("signal handler disabled: {err}")
        }));
    }
}

fn discovery_announce(hostname: &str, ip: &str, kcp_port: u16, tcp_port: u16) -> DiscoveryAnnounce {
    DiscoveryAnnounce {
        version: PROTOCOL_VERSION,
        hostname: hostname.to_string(),
        ip: ip.to_string(),
        port: kcp_port,
        kcp_port: Some(kcp_port),
        tcp_port: Some(tcp_port),
        ttl: None,
        load: None,
        degraded: Some(false),
    }
}

fn announce_startup_once(
    multicast: SocketAddr,
    hostname: String,
    ip: String,
    kcp_port: u16,
    tcp_port: u16,
    retry_limit: u32,
    multicast_outbound_ip: Vec<Ipv4Addr>,
) {
    let interfaces: Vec<Option<Ipv4Addr>> = if multicast_outbound_ip.is_empty() {
        vec![None]
    } else {
        multicast_outbound_ip.into_iter().map(Some).collect()
    };

    for attempt in 1..=retry_limit {
        let mut sent_any = false;
        for interface in &interfaces {
            let announce_ip = interface
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| ip.clone());
            let announce = discovery_announce(&hostname, &announce_ip, kcp_port, tcp_port);
            let msg = match serde_json::to_vec(&announce) {
                Ok(msg) => msg,
                Err(e) => {
                    log_json_stderr(
                        json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":format!("announce encode failed: {}",e)}),
                    );
                    return;
                }
            };
            let sock = match create_udp_socket(SocketAddr::from(([0, 0, 0, 0], 0)), *interface) {
                Ok(s) => s,
                Err(e) => {
                    log_json_stderr(
                        json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":multicast_route_hint(format!("announce bind failed: {}",e), &e),"attempt":attempt,"limit":retry_limit,"outbound_ip":interface.map(|ip| ip.to_string())}),
                    );
                    continue;
                }
            };
            match sock.send_to(&msg, multicast) {
                Ok(_) => {
                    sent_any = true;
                    log_json_stdout(
                        json!({"level":"INFO","event":"multicast_announce","target":multicast.to_string(),"attempt":attempt,"outbound_ip":interface.map(|ip| ip.to_string())}),
                    );
                }
                Err(e) => {
                    log_json_stderr(
                        json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":multicast_route_hint(format!("announce send failed: {}",e), &e),"attempt":attempt,"limit":retry_limit,"outbound_ip":interface.map(|ip| ip.to_string())}),
                    );
                }
            }
        }
        if sent_any {
            return;
        } else {
            log_json_stderr(
                json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":"announce send failed on every configured outbound IP","attempt":attempt,"limit":retry_limit}),
            );
            thread::sleep(Duration::from_millis(500));
        }
    }

    log_json_stderr(
        json!({"level":"WARN","event":"multicast_disabled","message":"startup announce retry limit reached","limit":retry_limit}),
    );
}

fn discovery_query_loop(
    multicast: SocketAddr,
    hostname: String,
    ip: String,
    kcp_port: u16,
    tcp_port: u16,
    multicast_outbound_ip: Vec<Ipv4Addr>,
) {
    let sock = match create_udp_socket(SocketAddr::from(([0, 0, 0, 0], multicast.port())), None) {
        Ok(s) => s,
        Err(e) => {
            log_json_stderr(
                json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":format!("discovery bind failed: {}",e)}),
            );
            return;
        }
    };

    if let std::net::IpAddr::V4(group) = multicast.ip() {
        let interfaces: Vec<Ipv4Addr> = if multicast_outbound_ip.is_empty() {
            vec![Ipv4Addr::UNSPECIFIED]
        } else {
            multicast_outbound_ip
        };
        let mut joined_any = false;
        for interface in interfaces {
            match sock.join_multicast_v4(&group, &interface) {
                Ok(_) => {
                    joined_any = true;
                    log_json_stdout(
                        json!({"level":"INFO","event":"multicast_join","addr":multicast.to_string(),"outbound_ip":interface.to_string()}),
                    );
                }
                Err(e) => log_json_stderr(
                    json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":multicast_route_hint(format!("join multicast group failed: {}",e), &e),"outbound_ip":interface.to_string()}),
                ),
            }
        }
        if !joined_any {
            return;
        }
    }

    log_json_stdout(
        json!({"level":"INFO","event":"multicast_listen","addr":multicast.to_string()}),
    );
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                if serde_json::from_slice::<DiscoveryQuery>(&buf[..n]).is_ok() {
                    let announce = discovery_announce(&hostname, &ip, kcp_port, tcp_port);
                    if let Ok(msg) = serde_json::to_vec(&announce) {
                        let _ = sock.send_to(&msg, src);
                    }
                }
            }
            Err(e) => log_json_stderr(
                json!({"level":"WARN","event":"multicast_error","code":ErrorCode::MulticastFailed.to_string(),"message":format!("discovery recv failed: {}",e)}),
            ),
        }
    }
}

fn create_udp_socket(
    bind_addr: SocketAddr,
    multicast_interface: Option<Ipv4Addr>,
) -> std::io::Result<UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&bind_addr.into())?;
    if let Some(interface) = multicast_interface {
        socket.set_multicast_if_v4(&interface)?;
    }
    Ok(socket.into())
}

fn multicast_route_hint(message: String, error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(101) | Some(19) => format!(
            "{}; multicast route/interface is unavailable, configure [connecting].multicast_outbound_ip with one or more local IPv4 addresses",
            message
        ),
        _ => message,
    }
}

fn query_server_loop(
    listen_addr: &str,
    collector: Arc<dyn GpuCollector>,
    cache: Arc<GpuCache>,
    ttl_ms: u64,
    connection_idle_timeout: Duration,
) {
    let listener = match TcpListener::bind(listen_addr) {
        Ok(l) => l,
        Err(e) => {
            log_json_stderr(
                json!({"level":"WARN","event":"query_error","code":ErrorCode::KcpInitFailed.to_string(),"message":format!("query bind failed on {}: {}", listen_addr, e)}),
            );
            return;
        }
    };
    log_json_stdout(json!({"level":"INFO","event":"tcp_listen","addr":listen_addr}));

    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                let collector = Arc::clone(&collector);
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(connection_idle_timeout));
                    let _ = stream.set_write_timeout(Some(connection_idle_timeout));
                    handle_query_stream(&mut stream, &collector, &cache, ttl_ms);
                });
            }
            Err(e) => log_json_stderr(
                json!({"level":"WARN","event":"query_error","code":ErrorCode::Internal.to_string(),"message":format!("accept failed: {}", e)}),
            ),
        }
    }
}

fn handle_query_stream(
    stream: &mut TcpStream,
    collector: &Arc<dyn GpuCollector>,
    cache: &Arc<GpuCache>,
    ttl_ms: u64,
) {
    let mut buf = [0u8; 16];
    let _ = stream.read(&mut buf);

    match cache.get_latest_or_refresh(collector.as_ref(), ttl_ms) {
        Ok(entry) => {
            let metrics = cache.metrics();
            let body = json!({
                "ok": true,
                "timestamp_ms": entry.timestamp_ms,
                "gpu_num": entry.gpu_num(),
                "avg_utilization": entry.avg_utilization(),
                "payload_len": entry.payload.len(),
                "payload_b64": BASE64_STANDARD.encode(entry.payload.as_slice()),
                "metrics": {
                    "cache_hits": metrics.cache_hits,
                    "cache_misses": metrics.cache_misses,
                    "merge_count": metrics.merge_count,
                    "collect_count": metrics.collect_count,
                    "avg_collect_latency_us": metrics.avg_collect_latency_us,
                    "collect_latency_p50_us": metrics.collect_latency_p50_us,
                    "collect_latency_p95_us": metrics.collect_latency_p95_us,
                    "cache_hit_rate_bps": metrics.cache_hit_rate_bps,
                    "cache_miss_rate_bps": metrics.cache_miss_rate_bps,
                    "merge_ratio_bps": metrics.merge_ratio_bps,
                }
            })
            .to_string();
            let _ = stream.write_all(body.as_bytes());
        }
        Err(code) => {
            log_json_stderr(
                json!({"level":"WARN","event":"query_error","code":code.to_string(),"message":"collector unavailable in degraded mode"}),
            );
            let metrics = cache.metrics();
            let body = json!({
                "ok":false,
                "error_code":code.to_string(),
                "message":"collector unavailable in degraded mode",
                "metrics": {
                    "cache_hits": metrics.cache_hits,
                    "cache_misses": metrics.cache_misses,
                    "merge_count": metrics.merge_count,
                    "collect_count": metrics.collect_count,
                    "avg_collect_latency_us": metrics.avg_collect_latency_us,
                    "collect_latency_p50_us": metrics.collect_latency_p50_us,
                    "collect_latency_p95_us": metrics.collect_latency_p95_us,
                    "cache_hit_rate_bps": metrics.cache_hit_rate_bps,
                    "cache_miss_rate_bps": metrics.cache_miss_rate_bps,
                    "merge_ratio_bps": metrics.merge_ratio_bps,
                }
            })
            .to_string();
            let _ = stream.write_all(body.as_bytes());
        }
    }
}

#[cfg(test)]
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

fn detect_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| read_hostname_file("/proc/sys/kernel/hostname"))
        .or_else(|| read_hostname_file("/etc/hostname"))
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn read_hostname_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{ConnectingConfig, LogConfig, RuntimeConfig, ServicesConfig};

    fn valid_config() -> Config {
        Config {
            connecting: ConnectingConfig {
                port_range: [30_000, 30_010],
                multicast_addr: "239.0.0.1:4000".to_string(),
                kcp_port: 0,
                tcp_port: 0,
                protocol: "kcp".to_string(),
                heartbeat_interval: 5,
                connection_idle_timeout: 10,
                max_connections: 64,
                kcp_retry_limit: 3,
                discover_wait_secs: 5,
                multicast_retry_limit: 5,
                multicast_outbound_ip: Vec::new(),
            },
            log: LogConfig {
                max_size: "5mb".to_string(),
            },
            services: ServicesConfig {
                cache_ttl_ms: 40,
                collector_interval_ms: 25,
                latency_display: true,
                uds_path: None,
            },
            runtime: RuntimeConfig::default(),
        }
    }

    #[test]
    fn config_validation_rejects_invalid_ttl() {
        let mut config = valid_config();
        config.services.cache_ttl_ms = 0;

        let err = validate_config(&config).expect_err("invalid ttl");

        assert_eq!(err.code, ErrorCode::ConfigInvalid);
        assert!(err.message.contains("cache_ttl_ms"));
    }

    #[test]
    fn load_config_rejects_invalid_config_with_config_invalid() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "gpustat4cluster-invalid-config-{}-{}.toml",
            std::process::id(),
            now_unix_ms()
        ));
        std::fs::write(
            &path,
            r#"
[connecting]
port_range = [30000, 30010]
multicast_addr = "239.0.0.1:4000"
protocol = "kcp" # or "tcp"
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
kcp_retry_limit = 3
discover_wait_secs = 5
multicast_retry_limit = 5
# Optional: one or more local IPv4 addresses used as multicast outbound interfaces.
# multicast_outbound_ip = ["192.0.2.10"]

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 0
# Optional: UDS path for client frontend <-> client-backend."#,
        )
        .expect("write invalid config");

        let err = load_config(&path).expect_err("invalid config");
        let _ = std::fs::remove_file(&path);

        assert_eq!(err.code, ErrorCode::ConfigInvalid);
        assert!(err.message.contains("cache_ttl_ms"));
    }

    #[test]
    fn config_validation_rejects_invalid_log_size() {
        let mut config = valid_config();
        config.log.max_size = "five mb".to_string();

        let err = validate_config(&config).expect_err("invalid log size");

        assert_eq!(err.code, ErrorCode::ConfigInvalid);
        assert!(err.message.contains("log.max_size"));
    }

    #[test]
    fn config_validation_rejects_invalid_multicast() {
        let mut config = valid_config();
        config.connecting.multicast_addr = "127.0.0.1:4000".to_string();

        let err = validate_config(&config).expect_err("invalid multicast");

        assert_eq!(err.code, ErrorCode::MulticastFailed);
        assert!(err.message.contains("not multicast"));
    }

    #[test]
    fn config_validation_rejects_invalid_port_range_and_timeouts() {
        let mut config = valid_config();
        config.connecting.port_range = [40_000, 30_000];
        assert_eq!(
            validate_config(&config)
                .expect_err("invalid port range")
                .code,
            ErrorCode::ConfigInvalid
        );

        let mut config = valid_config();
        config.connecting.heartbeat_interval = 0;
        assert!(validate_config(&config)
            .expect_err("invalid heartbeat")
            .message
            .contains("heartbeat_interval"));

        let mut config = valid_config();
        config.connecting.connection_idle_timeout = 0;
        assert!(validate_config(&config)
            .expect_err("invalid idle timeout")
            .message
            .contains("connection_idle_timeout"));

        let mut config = valid_config();
        config.connecting.max_connections = 0;
        assert!(validate_config(&config)
            .expect_err("invalid kcp runtime threads")
            .message
            .contains("max_connections"));

        let mut config = valid_config();
        config.connecting.discover_wait_secs = 0;
        assert!(validate_config(&config)
            .expect_err("invalid discover wait")
            .message
            .contains("discover_wait_secs"));
    }

    #[test]
    fn log_size_parser_accepts_common_units() {
        assert_eq!(parse_log_size_bytes("5mb").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_log_size_bytes("1GiB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_log_size_bytes("512").unwrap(), 512);
    }

    #[cfg(feature = "mock-nvml")]
    #[test]
    fn startup_mode_selection_uses_mock_when_requested() {
        let _guard = crate::collector::ENV_TEST_LOCK
            .lock()
            .expect("env test lock");
        std::env::remove_var("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING");
        std::env::set_var(collector::COLLECTOR_ENV, "mock");
        std::env::set_var(collector::MOCK_HOSTNAME_ENV, "mock-startup-node");
        std::env::set_var(collector::MOCK_GPU_COUNT_ENV, "2");

        let (_collector, degraded, mode) =
            build_collector("test-host", &valid_config()).expect("mock collector");

        std::env::remove_var(collector::COLLECTOR_ENV);
        std::env::remove_var(collector::MOCK_HOSTNAME_ENV);
        std::env::remove_var(collector::MOCK_GPU_COUNT_ENV);
        assert!(!degraded);
        assert_eq!(mode, "mock-nvml");
    }

    #[cfg(not(feature = "mock-nvml"))]
    #[test]
    fn startup_mode_selection_ignores_mock_env_without_feature() {
        let _guard = crate::collector::ENV_TEST_LOCK
            .lock()
            .expect("env test lock");
        std::env::remove_var("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING");
        std::env::set_var(collector::COLLECTOR_ENV, "mock");
        std::env::set_var(collector::MOCK_HOSTNAME_ENV, "mock-startup-node");
        std::env::set_var(collector::MOCK_GPU_COUNT_ENV, "2");

        let err = match build_collector("test-host", &valid_config()) {
            Ok(_) => panic!("expected nvml unavailable"),
            Err(err) => err,
        };

        std::env::remove_var(collector::COLLECTOR_ENV);
        std::env::remove_var(collector::MOCK_HOSTNAME_ENV);
        std::env::remove_var(collector::MOCK_GPU_COUNT_ENV);
        assert_eq!(err.code, ErrorCode::NvmlUnavailable);
    }

    #[test]
    fn startup_mode_selection_fails_when_nvml_missing() {
        let _guard = crate::collector::ENV_TEST_LOCK
            .lock()
            .expect("env test lock");
        std::env::remove_var(collector::COLLECTOR_ENV);
        std::env::remove_var(collector::FORCE_MOCK_ENV);
        std::env::set_var("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING", "1");

        let err = match build_collector("test-host", &valid_config()) {
            Ok(_) => panic!("expected nvml unavailable"),
            Err(err) => err,
        };

        std::env::remove_var("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING");
        assert_eq!(err.code, ErrorCode::NvmlUnavailable);
        assert!(err.message.contains("nvml_lib_path"));
    }
}
