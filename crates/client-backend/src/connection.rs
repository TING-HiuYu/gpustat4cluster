use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::ServerGresSnapshot;

pub type SharedServerConnection = Arc<dyn ServerConnection>;

pub trait ServerConnection: Send + Sync {
    fn protocol(&self) -> &'static str;
    fn addr(&self) -> SocketAddr;
    fn hostname(&self) -> String;
    fn gres_num(&self) -> u8;

    fn connection_count(&self) -> usize {
        1
    }

    fn query(&self, timeout: Duration) -> Result<ServerGresSnapshot, String>;

    fn heartbeat(&self) -> Result<(), String> {
        Ok(())
    }

    fn wants_heartbeat(&self) -> bool {
        false
    }

    fn disconnect(&self, reason: &str) -> Result<(), String>;
    fn close(&self);
}
