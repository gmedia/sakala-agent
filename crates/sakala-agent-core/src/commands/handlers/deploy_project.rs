use sakala_agent_protocol::AgentCommand;

use crate::ports::{
    CommandOutput, DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
};

pub async fn handle(
    command: &AgentCommand,
    runtime: &dyn RuntimeExecutor,
    reporter: &dyn RuntimeReporter,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let project_id = command.project_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("DeployProject requires project_id")
    })?;
    let deployment_id = command.deployment_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("DeployProject requires deployment_id")
    })?;
    let payload = command.deploy_payload().map_err(|error| {
        RuntimeExecutionError::invalid_command(format!("DeployProject payload is invalid: {error}"))
    })?;

    runtime
        .deploy_project(
            DeployProjectRequest {
                command_id: command.id,
                project_id,
                deployment_id,
                payload,
            },
            reporter,
        )
        .await
}
