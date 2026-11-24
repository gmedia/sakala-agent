use std::sync::Arc;

use sakala_agent_protocol::AgentCommand;
use tokio_util::sync::CancellationToken;

use crate::ports::{
    CommandOutput, RunJobRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
};

pub async fn handle(
    command: &AgentCommand,
    runtime: &dyn RuntimeExecutor,
    reporter: Arc<dyn RuntimeReporter>,
    cancellation: CancellationToken,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let project_id = command
        .project_id
        .ok_or_else(|| RuntimeExecutionError::invalid_command("RunJob requires project_id"))?;
    let deployment_id = command
        .deployment_id
        .ok_or_else(|| RuntimeExecutionError::invalid_command("RunJob requires deployment_id"))?;
    let payload = command.run_job_payload().map_err(|error| {
        RuntimeExecutionError::invalid_command(format!("RunJob payload is invalid: {error}"))
    })?;
    runtime
        .run_job(
            RunJobRequest {
                command_id: command.id,
                project_id,
                deployment_id,
                payload,
                cancellation,
            },
            reporter,
        )
        .await
}
