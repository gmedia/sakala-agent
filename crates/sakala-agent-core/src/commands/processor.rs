use std::sync::Arc;
use std::time::Duration;

use sakala_agent_protocol::{
    AgentCommand, CommandType, CompleteCommandPayload, DeploymentEvent, DeploymentEventLevel,
    LogBounds,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::{
    CoreError, NodeLifecycle,
    api::ApiClient,
    commands::CommandDispatcher,
    ports::{RuntimeExecutor, RuntimeReporter},
    reporting::ApiRuntimeReporter,
    repositories::ApiRepositoryCredentialProvider,
};

const POST_COMMIT_FINALIZATION_GRACE: Duration = Duration::from_secs(30);

pub struct CommandProcessor {
    client: ApiClient,
    dispatcher: CommandDispatcher,
    command_timeout: Duration,
    post_commit_finalization_grace: Duration,
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
        Self {
            client,
            dispatcher: CommandDispatcher::with_dependencies(
                runtime,
                repository_credentials,
                node_lifecycle,
            ),
            command_timeout,
            post_commit_finalization_grace: POST_COMMIT_FINALIZATION_GRACE,
        }
    }

    /// Overrides the bounded post-cutover finalization grace. Primarily useful
    /// for deterministic integration tests and embedded Agent policies.
    #[must_use]
    pub fn with_post_commit_finalization_grace(mut self, grace: Duration) -> Self {
        self.post_commit_finalization_grace = grace;
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

        let deadline_cancellation = cancellation.clone();
        let execution = self
            .dispatcher
            .dispatch(command, reporter.clone(), cancellation);
        tokio::pin!(execution);
        let execution = tokio::select! {
            biased;
            result = &mut execution => result,
            () = tokio::time::sleep(execution_timeout) => {
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
                                "post-commit finalization failed; deferring repair to reconciliation"
                            );
                            Ok(reporter.committed_output().unwrap_or_default())
                        }
                        Err(_) => {
                            deadline_cancellation.cancel();
                            warn!(
                                command_id = %command.id,
                                "post-commit finalization grace elapsed; deferring remaining cleanup to reconciliation"
                            );
                            Ok(reporter.committed_output().unwrap_or_default())
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
                Ok(reporter.committed_output().unwrap_or_default())
            }
            result => result,
        };

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
                    .fail(command.id, error.code(), &error.to_string())
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
