use std::sync::Arc;

use async_trait::async_trait;

use tracing::{debug, warn};

use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    routing::CaddyReloader,
};

/// Caddy output kept for the operator when validation or reload fails.
const OPERATOR_OUTPUT_LINES: usize = 20;

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
    /// Caddy's output describes the shared router: its config path, its own
    /// log lines, and warnings about every site on the node, other tenants'
    /// included. It goes to the Agent's own log, never to the deployment's.
    async fn validate_and_reload(
        &self,
        _reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        for action in ["validate", "reload"] {
            let output = self
                .runner
                .run(&self.command(action), &NullOutputSink)
                .await?;
            if !output.success {
                let lines = output
                    .stdout
                    .lines()
                    .chain(output.stderr.lines())
                    .collect::<Vec<_>>();
                let tail = lines[lines.len().saturating_sub(OPERATOR_OUTPUT_LINES)..].join("\n");
                warn!(
                    container = %self.container,
                    action,
                    status = %crate::error::describe_exit(output.code),
                    output = %tail,
                    "Caddy refused the route configuration"
                );
                return Err(RuntimeError::Routing(format!(
                    "the router could not {action} the route configuration ({})",
                    crate::error::describe_exit(output.code)
                )));
            }
            debug!(container = %self.container, action, "Caddy accepted the route configuration");
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

/// Reads Caddy's request counters from its admin endpoint inside the router
/// container, keeping only the request counter lines: the full output repeats
/// every host in several histograms and would outgrow the captured output on a
/// busy node. The address is an IP, not `localhost`, because BusyBox `wget`
/// tries `::1` first and `admin localhost:2019` listens on `127.0.0.1` only. A
/// failed read exits non-zero; a read without any counter line is empty.
const CADDY_REQUEST_COUNTERS_SCRIPT: &str = "m=$(wget -qO- http://127.0.0.1:2019/metrics) || exit 1; \
printf '%s\\n' \"$m\" | grep '^caddy_http_requests_total' || true";

/// Reads Caddy's request counters through its local admin endpoint. The agent
/// only reads them to judge idleness; it never serves or forwards them.
pub struct DockerExecCaddyMetrics {
    runner: Arc<dyn ProcessRunner>,
    container: String,
}

impl DockerExecCaddyMetrics {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, container: String) -> Self {
        Self { runner, container }
    }

    /// The request counter lines of the router's Prometheus text exposition, or
    /// an error that names only the exit status. The output is not logged.
    pub async fn read(&self) -> Result<String, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("exec")
            .arg(&self.container)
            .arg("sh")
            .arg("-c")
            .arg(CADDY_REQUEST_COUNTERS_SCRIPT);
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Routing(format!(
                "the router's request counters could not be read ({})",
                crate::error::describe_exit(output.code)
            )));
        }
        Ok(output.stdout)
    }
}
