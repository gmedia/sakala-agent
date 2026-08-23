use async_trait::async_trait;
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, LogBounds};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    api::ApiClient,
    logs::redactor::redact_line,
    ports::{RuntimeExecutionError, RuntimeReporter},
};

pub(crate) struct ApiRuntimeReporter {
    client: ApiClient,
    command_id: Uuid,
    log_bounds: LogBounds,
    log_bytes_sent: Mutex<u64>,
}

impl ApiRuntimeReporter {
    #[must_use]
    pub(crate) fn new(client: ApiClient, command_id: Uuid, log_bounds: LogBounds) -> Self {
        Self {
            client,
            command_id,
            log_bounds,
            log_bytes_sent: Mutex::new(0),
        }
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
        // The current agent transport sends one log per request. A zero-sized
        // batch policy therefore means no log may be emitted.
        if self.log_bounds.max_batch_lines == Some(0) {
            return Ok(());
        }
        log.message = redact_line(&log.message);
        truncate_utf8(&mut log.message, self.log_bounds.max_line_length);
        let mut sent = self.log_bytes_sent.lock().await;
        if let Some(maximum) = self.log_bounds.max_total_bytes {
            if *sent >= maximum {
                return Ok(());
            }
            truncate_utf8(&mut log.message, Some(maximum - *sent));
        }
        if log.message.is_empty() {
            return Ok(());
        }
        self.client
            .log(self.command_id, &log)
            .await
            .map_err(|error| RuntimeExecutionError::reporting(error.to_string()))?;
        *sent += u64::try_from(log.message.len()).unwrap_or(u64::MAX);
        Ok(())
    }
}

fn truncate_utf8(value: &mut String, maximum: Option<u64>) {
    let Some(maximum) = maximum else { return };
    let maximum = usize::try_from(maximum).unwrap_or(usize::MAX);
    if value.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}
