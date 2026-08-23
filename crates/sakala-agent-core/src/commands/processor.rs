use std::sync::Arc;
use std::time::Duration;

use sakala_agent_protocol::{
    AgentCommand, CommandType, CompleteCommandPayload, DeploymentEvent, DeploymentEventLevel,
    LogBounds,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    CoreError, api::ApiClient, commands::CommandDispatcher, ports::RuntimeExecutor,
    reporting::ApiRuntimeReporter, repositories::ApiRepositoryCredentialProvider,
};

pub struct CommandProcessor {
    client: ApiClient,
    dispatcher: CommandDispatcher,
    command_timeout: Duration,
}

impl CommandProcessor {
    #[must_use]
    pub fn new(
        client: ApiClient,
        runtime: Arc<dyn RuntimeExecutor>,
        command_timeout: Duration,
    ) -> Self {
        let repository_credentials = Arc::new(ApiRepositoryCredentialProvider::new(client.clone()));
        Self {
            client,
            dispatcher: CommandDispatcher::with_repository_credentials(
                runtime,
                repository_credentials,
            ),
            command_timeout,
        }
    }

    pub async fn process(
        &self,
        command: &AgentCommand,
        cancellation: CancellationToken,
    ) -> Result<(), CoreError> {
        self.client.claim(command.id).await?;
        self.client
            .event(
                command.id,
                &DeploymentEvent {
                    event_type: "command.claimed".to_owned(),
                    level: DeploymentEventLevel::Info,
                    message: "Agent claimed command.".to_owned(),
                    metadata: json!({}),
                    occurred_at: OffsetDateTime::now_utc(),
                },
            )
            .await?;

        let (execution_timeout, log_bounds) = match self.command_policy(command) {
            Ok(policy) => policy,
            Err(error) => {
                self.client
                    .fail(command.id, error.code(), &error.to_string())
                    .await?;
                return Err(error.into());
            }
        };
        let reporter = Arc::new(ApiRuntimeReporter::new(
            self.client.clone(),
            command.id,
            log_bounds,
        ));

        let execution = tokio::time::timeout(
            execution_timeout,
            self.dispatcher.dispatch(command, reporter, cancellation),
        )
        .await
        .unwrap_or_else(|_| {
            Err(crate::ports::RuntimeExecutionError::new(
                "runtime_timeout",
                format!(
                    "command execution exceeded its {}s timeout",
                    execution_timeout.as_secs()
                ),
            ))
        });

        match execution {
            Ok(output) => {
                self.client
                    .complete(
                        command.id,
                        &CompleteCommandPayload {
                            result: output.result,
                        },
                    )
                    .await
            }
            Err(error) => {
                self.client
                    .fail(command.id, error.code(), &error.to_string())
                    .await?;
                Err(error.into())
            }
        }
    }

    fn command_policy(
        &self,
        command: &AgentCommand,
    ) -> Result<(Duration, LogBounds), crate::ports::RuntimeExecutionError> {
        if command.command_type != CommandType::DeployProject {
            return Ok((self.command_timeout, LogBounds::default()));
        }
        let payload = command.deploy_payload().map_err(|error| {
            crate::ports::RuntimeExecutionError::invalid_command(format!(
                "invalid DeployProject payload: {error}"
            ))
        })?;
        let requested = payload
            .timeouts
            .command_timeout_seconds
            .unwrap_or(self.command_timeout.as_secs());
        if requested == 0 || requested > self.command_timeout.as_secs() {
            return Err(crate::ports::RuntimeExecutionError::invalid_command(
                format!(
                    "command_timeout_seconds ({requested}s) exceeds the node maximum of {}s",
                    self.command_timeout.as_secs()
                ),
            ));
        }
        Ok((Duration::from_secs(requested), payload.log_bounds))
    }
}
