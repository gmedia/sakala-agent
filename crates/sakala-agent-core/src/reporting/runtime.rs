use async_trait::async_trait;
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, LogBounds};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    api::ApiClient,
    logs::redactor::redact_line,
    ports::{RuntimeExecutionError, RuntimeReporter, RuntimeReporterFactory},
};

pub struct ApiRuntimeReporterFactory {
    client: ApiClient,
}

impl ApiRuntimeReporterFactory {
    #[must_use]
    pub fn new(client: ApiClient) -> Self {
        Self { client }
    }
}

impl RuntimeReporterFactory for ApiRuntimeReporterFactory {
    fn reporter(
        &self,
        command_id: Uuid,
        log_bounds: LogBounds,
    ) -> std::sync::Arc<dyn RuntimeReporter> {
        std::sync::Arc::new(ApiRuntimeReporter::new(
            self.client.clone(),
            command_id,
            log_bounds,
        ))
    }
}

pub(crate) struct ApiRuntimeReporter {
    client: ApiClient,
    command_id: Uuid,
    log_bounds: LogBounds,
    log_bytes_sent: Mutex<u64>,
    deployment_committed: AtomicBool,
}

impl ApiRuntimeReporter {
    #[must_use]
    pub(crate) fn new(client: ApiClient, command_id: Uuid, log_bounds: LogBounds) -> Self {
        Self {
            client,
            command_id,
            log_bounds,
            log_bytes_sent: Mutex::new(0),
            deployment_committed: AtomicBool::new(false),
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

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        let mut sent = self.log_bytes_sent.lock().await;
        let Some(log) = bounded_log(log, self.log_bounds, *sent) else {
            return Ok(());
        };
        self.client
            .log(self.command_id, &log)
            .await
            .map_err(|error| RuntimeExecutionError::reporting(error.to_string()))?;
        *sent += u64::try_from(log.message.len()).unwrap_or(u64::MAX);
        Ok(())
    }

    fn mark_deployment_committed(&self) {
        self.deployment_committed.store(true, Ordering::Release);
    }

    fn deployment_committed(&self) -> bool {
        self.deployment_committed.load(Ordering::Acquire)
    }
}

fn bounded_log(mut log: DeploymentLog, bounds: LogBounds, sent: u64) -> Option<DeploymentLog> {
    // The current agent transport sends one log per request. A zero-sized
    // batch policy therefore means no log may be emitted.
    if bounds.max_batch_lines == Some(0) {
        return None;
    }
    log.message = redact_line(&log.message);
    truncate_utf8(&mut log.message, bounds.max_line_length);
    if let Some(maximum) = bounds.max_total_bytes {
        if sent >= maximum {
            return None;
        }
        truncate_utf8(&mut log.message, Some(maximum - sent));
    }
    (!log.message.is_empty()).then_some(log)
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

#[cfg(test)]
mod tests {
    use sakala_agent_protocol::{DeploymentLog, LogStream};
    use time::OffsetDateTime;

    use super::{LogBounds, bounded_log};

    fn log(message: &str) -> DeploymentLog {
        DeploymentLog {
            stream: LogStream::Stdout,
            message: message.to_owned(),
            recorded_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn redacts_before_truncating_and_preserves_utf8_boundaries() {
        let secret = bounded_log(
            log("TOKEN=super-secret-value"),
            LogBounds {
                max_line_length: Some(16),
                ..LogBounds::default()
            },
            0,
        )
        .expect("redacted log should fit");
        assert_eq!(secret.message, "TOKEN=[REDACTED]");

        let unicode = bounded_log(
            log("ééé"),
            LogBounds {
                max_line_length: Some(5),
                ..LogBounds::default()
            },
            0,
        )
        .expect("unicode log should be retained");
        assert_eq!(unicode.message, "éé");
        assert_eq!(unicode.message.len(), 4);
    }

    #[test]
    fn total_byte_budget_is_shared_across_log_lines() {
        let bounds = LogBounds {
            max_total_bytes: Some(6),
            ..LogBounds::default()
        };
        let first = bounded_log(log("abcd"), bounds, 0).expect("first line should fit");
        let second = bounded_log(log("efgh"), bounds, first.message.len() as u64)
            .expect("remaining budget should be used");
        let third = bounded_log(
            log("ignored"),
            bounds,
            (first.message.len() + second.message.len()) as u64,
        );

        assert_eq!(first.message, "abcd");
        assert_eq!(second.message, "ef");
        assert!(third.is_none());
    }
}
