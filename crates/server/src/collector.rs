use common::{ErrorCode, ServerGpuSnapshot};
#[cfg(any(test, feature = "mock-nvml"))]
use common::{GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization};

pub const COLLECTOR_ENV: &str = "GPUSTAT4CLUSTER_COLLECTOR";
pub const FORCE_MOCK_ENV: &str = "GPUSTAT4CLUSTER_FORCE_MOCK";
#[cfg(any(test, feature = "mock-nvml"))]
pub const MOCK_HOSTNAME_ENV: &str = "GPUSTAT4CLUSTER_MOCK_HOSTNAME";
#[cfg(any(test, feature = "mock-nvml"))]
pub const MOCK_GPU_COUNT_ENV: &str = "GPUSTAT4CLUSTER_MOCK_GPU_COUNT";

#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub trait GpuCollector: Send + Sync {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode>;
}

#[cfg(any(test, feature = "mock-nvml"))]
#[derive(Debug)]
pub struct MockNvmlCollector {
    hostname: String,
    gpu_count: u8,
}

#[cfg(any(test, feature = "mock-nvml"))]
impl MockNvmlCollector {
    pub fn new(hostname: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            gpu_count: 1,
        }
    }

    pub fn from_env(default_hostname: impl Into<String>) -> Self {
        let hostname = std::env::var(MOCK_HOSTNAME_ENV).unwrap_or_else(|_| default_hostname.into());
        Self {
            hostname,
            gpu_count: mock_gpu_count_from_env(),
        }
    }

    #[cfg(test)]
    pub fn with_gpu_count(hostname: impl Into<String>, gpu_count: u8) -> Self {
        Self {
            hostname: hostname.into(),
            gpu_count: gpu_count.max(1),
        }
    }
}

#[cfg(any(test, feature = "mock-nvml"))]
impl Default for MockNvmlCollector {
    fn default() -> Self {
        Self::new("mock-host")
    }
}

