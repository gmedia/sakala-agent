use std::sync::Arc;
use std::time::Duration;

use sakala_agent_protocol::{
    AgentCommand, CommandType, CompleteCommandPayload, DeploymentEvent, DeploymentEventLevel,
    FinalizationDeferredReason, LogBounds,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    CoreError, NodeLifecycle,
    api::ApiClient,
    commands::CommandDispatcher,
    logs::redactor::ValueRedactor,
    ports::{CommandOutput, DatabaseAdmin, RuntimeExecutor, RuntimeReporter},
    reporting::{
        ApiRuntimeReporter, ApiRuntimeReporterFactory, EventDelivery, EventDeliveryPolicy,
    },
    repositories::ApiRepositoryCredentialProvider,
};

const POST_COMMIT_FINALIZATION_GRACE: Duration = Duration::from_secs(30);
/// Longest wait for buffered log lines before `complete`/`fail`, when the
/// control plane is unreachable at the end of a command.
const FINAL_LOG_FLUSH_GRACE: Duration = Duration::from_secs(120);
/// Shortest such wait, even for a command that used its whole timeout. The
/// control-plane lease outlives the command timeout by
/// `SAKALA_AGENT_LEASE_GRACE_SECONDS` (default 60), so this still leaves room
/// for the terminal report.
const MIN_FINAL_LOG_FLUSH_GRACE: Duration = Duration::from_secs(10);

/// Payload field of `CreateDatabase` and `RotateDatabasePassword`.
const DATABASE_PASSWORD_FIELD: &str = "password";

pub struct CommandProcessor {
    client: ApiClient,
    dispatcher: CommandDispatcher,
    command_timeout: Duration,
    post_commit_finalization_grace: Duration,
    final_log_flush_grace: Duration,
    event_delivery: EventDeliveryPolicy,
}

impl CommandProcessor {
    #[must_use]
    pub fn new(
        client: ApiClient,
        runtime: Arc<dyn RuntimeExecutor>,
        command_timeout: Duration,
    ) -> Self {
        Self::with_node_lifecycle(
            client,
            runtime,
            command_timeout,
            Arc::new(NodeLifecycle::new()),
        )
    }

    #[must_use]
    pub fn with_node_lifecycle(
        client: ApiClient,
        runtime: Arc<dyn RuntimeExecutor>,
        command_timeout: Duration,
        node_lifecycle: Arc<NodeLifecycle>,
    ) -> Self {
        let repository_credentials = Arc::new(ApiRepositoryCredentialProvider::new(client.clone()));
        let reporter_factory = Arc::new(ApiRuntimeReporterFactory::new(client.clone()));
        Self {
            client,
            dispatcher: CommandDispatcher::with_dependencies(
                runtime,
                repository_credentials,
                node_lifecycle,
            )
            .with_reporter_factory(reporter_factory),
            command_timeout,
            post_commit_finalization_grace: POST_COMMIT_FINALIZATION_GRACE,
            final_log_flush_grace: FINAL_LOG_FLUSH_GRACE,
            event_delivery: EventDeliveryPolicy::default(),
        }
    }

    /// Overrides the bounded post-cutover finalization grace. Primarily useful
    /// for deterministic integration tests and embedded Agent policies.
    #[must_use]
    pub fn with_post_commit_finalization_grace(mut self, grace: Duration) -> Self {
        self.post_commit_finalization_grace = grace;
        self
    }

    /// Overrides the longest wait for buffered log lines before the terminal
    /// report. Primarily useful for deterministic integration tests.
    #[must_use]
    pub fn with_final_log_flush_grace(mut self, grace: Duration) -> Self {
        self.final_log_flush_grace = grace;
        self
    }

    /// Overrides the event retry limits. Primarily useful for deterministic
    /// integration tests.
    #[must_use]
    pub fn with_event_delivery_policy(mut self, policy: EventDeliveryPolicy) -> Self {
        self.event_delivery = policy;
        self
    }

    /// Lets the processor run managed PostgreSQL commands on a database host.
    #[must_use]
    pub fn with_database_admin(mut self, database_admin: Arc<dyn DatabaseAdmin>) -> Self {
        self.dispatcher = self.dispatcher.with_database_admin(database_admin);
        self
    }

