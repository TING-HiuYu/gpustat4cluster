use common::ServerGpuSnapshot;

pub trait SnapshotSummary {
    fn gpu_num(&self) -> u8;
    fn avg_utilization(&self) -> u8;
}

impl SnapshotSummary for ServerGpuSnapshot {
    fn gpu_num(&self) -> u8 {
        self.gpus.len().min(u8::MAX as usize) as u8
    }

    fn avg_utilization(&self) -> u8 {
        if self.gpus.is_empty() {
            return 0;
        }

        let total: u16 = self
            .gpus
            .iter()
            .map(|gpu| gpu.utilization.gpu_percent as u16)
            .sum();
        (total / self.gpus.len() as u16).min(100) as u8
    }
}
