use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::RuntimeError;

mod docker;

pub use docker::DockerHealthChecker;

/// What must hold for a workload to count as ready, and until when to wait.
#[derive(Clone, Debug)]
pub struct ReadinessRequest {
    /// Container name. It doubles as the DNS name the route will use.
    pub container: String,
    pub port: u16,
    /// When the wait started, which may be before the container was created.
    pub started_at: Instant,
    pub timeout: Duration,
    /// Restarts the container may already have. `None` takes the first observed
    /// count, for a workload with earlier restarts that are no longer relevant.
    pub expected_restarts: Option<u32>,
    /// Names the operation when Docker itself never answers before the deadline.
    pub operation: &'static str,
}

#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn wait_until_ready(&self, request: &ReadinessRequest) -> Result<(), RuntimeError>;
}
