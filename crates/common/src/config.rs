use serde::{Deserialize, Serialize};

/// 连接相关配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectingConfig {
    pub port_range: [u16; 2],
    pub multicast_addr: String,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    #[serde(default = "default_discover_wait_secs")]
    pub discover_wait_secs: u64,
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
}

/// 通用配置根结构。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub connecting: ConnectingConfig,
    pub log: LogConfig,
    pub services: ServicesConfig,
}

fn default_heartbeat_timeout() -> u64 { 5 }
fn default_discover_wait_secs() -> u64 { 5 }
fn default_log_max_size() -> String { "5mb".to_string() }
fn default_cache_ttl_ms() -> u64 { 40 }

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
heartbeat_timeout = 5

[log]
max_size = "5mb"

[services]
cache_ttl_ms = 40
"#,
        )
        .expect("config should deserialize with defaults");

        assert_eq!(cfg.connecting.discover_wait_secs, 5);
    }
}
