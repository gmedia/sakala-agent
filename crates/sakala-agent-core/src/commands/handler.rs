use std::sync::Arc;

use sakala_agent_protocol::{AgentCommand, DeploymentEvent, DeploymentEventLevel};
use sakala_agent_runtime::RuntimeExecutor;
use serde_json::json;
use time::OffsetDateTime;

use crate::{CoreError, api::ApiClient, logs::reporter::report_logs};

pub struct CommandHandler {
    client: ApiClient,
    runtime: Arc<dyn RuntimeExecutor>,
}

impl CommandHandler {
    #[must_use]
    pub fn new(client: ApiClient, runtime: Arc<dyn RuntimeExecutor>) -> Self {
        Self { client, runtime }
    }

    pub async fn handle(&self, command: &AgentCommand) -> Result<(), CoreError> {
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

        match self.runtime.execute(command).await {
            Ok(outcome) => {
                for event in outcome.events {
                    self.client.event(command.id, &event).await?;
                }
                report_logs(&self.client, command.id, outcome.logs).await?;
                self.client.complete(command.id).await
            }
            Err(error) => {
                self.client.fail(command.id, &error.to_string()).await?;
                Err(error.into())
            }
        }
    }
}
