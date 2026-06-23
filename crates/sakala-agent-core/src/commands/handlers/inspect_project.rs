use sakala_agent_protocol::AgentCommand;

use crate::ports::{
    CommandOutput, InspectProjectRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
};

pub async fn handle(
    command: &AgentCommand,
    runtime: &dyn RuntimeExecutor,
    reporter: &dyn RuntimeReporter,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let payload = command.inspect_payload().map_err(|error| {
        RuntimeExecutionError::invalid_command(format!(
            "InspectProject payload is invalid: {error}"
        ))
    })?;

    runtime
        .inspect_project(
            InspectProjectRequest {
                command_id: command.id,
                payload,
            },
            reporter,
        )
        .await
}
