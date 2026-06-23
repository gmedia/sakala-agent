use async_trait::async_trait;
use sakala_agent_core::ports::{
    CommandOutput, DeployProjectRequest, InspectProjectRequest, RuntimeExecutionError,
    RuntimeExecutor, RuntimeReporter,
};
use sakala_agent_protocol::{DeploymentEvent, DeploymentEventLevel, DeploymentLog, LogStream};
use serde_json::json;
use time::OffsetDateTime;
use tracing::info;

/// Safe foundation executor: records the requested operation without touching a host runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for NoopRuntimeExecutor {
    async fn inspect_project(
        &self,
        request: InspectProjectRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        execute_noop(request.command_id, "InspectProject", reporter).await
    }

    async fn deploy_project(
        &self,
        request: DeployProjectRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        execute_noop(request.command_id, "DeployProject", reporter).await
    }
}

async fn execute_noop(
    command_id: uuid::Uuid,
    command_type: &str,
    reporter: &dyn RuntimeReporter,
) -> Result<CommandOutput, RuntimeExecutionError> {
    info!(
        command_id = %command_id,
        command_type,
        "noop runtime accepted command"
    );

    let now = OffsetDateTime::now_utc();

    reporter
        .event(DeploymentEvent {
            event_type: "runtime.noop.completed".to_owned(),
            level: DeploymentEventLevel::Info,
            message: "Noop runtime completed command without host changes.".to_owned(),
            metadata: json!({ "executor": "noop" }),
            occurred_at: now,
        })
        .await?;
    reporter
        .log(DeploymentLog {
            stream: LogStream::System,
            message: "Foundation mode: no Docker, Caddy, or Railpack operation executed."
                .to_owned(),
            recorded_at: now,
        })
        .await?;

    Ok(CommandOutput::empty())
}
