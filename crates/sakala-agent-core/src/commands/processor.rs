use std::sync::Arc;
use std::time::Duration;

use sakala_agent_protocol::{
    AgentCommand, CompleteCommandPayload, DeploymentEvent, DeploymentEventLevel,
};
use serde_json::json;
use time::OffsetDateTime;

use crate::{
    CoreError, api::ApiClient, commands::CommandDispatcher, ports::RuntimeExecutor,
    reporting::ApiRuntimeReporter,
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
        Self {
            client,
            dispatcher: CommandDispatcher::new(runtime),
            command_timeout,
        }
    }

    pub async fn process(&self, command: &AgentCommand) -> Result<(), CoreError> {
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

        let reporter = Arc::new(ApiRuntimeReporter::new(self.client.clone(), command.id));

        let execution = tokio::time::timeout(
            self.command_timeout,
            self.dispatcher.dispatch(command, reporter),
        )
        .await
        .unwrap_or_else(|_| {
            Err(crate::ports::RuntimeExecutionError::new(
                "runtime_timeout",
                format!(
                    "command execution exceeded its {}s timeout",
                    self.command_timeout.as_secs()
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
}
