use async_trait::async_trait;
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};
use uuid::Uuid;

use crate::{
    api::ApiClient,
    logs::redactor::redact_line,
    ports::{RuntimeExecutionError, RuntimeReporter},
};

pub(crate) struct ApiRuntimeReporter {
    client: ApiClient,
    command_id: Uuid,
}

impl ApiRuntimeReporter {
    #[must_use]
    pub(crate) fn new(client: ApiClient, command_id: Uuid) -> Self {
        Self { client, command_id }
    }
}

#[async_trait]
impl RuntimeReporter for ApiRuntimeReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        self.client
            .event(self.command_id, &event)
            .await
            .map_err(|error| RuntimeExecutionError::reporting(error.to_string()))
    }

    async fn log(&self, mut log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        log.message = redact_line(&log.message);
        self.client
            .log(self.command_id, &log)
            .await
            .map_err(|error| RuntimeExecutionError::reporting(error.to_string()))
    }
}
