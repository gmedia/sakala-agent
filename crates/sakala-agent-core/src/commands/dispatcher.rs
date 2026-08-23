use std::sync::Arc;

use sakala_agent_protocol::{AgentCommand, CommandType};
use tokio_util::sync::CancellationToken;

use crate::{
    commands::handlers::{deploy_project, inspect_project},
    ports::{
        CommandOutput, RepositoryCredentialProvider, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, UnavailableRepositoryCredentialProvider, WorkloadLifecycleRequest,
    },
};

pub struct CommandDispatcher {
    runtime: Arc<dyn RuntimeExecutor>,
    repository_credentials: Arc<dyn RepositoryCredentialProvider>,
}

impl CommandDispatcher {
    #[must_use]
    pub fn new(runtime: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            runtime,
            repository_credentials: Arc::new(UnavailableRepositoryCredentialProvider),
        }
    }

    #[must_use]
    pub fn with_repository_credentials(
        runtime: Arc<dyn RuntimeExecutor>,
        repository_credentials: Arc<dyn RepositoryCredentialProvider>,
    ) -> Self {
        Self {
            runtime,
            repository_credentials,
        }
    }

    pub async fn dispatch(
        &self,
        command: &AgentCommand,
        reporter: Arc<dyn RuntimeReporter>,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        match command.command_type {
            CommandType::InspectProject => {
                inspect_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                    cancellation,
                )
                .await
            }
            CommandType::DeployProject => {
                deploy_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                    cancellation,
                )
                .await
            }
            CommandType::RestartProject => {
                self.runtime
                    .restart_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::StopProject => {
                self.runtime
                    .stop_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::SleepProject => {
                self.runtime
                    .sleep_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::WakeProject => {
                self.runtime
                    .wake_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::HealthCheck => {
                self.runtime
                    .health_check(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::RefreshRoute => {
                self.runtime
                    .refresh_route(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
        }
    }
}

fn lifecycle_request(
    command: &AgentCommand,
    cancellation: CancellationToken,
) -> Result<WorkloadLifecycleRequest, RuntimeExecutionError> {
    let project_id = command.project_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("lifecycle command requires project_id")
    })?;
    let deployment_id = command.deployment_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("lifecycle command requires deployment_id")
    })?;
    Ok(WorkloadLifecycleRequest {
        command_id: command.id,
        project_id,
        deployment_id,
        cancellation,
    })
}
