use std::{sync::Arc, time::Duration};

use tokio::time::sleep;

use async_trait::async_trait;

use crate::{CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, health::HealthChecker};

pub struct DockerHealthChecker {
    runner: Arc<dyn ProcessRunner>,
    attempts: u32,
    interval: Duration,
}

impl DockerHealthChecker {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, attempts: u32, interval: Duration) -> Self {
        Self {
            runner,
            attempts,
            interval,
        }
    }
}

#[async_trait]
impl HealthChecker for DockerHealthChecker {
    async fn wait_until_ready(&self, container: &str) -> Result<(), RuntimeError> {
        for attempt in 1..=self.attempts {
            let output = self.runner
            .run(
                &CommandSpec::new("docker")
                    .arg("inspect")
                    .arg("--format")
                    .arg("{{if .State.Health}}{{.State.Health.Status}}{{else if .State.Running}}running{{else}}stopped{{end}}")
                    .arg(container),
                &NullOutputSink,
            )
            .await?;
            let status = output.stdout.trim();

            if output.success && matches!(status, "healthy" | "running") {
                return Ok(());
            }

            if status == "unhealthy" || attempt == self.attempts {
                return Err(RuntimeError::Execution(format!(
                    "container {container} failed its basic health check with status {status:?}"
                )));
            }

            sleep(self.interval).await;
        }

        Err(RuntimeError::Execution(format!(
            "container {container} did not become healthy"
        )))
    }
}
