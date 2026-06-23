use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    process::run_checked, routing::CaddyReloader,
};

pub struct DockerExecCaddyReloader {
    runner: Arc<dyn ProcessRunner>,
    container: String,
}

impl DockerExecCaddyReloader {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, container: String) -> Self {
        Self { runner, container }
    }

    fn command(&self, action: &str) -> CommandSpec {
        CommandSpec::new("docker")
            .arg("exec")
            .arg(&self.container)
            .arg("caddy")
            .arg(action)
            .arg("--config")
            .arg("/etc/caddy/Caddyfile")
            .arg("--adapter")
            .arg("caddyfile")
    }
}

#[async_trait]
impl CaddyReloader for DockerExecCaddyReloader {
    async fn validate_and_reload(
        &self,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        for action in ["validate", "reload"] {
            run_checked(
                self.runner.as_ref(),
                &self.command(action),
                &format!("caddy-{action}"),
                reporter,
            )
            .await?;
        }
        Ok(())
    }

    async fn reload_after_rollback(&self) {
        let _ = self
            .runner
            .run(&self.command("reload"), &NullOutputSink)
            .await;
    }
}
