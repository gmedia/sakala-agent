use async_trait::async_trait;

use crate::RuntimeError;

mod docker;

pub use docker::DockerHealthChecker;

#[async_trait]
pub trait HealthChecker: Send + Sync {
    async fn wait_until_ready(&self, container: &str) -> Result<(), RuntimeError>;
}