    pub async fn process(
        &self,
        command: &AgentCommand,
        cancellation: CancellationToken,
    ) -> Result<(), CoreError> {
        let started = std::time::Instant::now();
        info!(
            command_id = %command.id,
            project_id = ?command.project_id,
            deployment_id = ?command.deployment_id,
            command_type = ?command.command_type,
            "processing control-plane command"
        );
        if let Err(error) = self.client.claim(command.id).await {
            if matches!(error, CoreError::CommandNotClaimable) {
                info!(command_id = %command.id, "command claim conflicted; skipping execution");
                return Ok(());
            }
            return Err(error);
        }
        // One delivery for the whole command, so an outage that begins here
        // costs the command one retry budget, not one per event.
        let events = Arc::new(EventDelivery::new(
            self.client.clone(),
            command.id,
            self.event_delivery,
        ));
        events
            .send(&DeploymentEvent {
                event_type: "command.claimed".to_owned(),
                level: DeploymentEventLevel::Info,
                message: "Agent claimed command.".to_owned(),
                metadata: json!({}),
                occurred_at: OffsetDateTime::now_utc(),
            })
            .await?;

        let (execution_timeout, log_bounds, redactor) = match self.command_policy(command) {
            Ok(policy) => policy,
            Err(error) => {
                self.client
                    .fail(command.id, error.code(), &error.to_string())
                    .await?;
                return Err(error.into());
            }
        };
        let reporter = Arc::new(
            ApiRuntimeReporter::new(self.client.clone(), command.id, log_bounds)
                .with_redactor(redactor)
                .with_event_delivery(events),
        );

        let deadline_cancellation = cancellation.clone();
        let execution = self
            .dispatcher
            .dispatch(command, reporter.clone(), cancellation);
        tokio::pin!(execution);
        // The control-plane lease starts at the claim, and `command.claimed`
        // may have waited out its retry budget since then, so the deadline is
        // counted from there too.
        let execution_deadline = tokio::time::Instant::from_std(started) + execution_timeout;
        let execution = tokio::select! {
            biased;
            result = &mut execution => result,
            () = tokio::time::sleep_until(execution_deadline) => {
                if reporter.deployment_committed() {
                    warn!(
                        command_id = %command.id,
                        grace_seconds = self.post_commit_finalization_grace.as_secs(),
                        "command deadline reached after deployment cutover; entering bounded finalization grace"
                    );
                    match tokio::time::timeout(
                        self.post_commit_finalization_grace,
                        &mut execution,
                    )
                    .await
                    {
                        Ok(Ok(output)) => Ok(output),
                        Ok(Err(error)) => {
                            warn!(
                                command_id = %command.id,
                                error_code = error.code(),
                                %error,
                                "post-commit finalization failed; requesting control-plane repair"
                            );
                            Ok(deferred_committed_output(
                                reporter.as_ref(),
                                FinalizationDeferredReason::RuntimeError,
                            ))
                        }
                        Err(_) => {
                            deadline_cancellation.cancel();
                            warn!(
                                command_id = %command.id,
                                "post-commit finalization grace elapsed; requesting control-plane repair"
                            );
                            Ok(deferred_committed_output(
                                reporter.as_ref(),
                                FinalizationDeferredReason::GraceElapsed,
                            ))
                        }
                    }
                } else {
                    deadline_cancellation.cancel();
                    // Give cancellation-aware runtimes a short cleanup window.
                    // The terminal error remains timeout even when cleanup returns Cancelled.
                    let _ = tokio::time::timeout(Duration::from_secs(1), &mut execution).await;
                    Err(crate::ports::RuntimeExecutionError::new(
                        "runtime_timeout",
                        format!(
                            "command execution exceeded its {}s timeout",
                            execution_timeout.as_secs()
                        ),
                    ))
                }
            }
        };
        let execution = match execution {
            Err(error) if reporter.deployment_committed() => {
                warn!(
                    command_id = %command.id,
                    error_code = error.code(),
                    %error,
                    "committed deployment finalization failed; preserving live runtime state"
                );
                Ok(deferred_committed_output(
                    reporter.as_ref(),
                    FinalizationDeferredReason::RuntimeError,
                ))
            }
            result => result,
        };

        // Buffered log batches are delivered before the terminal transition so
        // the control-plane timeline is complete; a failed command no longer
        // accepts new lines afterwards. The wait ends inside the command's own
        // timeout so the lease cannot expire under it. Undeliverable logs are
        // a warning: the command outcome is decided by the runtime, not by log
        // transport.
        let flush_grace = self.final_log_flush_grace.min(
            execution_timeout
                .saturating_sub(started.elapsed())
                .max(MIN_FINAL_LOG_FLUSH_GRACE),
        );
        if let Err(error) = reporter.flush_within(flush_grace).await {
            warn!(command_id = %command.id, %error, "deployment logs could not be fully delivered");
        }
        // Each given-up event was already logged when it was dropped; this is
        // the command-level total next to the log warning above.
        let undelivered_events = reporter.undelivered_events();
        if undelivered_events > 0 {
            warn!(
                command_id = %command.id,
                undelivered_events,
                "deployment events could not be fully delivered"
            );
        }

        match execution {
            Ok(output) => {
                self.client
                    .complete(
                        command.id,
                        &CompleteCommandPayload {
                            result: output.result,
                        },
                    )
                    .await?;
                info!(
                    command_id = %command.id,
                    project_id = ?command.project_id,
                    deployment_id = ?command.deployment_id,
                    command_type = ?command.command_type,
                    elapsed_ms = started.elapsed().as_millis(),
                    "control-plane command completed"
                );
                Ok(())
            }
            Err(error) => {
                self.client
                    .fail(
                        command.id,
                        error.code(),
                        &reporter.redact(&error.to_string()),
                    )
                    .await?;
                warn!(
                    command_id = %command.id,
                    project_id = ?command.project_id,
                    deployment_id = ?command.deployment_id,
                    command_type = ?command.command_type,
                    error_code = error.code(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "control-plane command failed"
                );
                Err(error.into())
            }
        }
    }

    fn command_policy(
        &self,
        command: &AgentCommand,
    ) -> Result<(Duration, LogBounds, ValueRedactor), crate::ports::RuntimeExecutionError> {
        if command.command_type != CommandType::DeployProject {
            return Ok((
                self.command_timeout,
                LogBounds::default(),
                ValueRedactor::new(command_password(command)),
            ));
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
        Ok((
            Duration::from_secs(requested),
            payload.log_bounds,
            ValueRedactor::new(payload.environment.into_values()),
        ))
    }
}

/// The tenant password a database command carries, hidden from its failure
/// message like a deployment's environment values.
fn command_password(command: &AgentCommand) -> Option<String> {
    matches!(
        command.command_type,
        CommandType::CreateDatabase | CommandType::RotateDatabasePassword
    )
    .then(|| {
        command
            .payload
            .get(DATABASE_PASSWORD_FIELD)?
            .as_str()
            .map(str::to_owned)
    })
    .flatten()
}

fn deferred_committed_output(
    reporter: &dyn RuntimeReporter,
    reason: FinalizationDeferredReason,
) -> CommandOutput {
    let mut output = reporter.committed_output().unwrap_or_default();
    if let Some(result) = output.result.as_object_mut() {
        result.insert("finalization_deferred".to_owned(), json!(true));
        result.insert("finalization_deferred_reason".to_owned(), json!(reason));
        return output;
    }

    let committed_result = std::mem::take(&mut output.result);
    output.result = json!({
        "committed_result": committed_result,
        "finalization_deferred": true,
        "finalization_deferred_reason": reason,
    });
    output
}

#[cfg(test)]
mod tests {
    use sakala_agent_protocol::CommandStatus;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const PASSWORD: &str = "c2VjcmV0LXBhc3N3b3JkLXZhbHVlLTEyMzQ1";

    fn command(command_type: CommandType, payload: Value) -> AgentCommand {
        AgentCommand {
            id: Uuid::new_v4(),
            command_type,
            status: CommandStatus::Claimed,
            project_id: None,
            deployment_id: None,
            payload,
        }
    }

    #[test]
    fn a_database_password_is_redacted_from_the_failure_message() {
        for command_type in [
            CommandType::CreateDatabase,
            CommandType::RotateDatabasePassword,
        ] {
            let password = command_password(&command(
                command_type,
                json!({ "role": "p_0123456789abcdef", "password": PASSWORD }),
            ));
            let redacted = ValueRedactor::new(password)
                .redact(&format!("PostgreSQL refused near '{PASSWORD}'"));
            assert!(!redacted.contains(PASSWORD), "{command_type:?}: {redacted}");
        }
        for command in [
            command(CommandType::LockDatabase, json!({ "password": PASSWORD })),
            command(CommandType::CreateDatabase, json!({ "password": 7 })),
            command(CommandType::CreateDatabase, Value::Null),
        ] {
            assert_eq!(command_password(&command), None);
        }
    }
}
