use common::ServerGresSnapshot;

pub trait SnapshotSummary {
    fn gres_num(&self) -> u8;
    fn gpu_num(&self) -> u8;
    fn avg_utilization(&self) -> u8;
}

impl SnapshotSummary for ServerGresSnapshot {
    fn gres_num(&self) -> u8 {
        self.gres.len().min(u8::MAX as usize) as u8
    }

    fn gpu_num(&self) -> u8 {
        self.gres_num()
    }

    fn avg_utilization(&self) -> u8 {
        if self.gres.is_empty() {
            return 0;
        }
        let total: u64 = self
            .gres
            .iter()
            .map(|gres| gres.utilization.gres_percent as u64)
            .sum();
        (total / self.gres.len() as u64).min(100) as u8
    }
}
