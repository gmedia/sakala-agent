use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::RuntimeError;

mod docker;
mod host_check;

pub use docker::DockerHealthChecker;
pub use host_check::HostCheck;

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
    /// The domain visitors use. When set, an answer of 400 or 403 is checked once
    /// more with this `Host`, so a framework that refuses it fails readiness.
    pub host: Option<String>,
}

/// What must hold for a worker to count as started: it keeps running,
/// without a restart, for `window` after `started_at`. A worker serves no
/// port, so nothing else can be checked.
#[derive(Clone, Debug)]
pub struct StabilityRequest {
    pub container: String,
    /// The worker process, for the failure message.
    pub process: String,
    /// When the container was started.
    pub started_at: Instant,
    pub window: Duration,
}

#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn wait_until_ready(&self, request: &ReadinessRequest) -> Result<(), RuntimeError>;

    /// Waits out the request's window and fails with
    /// [`RuntimeError::WorkerStartFailed`] as soon as the container exits or
    /// Docker restarts it, whatever its exit code.
    async fn wait_until_stable(&self, request: &StabilityRequest) -> Result<(), RuntimeError>;
}
