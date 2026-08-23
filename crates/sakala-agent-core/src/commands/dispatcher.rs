use std::sync::Arc;

use sakala_agent_protocol::{AgentCommand, CommandType};

use crate::{
    commands::handlers::{deploy_project, inspect_project},
    ports::{
        CommandOutput, RepositoryCredentialProvider, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, UnavailableRepositoryCredentialProvider,
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
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        match command.command_type {
            CommandType::InspectProject => {
                inspect_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                )
                .await
            }
            CommandType::DeployProject => {
                deploy_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                )
                .await
            }
            command_type => Err(RuntimeExecutionError::unsupported_command(format!(
                "command {command_type:?} does not have a core handler yet"
            ))),
        }
    }
}
