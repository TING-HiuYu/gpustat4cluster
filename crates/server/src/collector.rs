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

#[cfg(test)]
#[derive(Debug)]
pub struct TestGresCollector {
    hostname: String,
    gres_count: u8,
}

#[cfg(test)]
impl TestGresCollector {
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            gres_count: 1,
        }
    }

    pub fn with_gres_count(hostname: impl Into<String>, gres_count: u8) -> Self {
        Self {
            hostname: hostname.into(),
            gres_count: gres_count.max(1),
        }
    }
}

#[cfg(test)]
impl Default for TestGresCollector {
    fn default() -> Self {
        Self::new("test-host")
    }
}

#[cfg(test)]
impl GresCollector for TestGresCollector {
    fn collect_gres(&self) -> Result<GresNodeSnapshot, ErrorCode> {
        Ok(GresNodeSnapshot::from_gres_snapshot(test_snapshot(
            self.hostname.clone(),
            self.gres_count,
        )))
    }
}

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

#[cfg(test)]
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

#[cfg(test)]
fn test_snapshot(hostname: String, gres_count: u8) -> ServerGresSnapshot {
    ServerGresSnapshot {
        hostname: hostname.clone(),
        driver_version: Some("test-driver".to_string()),
        gres: (0..gres_count)
            .map(|index| {
                let index_u64 = index as u64;
                GresInfo {
                    index,
                    name: format!("NVIDIA Test GPU {index}"),
                    temperature_c: Some(30 + index as u32),
                    uuid: Some(format!(
                        "GRES-TEST-{}-{index:04}",
                        sanitize_uuid_part(&hostname)
                    )),
                    memory: GresMemory {
                        used_mb: 1_234 + index_u64 * 512,
                        total_mb: 16_384 + index_u64 * 1_024,
                    },
                    utilization: GresUtilization {
                        gres_percent: (87u16.saturating_sub(index as u16 * 7)).max(1) as u8,
                        memory_percent: (8 + index).min(100),
                    },
                    processes: vec![
                        GresProcessInfo {
                            pid: 4_242 + index as u32 * 10,
                            uid: 1000 + index as u32,
                            command: Some(format!("python train_gres_{index}.py")),
                            used_memory_mb: 768 + index_u64 * 128,
                        },
                        GresProcessInfo {
                            pid: 4_243 + index as u32 * 10,
                            uid: 2000 + index as u32,
                            command: Some("nvidia-smi dmon".to_string()),
                            used_memory_mb: 128,
                        },
                    ],
                }
            })
            .collect(),
    }
}

#[cfg(test)]
fn sanitize_uuid_part(input: &str) -> String {
    input
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
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
