use std::sync::Arc;

use sakala_agent_protocol::{
    AgentCommand, CommandType, DeploymentEvent, DeploymentEventLevel, SleepingPage,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::{
    NodeLifecycle, NodeLifecycleState,
    commands::handlers::{deploy_project, inspect_project},
    ports::{
        CleanupRuntimeRequest, CommandOutput, ReconcileWorkloadRequest,
        RepositoryCredentialProvider, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
        RuntimeReporterFactory, UnavailableRepositoryCredentialProvider, WorkloadLifecycleRequest,
    },
};

pub struct CommandDispatcher {
    runtime: Arc<dyn RuntimeExecutor>,
    repository_credentials: Arc<dyn RepositoryCredentialProvider>,
    node_lifecycle: Arc<NodeLifecycle>,
    reporter_factory: Option<Arc<dyn RuntimeReporterFactory>>,
}

impl CommandDispatcher {
    #[must_use]
    pub fn new(runtime: Arc<dyn RuntimeExecutor>) -> Self {
        Self {
            runtime,
            repository_credentials: Arc::new(UnavailableRepositoryCredentialProvider),
            node_lifecycle: Arc::new(NodeLifecycle::new()),
            reporter_factory: None,
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
            reporter_factory: None,
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
            reporter_factory: None,
        }
    }

    /// Supplies reporters bound to a workload's original deploy command for
    /// runtime work that outlives the command being dispatched (restarted
    /// log followers).
    #[must_use]
    pub fn with_reporter_factory(
        mut self,
        reporter_factory: Arc<dyn RuntimeReporterFactory>,
    ) -> Self {
        self.reporter_factory = Some(reporter_factory);
        self
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
                let payload = command.stop_project_payload().map_err(|error| {
                    RuntimeExecutionError::invalid_command(format!(
                        "StopProject payload is invalid: {error}"
                    ))
                })?;
                let mut request = lifecycle_request(command, cancellation)?;
                request.remove_images = payload.remove_images;
                self.runtime.stop_project(request, reporter).await
            }
            CommandType::SleepProject => {
                let mut request = lifecycle_request(command, cancellation)?;
                request.sleeping_page = sleeping_page(command);
                self.runtime.sleep_project(request, reporter).await
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
            CommandType::ReconcileWorkload => {
                let lifecycle = lifecycle_request(command, cancellation)?;
                let payload = command.reconcile_workload_payload().map_err(|error| {
                    RuntimeExecutionError::invalid_command(format!(
                        "ReconcileWorkload payload is invalid: {error}"
                    ))
                })?;
                self.runtime
                    .reconcile_workload(
                        ReconcileWorkloadRequest {
                            project_id: lifecycle.project_id,
                            deployment_id: lifecycle.deployment_id,
                            desired_state: payload.desired_state,
                            actions: payload.actions,
                            cancellation: lifecycle.cancellation,
                            reporter_factory: self.reporter_factory.clone(),
                        },
                        reporter,
                    )
                    .await
            }
            CommandType::CleanupRuntime => {
                let payload = command.cleanup_runtime_payload().map_err(|error| {
                    RuntimeExecutionError::invalid_command(format!(
                        "CleanupRuntime payload is invalid: {error}"
                    ))
                })?;
                if !payload.approved {
                    return Err(RuntimeExecutionError::invalid_command(
                        "CleanupRuntime requires approved=true",
                    ));
                }
                if payload.targets.is_empty() {
                    return Err(RuntimeExecutionError::invalid_command(
                        "CleanupRuntime requires at least one target",
                    ));
                }
                self.runtime
                    .cleanup_runtime(
                        CleanupRuntimeRequest {
                            command_id: command.id,
                            approved: payload.approved,
                            targets: payload.targets,
                            cancellation,
                        },
                        reporter,
                    )
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
                let capacity = self.runtime.capacity().await?;
                let available_workload_slots = capacity.available_workload_slots();
                self.node_lifecycle.set(NodeLifecycleState::Active);
                reporter
                    .event(DeploymentEvent {
                        event_type: "node.resume.completed".to_owned(),
                        level: DeploymentEventLevel::Info,
                        message: "Node preflight passed and workload command processing resumed."
                            .to_owned(),
                        metadata: json!({
                            "active_workloads": capacity.active_workloads,
                            "maximum_active_workloads": capacity.maximum_active_workloads,
                            "available_workload_slots": available_workload_slots,
                        }),
                        occurred_at: OffsetDateTime::now_utc(),
                    })
                    .await?;
                Ok(CommandOutput::with_result(json!({
                    "state": "active",
                    "capacity": {
                        "active_workloads": capacity.active_workloads,
                        "maximum_active_workloads": capacity.maximum_active_workloads,
                        "available_workload_slots": available_workload_slots,
                    },
                })))
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
        remove_images: false,
        sleeping_page: SleepingPage::default(),
        cancellation,
    })
}

/// A payload this build does not understand still puts the project to sleep,
/// with the waking page every revision-4 Agent shows, because refusing the
/// sleep would keep an idle project running.
fn sleeping_page(command: &AgentCommand) -> SleepingPage {
    match command.sleep_project_payload() {
        Ok(payload) => payload.page,
        Err(error) => {
            tracing::warn!(
                command_id = %command.id,
                %error,
                "SleepProject payload is not understood; showing the waking page"
            );
            SleepingPage::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sakala_agent_protocol::{CommandStatus, DeploymentLog};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    #[derive(Default)]
    struct RecordingRuntime {
        stops: Mutex<Vec<bool>>,
        sleeps: Mutex<Vec<(bool, SleepingPage)>>,
    }

    #[async_trait]
    impl RuntimeExecutor for RecordingRuntime {
        async fn stop_project(
            &self,
            request: WorkloadLifecycleRequest,
            _reporter: Arc<dyn RuntimeReporter>,
        ) -> Result<CommandOutput, RuntimeExecutionError> {
            self.stops.lock().expect("lock").push(request.remove_images);
            Ok(CommandOutput::empty())
        }

        async fn sleep_project(
            &self,
            request: WorkloadLifecycleRequest,
            _reporter: Arc<dyn RuntimeReporter>,
        ) -> Result<CommandOutput, RuntimeExecutionError> {
            self.sleeps
                .lock()
                .expect("lock")
                .push((request.remove_images, request.sleeping_page));
            Ok(CommandOutput::empty())
        }
    }

    struct SilentReporter;

    #[async_trait]
    impl RuntimeReporter for SilentReporter {
        async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }
    }

    fn command(command_type: CommandType, payload: Value) -> AgentCommand {
        AgentCommand {
            id: Uuid::new_v4(),
            command_type,
            status: CommandStatus::Claimed,
            project_id: Some(Uuid::new_v4()),
            deployment_id: Some(Uuid::new_v4()),
            payload,
        }
    }

    async fn dispatch(
        dispatcher: &CommandDispatcher,
        command_type: CommandType,
        payload: Value,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        dispatcher
            .dispatch(
                &command(command_type, payload),
                Arc::new(SilentReporter),
                CancellationToken::new(),
            )
            .await
    }

    #[tokio::test]
    async fn only_a_stop_that_asks_for_it_removes_the_project_images() {
        let runtime = Arc::new(RecordingRuntime::default());
        let dispatcher = CommandDispatcher::new(Arc::clone(&runtime) as Arc<dyn RuntimeExecutor>);

        for payload in [json!({}), Value::Null, json!({ "remove_images": true })] {
            dispatch(&dispatcher, CommandType::StopProject, payload)
                .await
                .expect("stop");
        }
        dispatch(
            &dispatcher,
            CommandType::SleepProject,
            json!({ "remove_images": true }),
        )
        .await
        .expect("sleep");
        let error = dispatch(
            &dispatcher,
            CommandType::StopProject,
            json!({ "remove_images": "yes" }),
        )
        .await
        .expect_err("a malformed stop payload is rejected");

        assert_eq!(*runtime.stops.lock().expect("lock"), [false, false, true]);
        assert_eq!(
            *runtime.sleeps.lock().expect("lock"),
            [(false, SleepingPage::Waking)]
        );
        assert_eq!(error.code(), "invalid_runtime_command");
    }

    #[tokio::test]
    async fn sleep_shows_the_requested_page_and_never_refuses_a_payload() {
        let runtime = Arc::new(RecordingRuntime::default());
        let dispatcher = CommandDispatcher::new(Arc::clone(&runtime) as Arc<dyn RuntimeExecutor>);

        for payload in [
            Value::Null,
            json!({}),
            json!({ "page": "waking" }),
            json!({ "page": "unavailable" }),
            json!({ "page": "suspended" }),
            json!({ "page": 1 }),
            json!("unavailable"),
        ] {
            dispatch(&dispatcher, CommandType::SleepProject, payload)
                .await
                .expect("sleep");
        }

        let pages: Vec<_> = runtime
            .sleeps
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, page)| *page)
            .collect();
        assert_eq!(
            pages,
            [
                SleepingPage::Waking,
                SleepingPage::Waking,
                SleepingPage::Waking,
                SleepingPage::Unavailable,
                SleepingPage::Waking,
                SleepingPage::Waking,
                SleepingPage::Waking,
            ]
        );
    }
}
