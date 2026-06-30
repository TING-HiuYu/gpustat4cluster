mod cache;
mod collector;
mod transport;
mod udp_transport;

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

use cache::GresCache;
use chrono::Local;
#[cfg(feature = "debug")]
use collector::TestGresCollector;
use collector::{GresCollector, NvmlCollector};
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
    let multicast_addr = validate_multicast_addr(&config.connecting.multicast_addr)?;
    let multicast_outbound_ip =
        validate_multicast_outbound_ips(&config.connecting.multicast_outbound_ip)?;
    let udp_port = pick_udp_port(config.connecting.port_range, config.connecting.udp_port)?;
    let tcp_port = pick_tcp_port(
        config.connecting.port_range,
        config.connecting.tcp_port,
        Some(udp_port),
    )?;

    let hostname = detect_hostname();

    let (collector, degraded, collector_mode) = build_collector(&hostname, &config)?;

    let cache = Arc::new(GresCache::new());
    let ttl_ms = config.services.cache_ttl_ms;
    let collector_interval_ms = config.services.collector_interval_ms;
    let _query_addr =
        std::env::var(QUERY_LISTEN_ENV).unwrap_or_else(|_| DEFAULT_QUERY_ADDR.to_string());
    let metrics = cache.metrics();
    log_json_stdout(json!({
        "level":"INFO",
        "event":"startup",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": PROTOCOL_VERSION,
        "config":config_path.display().to_string(),
        "hostname": hostname.clone(),
        "udp_port":udp_port,
        "udp_mtu": config.connecting.udp_mtu,
        "tcp_port":tcp_port,
        "protocols": ["udp", "tcp"],
        "multicast":multicast_addr.to_string(),
        "multicast_outbound_ip": multicast_outbound_ip.iter().map(|ip| ip.to_string()).collect::<Vec<_>>(),
        "degraded":degraded,
        "collector_mode":collector_mode,
        "cache_ttl_ms": ttl_ms,
        "collector_interval_ms": collector_interval_ms,
        "heartbeat_interval": config.connecting.heartbeat_interval,
        "connection_idle_timeout": config.connecting.connection_idle_timeout,
        "max_connections": config.connecting.max_connections,
        "udp_addr":format!("0.0.0.0:{udp_port}"),
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
        udp_port,
        tcp_port,
        multicast_addr,
        hostname,
        collector,
        cache,
        ttl_ms,
        collector_interval_ms,
        config.connecting.multicast_retry_limit,
        config.connecting.connection_idle_timeout,
        multicast_outbound_ip,
        config.connecting.udp_mtu,
    )
}

fn build_collector(
    hostname: &str,
    config: &Config,
) -> Result<(Arc<dyn GresCollector>, bool, &'static str), StartupError> {
    #[cfg(feature = "debug")]
    if let Some(inventory_path) = config.runtime.test_inventory_path.as_deref() {
        let collector = TestGresCollector::from_inventory_file_with_reload(
            inventory_path,
            config.runtime.test_inventory_reload,
        )
        .map_err(|err| {
            StartupError::new(
                ErrorCode::ConfigInvalid,
                format!("test collector inventory failed: {err}"),
            )
        })?;
        let collector = if let Some(runtime_path) = config.runtime.test_runtime_path.as_deref() {
            let collector = collector.with_runtime_path(runtime_path);
            let writer = collector
                .start_runtime_writer(runtime_path, Duration::from_millis(5))
                .map_err(|err| {
                    StartupError::new(
                        ErrorCode::ConfigInvalid,
                        format!("test collector runtime mmap failed: {err}"),
                    )
                })?;
            std::mem::forget(writer);
            collector
        } else {
            collector
        };
        let _ = hostname;
        return Ok((Arc::new(collector) as Arc<dyn GresCollector>, false, "test"));
    }
    #[cfg(not(feature = "debug"))]
    if config.runtime.test_inventory_path.is_some() || config.runtime.test_runtime_path.is_some() {
        return Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            "runtime.test_inventory_path/runtime.test_runtime_path require building the server with --features debug".to_string(),
        ));
    }

    NvmlCollector::new(
        hostname.to_string(),
        config.runtime.nvml_lib_path.as_deref(),
    )
    .map(|c| (Arc::new(c) as Arc<dyn GresCollector>, false, "nvml"))
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
        "udp" | "tcp" => Ok(()),
        other => Err(StartupError::new(
            ErrorCode::ConfigInvalid,
            format!(
                "invalid connecting.protocol '{}': expected 'udp' or 'tcp'",
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
    pick_udp_port_excluding(range, requested, None)
}

fn pick_udp_port_excluding(
    range: [u16; 2],
    requested: u16,
    exclude: Option<u16>,
) -> Result<u16, StartupError> {
    if requested != 0 {
        if Some(requested) == exclude {
            return Err(StartupError::new(
                ErrorCode::PortExhausted,
                format!(
                    "configured udp_port {} conflicts with another UDP port",
                    requested
                ),
            ));
        }
        if UdpSocket::bind(("0.0.0.0", requested)).is_ok() {
            return Ok(requested);
        }
        return Err(StartupError::new(
            ErrorCode::PortExhausted,
            format!("configured udp_port {} is not available", requested),
        ));
    }
    let [start, end] = range;
    for port in port_candidates(range) {
        if Some(port) == exclude {
            continue;
        }
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
                format!("configured tcp_port {} conflicts with udp_port", requested),
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
    udp_port: u16,
    tcp_port: u16,
    multicast: SocketAddr,
    hostname: String,
    collector: Arc<dyn GresCollector>,
    cache: Arc<GresCache>,
    ttl_ms: u64,
    collector_interval_ms: u64,
    multicast_retry_limit: u32,
    connection_idle_timeout_secs: u64,
    multicast_outbound_ip: Vec<Ipv4Addr>,
    udp_mtu: u16,
) -> Result<(), StartupError> {
    let local_ip = multicast_outbound_ip
        .first()
        .map(|ip| ip.to_string())
        .unwrap_or_else(infer_local_ip);

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

    let udp_hostname = hostname.clone();
    let udp_collector = Arc::clone(&collector);
    let udp_cache = Arc::clone(&cache);
    let udp = thread::spawn(move || {
        udp_transport::server_loop(
            &format!("0.0.0.0:{udp_port}"),
            udp_hostname,
            udp_collector,
            udp_cache,
            ttl_ms,
            Duration::from_secs(connection_idle_timeout_secs),
            udp_mtu,
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
            udp_port,
            tcp_port,
            discovery_outbound_ip,
        )
    });

    let q_hostname = hostname.clone();
    let ann_hostname = hostname;
    let ann_ip = local_ip;
    let ann_outbound_ip = multicast_outbound_ip;
    let ann = thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        announce_startup_once(
            multicast,
            ann_hostname,
            ann_ip,
            udp_port,
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
            q_hostname,
            q_collector,
            q_cache,
            ttl_ms,
            Duration::from_secs(connection_idle_timeout_secs),
        )
    });

    let _ = ann.join();
    let _ = discovery.join();
    let _ = udp.join();
    let _ = refresh.join();
    let _ = query.join();
    Ok(())
}

fn background_refresh_loop(
    collector: Arc<dyn GresCollector>,
    cache: Arc<GresCache>,
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

fn discovery_announce(hostname: &str, ip: &str, udp_port: u16, tcp_port: u16) -> DiscoveryAnnounce {
    DiscoveryAnnounce {
        version: PROTOCOL_VERSION,
        hostname: hostname.to_string(),
        ip: ip.to_string(),
        port: udp_port,
        tcp_port: Some(tcp_port),
        udp_port: Some(udp_port),
        kcp_port: None,
        ttl: None,
        load: None,
        degraded: Some(false),
    }
}

fn announce_startup_once(
    multicast: SocketAddr,
    hostname: String,
    ip: String,
    udp_port: u16,
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
            let announce = discovery_announce(&hostname, &announce_ip, udp_port, tcp_port);
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
    udp_port: u16,
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
                    let announce = discovery_announce(&hostname, &ip, udp_port, tcp_port);
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
    hostname: String,
    collector: Arc<dyn GresCollector>,
    cache: Arc<GresCache>,
    ttl_ms: u64,
    connection_idle_timeout: Duration,
) {
    let listener = match TcpListener::bind(listen_addr) {
        Ok(l) => l,
        Err(e) => {
            log_json_stderr(
                json!({"level":"WARN","event":"query_error","code":ErrorCode::Internal.to_string(),"message":format!("query bind failed on {}: {}", listen_addr, e)}),
            );
            return;
        }
    };
    log_json_stdout(json!({"level":"INFO","event":"tcp_listen","addr":listen_addr}));

    for conn in listener.incoming() {
        match conn {
            Ok(mut stream) => {
                let context = transport::TransportContext::new(
                    hostname.clone(),
                    Arc::clone(&collector),
                    Arc::clone(&cache),
                    ttl_ms,
                );
                thread::spawn(move || {
                    let _ = stream.set_write_timeout(Some(connection_idle_timeout));
                    handle_query_stream(&mut stream, &context);
                });
            }
            Err(e) => log_json_stderr(
                json!({"level":"WARN","event":"query_error","code":ErrorCode::Internal.to_string(),"message":format!("accept failed: {}", e)}),
            ),
        }
    }
}

fn handle_query_stream(stream: &mut TcpStream, context: &transport::TransportContext) {
    let peer = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    loop {
        let frame = match read_tcp_frame(stream) {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(e) => {
                log_json_stderr(
                    json!({"level":"WARN","event":"query_error","code":ErrorCode::Internal.to_string(),"message":format!("read TCP frame failed: {}", e)}),
                );
                return;
            }
        };

        if let Ok(decoded) = transport::decode_transport_frame(&frame) {
            if decoded.header.frame_type == common::FrameType::Disconnect {
                log_json_stdout(json!({
                    "level":"INFO",
                    "event":"tcp_peer_disconnect",
                    "peer":peer,
                    "reason":String::from_utf8_lossy(&decoded.payload).trim().to_string()
                }));
                return;
            }
        }

        let response = match context.handle_frame(&frame) {
            Ok(response) => response,
            Err(e) => {
                log_json_stderr(
                    json!({"level":"WARN","event":"query_error","code":ErrorCode::Internal.to_string(),"message":format!("handle TCP frame failed: {}", e)}),
                );
                return;
            }
        };

        if stream.write_all(&response).is_err() || stream.flush().is_err() {
            return;
        }
    }
}

fn read_tcp_frame(stream: &mut TcpStream) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut header_bytes = [0u8; common::FRAME_HEADER_LEN];
    match stream.read_exact(&mut header_bytes) {
        Ok(()) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.kind() == std::io::ErrorKind::ConnectionReset =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let header = common::FrameHeader::decode(&header_bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid frame header: {e:?}"),
        )
    })?;
    let payload_len = header.payload_len as usize;
    let mut frame = Vec::with_capacity(common::FRAME_HEADER_LEN + payload_len);
    frame.extend_from_slice(&header_bytes);
    if payload_len > 0 {
        let mut payload = vec![0u8; payload_len];
        stream.read_exact(&mut payload)?;
        frame.extend_from_slice(&payload);
    }
    Ok(Some(frame))
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
                tcp_port: 0,
                udp_port: 0,
                udp_mtu: 0,
                protocol: "udp".to_string(),
                heartbeat_interval: 5,
                connection_idle_timeout: 10,
                max_connections: 64,
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
protocol = "udp" # or "tcp"
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64
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
            .expect_err("invalid max connections")
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

    #[test]
    fn startup_mode_selection_fails_when_nvml_missing() {
        let _guard = crate::collector::ENV_TEST_LOCK
            .lock()
            .expect("env test lock");
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
