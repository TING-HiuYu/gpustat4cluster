use serde::{Deserialize, Serialize};

/// 连接相关配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectingConfig {
    pub port_range: [u16; 2],
    pub multicast_addr: String,
    #[serde(default = "default_tcp_port")]
    pub tcp_port: u16,
    #[serde(default = "default_udp_port")]
    pub udp_port: u16,
    #[serde(default = "default_udp_mtu")]
    pub udp_mtu: u16,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval: u64,
    #[serde(default = "default_connection_idle_timeout_secs")]
    pub connection_idle_timeout: u64,
    #[serde(default = "default_max_connections", alias = "connections")]
    pub max_connections: usize,
    #[serde(default = "default_discover_wait_secs")]
    pub discover_wait_secs: u64,
    #[serde(default = "default_multicast_retry_limit")]
    pub multicast_retry_limit: u32,
    #[serde(default)]
    pub multicast_outbound_ip: Vec<String>,
}

/// 日志配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogConfig {
    #[serde(default = "default_log_max_size")]
    pub max_size: String,
}

/// 服务运行参数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServicesConfig {
    #[serde(default = "default_cache_ttl_ms")]
    pub cache_ttl_ms: u64,
    #[serde(default = "default_collector_interval_ms")]
    pub collector_interval_ms: u64,
    #[serde(default = "default_latency_display")]
    pub latency_display: bool,
    #[serde(default)]
    pub uds_path: Option<String>,
}

/// 运行时依赖和本机环境配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub nvml_lib_path: Option<String>,
}

/// 通用配置根结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub connecting: ConnectingConfig,
    pub log: LogConfig,
    pub services: ServicesConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

fn default_heartbeat_interval_secs() -> u64 {
    5
}
fn default_connection_idle_timeout_secs() -> u64 {
    10
}
fn default_max_connections() -> usize {
    64
}
fn default_protocol() -> String {
    "udp".to_string()
}
fn default_tcp_port() -> u16 {
    0
}
fn default_udp_port() -> u16 {
    0
}
fn default_udp_mtu() -> u16 {
    0
}
fn default_discover_wait_secs() -> u64 {
    5
}
fn default_multicast_retry_limit() -> u32 {
    5
}
fn default_log_max_size() -> String {
    "5mb".to_string()
}
fn default_cache_ttl_ms() -> u64 {
    40
}
fn default_collector_interval_ms() -> u64 {
    25
}
fn default_latency_display() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_wait_secs_defaults_to_5_for_backward_compat() {
        let cfg: Config = toml::from_str(
            r#"
[connecting]
port_range = [30000, 40000]
multicast_addr = "239.0.0.1:4000"
tcp_port = 0
udp_port = 0
udp_mtu = 0
protocol = "udp" # or "tcp"
heartbeat_interval = 5
connection_idle_timeout = 10
max_connections = 64

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
collector_interval_ms = 25
latency_display = true
"#,
        )
        .expect("config should deserialize with defaults");

        assert_eq!(cfg.connecting.discover_wait_secs, 5);
        assert_eq!(cfg.connecting.protocol, "udp");
        assert_eq!(cfg.connecting.tcp_port, 0);
        assert_eq!(cfg.connecting.udp_port, 0);
        assert_eq!(cfg.connecting.udp_mtu, 0);
        assert_eq!(cfg.connecting.multicast_retry_limit, 5);
        assert_eq!(cfg.connecting.heartbeat_interval, 5);
        assert_eq!(cfg.connecting.connection_idle_timeout, 10);
        assert_eq!(cfg.connecting.max_connections, 64);
        assert!(cfg.connecting.multicast_outbound_ip.is_empty());
        assert_eq!(cfg.services.collector_interval_ms, 25);
        assert!(cfg.services.latency_display);
        assert_eq!(cfg.services.uds_path, None);
        assert_eq!(cfg.runtime.nvml_lib_path, None);
    }
}
