use common::{
    ErrorCode, GresInfo, GresMemory, GresProcessInfo, GresUtilization, ServerGresSnapshot,
};

#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum GresResourceKind {
    Nvml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GresNodeSnapshot {
    pub hostname: String,
    pub driver_version: Option<String>,
    pub resources: Vec<GresResource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GresResource {
    pub kind: GresResourceKind,
    pub index: u8,
    pub name: String,
    pub uuid: Option<String>,
    pub temperature_c: Option<u32>,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub utilization_gres_percent: u8,
    pub utilization_memory_percent: u8,
    pub processes: Vec<GresProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GresProcess {
    pub pid: u32,
    pub uid: u32,
    pub used_memory_mb: u64,
}

impl GresNodeSnapshot {
    #[allow(dead_code)]
    pub fn from_gres_snapshot(snapshot: ServerGresSnapshot) -> Self {
        Self {
            hostname: snapshot.hostname,
            driver_version: snapshot.driver_version,
            resources: snapshot
                .gres
                .into_iter()
                .map(|gres| GresResource {
                    kind: GresResourceKind::Nvml,
                    index: gres.index,
                    name: gres.name,
                    uuid: gres.uuid,
                    temperature_c: gres.temperature_c,
                    memory_used_mb: gres.memory.used_mb,
                    memory_total_mb: gres.memory.total_mb,
                    utilization_gres_percent: gres.utilization.gres_percent,
                    utilization_memory_percent: gres.utilization.memory_percent,
                    processes: gres
                        .processes
                        .into_iter()
                        .map(|process| GresProcess {
                            pid: process.pid,
                            uid: process.uid,
                            used_memory_mb: process.used_memory_mb,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn into_gres_snapshot(self) -> ServerGresSnapshot {
        ServerGresSnapshot {
            hostname: self.hostname,
            driver_version: self.driver_version,
            gres: self
                .resources
                .into_iter()
                .map(|resource| GresInfo {
                    index: resource.index,
                    name: resource.name,
                    temperature_c: resource.temperature_c,
                    uuid: resource.uuid,
                    memory: GresMemory {
                        used_mb: resource.memory_used_mb,
                        total_mb: resource.memory_total_mb,
                    },
                    utilization: GresUtilization {
                        gres_percent: resource.utilization_gres_percent,
                        memory_percent: resource.utilization_memory_percent,
                    },
                    processes: resource
                        .processes
                        .into_iter()
                        .map(|process| GresProcessInfo {
                            pid: process.pid,
                            uid: process.uid,
                            command: None,
                            used_memory_mb: process.used_memory_mb,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

pub trait GresCollector: Send + Sync {
    fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode>;

    fn collect_gres_snapshot(&self) -> Result<ServerGresSnapshot, ErrorCode> {
        self.collect_gres()
            .map(GresNodeSnapshot::into_gres_snapshot)
    }
}

#[cfg(any(test, feature = "test-collector"))]
mod test_collector {
    use super::*;
    use memmap2::{Mmap, MmapMut};
    use serde::{Deserialize, Serialize};
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    };

    const DEFAULT_RUNTIME_CAPACITY: usize = 64 * 1024;
    const RUNTIME_HEADER_LEN: usize = 8;
    const DEFAULT_REFRESH_MS: u64 = 5;

    #[derive(Debug, Clone)]
    pub struct TestGresCollector {
        inventory: TestGresInventory,
        inventory_path: Option<PathBuf>,
        reload_inventory: bool,
        runtime_path: Option<PathBuf>,
    }

    #[derive(Debug)]
    #[allow(dead_code)]
    pub struct RuntimeWriterHandle {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    #[allow(dead_code)]
    impl RuntimeWriterHandle {
        pub fn stop(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for RuntimeWriterHandle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestGresInventory {
        pub hostname: String,
        #[serde(default)]
        pub driver_version: Option<String>,
        pub gres: Vec<TestGresInventoryResource>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestGresInventoryResource {
        pub index: u8,
        pub name: String,
        #[serde(default)]
        pub uuid: Option<String>,
        pub memory_total_mb: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestGresRuntimeState {
        pub gres: Vec<TestGresRuntimeResource>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestGresRuntimeResource {
        pub index: u8,
        #[serde(default)]
        pub temperature_c: Option<u32>,
        pub memory_used_mb: u64,
        pub utilization_gres_percent: u8,
        pub utilization_memory_percent: u8,
        #[serde(default)]
        pub processes: Vec<TestGresRuntimeProcess>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TestGresRuntimeProcess {
        pub pid: u32,
        pub uid: u32,
        pub used_memory_mb: u64,
    }

    impl TestGresCollector {
        pub fn new(hostname: impl Into<String>) -> Self {
            Self::from_inventory(default_inventory(hostname.into(), 1))
        }

        #[allow(dead_code)]
        pub fn with_gres_count(hostname: impl Into<String>, gres_count: u8) -> Self {
            Self::from_inventory(default_inventory(hostname.into(), gres_count.max(1)))
        }

        pub fn from_inventory(inventory: TestGresInventory) -> Self {
            Self {
                inventory,
                inventory_path: None,
                reload_inventory: false,
                runtime_path: None,
            }
        }

        #[allow(dead_code)]
        pub fn from_inventory_file(path: impl AsRef<Path>) -> Result<Self, String> {
            Self::from_inventory_file_with_reload(path, false)
        }

        pub fn from_inventory_file_with_reload(
            path: impl AsRef<Path>,
            reload_inventory: bool,
        ) -> Result<Self, String> {
            let path = path.as_ref();
            let inventory = read_inventory_file(path)?;
            Ok(Self {
                inventory,
                inventory_path: Some(path.to_path_buf()),
                reload_inventory,
                runtime_path: None,
            })
        }

        pub fn with_runtime_path(mut self, path: impl Into<PathBuf>) -> Self {
            self.runtime_path = Some(path.into());
            self
        }

        pub fn start_runtime_writer(
            &self,
            path: impl AsRef<Path>,
            refresh_interval: Duration,
        ) -> Result<RuntimeWriterHandle, String> {
            start_runtime_writer(
                self.inventory.clone(),
                path.as_ref().to_path_buf(),
                refresh_interval,
            )
        }

        #[allow(dead_code)]
        pub fn inventory(&self) -> &TestGresInventory {
            &self.inventory
        }
    }

    impl Default for TestGresCollector {
        fn default() -> Self {
            Self::new("test-host")
        }
    }

    impl GresCollector for TestGresCollector {
        fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode> {
            let inventory = if self.reload_inventory {
                let path = self
                    .inventory_path
                    .as_deref()
                    .ok_or(ErrorCode::ConfigInvalid)?;
                match read_inventory_file(path) {
                    Ok(inventory) => inventory,
                    Err(error) => {
                        eprintln!(
                            "{}",
                            serde_json::json!({
                                "level": "WARN",
                                "event": "test_collector_error",
                                "path": path.display().to_string(),
                                "message": error,
                            })
                        );
                        return Err(ErrorCode::ConfigInvalid);
                    }
                }
            } else {
                self.inventory.clone()
            };
            let runtime = match self.runtime_path.as_deref() {
                Some(path) => {
                    read_runtime_mmap(path).unwrap_or_else(|_| initial_runtime(&inventory))
                }
                None => initial_runtime(&inventory),
            };
            Ok(snapshot_from_inventory_runtime(&inventory, &runtime))
        }
    }

    pub fn read_inventory_file(path: impl AsRef<Path>) -> Result<TestGresInventory, String> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read test inventory {} failed: {}", path.display(), e))?;
        let inventory: TestGresInventory = serde_json::from_str(&raw)
            .map_err(|e| format!("parse test inventory {} failed: {}", path.display(), e))?;
        validate_inventory(&inventory)?;
        Ok(inventory)
    }

    #[allow(dead_code)]
    pub fn write_inventory_file(
        path: impl AsRef<Path>,
        inventory: &TestGresInventory,
    ) -> Result<(), String> {
        let path = path.as_ref();
        let raw = serde_json::to_vec_pretty(inventory)
            .map_err(|e| format!("encode test inventory failed: {e}"))?;
        std::fs::write(path, raw)
            .map_err(|e| format!("write test inventory {} failed: {}", path.display(), e))
    }

    pub fn start_runtime_writer(
        inventory: TestGresInventory,
        path: PathBuf,
        refresh_interval: Duration,
    ) -> Result<RuntimeWriterHandle, String> {
        validate_inventory(&inventory)?;
        init_runtime_file(&path, DEFAULT_RUNTIME_CAPACITY)?;
        write_runtime_mmap(&path, &initial_runtime(&inventory))?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut tick = 0u64;
            while !thread_stop.load(Ordering::SeqCst) {
                let state = runtime_for_tick(&inventory, tick);
                let _ = write_runtime_mmap(&path, &state);
                tick = tick.wrapping_add(1);
                thread::sleep(refresh_interval.max(Duration::from_millis(1)));
            }
        });

        Ok(RuntimeWriterHandle {
            stop,
            thread: Some(thread),
        })
    }

    pub fn init_runtime_file(path: impl AsRef<Path>, capacity: usize) -> Result<(), String> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create runtime dir {} failed: {}", parent.display(), e))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("open runtime mmap {} failed: {}", path.display(), e))?;
        file.set_len(capacity as u64)
            .map_err(|e| format!("resize runtime mmap {} failed: {}", path.display(), e))
    }

    pub fn read_runtime_mmap(path: impl AsRef<Path>) -> Result<TestGresRuntimeState, String> {
        let path = path.as_ref();
        let file = File::open(path)
            .map_err(|e| format!("open runtime mmap {} failed: {}", path.display(), e))?;
        let mmap = unsafe { Mmap::map(&file) }
            .map_err(|e| format!("map runtime mmap {} failed: {}", path.display(), e))?;
        if mmap.len() < RUNTIME_HEADER_LEN {
            return Err(format!("runtime mmap {} is too small", path.display()));
        }
        let mut len_buf = [0u8; RUNTIME_HEADER_LEN];
        len_buf.copy_from_slice(&mmap[..RUNTIME_HEADER_LEN]);
        let len = u64::from_le_bytes(len_buf) as usize;
        if len == 0 || RUNTIME_HEADER_LEN + len > mmap.len() {
            return Err(format!(
                "runtime mmap {} contains invalid length",
                path.display()
            ));
        }
        serde_json::from_slice(&mmap[RUNTIME_HEADER_LEN..RUNTIME_HEADER_LEN + len])
            .map_err(|e| format!("parse runtime mmap {} failed: {}", path.display(), e))
    }

    pub fn write_runtime_mmap(
        path: impl AsRef<Path>,
        state: &TestGresRuntimeState,
    ) -> Result<(), String> {
        let path = path.as_ref();
        let payload =
            serde_json::to_vec(state).map_err(|e| format!("encode runtime state failed: {e}"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open runtime mmap {} failed: {}", path.display(), e))?;
        let mut mmap = unsafe { MmapMut::map_mut(&file) }
            .map_err(|e| format!("map runtime mmap {} failed: {}", path.display(), e))?;
        if RUNTIME_HEADER_LEN + payload.len() > mmap.len() {
            return Err(format!(
                "runtime mmap {} capacity {} is too small for {} byte payload",
                path.display(),
                mmap.len(),
                payload.len()
            ));
        }
        mmap[..RUNTIME_HEADER_LEN].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        mmap[RUNTIME_HEADER_LEN..RUNTIME_HEADER_LEN + payload.len()].copy_from_slice(&payload);
        mmap.flush()
            .map_err(|e| format!("flush runtime mmap {} failed: {}", path.display(), e))
    }

    pub fn initial_runtime(inventory: &TestGresInventory) -> TestGresRuntimeState {
        runtime_for_tick(inventory, 0)
    }

    pub fn runtime_for_tick(inventory: &TestGresInventory, tick: u64) -> TestGresRuntimeState {
        TestGresRuntimeState {
            gres: inventory
                .gres
                .iter()
                .map(|resource| {
                    let idx = resource.index as u64;
                    let max_used = resource.memory_total_mb.saturating_sub(1).max(1);
                    let base_used = 1_234 + idx * 512;
                    let used = (base_used + tick * 257) % max_used;
                    let used = used.max(1).min(max_used);
                    let base_util = 87u64.saturating_sub(idx * 7).max(1);
                    let util = ((base_util + tick * 13) % 101).max(1) as u8;
                    let memory_percent = used.saturating_mul(100) / resource.memory_total_mb.max(1);
                    TestGresRuntimeResource {
                        index: resource.index,
                        temperature_c: Some(28 + ((tick + idx) % 55) as u32),
                        memory_used_mb: used,
                        utilization_gres_percent: util,
                        utilization_memory_percent: memory_percent.min(100) as u8,
                        processes: vec![
                            TestGresRuntimeProcess {
                                pid: 4_242 + resource.index as u32 * 10,
                                uid: 1000 + resource.index as u32,
                                used_memory_mb: 768 + idx * 128 + tick % 64,
                            },
                            TestGresRuntimeProcess {
                                pid: 4_243 + resource.index as u32 * 10,
                                uid: 2000 + resource.index as u32,
                                used_memory_mb: 128,
                            },
                        ],
                    }
                })
                .collect(),
        }
    }

    pub fn default_inventory(hostname: String, gres_count: u8) -> TestGresInventory {
        TestGresInventory {
            hostname: hostname.clone(),
            driver_version: Some("test-driver".to_string()),
            gres: (0..gres_count)
                .map(|index| TestGresInventoryResource {
                    index,
                    name: format!("NVIDIA Test GPU {index}"),
                    uuid: Some(format!(
                        "GRES-TEST-{}-{index:04}",
                        sanitize_uuid_part(&hostname)
                    )),
                    memory_total_mb: 16_384 + index as u64 * 1_024,
                })
                .collect(),
        }
    }

    #[allow(dead_code)]
    pub fn deterministic_inventory(seed: u64, hostname: String) -> TestGresInventory {
        const MEMORY_GIB: [u64; 7] = [8, 16, 24, 36, 48, 80, 96];
        let count = (seed % 8 + 1) as u8;
        TestGresInventory {
            hostname: hostname.clone(),
            driver_version: Some(format!("test-driver-{seed}")),
            gres: (0..count)
                .map(|index| {
                    let mem_gib =
                        MEMORY_GIB[((seed + index as u64 * 3) as usize) % MEMORY_GIB.len()];
                    TestGresInventoryResource {
                        index,
                        name: format!("NVIDIA Test GPU {index}"),
                        uuid: Some(format!(
                            "GRES-TEST-{}-{index:04}",
                            sanitize_uuid_part(&hostname)
                        )),
                        memory_total_mb: mem_gib * 1024,
                    }
                })
                .collect(),
        }
    }

    fn snapshot_from_inventory_runtime(
        inventory: &TestGresInventory,
        runtime: &TestGresRuntimeState,
    ) -> GresNodeSnapshot {
        let runtime_by_index = runtime
            .gres
            .iter()
            .map(|resource| (resource.index, resource))
            .collect::<std::collections::HashMap<_, _>>();
        GresNodeSnapshot {
            hostname: inventory.hostname.clone(),
            driver_version: inventory.driver_version.clone(),
            resources: inventory
                .gres
                .iter()
                .map(|resource| {
                    let runtime = runtime_by_index.get(&resource.index);
                    let used = runtime
                        .map(|runtime| runtime.memory_used_mb)
                        .unwrap_or_default()
                        .min(resource.memory_total_mb);
                    GresResource {
                        kind: GresResourceKind::Nvml,
                        index: resource.index,
                        name: resource.name.clone(),
                        uuid: resource.uuid.clone(),
                        temperature_c: runtime.and_then(|runtime| runtime.temperature_c),
                        memory_used_mb: used,
                        memory_total_mb: resource.memory_total_mb,
                        utilization_gres_percent: runtime
                            .map(|runtime| runtime.utilization_gres_percent.min(100))
                            .unwrap_or_default(),
                        utilization_memory_percent: runtime
                            .map(|runtime| runtime.utilization_memory_percent.min(100))
                            .unwrap_or_default(),
                        processes: runtime
                            .map(|runtime| {
                                runtime
                                    .processes
                                    .iter()
                                    .map(|process| GresProcess {
                                        pid: process.pid,
                                        uid: process.uid,
                                        used_memory_mb: process.used_memory_mb,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }
                })
                .collect(),
        }
    }

    fn validate_inventory(inventory: &TestGresInventory) -> Result<(), String> {
        if inventory.hostname.trim().is_empty() {
            return Err("test inventory hostname must not be empty".to_string());
        }
        let snapshot = snapshot_from_inventory_runtime(inventory, &initial_runtime(inventory));
        super::validate_gres_node_snapshot_contract(&snapshot)
    }

    fn sanitize_uuid_part(input: &str) -> String {
        input
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect()
    }

    #[allow(dead_code)]
    pub fn runtime_refresh_interval() -> Duration {
        Duration::from_millis(DEFAULT_REFRESH_MS)
    }

    #[allow(dead_code)]
    pub fn dump_runtime_json(path: impl AsRef<Path>) -> Result<String, String> {
        let state = read_runtime_mmap(path)?;
        serde_json::to_string_pretty(&state).map_err(|e| format!("encode runtime json failed: {e}"))
    }

    #[allow(dead_code)]
    fn read_all(path: impl AsRef<Path>) -> Result<Vec<u8>, String> {
        let mut file = File::open(path.as_ref())
            .map_err(|e| format!("open {} failed: {}", path.as_ref().display(), e))?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)
            .map_err(|e| format!("read {} failed: {}", path.as_ref().display(), e))?;
        Ok(out)
    }

    #[allow(dead_code)]
    fn write_all(path: impl AsRef<Path>, data: &[u8]) -> Result<(), String> {
        let mut file = File::create(path.as_ref())
            .map_err(|e| format!("create {} failed: {}", path.as_ref().display(), e))?;
        file.write_all(data)
            .map_err(|e| format!("write {} failed: {}", path.as_ref().display(), e))
    }
}

#[cfg(any(test, feature = "test-collector"))]
#[allow(unused_imports)]
pub use test_collector::{
    deterministic_inventory, init_runtime_file, initial_runtime, read_inventory_file,
    read_runtime_mmap, runtime_for_tick, start_runtime_writer, write_inventory_file,
    write_runtime_mmap, RuntimeWriterHandle, TestGresCollector, TestGresInventory,
    TestGresInventoryResource, TestGresRuntimeProcess, TestGresRuntimeResource,
    TestGresRuntimeState,
};

#[cfg(test)]
#[derive(Debug)]
pub struct DegradedCollector {
    code: ErrorCode,
}

#[cfg(test)]
impl DegradedCollector {
    pub fn new(code: ErrorCode) -> Self {
        Self { code }
    }
}

#[cfg(test)]
impl GresCollector for DegradedCollector {
    fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode> {
        Err(self.code)
    }
}

#[cfg(test)]
pub fn assert_gres_collector_contract(collector: &dyn GresCollector) {
    let snapshot = collector
        .collect_gres()
        .expect("GRES collector should return a normalized snapshot");
    validate_gres_node_snapshot_contract(&snapshot)
        .expect("GRES collector returned a non-normalized snapshot");

    let protocol_snapshot = snapshot.into_gres_snapshot();
    let metadata = common::HostMetadata::from_snapshot(&protocol_snapshot);
    let runtime = common::RuntimeSnapshot::from_snapshot(&protocol_snapshot);
    let rebuilt = runtime.to_snapshot(&metadata);
    assert_eq!(
        rebuilt, protocol_snapshot,
        "GRES metadata/runtime split must rebuild the original protocol snapshot"
    );

    let payload =
        common::encode_snapshot_payload(&protocol_snapshot).expect("encode GRES snapshot payload");
    let decoded = common::decode_snapshot_payload(&payload).expect("decode GRES snapshot payload");
    assert_eq!(
        decoded, protocol_snapshot,
        "GRES snapshot must round-trip through the binary payload format"
    );
}

#[cfg(any(test, feature = "test-collector"))]
pub fn validate_gres_node_snapshot_contract(snapshot: &GresNodeSnapshot) -> Result<(), String> {
    if snapshot.hostname.trim().is_empty() {
        return Err("hostname must not be empty".to_string());
    }
    if snapshot
        .driver_version
        .as_deref()
        .is_some_and(|version| version.trim().is_empty())
    {
        return Err("driver_version must be omitted instead of empty".to_string());
    }

    let mut expected_index = 0u8;
    let mut uuids = std::collections::HashSet::new();

    for resource in &snapshot.resources {
        if resource.index != expected_index {
            return Err(format!(
                "resource indices must be dense and zero-based; expected {}, got {}",
                expected_index, resource.index
            ));
        }
        expected_index = expected_index
            .checked_add(1)
            .ok_or_else(|| "resource index overflow".to_string())?;

        if resource.name.trim().is_empty() {
            return Err(format!(
                "resource {} name must not be empty",
                resource.index
            ));
        }
        if resource.memory_total_mb == 0 {
            return Err(format!(
                "resource {} memory_total_mb must be greater than zero",
                resource.index
            ));
        }
        if resource.memory_used_mb > resource.memory_total_mb {
            return Err(format!(
                "resource {} memory_used_mb must not exceed memory_total_mb",
                resource.index
            ));
        }
        if resource.utilization_gres_percent > 100 {
            return Err(format!(
                "resource {} utilization_gres_percent must be <= 100",
                resource.index
            ));
        }
        if resource.utilization_memory_percent > 100 {
            return Err(format!(
                "resource {} utilization_memory_percent must be <= 100",
                resource.index
            ));
        }

        if let Some(uuid) = resource.uuid.as_deref() {
            if uuid.trim().is_empty() {
                return Err(format!(
                    "resource {} uuid must be omitted instead of empty",
                    resource.index
                ));
            }
            if !uuids.insert(uuid.to_string()) {
                return Err(format!("resource uuid {} is duplicated", uuid));
            }
        }

        for process in &resource.processes {
            if process.pid == 0 {
                return Err(format!(
                    "resource {} process pid must not be zero",
                    resource.index
                ));
            }
            if process.used_memory_mb > resource.memory_total_mb {
                return Err(format!(
                    "resource {} process {} used memory exceeds resource total memory",
                    resource.index, process.pid
                ));
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct NvmlCollector {
    #[cfg(feature = "nvml")]
    inner: real_nvml::RealNvmlCollector,
}

impl NvmlCollector {
    pub fn new(
        hostname: impl Into<String>,
        nvml_lib_path: Option<&str>,
    ) -> Result<Self, ErrorCode> {
        if simulate_nvml_missing() {
            return Err(ErrorCode::NvmlUnavailable);
        }

        #[cfg(feature = "nvml")]
        {
            real_nvml::RealNvmlCollector::new(hostname.into(), nvml_lib_path)
                .map(|inner| Self { inner })
        }

        #[cfg(not(feature = "nvml"))]
        {
            let _ = hostname.into();
            let _ = nvml_lib_path;
            Err(ErrorCode::NvmlUnavailable)
        }
    }
}

impl GresCollector for NvmlCollector {
    fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode> {
        #[cfg(feature = "nvml")]
        {
            self.inner.collect_gres()
        }

        #[cfg(not(feature = "nvml"))]
        {
            Err(ErrorCode::NvmlUnavailable)
        }
    }
}

fn simulate_nvml_missing() -> bool {
    env_truthy("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING")
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

#[cfg(feature = "nvml")]
mod real_nvml {
    use chrono::Local;
    use common::{
        ErrorCode, GresInfo, GresMemory, GresProcessInfo, GresUtilization, ServerGresSnapshot,
    };
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    use nvml_wrapper::enums::device::UsedGpuMemory;
    use nvml_wrapper::Nvml;
    use std::{collections::HashSet, ffi::OsStr, fmt::Debug, fs};

    use crate::collector::{GresCollector, GresNodeSnapshot};

    #[derive(Debug)]
    pub struct RealNvmlCollector {
        hostname: String,
        nvml: Nvml,
    }

    impl RealNvmlCollector {
        pub fn new(hostname: String, nvml_lib_path: Option<&str>) -> Result<Self, ErrorCode> {
            let init_result = match nvml_lib_path {
                Some(path) if !path.trim().is_empty() => {
                    Nvml::builder().lib_path(OsStr::new(path.trim())).init()
                }
                _ => Nvml::init(),
            };

            init_result
                .map(|nvml| Self { hostname, nvml })
                .map_err(|e| {
                    log_nvml_error("init", &e);
                    ErrorCode::NvmlUnavailable
                })
        }
    }

    impl RealNvmlCollector {
        fn collect_gres_snapshot(&self) -> Result<ServerGresSnapshot, ErrorCode> {
            let count = self.nvml.device_count().map_err(|e| {
                log_nvml_error("device_count", &e);
                ErrorCode::NvmlUnavailable
            })?;
            let mut gres = Vec::with_capacity(count as usize);

            for index in 0..count.min(u8::MAX as u32) {
                let device = self.nvml.device_by_index(index).map_err(|e| {
                    log_nvml_error("device_by_index", &e);
                    ErrorCode::NvmlUnavailable
                })?;
                let utilization = device.utilization_rates().map_err(|e| {
                    log_nvml_error("utilization_rates", &e);
                    ErrorCode::NvmlUnavailable
                })?;
                let memory = device.memory_info().map_err(|e| {
                    log_nvml_error("memory_info", &e);
                    ErrorCode::NvmlUnavailable
                })?;
                let temperature_c = device.temperature(TemperatureSensor::Gpu).ok();
                let processes = collect_processes(&device);

                gres.push(GresInfo {
                    index: index as u8,
                    name: device.name().unwrap_or_else(|_| "unknown".to_string()),
                    temperature_c,
                    uuid: device.uuid().ok(),
                    memory: GresMemory {
                        used_mb: memory.used / 1024 / 1024,
                        total_mb: memory.total / 1024 / 1024,
                    },
                    utilization: GresUtilization {
                        gres_percent: utilization.gpu.min(100) as u8,
                        memory_percent: utilization.memory.min(100) as u8,
                    },
                    processes,
                });
            }

            Ok(ServerGresSnapshot {
                hostname: self.hostname.clone(),
                driver_version: self.nvml.sys_driver_version().ok(),
                gres,
            })
        }
    }

    impl GresCollector for RealNvmlCollector {
        fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode> {
            self.collect_gres_snapshot()
                .map(GresNodeSnapshot::from_gres_snapshot)
        }
    }

    fn log_nvml_error(context: &str, error: &impl Debug) {
        let time = Local::now().format("%Y-%m-%d %H:%M:%S");
        let hint = if context == "init" {
            "If this happened at startup, configure [runtime].nvml_lib_path to a real NVIDIA NVML library, for example /usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1. Do not use CUDA stubs/libnvidia-ml.so."
        } else {
            "NVML returned an error during collection; verify the NVIDIA driver is healthy and accessible to the gpustat4cluster service user."
        };
        eprintln!(
            "{}",
            serde_json::json!({
                "time": time.to_string(),
                "level": "WARN",
                "event": "nvml_error",
                "context": context,
                "error": format!("{:?}", error),
                "hint": hint,
            })
        );
    }

    fn collect_processes(device: &nvml_wrapper::Device<'_>) -> Vec<GresProcessInfo> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for process in device
            .running_compute_processes()
            .unwrap_or_default()
            .into_iter()
            .chain(device.running_graphics_processes().unwrap_or_default())
        {
            if !seen.insert(process.pid) {
                continue;
            }
            out.push(GresProcessInfo {
                pid: process.pid,
                uid: uid_for_pid(process.pid).unwrap_or(u32::MAX),
                command: command_for_pid(process.pid),
                used_memory_mb: used_gres_memory_mb(process.used_gpu_memory),
            });
        }
        out
    }

    fn used_gres_memory_mb(memory: UsedGpuMemory) -> u64 {
        match memory {
            UsedGpuMemory::Used(bytes) => bytes / 1024 / 1024,
            UsedGpuMemory::Unavailable => 0,
        }
    }

    fn command_for_pid(pid: u32) -> Option<String> {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn uid_for_pid(pid: u32) -> Option<u32> {
        fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()?
            .lines()
            .find(|line| line.starts_with("Uid:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|raw| raw.parse::<u32>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gres_collector_returns_expected_snapshot() {
        let snapshot = TestGresCollector::new("node-test")
            .collect_gres_snapshot()
            .expect("test collect");
        assert_eq!(snapshot.hostname, "node-test");
        assert_eq!(snapshot.gres.len(), 1);
        assert_eq!(snapshot.gres[0].utilization.gres_percent, 87);
        assert_eq!(snapshot.gres[0].name, "NVIDIA Test GPU 0");
        assert_eq!(snapshot.gres[0].utilization.gres_percent, 87);
        assert_eq!(snapshot.gres[0].memory.used_mb, 1_234);
        assert_eq!(snapshot.gres[0].memory.total_mb, 16_384);
        assert_eq!(snapshot.gres[0].processes.len(), 2);
        assert_eq!(snapshot.gres[0].processes[0].uid, 1000);
    }

    #[test]
    fn test_gres_collector_generates_multi_gres_multi_process_snapshot() {
        let snapshot = TestGresCollector::with_gres_count("node-a", 2)
            .collect_gres_snapshot()
            .expect("test collect");

        assert_eq!(snapshot.hostname, "node-a");
        assert_eq!(snapshot.gres.len(), 2);
        assert_eq!(snapshot.gres[0].index, 0);
        assert_eq!(snapshot.gres[1].index, 1);
        assert_eq!(
            snapshot.gres[0].uuid.as_deref(),
            Some("GRES-TEST-node-a-0000")
        );
        assert_eq!(
            snapshot.gres[1].uuid.as_deref(),
            Some("GRES-TEST-node-a-0001")
        );
        assert_eq!(snapshot.gres[0].processes.len(), 2);
        assert_eq!(snapshot.gres[1].processes.len(), 2);
        assert_eq!(snapshot.gres[1].processes[0].command, None);
    }

    #[test]
    fn test_gres_collector_loads_inventory_json_and_runtime_mmap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inventory_path = tmp.path().join("inventory.json");
        let runtime_path = tmp.path().join("runtime.mmap");
        let inventory = deterministic_inventory(42, "node-json".to_string());
        write_inventory_file(&inventory_path, &inventory).expect("write inventory");

        let collector = TestGresCollector::from_inventory_file(&inventory_path)
            .expect("load inventory")
            .with_runtime_path(&runtime_path);
        init_runtime_file(&runtime_path, 64 * 1024).expect("init runtime");
        write_runtime_mmap(&runtime_path, &runtime_for_tick(&inventory, 7)).expect("write runtime");

        let snapshot = collector.collect_gres().expect("collect");
        validate_gres_node_snapshot_contract(&snapshot).expect("contract");
        assert_eq!(snapshot.hostname, "node-json");
        assert_eq!(snapshot.resources.len(), inventory.gres.len());
        assert_eq!(
            snapshot.resources[0].memory_total_mb,
            inventory.gres[0].memory_total_mb
        );
        assert_eq!(
            snapshot.resources[0].memory_used_mb,
            runtime_for_tick(&inventory, 7).gres[0].memory_used_mb
        );
    }

    #[test]
    fn test_gres_runtime_writer_updates_mmap_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime_path = tmp.path().join("runtime.mmap");
        let inventory = deterministic_inventory(7, "node-runtime".to_string());
        let collector = TestGresCollector::from_inventory(inventory.clone());
        let writer = collector
            .start_runtime_writer(&runtime_path, std::time::Duration::from_millis(5))
            .expect("start writer");

        std::thread::sleep(std::time::Duration::from_millis(20));
        let first = read_runtime_mmap(&runtime_path).expect("read first runtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = read_runtime_mmap(&runtime_path).expect("read second runtime");
        writer.stop();

        assert_eq!(first.gres.len(), inventory.gres.len());
        assert_eq!(second.gres.len(), inventory.gres.len());
        assert_ne!(
            first.gres[0].memory_used_mb, second.gres[0].memory_used_mb,
            "runtime writer should update dynamic fields"
        );
    }

    #[test]
    fn deterministic_inventory_covers_expected_memory_sizes_and_counts() {
        let allowed = [8, 16, 24, 36, 48, 80, 96]
            .into_iter()
            .map(|gib| gib * 1024)
            .collect::<std::collections::HashSet<u64>>();
        for seed in 0..64 {
            let inventory = deterministic_inventory(seed, format!("node-{seed}"));
            assert!((1..=8).contains(&inventory.gres.len()));
            for resource in &inventory.gres {
                assert!(allowed.contains(&resource.memory_total_mb));
            }
            let collector = TestGresCollector::from_inventory(inventory);
            assert_gres_collector_contract(&collector);
        }
    }

    #[test]
    fn gres_collector_contract_accepts_normalized_test_collector() {
        let collector = TestGresCollector::with_gres_count("node-a", 2);
        assert_gres_collector_contract(&collector);
    }

    #[test]
    fn gres_snapshot_contract_accepts_empty_resource_inventory() {
        let snapshot = GresNodeSnapshot {
            hostname: "node-empty".to_string(),
            driver_version: None,
            resources: Vec::new(),
        };
        validate_gres_node_snapshot_contract(&snapshot).expect("empty inventory is valid");
    }

    #[test]
    fn gres_snapshot_contract_rejects_non_dense_indices() {
        let mut snapshot = TestGresCollector::with_gres_count("node-a", 2)
            .collect_gres()
            .expect("test collect");
        snapshot.resources[1].index = 3;

        let error = validate_gres_node_snapshot_contract(&snapshot).unwrap_err();
        assert!(error.contains("dense and zero-based"), "{error}");
    }

    #[test]
    fn gres_snapshot_contract_rejects_invalid_resource_fields() {
        let mut snapshot = TestGresCollector::new("node-a")
            .collect_gres()
            .expect("test collect");
        snapshot.resources[0].name.clear();
        snapshot.resources[0].memory_used_mb = 2;
        snapshot.resources[0].memory_total_mb = 1;
        snapshot.resources[0].utilization_gres_percent = 101;

        let error = validate_gres_node_snapshot_contract(&snapshot).unwrap_err();
        assert!(error.contains("name must not be empty"), "{error}");
    }

    #[test]
    fn gres_snapshot_contract_rejects_duplicate_uuid() {
        let mut snapshot = TestGresCollector::with_gres_count("node-a", 2)
            .collect_gres()
            .expect("test collect");
        snapshot.resources[1].uuid = snapshot.resources[0].uuid.clone();

        let error = validate_gres_node_snapshot_contract(&snapshot).unwrap_err();
        assert!(error.contains("duplicated"), "{error}");
    }

    #[test]
    fn gres_snapshot_contract_rejects_invalid_process_fields() {
        let mut snapshot = TestGresCollector::new("node-a")
            .collect_gres()
            .expect("test collect");
        snapshot.resources[0].processes[0].pid = 0;

        let error = validate_gres_node_snapshot_contract(&snapshot).unwrap_err();
        assert!(error.contains("process pid must not be zero"), "{error}");
    }

    #[test]
    fn degraded_collector_returns_error() {
        let collector = DegradedCollector::new(ErrorCode::NvmlUnavailable);
        assert_eq!(
            collector.collect_gres().unwrap_err(),
            ErrorCode::NvmlUnavailable
        );
    }
}
