use std::sync::Arc;

use sakala_agent_protocol::{AgentCommand, RequestedProcesses, RequestedReplicas};
use tokio_util::sync::CancellationToken;

use crate::ports::{
    CommandOutput, DeployProjectRequest, RepositoryCredentialProvider, RuntimeExecutionError,
    RuntimeExecutor, RuntimeReporter,
};

pub async fn handle(
    command: &AgentCommand,
    runtime: &dyn RuntimeExecutor,
    repository_credentials: &dyn RepositoryCredentialProvider,
    reporter: Arc<dyn RuntimeReporter>,
    cancellation: CancellationToken,
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
    // Refusing the deployment over this field would leave the project without
    // a new release; one replica is what every older Agent runs anyway.
    if payload.replicas == RequestedReplicas::Unreadable {
        tracing::warn!(
            command_id = %command.id,
            %project_id,
            "DeployProject replicas is not a positive integer; running one replica"
        );
    }
    // Same reasoning: the web process still gets its new release, and the
    // runtime tells the deployment's timeline that no worker runs.
    if payload.processes == RequestedProcesses::Unreadable {
        tracing::warn!(
            command_id = %command.id,
            %project_id,
            "DeployProject processes is not a list of worker definitions; running the web process only"
        );
    }
    let repository_credential = repository_credentials
        .credential(command.id, payload.repository_access)
        .await?;

    runtime
        .deploy_project(
            DeployProjectRequest {
                command_id: command.id,
                project_id,
                deployment_id,
                payload,
                repository_credential,
                cancellation,
            },
            reporter,
        )
        .await
}
