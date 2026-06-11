//! Chopsticks supervisor — owns the Chopsticks child process or attaches to a
//! running instance (ticket T02).

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::contracts::{ChopsticksSupervisor, ForkConfig, Result, WsEndpoint};

/// Spawns/attaches and supervises Chopsticks. (Stub — implemented in T02.)
pub struct Supervisor;

#[async_trait]
impl ChopsticksSupervisor for Supervisor {
    async fn start(&self, _cfg: &ForkConfig) -> Result<WsEndpoint> {
        todo!("T02: spawn/attach Chopsticks; resolve the listening ws endpoint")
    }

    fn log_lines(&self) -> broadcast::Receiver<String> {
        todo!("T02: boot/log line broadcast")
    }

    async fn shutdown(&self) -> Result<()> {
        todo!("T02: terminate the child process")
    }
}
