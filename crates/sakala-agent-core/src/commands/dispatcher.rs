use std::sync::Arc;

use sakala_agent_protocol::{AgentCommand, CommandType, DeploymentEvent, DeploymentEventLevel};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    NodeLifecycle, NodeLifecycleState,
    commands::handlers::{deploy_project, inspect_project},
    ports::{
        CommandOutput, RepositoryCredentialProvider, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, UnavailableRepositoryCredentialProvider, WorkloadLifecycleRequest,
    },
};

pub struct CommandDispatcher {
    runtime: Arc<dyn RuntimeExecutor>,
    repository_credentials: Arc<dyn RepositoryCredentialProvider>,
    node_lifecycle: Arc<NodeLifecycle>,
}

impl CommandDispatcher {
    #[must_use]
    pub fn new(runtime: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            runtime,
            repository_credentials: Arc::new(UnavailableRepositoryCredentialProvider),
            node_lifecycle: Arc::new(NodeLifecycle::new()),
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
            node_lifecycle: Arc::new(NodeLifecycle::new()),
        }
    }

    #[must_use]
    pub fn with_dependencies(
        runtime: Arc<dyn RuntimeExecutor>,
        repository_credentials: Arc<dyn RepositoryCredentialProvider>,
        node_lifecycle: Arc<NodeLifecycle>,
    ) -> Self {
        Self {
            runtime,
            repository_credentials,
            node_lifecycle,
        }
    }

    pub async fn dispatch(
        &self,
        command: &AgentCommand,
        reporter: Arc<dyn RuntimeReporter>,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        match command.command_type {
            CommandType::InspectProject => {
                inspect_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                    cancellation,
                )
                .await
            }
            CommandType::DeployProject => {
                deploy_project::handle(
                    command,
                    self.runtime.as_ref(),
                    self.repository_credentials.as_ref(),
                    reporter,
                    cancellation,
                )
                .await
            }
            CommandType::RestartProject => {
                self.runtime
                    .restart_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::StopProject => {
                self.runtime
                    .stop_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::SleepProject => {
                self.runtime
                    .sleep_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::WakeProject => {
                self.runtime
                    .wake_project(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::HealthCheck => {
                self.runtime
                    .health_check(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::RefreshRoute => {
                self.runtime
                    .refresh_route(lifecycle_request(command, cancellation)?, reporter)
                    .await
            }
            CommandType::DrainNode => {
                self.node_lifecycle.set(NodeLifecycleState::Draining);
                reporter
                    .event(DeploymentEvent {
                        event_type: "node.drain.started".to_owned(),
                        level: DeploymentEventLevel::Info,
                        message: "Node stopped accepting new workload commands.".to_owned(),
                        metadata: json!({}),
                        occurred_at: OffsetDateTime::now_utc(),
                    })
                    .await?;
                Ok(CommandOutput::with_result(json!({ "state": "draining" })))
            }
            CommandType::ResumeNode => {
                let preflight = self.runtime.preflight().await?;
                if preflight.has_fatal_failure() {
                    return Err(RuntimeExecutionError::new(
                        "runtime_preflight_failed",
                        "node cannot resume because runtime preflight has fatal failures",
                    ));
                }
                self.node_lifecycle.set(NodeLifecycleState::Active);
                reporter
                    .event(DeploymentEvent {
                        event_type: "node.resume.completed".to_owned(),
                        level: DeploymentEventLevel::Info,
                        message: "Node preflight passed and workload command processing resumed."
                            .to_owned(),
                        metadata: json!({}),
                        occurred_at: OffsetDateTime::now_utc(),
                    })
                    .await?;
                Ok(CommandOutput::with_result(json!({ "state": "active" })))
            }
        }
    }
}

fn lifecycle_request(
    command: &AgentCommand,
    cancellation: CancellationToken,
) -> Result<WorkloadLifecycleRequest, RuntimeExecutionError> {
    let project_id = command.project_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("lifecycle command requires project_id")
    })?;
    let deployment_id = command.deployment_id.ok_or_else(|| {
        RuntimeExecutionError::invalid_command("lifecycle command requires deployment_id")
    })?;
    Ok(WorkloadLifecycleRequest {
        command_id: command.id,
        project_id,
        deployment_id,
        cancellation,
    })
}