#[cfg(any(test, feature = "mock-nvml"))]
impl GpuCollector for MockNvmlCollector {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode> {
        Ok(mock_snapshot(self.hostname.clone(), self.gpu_count))
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
impl GpuCollector for DegradedCollector {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode> {
        Err(self.code)
    }
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

impl GpuCollector for NvmlCollector {
    fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode> {
        #[cfg(feature = "nvml")]
        {
            self.inner.collect()
        }

        #[cfg(not(feature = "nvml"))]
        {
            Err(ErrorCode::NvmlUnavailable)
        }
    }
}

pub fn mock_nvml_requested_from_env() -> bool {
    std::env::var(COLLECTOR_ENV)
        .map(|v| v.eq_ignore_ascii_case("mock"))
        .unwrap_or(false)
        || env_truthy(FORCE_MOCK_ENV)
}

fn simulate_nvml_missing() -> bool {
    env_truthy("GPUSTAT4CLUSTER_SIMULATE_NVML_MISSING")
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

#[cfg(any(test, feature = "mock-nvml"))]
fn mock_gpu_count_from_env() -> u8 {
    std::env::var(MOCK_GPU_COUNT_ENV)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .map(|value| value.clamp(1, u8::MAX as u16) as u8)
        .unwrap_or(1)
}

#[cfg(any(test, feature = "mock-nvml"))]
fn mock_snapshot(hostname: String, gpu_count: u8) -> ServerGpuSnapshot {
    ServerGpuSnapshot {
        hostname: hostname.clone(),
        driver_version: Some("mock-driver".to_string()),
        gpus: (0..gpu_count)
            .map(|index| {
                let index_u64 = index as u64;
                GpuInfo {
                    index,
                    name: format!("NVIDIA Mock GPU {index}"),
                    temperature_c: Some(30 + index as u32),
                    uuid: Some(format!(
                        "GPU-MOCK-{}-{index:04}",
                        sanitize_uuid_part(&hostname)
                    )),
                    memory: GpuMemory {
                        used_mb: 1_234 + index_u64 * 512,
                        total_mb: 16_384 + index_u64 * 1_024,
                    },
                    utilization: GpuUtilization {
                        gpu_percent: (87u16.saturating_sub(index as u16 * 7)).max(1) as u8,
                        memory_percent: (8 + index).min(100),
                    },
                    processes: vec![
                        GpuProcessInfo {
                            pid: 4_242 + index as u32 * 10,
                            uid: 1000 + index as u32,
                            command: Some(format!("python train_gpu_{index}.py")),
                            used_memory_mb: 768 + index_u64 * 128,
                        },
                        GpuProcessInfo {
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

#[cfg(any(test, feature = "mock-nvml"))]
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
        ErrorCode, GpuInfo, GpuMemory, GpuProcessInfo, GpuUtilization, ServerGpuSnapshot,
    };
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    use nvml_wrapper::enums::device::UsedGpuMemory;
    use nvml_wrapper::Nvml;
    use std::{collections::HashSet, ffi::OsStr, fmt::Debug, fs};

    use crate::collector::GpuCollector;

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

    impl GpuCollector for RealNvmlCollector {
        fn collect(&self) -> Result<ServerGpuSnapshot, ErrorCode> {
            let count = self.nvml.device_count().map_err(|e| {
                log_nvml_error("device_count", &e);
                ErrorCode::NvmlUnavailable
            })?;
            let mut gpus = Vec::with_capacity(count as usize);

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

                gpus.push(GpuInfo {
                    index: index as u8,
                    name: device.name().unwrap_or_else(|_| "unknown".to_string()),
                    temperature_c,
                    uuid: device.uuid().ok(),
                    memory: GpuMemory {
                        used_mb: memory.used / 1024 / 1024,
                        total_mb: memory.total / 1024 / 1024,
                    },
                    utilization: GpuUtilization {
                        gpu_percent: utilization.gpu.min(100) as u8,
                        memory_percent: utilization.memory.min(100) as u8,
                    },
                    processes,
                });
            }

            Ok(ServerGpuSnapshot {
                hostname: self.hostname.clone(),
                driver_version: self.nvml.sys_driver_version().ok(),
                gpus,
            })
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

    fn collect_processes(device: &nvml_wrapper::Device<'_>) -> Vec<GpuProcessInfo> {
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
            out.push(GpuProcessInfo {
                pid: process.pid,
                uid: uid_for_pid(process.pid).unwrap_or(u32::MAX),
                command: command_for_pid(process.pid),
                used_memory_mb: used_gpu_memory_mb(process.used_gpu_memory),
            });
        }
        out
    }

    fn used_gpu_memory_mb(memory: UsedGpuMemory) -> u64 {
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
    use crate::model::SnapshotSummary;
    use common::{decode_snapshot_payload, encode_snapshot_payload};

    #[test]
    fn mock_nvml_collector_returns_expected_snapshot() {
        let snapshot = MockNvmlCollector::new("node-mock")
            .collect()
            .expect("mock collect");
        assert_eq!(snapshot.hostname, "node-mock");
        assert_eq!(snapshot.gpu_num(), 1);
        assert_eq!(snapshot.avg_utilization(), 87);
        assert_eq!(snapshot.gpus[0].name, "NVIDIA Mock GPU 0");
        assert_eq!(snapshot.gpus[0].utilization.gpu_percent, 87);
        assert_eq!(snapshot.gpus[0].memory.used_mb, 1_234);
        assert_eq!(snapshot.gpus[0].memory.total_mb, 16_384);
        assert_eq!(snapshot.gpus[0].processes.len(), 2);
        assert_eq!(snapshot.gpus[0].processes[0].uid, 1000);
    }

    #[test]
    fn mock_nvml_collector_generates_multi_gpu_multi_process_snapshot() {
        let snapshot = MockNvmlCollector::with_gpu_count("node-a", 2)
            .collect()
            .expect("mock collect");

        assert_eq!(snapshot.hostname, "node-a");
        assert_eq!(snapshot.gpus.len(), 2);
        assert_eq!(snapshot.gpus[0].index, 0);
        assert_eq!(snapshot.gpus[1].index, 1);
        assert_eq!(
            snapshot.gpus[0].uuid.as_deref(),
            Some("GPU-MOCK-node-a-0000")
        );
        assert_eq!(
            snapshot.gpus[1].uuid.as_deref(),
            Some("GPU-MOCK-node-a-0001")
        );
        assert_eq!(snapshot.gpus[0].processes.len(), 2);
        assert_eq!(snapshot.gpus[1].processes.len(), 2);
        assert_eq!(
            snapshot.gpus[1].processes[0].command.as_deref(),
            Some("python train_gpu_1.py")
        );
    }

    #[test]
    fn mock_nvml_env_and_payload_roundtrip_preserve_hostname_and_uuid() {
        let _guard = ENV_TEST_LOCK.lock().expect("env test lock");
        std::env::set_var(MOCK_HOSTNAME_ENV, "node-b");
        std::env::set_var(MOCK_GPU_COUNT_ENV, "2");

        let snapshot = MockNvmlCollector::from_env("fallback-node")
            .collect()
            .expect("mock collect");
        let payload = encode_snapshot_payload(&snapshot).expect("encode snapshot");
        let decoded = decode_snapshot_payload(&payload).expect("decode snapshot");

        std::env::remove_var(MOCK_HOSTNAME_ENV);
        std::env::remove_var(MOCK_GPU_COUNT_ENV);

        assert_eq!(decoded.hostname, "node-b");
        assert_eq!(decoded.gpus.len(), 2);
        assert_eq!(
            decoded.gpus[0].uuid.as_deref(),
            Some("GPU-MOCK-node-b-0000")
        );
        assert_eq!(
            decoded.gpus[1].uuid.as_deref(),
            Some("GPU-MOCK-node-b-0001")
        );
    }

    #[test]
    fn mock_nvml_env_is_detected() {
        let _guard = ENV_TEST_LOCK.lock().expect("env test lock");
        std::env::set_var(COLLECTOR_ENV, "mock");
        assert!(mock_nvml_requested_from_env());
        std::env::remove_var(COLLECTOR_ENV);

        std::env::set_var(FORCE_MOCK_ENV, "1");
        assert!(mock_nvml_requested_from_env());
        std::env::remove_var(FORCE_MOCK_ENV);
    }

    #[test]
    fn degraded_collector_returns_error() {
        let collector = DegradedCollector::new(ErrorCode::NvmlUnavailable);
        assert_eq!(collector.collect().unwrap_err(), ErrorCode::NvmlUnavailable);
    }
}
