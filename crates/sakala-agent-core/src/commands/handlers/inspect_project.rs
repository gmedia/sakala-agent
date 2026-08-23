use std::sync::Arc;

use sakala_agent_protocol::AgentCommand;

use crate::ports::{
    CommandOutput, InspectProjectRequest, RepositoryCredentialProvider, RuntimeExecutionError,
    RuntimeExecutor, RuntimeReporter,
};

pub async fn handle(
    command: &AgentCommand,
    runtime: &dyn RuntimeExecutor,
    repository_credentials: &dyn RepositoryCredentialProvider,
    reporter: Arc<dyn RuntimeReporter>,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let payload = command.inspect_payload().map_err(|error| {
        RuntimeExecutionError::invalid_command(format!(
            "InspectProject payload is invalid: {error}"
        ))
    })?;
    let repository_credential = repository_credentials
        .credential(command.id, payload.repository_access)
        .await?;

    runtime
        .inspect_project(
            InspectProjectRequest {
                command_id: command.id,
                payload,
                repository_credential,
            },
            reporter,
        )
        .await
}
