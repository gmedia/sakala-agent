use async_trait::async_trait;
use sakala_agent_protocol::{
    AgentCommand, DeploymentEvent, DeploymentEventLevel, DeploymentLog, LogStream,
};
use serde_json::json;
use time::OffsetDateTime;
use tracing::info;

use crate::{ExecutionOutcome, RuntimeError, RuntimeExecutor};

/// Safe foundation executor: records the requested operation without touching a host runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for NoopRuntimeExecutor {
    async fn execute(&self, command: &AgentCommand) -> Result<ExecutionOutcome, RuntimeError> {
        info!(
            command_id = %command.id,
            command_type = ?command.command_type,
            "noop runtime accepted command"
        );

        let now = OffsetDateTime::now_utc();

        Ok(ExecutionOutcome {
            events: vec![DeploymentEvent {
                event_type: "runtime.noop.completed".to_owned(),
                level: DeploymentEventLevel::Info,
                message: "Noop runtime completed command without host changes.".to_owned(),
                metadata: json!({ "executor": "noop" }),
                occurred_at: now,
            }],
            logs: vec![DeploymentLog {
                stream: LogStream::System,
                message: "Foundation mode: no Docker, Caddy, or Railpack operation executed."
                    .to_owned(),
                recorded_at: now,
            }],
        })
    }
}
