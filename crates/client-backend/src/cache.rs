use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use common::ServerGresSnapshot;

use crate::discovery::DiscoveredNode;

pub type CacheMap = HashMap<String, ConnectionCacheEntry>;
pub type SharedCache = Arc<Mutex<CacheMap>>;

#[derive(Debug, Clone)]
pub struct ConnectionCacheEntry {
    pub connection_id: String,
    pub hostname: String,
    pub num: u8,
    pub server_gres: Vec<u8>,
    pub record_timestamp: i64,
    pub addr: SocketAddr,
    pub last_snapshot: Option<ServerGresSnapshot>,
    pub last_error: Option<String>,
    pub last_query_latency_us: Option<u64>,
}

impl ConnectionCacheEntry {
    pub fn matches_filter(&self, filter: &crate::filter::NodeFilter) -> bool {
        filter.matches_target(&self.hostname, self.addr, &self.connection_id)
    }

    pub fn from_snapshot(
        connection_id: impl Into<String>,
        addr: SocketAddr,
        snapshot: ServerGresSnapshot,
        last_query_latency_us: Option<u64>,
    ) -> Self {
        Self {
            connection_id: connection_id.into(),
            hostname: snapshot.hostname.clone(),
            num: snapshot.gres.len().min(u8::MAX as usize) as u8,
            server_gres: Vec::new(),
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
                    server_gres: Vec::new(),
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

pub fn upsert_snapshot(
    rows: &mut CacheMap,
    connection_id: impl Into<String>,
    addr: SocketAddr,
    snapshot: ServerGresSnapshot,
    last_query_latency_us: Option<u64>,
) {
    rows.insert(
        format!("{}-{}", addr.ip(), addr.port()),
        ConnectionCacheEntry::from_snapshot(connection_id, addr, snapshot, last_query_latency_us),
    );
}

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
            server_gres: Vec::new(),
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
    use common::{GresInfo, GresMemory, GresUtilization, ServerGresSnapshot};
    #[test]
    fn upsert_snapshot_adds_result_to_cache() {
        let mut rows = CacheMap::new();
        let addr = "127.0.0.1:30000".parse().unwrap();

        let before = now_ms();
        upsert_snapshot(
            &mut rows,
            "conn-001",
            addr,
            ServerGresSnapshot {
                hostname: "test-node".to_string(),
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
                    processes: Vec::new(),
                }],
            },
            Some(987),
        );

        let entry = rows.get("127.0.0.1-30000").expect("cache entry");
        assert_eq!(entry.hostname, "test-node");
        assert_eq!(entry.num, 1);
        assert!(entry.record_timestamp >= before);
        assert!(entry.last_snapshot.is_some());
        assert_eq!(entry.last_query_latency_us, Some(987));
    }
}
