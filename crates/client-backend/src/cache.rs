use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[cfg(feature = "kcp-transport")]
use common::HandshakeInfo;
use common::ServerGpuSnapshot;

use crate::discovery::DiscoveredNode;

pub type CacheMap = HashMap<String, ConnectionCacheEntry>;
pub type SharedCache = Arc<Mutex<CacheMap>>;

#[derive(Debug, Clone)]
pub struct ConnectionCacheEntry {
    pub connection_id: String,
    pub hostname: String,
    pub num: u8,
    pub server_gpus: Vec<u8>,
    pub record_timestamp: i64,
    pub addr: SocketAddr,
    pub last_snapshot: Option<ServerGpuSnapshot>,
    pub last_error: Option<String>,
    pub last_query_latency_us: Option<u64>,
}

#[cfg(feature = "kcp-transport")]
#[derive(Debug, Clone)]
pub struct KcpConnectionCacheEntry {
    // Retained with the session entry for diagnostics and reconnect bookkeeping.
    #[allow(dead_code)]
    pub connection_id: String,
    #[allow(dead_code)]
    pub hostname: String,
    #[allow(dead_code)]
    pub addr: SocketAddr,
    pub gpu_num: u8,
    #[allow(dead_code)]
    pub payload_len: u16,
    pub last_snapshot: Option<ServerGpuSnapshot>,
    pub record_timestamp: i64,
}

#[cfg(feature = "kcp-transport")]
impl KcpConnectionCacheEntry {
    pub fn from_handshake(
        connection_id: impl Into<String>,
        addr: SocketAddr,
        info: &common::HandshakeInfo,
    ) -> Self {
        Self {
            connection_id: connection_id.into(),
            hostname: info.hostname.clone(),
            addr,
            gpu_num: info.gpu_num,
            payload_len: info.payload_len,
            last_snapshot: None,
            record_timestamp: 0,
        }
    }

    pub fn update_snapshot(&mut self, snapshot: ServerGpuSnapshot) {
        self.gpu_num = snapshot.gpus.len().min(u8::MAX as usize) as u8;
        self.record_timestamp = now_ms();
        self.last_snapshot = Some(snapshot);
    }
}

impl ConnectionCacheEntry {
    pub fn matches_filter(&self, filter: &crate::filter::NodeFilter) -> bool {
        filter.matches_target(&self.hostname, self.addr, &self.connection_id)
    }

    #[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
    pub fn from_snapshot(
        connection_id: impl Into<String>,
        addr: SocketAddr,
        snapshot: ServerGpuSnapshot,
        last_query_latency_us: Option<u64>,
    ) -> Self {
        Self {
            connection_id: connection_id.into(),
            hostname: snapshot.hostname.clone(),
            num: snapshot.gpus.len().min(u8::MAX as usize) as u8,
            server_gpus: Vec::new(),
            // Cache freshness must be measured on the client-backend host.
            // Server timestamps are preserved inside `last_snapshot` for display, but
            // cluster clocks can drift enough to otherwise delay refreshes by seconds.
            record_timestamp: now_ms(),
            addr,
            last_snapshot: Some(snapshot),
            last_error: None,
            last_query_latency_us,
        }
    }
}

pub fn build_cache(discovered: Vec<DiscoveredNode>) -> CacheMap {
    discovered
        .into_iter()
        .enumerate()
        .map(|(idx, n)| {
            let key = format!("{}-{}", n.addr.ip(), n.addr.port());
            (
                key.clone(),
                ConnectionCacheEntry {
                    connection_id: format!("conn-{:03}", idx + 1),
                    hostname: n.hostname,
                    num: 0,
                    server_gpus: Vec::new(),
                    record_timestamp: n.ts_ms,
                    addr: n.addr,
                    last_snapshot: None,
                    last_error: None,
                    last_query_latency_us: None,
                },
            )
        })
        .collect()
}

