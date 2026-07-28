use std::{
    fs,
    path::{Path, PathBuf},
};

use common::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/clustat/client.toml";
const CONFIG_PATH_ENV: &str = "CLUSTAT_CONFIG";

pub fn get_config_path() -> PathBuf {
    std::env::var(CONFIG_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH))
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("read config failed at {}: {}", path.display(), e))?;
    let cfg: Config = toml::from_str(&raw)
        .map_err(|e| format!("parse config failed at {}: {}", path.display(), e))?;
    match cfg.connecting.protocol.trim().to_ascii_lowercase().as_str() {
        "udp" | "tcp" => {
            if cfg.connecting.heartbeat_interval == 0 {
                return Err("invalid connecting.heartbeat_interval: must be > 0".to_string());
            }
            if cfg.connecting.connection_idle_timeout == 0 {
                return Err("invalid connecting.connection_idle_timeout: must be > 0".to_string());
            }
            if cfg.connecting.max_connections == 0 {
                return Err("invalid connecting.max_connections: must be > 0".to_string());
            }
            Ok(cfg)
        }
        other => Err(format!(
            "invalid connecting.protocol '{}': expected 'udp' or 'tcp'",
            other
        )),
    }
}
