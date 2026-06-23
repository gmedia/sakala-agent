use std::sync::Arc;

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
}

impl CommandProcessor {
    #[must_use]
    pub fn new(client: ApiClient, runtime: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            client,
            dispatcher: CommandDispatcher::new(runtime),
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

        let reporter = ApiRuntimeReporter::new(self.client.clone(), command.id);

        match self.dispatcher.dispatch(command, &reporter).await {
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