// Shared by transport code and local API tests so snapshot-backed cache entries
// stay covered even when KCP support is not compiled into the binary.
#[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
pub fn upsert_snapshot(
    rows: &mut CacheMap,
    connection_id: impl Into<String>,
    addr: SocketAddr,
    snapshot: ServerGpuSnapshot,
    last_query_latency_us: Option<u64>,
) {
    rows.insert(
        format!("{}-{}", addr.ip(), addr.port()),
        ConnectionCacheEntry::from_snapshot(connection_id, addr, snapshot, last_query_latency_us),
    );
}

#[cfg(feature = "kcp-transport")]
pub fn upsert_handshake(
    rows: &mut CacheMap,
    connection_id: impl Into<String>,
    addr: SocketAddr,
    info: &HandshakeInfo,
) {
    let key = format!("{}-{}", addr.ip(), addr.port());
    rows.insert(
        key,
        ConnectionCacheEntry {
            connection_id: connection_id.into(),
            hostname: info.hostname.clone(),
            num: info.gpu_num,
            server_gpus: Vec::new(),
            record_timestamp: now_ms(),
            addr,
            last_snapshot: None,
            last_error: None,
            last_query_latency_us: None,
        },
    );
}

#[cfg_attr(not(feature = "kcp-transport"), allow(dead_code))]
pub fn mark_stale(
    rows: &mut CacheMap,
    connection_id: impl Into<String>,
    addr: SocketAddr,
    hostname: impl Into<String>,
    error: impl Into<String>,
) {
    rows.insert(
        format!("{}-{}", addr.ip(), addr.port()),
        ConnectionCacheEntry {
            connection_id: connection_id.into(),
            hostname: hostname.into(),
            num: 0,
            server_gpus: Vec::new(),
            record_timestamp: now_ms(),
            addr,
            last_snapshot: None,
            last_error: Some(error.into()),
            last_query_latency_us: None,
        },
    );
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
    #[cfg(feature = "kcp-transport")]
    use common::HandshakeInfo;
    use common::{GpuInfo, GpuMemory, GpuUtilization, ServerGpuSnapshot};

    #[cfg(feature = "kcp-transport")]
    #[test]
    fn kcp_cache_entry_can_seed_from_handshake_and_update_snapshot() {
        let info = HandshakeInfo::new("node-a", 2, 4096);
        let addr = "10.0.0.1:30000".parse().unwrap();
        let mut entry = KcpConnectionCacheEntry::from_handshake("conn-001", addr, &info);

        assert_eq!(entry.hostname, "node-a");
        assert_eq!(entry.connection_id, "conn-001");
        assert_eq!(entry.addr, addr);
        assert_eq!(entry.gpu_num, 2);
        assert_eq!(entry.payload_len, 4096);
        assert!(entry.last_snapshot.is_none());

        entry.update_snapshot(ServerGpuSnapshot {
            hostname: "node-a".to_string(),
            driver_version: None,
            gpus: vec![GpuInfo {
                index: 0,
                name: "NVIDIA A100".to_string(),
                temperature_c: None,
                uuid: None,
                memory: GpuMemory {
                    used_mb: 1,
                    total_mb: 2,
                },
                utilization: GpuUtilization {
                    gpu_percent: 3,
                    memory_percent: 4,
                },
                processes: Vec::new(),
            }],
        });

        assert_eq!(entry.gpu_num, 1);
        assert!(entry.record_timestamp > 0);
        assert!(entry.last_snapshot.is_some());
    }

    #[test]
    fn upsert_snapshot_adds_kcp_result_to_cache() {
        let mut rows = CacheMap::new();
        let addr = "127.0.0.1:30000".parse().unwrap();

        let before = now_ms();
        upsert_snapshot(
            &mut rows,
            "conn-001",
            addr,
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
                    processes: Vec::new(),
                }],
            },
            Some(987),
        );

        let entry = rows.get("127.0.0.1-30000").expect("cache entry");
        assert_eq!(entry.hostname, "kcp-node");
        assert_eq!(entry.num, 1);
        assert!(entry.record_timestamp >= before);
        assert!(entry.last_snapshot.is_some());
        assert_eq!(entry.last_query_latency_us, Some(987));
    }
}
