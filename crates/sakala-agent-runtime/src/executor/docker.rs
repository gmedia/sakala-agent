use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use sakala_agent_core::logs::redactor::ValueRedactor;
use sakala_agent_core::ports::{
    CleanupRuntimeRequest, CommandOutput, DeployProjectRequest, InspectProjectRequest,
    NodeTelemetry, ReconcileWorkloadRequest, RuntimeCapacity, RuntimeCompatibilityIssue,
    RuntimeExecutionError, RuntimeExecutor, RuntimeHealthSnapshot, RuntimeImageCleanup,
    RuntimePreflightCheck, RuntimePreflightReport, RuntimeReconciliationReport, RuntimeReporter,
    RuntimeReporterFactory, WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, DeployProjectPayload, DeployProjectResult,
    DeploymentBuilder, DeploymentLog, InspectProjectPayload, LogBounds, LogStream,
    ReconcileWorkloadAction, RuntimeCleanupTarget,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};
use url::Url;
use uuid::Uuid;

use crate::{
    CommandSpec, DockerRuntimeConfig, ProcessRunner, RuntimeError, TokioProcessRunner,
    builders::{BuildRequest, ImageBuildService, ImageBuilder},
    containers::{
        ContainerEngine, DockerContainerEngine, FailureOutput, ImageCleanup, ImageRetention,
        ManagedWorkload, RunContainerRequest, container_name, image_name,
    },
    health::{DockerHealthChecker, HealthChecker, ReadinessRequest},
    inspections::{ProjectInspector, RailpackProjectInspector},
    logs::emit_event,
    railpack::{RailpackSandbox, is_pinned_image},
    routing::{
        CaddyFileRouteManager, DockerExecCaddyReloader, RouteIdentity, RouteManager, RouteSpec,
    },
    telemetry::NodeTelemetryCollector,
    workspace::{
        DeploymentWorkspace, GitWorkspaceManager, RepositorySource, WorkspaceManager,
        requested_root_directory, validate_root_directory,
    },
};

pub struct DockerRuntimeExecutor {
    workspace: Arc<dyn WorkspaceManager>,
    inspector: Arc<dyn ProjectInspector>,
    builder: Arc<dyn ImageBuilder>,
    containers: Arc<dyn ContainerEngine>,
    health: Arc<dyn HealthChecker>,
    routes: Arc<dyn RouteManager>,
    timeout_safety: crate::TimeoutSafetyConfig,
    health_check_window: std::time::Duration,
    build_permits: Arc<Semaphore>,
    container_admission: Arc<Mutex<()>>,
    max_concurrent_builds: usize,
    workspace_gc_max_age: std::time::Duration,
    image_retention: ImageRetention,
    min_workspace_free_bytes: u64,
    preflight: DockerPreflight,
    telemetry: NodeTelemetryCollector,
}

pub(crate) struct DockerPreflight {
    runner: Arc<dyn ProcessRunner>,
    workspace_root: std::path::PathBuf,
    runtime_network: String,
    caddy_sites_dir: std::path::PathBuf,
    caddy_container: String,
    railpack_image: String,
    railpack_sandbox: RailpackSandbox,
    container_runtime: Option<String>,
    container_dns_configured: bool,
}

impl DockerRuntimeExecutor {
    #[must_use]
    pub fn new(config: DockerRuntimeConfig) -> Self {
        let runner = Arc::new(TokioProcessRunner::new(
            config.timeout_safety.max_command_timeout,
        ));
        Self::with_runner(config, runner)
    }

    #[must_use]
    pub fn with_runner(config: DockerRuntimeConfig, runner: Arc<dyn ProcessRunner>) -> Self {
        let preflight = DockerPreflight {
            runner: Arc::clone(&runner),
            workspace_root: config.workspace_root.clone(),
            runtime_network: config.runtime_network.clone(),
            caddy_sites_dir: config.caddy_sites_dir.clone(),
            caddy_container: config.caddy_container.clone(),
            railpack_image: config.railpack_image.clone(),
            railpack_sandbox: RailpackSandbox::new(
                Arc::clone(&runner),
                config.railpack_image.clone(),
                config.railpack_network.clone(),
            ),
            container_runtime: config.container_runtime.clone(),
            container_dns_configured: !config.container_dns.is_empty(),
        };
        let container_dns = crate::containers::ContainerDns::new(
            &config.workspace_root,
            config.container_dns.clone(),
        );
        let agent_id = config.agent_id;
        let max_concurrent_builds = config.max_concurrent_builds;
        let workspace_gc_max_age = config.workspace_gc_max_age;
        let image_retention = ImageRetention {
            max_age: config.image_gc_max_age,
            orphaned_grace: config.orphaned_image_grace,
        };
        let min_workspace_free_bytes = config.min_workspace_free_bytes;
        let health_check_window = config.health_interval * config.health_attempts;
        let router_container = config.caddy_container.clone();
        let reloader = Arc::new(DockerExecCaddyReloader::new(
            Arc::clone(&runner),
            config.caddy_container,
        ));
        Self::with_services(
            Arc::new(GitWorkspaceManager::new(
                config.workspace_root,
                Arc::clone(&runner),
            )),
            Arc::new(RailpackProjectInspector::new(Arc::new(
                RailpackSandbox::new(
                    Arc::clone(&runner),
                    config.railpack_image.clone(),
                    config.railpack_network.clone(),
                ),
            ))),
            Arc::new(ImageBuildService::new(
                Arc::clone(&runner),
                config.railpack_frontend,
                RailpackSandbox::new(
                    Arc::clone(&runner),
                    config.railpack_image,
                    config.railpack_network,
                ),
            )),
            Arc::new(DockerContainerEngine::new(
                Arc::clone(&runner),
                config.runtime_network,
                config.resource_safety,
                config.max_active_containers,
                agent_id,
                config.crash_stable_window,
                crate::containers::WorkloadIsolation {
                    container_runtime: config.container_runtime,
                    container_dns,
                },
            )),
            Arc::new(DockerHealthChecker::new(
                Arc::clone(&runner),
                router_container,
                config.health_interval,
            )),
            Arc::new(CaddyFileRouteManager::new(config.caddy_sites_dir, reloader)),
            config.timeout_safety,
            health_check_window,
            max_concurrent_builds,
            workspace_gc_max_age,
            image_retention,
            min_workspace_free_bytes,
            preflight,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub(crate) fn with_services(
        workspace: Arc<dyn WorkspaceManager>,
        inspector: Arc<dyn ProjectInspector>,
        builder: Arc<dyn ImageBuilder>,
        containers: Arc<dyn ContainerEngine>,
        health: Arc<dyn HealthChecker>,
        routes: Arc<dyn RouteManager>,
        timeout_safety: crate::TimeoutSafetyConfig,
        health_check_window: std::time::Duration,
        max_concurrent_builds: usize,
        workspace_gc_max_age: std::time::Duration,
        image_retention: ImageRetention,
        min_workspace_free_bytes: u64,
        preflight: DockerPreflight,
    ) -> Self {
        let telemetry = NodeTelemetryCollector::new(
            Arc::clone(&preflight.runner),
            preflight.workspace_root.clone(),
            preflight.runtime_network.clone(),
            preflight.caddy_container.clone(),
            preflight.railpack_image.clone(),
            preflight.container_runtime.clone(),
        );
        Self {
            workspace,
            inspector,
            builder,
            containers,
            health,
            routes,
            timeout_safety,
            health_check_window,
            build_permits: Arc::new(Semaphore::new(max_concurrent_builds)),
            container_admission: Arc::new(Mutex::new(())),
            max_concurrent_builds,
            workspace_gc_max_age,
            image_retention,
            min_workspace_free_bytes,
            preflight,
            telemetry,
        }
    }

    async fn run_inspection(
        &self,
        request: InspectProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let reporter = reporter.as_ref();
        let source = inspection_source(&request.payload, request.repository_credential)?;

        emit_event(
            reporter,
            "project.inspection.started",
            "Checking out repository for project preview.",
            json!({ "commit_sha": source.commit_sha }),
        )
        .await?;
        let workspace = self
            .workspace
            .checkout(
                request.command_id,
                &source,
                reporter,
                request.cancellation.clone(),
            )
            .await?;
        let inspection_source = source.without_credential();
        drop(source);
        let result = match workspace
            .build_context(request.payload.root_directory.as_deref())
            .await
        {
            Ok(context) => tokio::select! {
                result = self.inspector.inspect(&context, &inspection_source, reporter) => result,
                () = request.cancellation.cancelled() => Err(RuntimeError::Cancelled),
            },
            Err(error) => Err(error),
        };
        let cleanup = self.workspace.cleanup(&workspace).await;

        let inspection = result?;
        cleanup?;
        emit_event(
            reporter,
            "project.inspection.completed",
            "Project stack preview is ready.",
            json!({
                "dockerfile_found": inspection.dockerfile_found,
                "package_manager": inspection.package_manager,
            }),
        )
        .await?;

        let result = serde_json::to_value(inspection).map_err(|error| {
            RuntimeError::Execution(format!("failed to serialize project inspection: {error}"))
        })?;
        Ok(CommandOutput::with_result(result))
    }

    async fn run_deployment(
        &self,
        request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let project_id = request.project_id;
        let deployment_id = request.deployment_id;
        let payload = request.payload;
        validate_payload(&payload)?;
        let applied_timeouts = self.timeout_safety.resolve(payload.timeouts)?;
        let applied_resources = self.containers.resolve_resources(payload.resources)?;
        let source = deployment_source(&payload, request.repository_credential)?;
        self.ensure_workspace_capacity().await?;
        self.containers.ensure_capacity(project_id).await?;

        emit_event(
            reporter.as_ref(),
            "deployment.resources.resolved",
            "Runtime resource request passed node safety checks.",
            json!({
                "requested": payload.resources,
                "applied": applied_resources,
            }),
        )
        .await?;

        emit_event(
            reporter.as_ref(),
            "deployment.timeouts.resolved",
            "Runtime timeout request passed node safety checks.",
            json!({
                "build_timeout_seconds": applied_timeouts.build.as_secs(),
                "start_timeout_seconds": applied_timeouts.start.as_secs(),
            }),
        )
        .await?;

        emit_event(
            reporter.as_ref(),
            "deployment.checkout.started",
            "Cloning repository at the requested commit.",
            json!({ "commit_sha": payload.commit_sha }),
        )
        .await?;
        let workspace = self
            .workspace
            .checkout(
                request.command_id,
                &source,
                reporter.as_ref(),
                request.cancellation.clone(),
            )
            .await?;
        drop(source);
        let image = image_name(project_id, deployment_id, &payload.commit_sha);
        let container = container_name(project_id, deployment_id);
        let route_activated = AtomicBool::new(false);
        let candidate_started = AtomicBool::new(false);
        let cutover_started = AtomicBool::new(false);

        let deployment = self.deploy_inner(
            request.command_id,
            project_id,
            deployment_id,
            &payload,
            &workspace,
            &image,
            &container,
            applied_resources,
            applied_timeouts,
            &route_activated,
            &candidate_started,
            &cutover_started,
            Arc::clone(&reporter),
        );
        tokio::pin!(deployment);
        let mut result = tokio::select! {
            biased;
            result = &mut deployment => result,
            () = request.cancellation.cancelled() => {
                // Dropping a route activation halfway could leave the project's
                // route pointing at the candidate that cleanup is about to remove,
                // which would take down the deployment it was replacing. It either
                // commits or rolls itself back, so let it finish.
                if cutover_started.load(Ordering::Acquire) {
                    deployment.await
                } else {
                    Err(RuntimeError::Cancelled)
                }
            },
        };

        // The app's output only exists in the container, so it must be read before
        // cleanup removes it. A cancelled command is not an app failure, and its
        // shutdown grace is better spent on cleanup.
        let failed_before_cutover = result.is_err() && !route_activated.load(Ordering::Acquire);
        if failed_before_cutover
            && candidate_started.load(Ordering::Acquire)
            && !matches!(result, Err(RuntimeError::Cancelled))
        {
            let output = self
                .report_candidate_output(&container, payload.log_bounds, reporter.as_ref())
                .await;
            result = result.map_err(|error| error.refine_crash(output.exec_denied));
        }
        if failed_before_cutover
            && let Err(error) = self.containers.cleanup_candidate(&container, &image).await
        {
            let _ = reporter
                .log(system_log(format!(
                    "candidate cleanup warning for {container}: {error}"
                )))
                .await;
        }
        // The path is the node's, so it stays in the Agent's own log.
        if let Err(error) = self.workspace.cleanup(&workspace).await {
            tracing::warn!(
                workspace = %workspace.root().display(),
                %error,
                "deployment workspace cleanup failed"
            );
            let _ = reporter
                .log(system_log(
                    "The build files of this deployment could not be fully removed from the server. This does not affect the deployment."
                        .to_owned(),
                ))
                .await;
        }

        result?;
        let completion = serde_json::to_value(DeployProjectResult {
            requested_resources: payload.resources,
            applied_resources,
            finalization_deferred: false,
            finalization_deferred_reason: None,
        })
        .map_err(|error| {
            RuntimeError::Execution(format!("failed to serialize deployment result: {error}"))
        })?;
        Ok(CommandOutput::with_result(completion))
    }

    /// Best effort by design: nothing here may replace the deployment's own error,
    /// though what the output shows may refine it.
    async fn report_candidate_output(
        &self,
        container: &str,
        log_bounds: LogBounds,
        reporter: &dyn RuntimeReporter,
    ) -> FailureOutput {
        let _ = reporter
            .log(system_log(
                "Last application output before the failed start was removed:".to_owned(),
            ))
            .await;
        let output = match self
            .containers
            .report_failure_logs(container, log_bounds, reporter)
            .await
        {
            Ok(output) => {
                if !output.had_output {
                    let _ = reporter
                        .log(system_log(
                            "The application wrote no output before it failed.".to_owned(),
                        ))
                        .await;
                }
                output
            }
            Err(error) => {
                let _ = reporter
                    .log(system_log(format!(
                        "application output could not be collected: {error}"
                    )))
                    .await;
                FailureOutput::default()
            }
        };
        if let Err(error) = reporter.flush().await {
            tracing::warn!(%error, "failed start output could not be fully delivered");
        }
        output
    }

    async fn ensure_workspace_capacity(&self) -> Result<(), RuntimeError> {
        let available = self.workspace.available_disk_bytes().await?;
        if available < self.min_workspace_free_bytes {
            return Err(RuntimeError::DiskPressure(format!(
                "workspace has {available} free bytes; node requires at least {} bytes before a deployment build",
                self.min_workspace_free_bytes
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn deploy_inner(
        &self,
        command_id: Uuid,
        project_id: Uuid,
        deployment_id: Uuid,
        payload: &DeployProjectPayload,
        workspace: &DeploymentWorkspace,
        image: &str,
        container: &str,
        applied_resources: AppliedRuntimeResources,
        applied_timeouts: crate::config::AppliedRuntimeTimeouts,
        route_activated: &AtomicBool,
        candidate_started: &AtomicBool,
        cutover_started: &AtomicBool,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<(), RuntimeError> {
        let reporter_ref = reporter.as_ref();
        let context = workspace
            .build_context(payload.root_directory.as_deref())
            .await?;
        let build_permit = Arc::clone(&self.build_permits)
            .acquire_owned()
            .await
            .map_err(|_| {
                RuntimeError::Execution("build concurrency scheduler is unavailable".to_owned())
            })?;
        let build = tokio::time::timeout(
            applied_timeouts.build,
            self.builder.build(
                &BuildRequest {
                    project_id,
                    deployment_id,
                    workspace: workspace.root().to_owned(),
                    source: context,
                    image: image.to_owned(),
                    requested: payload.builder,
                    environment: build_environment(payload),
                    build_command: command_field(payload.build_command.as_deref()),
                    start_command: command_field(payload.start_command.as_deref()),
                },
                reporter_ref,
            ),
        )
        .await
        .map_err(|_| RuntimeError::Timeout {
            operation: "deployment-build".to_owned(),
            seconds: applied_timeouts.build.as_secs(),
        })??;
        // Build concurrency protects builder capacity only; start/readiness and routing
        // must not prevent another deployment from entering the build phase.
        drop(build_permit);

        emit_event(
            reporter_ref,
            "deployment.container.started",
            "Starting candidate application container.",
            json!({ "container": container, "image": image }),
        )
        .await?;
        let start_began = Instant::now();
        tokio::time::timeout(applied_timeouts.start, async {
            // The early capacity check is only a fast-fail optimization. This
            // serialized re-check is authoritative and closes concurrent admission races.
            let admission = self.container_admission.lock().await;
            self.containers.ensure_capacity(project_id).await?;
            self.containers
                .start(
                    &RunContainerRequest {
                        command_id,
                        project_id,
                        deployment_id,
                        name: container.to_owned(),
                        image: image.to_owned(),
                        workspace: workspace.root().to_owned(),
                        environment: payload.environment.clone(),
                        resources: applied_resources,
                        domain: payload.domain.clone(),
                        port: payload.container_port,
                        log_bounds: payload.log_bounds,
                        // A Railpack image already starts with this command.
                        start_command: command_field(payload.start_command.as_deref())
                            .filter(|_| build.builder == DeploymentBuilder::Dockerfile),
                    },
                    reporter_ref,
                )
                .await?;
            drop(admission);
            Ok::<(), RuntimeError>(())
        })
        .await
        .map_err(|_| RuntimeError::Timeout {
            operation: "deployment-start".to_owned(),
            seconds: applied_timeouts.start.as_secs(),
        })??;
        candidate_started.store(true, Ordering::Release);
        emit_event(
            reporter_ref,
            "deployment.health.started",
            "Waiting for the application to answer on its port.",
            json!({
                "port": payload.container_port,
                "timeout_seconds": applied_timeouts.start.as_secs(),
            }),
        )
        .await?;
        self.health
            .wait_until_ready(&ReadinessRequest {
                container: container.to_owned(),
                port: payload.container_port,
                started_at: start_began,
                timeout: applied_timeouts.start,
                expected_restarts: Some(0),
                operation: "deployment-start",
            })
            .await?;
        emit_event(
            reporter_ref,
            "deployment.route.started",
            "Application answered; activating its route.",
            json!({ "domain": payload.domain, "port": payload.container_port }),
        )
        .await?;
        cutover_started.store(true, Ordering::Release);
        self.routes
            .activate(
                &RouteSpec {
                    project_id,
                    deployment_id,
                    domain: payload.domain.clone(),
                    upstream: container.to_owned(),
                    port: payload.container_port,
                },
                reporter_ref,
            )
            .await?;
        route_activated.store(true, Ordering::Release);
        reporter_ref.mark_deployment_committed(CommandOutput::with_result(json!({
            "requested_resources": payload.resources,
            "applied_resources": applied_resources,
        })));

        if let Err(error) = self
            .containers
            .report_startup_logs(container, reporter_ref)
            .await
        {
            let _ = reporter
                .log(system_log(format!(
                    "startup log collection warning for {container}: {error}"
                )))
                .await;
        }
        let finalization_error = match self
            .containers
            .cleanup_previous(project_id, container, reporter_ref)
            .await
        {
            Ok(()) => None,
            Err(error) => {
                let _ = reporter
                    .log(system_log(format!(
                        "previous container cleanup warning for project {project_id}: {error}"
                    )))
                    .await;
                Some(error)
            }
        };

        if let Err(error) = emit_event(
            reporter_ref,
            "deployment.runtime.ready",
            "Application container and route are ready.",
            json!({
                "builder": build.builder,
                "container": container,
                "domain": payload.domain,
                "image": image,
                "resources": applied_resources,
            }),
        )
        .await
        {
            tracing::warn!(
                %error,
                %project_id,
                %deployment_id,
                "deployment committed but ready event reporting failed"
            );
        }
        let _ = self.containers.start_log_follower(container, reporter);
        if let Some(error) = finalization_error {
            return Err(error);
        }
        Ok(())
    }

    async fn workload(
        &self,
        request: &WorkloadLifecycleRequest,
    ) -> Result<ManagedWorkload, RuntimeError> {
        self.containers
            .workload(request.project_id, request.deployment_id)
            .await?
            .ok_or(RuntimeError::WorkloadNotFound)
    }

    /// A lifecycle command has no start deadline of its own in its payload.
    fn workload_readiness(
        &self,
        workload: &ManagedWorkload,
        timeout: std::time::Duration,
    ) -> ReadinessRequest {
        ReadinessRequest {
            container: container_name(workload.project_id, workload.deployment_id),
            port: workload.port,
            started_at: Instant::now(),
            timeout,
            expected_restarts: None,
            operation: "workload-readiness",
        }
    }

    /// A follower that outlives its command has lost the payload's values, so
    /// they are read back from the container. Without them it still redacts by
    /// key name and token prefix, so a failure here does not stop its logs.
    async fn follower_redactor(&self, container: &str) -> ValueRedactor {
        match self.containers.environment_values(container).await {
            Ok(values) => ValueRedactor::new(values),
            Err(error) => {
                tracing::warn!(
                    %error,
                    container,
                    "log follower cannot redact environment values for this workload"
                );
                ValueRedactor::default()
            }
        }
    }

    async fn activate_workload(
        &self,
        workload: &ManagedWorkload,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.health
            .wait_until_ready(
                &self.workload_readiness(workload, self.timeout_safety.max_start_timeout),
            )
            .await?;
        self.routes
            .activate(
                &RouteSpec {
                    project_id: workload.project_id,
                    deployment_id: workload.deployment_id,
                    domain: workload.domain.clone(),
                    upstream: container_name(workload.project_id, workload.deployment_id),
                    port: workload.port,
                },
                reporter,
            )
            .await
    }

    async fn run_restart(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workload = self.workload(&request).await?;
        if !workload.status.to_ascii_lowercase().starts_with("up") {
            return Err(RuntimeError::WorkloadNotRunning);
        }
        self.containers.restart(&workload, 10).await?;
        self.activate_workload(&workload, reporter.as_ref()).await?;
        emit_event(
            reporter.as_ref(),
            "workload.restart.completed",
            "Workload restarted and route revalidated.",
            json!({ "project_id": workload.project_id, "deployment_id": workload.deployment_id }),
        )
        .await?;
        Ok(CommandOutput::with_result(json!({ "status": "ready" })))
    }

    async fn run_stop(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
        remove: bool,
    ) -> Result<CommandOutput, RuntimeError> {
        let Some(workload) = self
            .containers
            .workload(request.project_id, request.deployment_id)
            .await?
        else {
            if remove {
                self.routes
                    .deactivate(
                        RouteIdentity {
                            project_id: request.project_id,
                            deployment_id: Some(request.deployment_id),
                        },
                        reporter.as_ref(),
                    )
                    .await?;
                let mut result = json!({ "status": "already_stopped" });
                if request.remove_images {
                    self.remove_project_images(request.project_id, &mut result, reporter.as_ref())
                        .await?;
                }
                return Ok(CommandOutput::with_result(result));
            }
            return Err(RuntimeError::WorkloadNotFound);
        };
        self.containers.stop(&workload, 10).await?;
        self.routes
            .deactivate(
                RouteIdentity {
                    project_id: workload.project_id,
                    deployment_id: Some(workload.deployment_id),
                },
                reporter.as_ref(),
            )
            .await?;
        if remove {
            self.containers.remove(&workload).await?;
        }
        let remove_images = remove && request.remove_images;
        let event_type = if remove {
            "workload.stop.completed"
        } else {
            "workload.sleep.completed"
        };
        emit_event(
            reporter.as_ref(),
            event_type,
            if remove_images {
                "Workload stopped and container removed; project images are removed next."
            } else if remove {
                "Workload stopped and container removed; image is retained."
            } else {
                "Workload stopped for sleep; container and image are retained."
            },
            json!({ "project_id": workload.project_id, "deployment_id": workload.deployment_id }),
        )
        .await?;
        let mut result = json!({ "status": if remove { "stopped" } else { "sleeping" } });
        if remove_images {
            self.remove_project_images(workload.project_id, &mut result, reporter.as_ref())
                .await?;
        }
        Ok(CommandOutput::with_result(result))
    }

    /// Removes the images of a deleted project once its container is gone, and
    /// adds the counts to the stop result. The stop itself already happened, so
    /// a Docker failure here becomes an event and is left to the periodic image
    /// GC instead of failing the command.
    async fn remove_project_images(
        &self,
        project_id: Uuid,
        result: &mut serde_json::Value,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let cleanup = match self.containers.remove_project_images(project_id).await {
            Ok(cleanup) => cleanup,
            Err(error) => {
                tracing::warn!(%project_id, %error, "project image removal failed");
                emit_event(
                    reporter,
                    "workload.images.retained",
                    "Project images could not be removed now; image GC removes them later.",
                    json!({ "project_id": project_id }),
                )
                .await?;
                ImageCleanup::default()
            }
        };
        result["removed_images"] = json!(cleanup.removed_images);
        result["reclaimed_image_bytes"] = json!(cleanup.reclaimed_bytes);
        Ok(())
    }

    async fn run_wake(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workload = self.workload(&request).await?;
        self.containers.start_existing(&workload).await?;
        self.activate_workload(&workload, reporter.as_ref()).await?;
        emit_event(
            reporter.as_ref(),
            "workload.wake.completed",
            "Sleeping workload is ready and its route is restored.",
            json!({ "project_id": workload.project_id, "deployment_id": workload.deployment_id }),
        )
        .await?;
        Ok(CommandOutput::with_result(json!({ "status": "ready" })))
    }

    async fn run_health_check(
        &self,
        request: WorkloadLifecycleRequest,
    ) -> Result<CommandOutput, RuntimeError> {
        let workload = self.workload(&request).await?;
        let running = workload.status.to_ascii_lowercase().starts_with("up");
        let readiness = if running {
            self.health
                .wait_until_ready(&self.workload_readiness(&workload, self.health_check_window))
                .await
        } else {
            Err(RuntimeError::WorkloadNotRunning)
        };
        let failure = readiness.err();
        Ok(CommandOutput::with_result(json!({
            "container_id": workload.container_id,
            "running": running,
            "ready": failure.is_none(),
            "docker_status": workload.status,
            "reason": failure.as_ref().map(ToString::to_string),
            "error_code": failure.as_ref().map(RuntimeError::code),
        })))
    }

    async fn run_refresh_route(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workload = self.workload(&request).await?;
        if !workload.status.to_ascii_lowercase().starts_with("up") {
            return Err(RuntimeError::WorkloadNotRunning);
        }
        self.activate_workload(&workload, reporter.as_ref()).await?;
        Ok(CommandOutput::with_result(json!({ "status": "ready" })))
    }
}

impl DockerPreflight {
    async fn run(&self) -> Result<RuntimePreflightReport, RuntimeError> {
        // First, because `workspace-disk` fails on a path that does not exist,
        // which on a fresh node would be this directory.
        let workspace = directory_check("workspace", &self.workspace_root, true).await;
        let mut checks = vec![
            self.command_check("git", CommandSpec::new("git").arg("--version"))
                .await,
            self.command_check(
                "docker",
                CommandSpec::new("docker")
                    .arg("version")
                    .arg("--format")
                    .arg("{{.Server.Version}}"),
            )
            .await,
            self.command_check(
                "docker-buildx",
                CommandSpec::new("docker").arg("buildx").arg("version"),
            )
            .await,
            self.command_check(
                "caddy-routing",
                CommandSpec::new("docker")
                    .arg("inspect")
                    .arg(&self.caddy_container),
            )
            .await,
            self.command_check(
                "runtime-network",
                CommandSpec::new("docker")
                    .arg("network")
                    .arg("inspect")
                    .arg(&self.runtime_network),
            )
            .await,
            self.command_check(
                "workspace-disk",
                CommandSpec::new("df")
                    .arg("-Pk")
                    .arg(self.workspace_root.as_os_str()),
            )
            .await,
        ];
        // Not fatal: a stopped router is already a degraded heartbeat, and a node
        // must keep starting in any order relative to it. Readiness then fails
        // with a message that names this dependency.
        let mut http_probe = self
            .command_check(
                "caddy-http-probe",
                CommandSpec::new("docker")
                    .arg("exec")
                    .arg(&self.caddy_container)
                    .arg("wget")
                    .arg("--help"),
            )
            .await;
        http_probe.fatal = false;
        checks.push(http_probe);
        checks.push(self.railpack_sandbox_check().await);
        if let Some(runtime) = &self.container_runtime {
            checks.extend(self.container_runtime_checks(runtime).await);
        }
        checks.push(workspace);
        checks.push(directory_check("caddy-sites", &self.caddy_sites_dir, true).await);

        Ok(RuntimePreflightReport { checks })
    }

    /// Railpack only ever runs in its sandbox, so the host needs the image, not the
    /// CLI. A floating tag is refused outright.
    async fn railpack_sandbox_check(&self) -> RuntimePreflightCheck {
        const NAME: &str = "railpack-sandbox";
        let failed = |detail: String| RuntimePreflightCheck {
            name: NAME.to_owned(),
            fatal: true,
            ready: false,
            detail,
        };
        if !is_pinned_image(&self.railpack_image) {
            return failed("SAKALA_RAILPACK_IMAGE must be set and pinned by digest".to_owned());
        }
        if let Err(error) = self.railpack_sandbox.ensure_network().await {
            return failed(error.to_string());
        }
        self.command_check(
            NAME,
            CommandSpec::new("docker")
                .arg("image")
                .arg("inspect")
                .arg("--format")
                .arg("{{.Id}}")
                .arg(&self.railpack_image),
        )
        .await
    }

    /// Fatal when the runtime is missing, because the alternative is finding out on the
    /// first deployment, and a node that was promised an isolation runtime must not
    /// start without it. What the runtime then cannot do is only reported.
    async fn container_runtime_checks(&self, runtime: &str) -> Vec<RuntimePreflightCheck> {
        let registered = |ready: bool, detail: String| RuntimePreflightCheck {
            name: "container-runtime".to_owned(),
            fatal: true,
            ready,
            detail,
        };
        let listing = crate::telemetry::registered_runtimes_command();
        let output = match self.runner.run(&listing, &crate::NullOutputSink).await {
            Ok(output) if output.success => output,
            Ok(output) => {
                return vec![registered(
                    false,
                    format!(
                        "could not list Docker's runtimes: {}",
                        crate::error::describe_exit(output.code)
                    ),
                )];
            }
            Err(error) => {
                return vec![registered(
                    false,
                    format!("could not list Docker's runtimes: {error}"),
                )];
            }
        };
        let names = crate::telemetry::registered_runtime_names(&output.stdout);
        if !names.contains(&runtime) {
            return vec![registered(
                false,
                format!(
                    "SAKALA_CONTAINER_RUNTIME is {runtime}, but Docker has no runtime of that name \
                     (registered: {}). Register it under \"runtimes\" in Docker's daemon.json and \
                     restart Docker. The Agent does not fall back to another runtime.",
                    names.join(", ")
                ),
            )];
        }
        let warning = |name: &str, ready: bool, detail: String| RuntimePreflightCheck {
            name: name.to_owned(),
            fatal: false,
            ready,
            detail,
        };
        let ignores_cgroups = crate::telemetry::runtime_ignores_cgroups(&output.stdout, runtime);
        vec![
            registered(true, runtime.to_owned()),
            warning(
                "container-runtime-limits",
                !ignores_cgroups,
                if ignores_cgroups {
                    format!(
                        "{runtime} runs with --ignore-cgroups, so memory, CPU, and process limits \
                         are not enforced on workloads. See docs/RUNTIME_HARDENING.md."
                    )
                } else {
                    "enforced through cgroups".to_owned()
                },
            ),
            warning(
                "container-dns",
                self.container_dns_configured,
                if self.container_dns_configured {
                    "SAKALA_CONTAINER_DNS is set".to_owned()
                } else {
                    format!(
                        "SAKALA_CONTAINER_DNS is not set. Under {runtime}, Docker's embedded DNS is \
                         out of reach, so a workload cannot resolve any hostname."
                    )
                },
            ),
        ]
    }

    async fn command_check(&self, name: &str, command: CommandSpec) -> RuntimePreflightCheck {
        match self.runner.run(&command, &crate::NullOutputSink).await {
            Ok(output) if output.success => RuntimePreflightCheck {
                name: name.to_owned(),
                fatal: true,
                ready: true,
                detail: output.stdout.trim().to_owned(),
            },
            Ok(output) => RuntimePreflightCheck {
                name: name.to_owned(),
                fatal: true,
                ready: false,
                detail: format!(
                    "command failed with {}",
                    crate::error::describe_exit(output.code)
                ),
            },
            Err(error) => RuntimePreflightCheck {
                name: name.to_owned(),
                fatal: true,
                ready: false,
                detail: error.to_string(),
            },
        }
    }
}

async fn directory_check(name: &str, path: &std::path::Path, fatal: bool) -> RuntimePreflightCheck {
    match tokio::fs::create_dir_all(path).await {
        Ok(()) => RuntimePreflightCheck {
            name: name.to_owned(),
            fatal,
            ready: true,
            detail: path.display().to_string(),
        },
        Err(error) => RuntimePreflightCheck {
            name: name.to_owned(),
            fatal,
            ready: false,
            detail: format!("{}: {error}", path.display()),
        },
    }
}

fn routable_routes(
    report: &RuntimeReconciliationReport,
) -> std::collections::HashSet<RouteIdentity> {
    report
        .workloads
        .iter()
        .filter(|workload| workload.status.to_ascii_lowercase().starts_with("up"))
        .map(|workload| RouteIdentity {
            project_id: workload.project_id,
            deployment_id: Some(workload.deployment_id),
        })
        .collect()
}

#[async_trait]
impl RuntimeExecutor for DockerRuntimeExecutor {
    async fn preflight(&self) -> Result<RuntimePreflightReport, RuntimeExecutionError> {
        self.preflight.run().await.map_err(Into::into)
    }

    async fn reconcile(&self) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        let mut report = self.containers.detect_orphans().await?;
        let known_routes = routable_routes(&report);
        report.stale_routes = self.routes.discover_stale_routes(&known_routes).await?;
        report.stale_images = self
            .containers
            .detect_stale_images(self.image_retention)
            .await?;
        report.cleaned_workspaces = self
            .workspace
            .cleanup_stale(self.workspace_gc_max_age)
            .await?;
        Ok(report)
    }

    async fn recover(
        &self,
        reporter_factory: Option<Arc<dyn RuntimeReporterFactory>>,
    ) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        let mut report = self.reconcile().await?;
        for discovered in report.workloads.clone() {
            let workload = match self
                .containers
                .workload(discovered.project_id, discovered.deployment_id)
                .await
            {
                Ok(Some(workload)) => workload,
                Ok(None) => continue,
                Err(error) => {
                    report.compatibility_issues.push(RuntimeCompatibilityIssue {
                        container_id: discovered.container_id,
                        project_id: discovered.project_id,
                        deployment_id: discovered.deployment_id,
                        reason: format!(
                            "managed workload metadata is incompatible with this Agent: {error}; redeploy is required"
                        ),
                    });
                    continue;
                }
            };
            let Some(command_id) = workload.command_id else {
                report.compatibility_issues.push(RuntimeCompatibilityIssue {
                    container_id: discovered.container_id,
                    project_id: discovered.project_id,
                    deployment_id: discovered.deployment_id,
                    reason:
                        "managed workload predates recovery command-id labels; redeploy is required"
                            .to_owned(),
                });
                continue;
            };
            report.recovered_execution_records += 1;
            if !workload.status.to_ascii_lowercase().starts_with("up") {
                continue;
            }
            let Some(factory) = &reporter_factory else {
                continue;
            };
            let redactor = self.follower_redactor(&workload.container_id).await;
            let reporter = factory.reporter(command_id, workload.log_bounds, redactor);
            if self
                .containers
                .start_log_follower(&workload.container_id, reporter)
            {
                report.reattached_log_followers += 1;
            }
        }
        Ok(report)
    }

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeExecutionError> {
        let mut capacity = self.containers.capacity().await?;
        capacity.active_builds = Some(
            self.max_concurrent_builds
                .saturating_sub(self.build_permits.available_permits()),
        );
        capacity.maximum_concurrent_builds = Some(self.max_concurrent_builds);
        Ok(capacity)
    }

    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeExecutionError> {
        self.containers.health_snapshot().await.map_err(Into::into)
    }

    async fn crashed_workloads(
        &self,
        snapshots: &[RuntimeHealthSnapshot],
    ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeExecutionError> {
        self.containers
            .crashed_workloads(snapshots)
            .await
            .map_err(Into::into)
    }

    async fn node_telemetry(&self) -> Result<NodeTelemetry, RuntimeExecutionError> {
        Ok(self.telemetry.snapshot().await)
    }

    async fn reconcile_workload(
        &self,
        request: ReconcileWorkloadRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        if request.cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled.into());
        }
        let workload = self
            .containers
            .workload(request.project_id, request.deployment_id)
            .await?;
        let actual_state = match &workload {
            None => "missing",
            Some(workload) if workload.status.to_ascii_lowercase().starts_with("up") => "running",
            Some(_) => "stopped",
        };
        let desired_state = match request.desired_state {
            sakala_agent_protocol::DesiredWorkloadState::Running => "running",
            sakala_agent_protocol::DesiredWorkloadState::Stopped => "stopped",
            sakala_agent_protocol::DesiredWorkloadState::Missing => "missing",
        };
        let mut actions_applied = Vec::new();
        for action in request.actions {
            if request.cancellation.is_cancelled() {
                return Err(RuntimeError::Cancelled.into());
            }
            match action {
                ReconcileWorkloadAction::RestartLogFollower => {
                    let workload = workload.as_ref().ok_or(RuntimeError::WorkloadNotFound)?;
                    if !workload.status.to_ascii_lowercase().starts_with("up") {
                        return Err(RuntimeError::WorkloadNotRunning.into());
                    }
                    // The follower outlives this reconcile command, so it must
                    // report under the workload's original DeployProject
                    // identity, exactly like startup recovery does.
                    let Some(command_id) = workload.command_id else {
                        return Err(RuntimeError::InvalidCommand(
                            "managed workload predates recovery command-id labels; redeploy is required before its log follower can be restarted"
                                .to_owned(),
                        )
                        .into());
                    };
                    let Some(factory) = &request.reporter_factory else {
                        return Err(RuntimeError::InvalidCommand(
                            "restart_log_follower requires a connected control plane reporter"
                                .to_owned(),
                        )
                        .into());
                    };
                    let redactor = self.follower_redactor(&workload.container_id).await;
                    let follower_reporter =
                        factory.reporter(command_id, workload.log_bounds, redactor);
                    let started = self
                        .containers
                        .start_log_follower(&workload.container_id, follower_reporter);
                    actions_applied.push(json!({
                        "action": "restart_log_follower",
                        "started": started,
                        "command_id": command_id,
                    }));
                }
                ReconcileWorkloadAction::CleanupFailedCandidate => {
                    let workload = workload.as_ref().ok_or(RuntimeError::WorkloadNotFound)?;
                    let status = workload.status.to_ascii_lowercase();
                    if !(status.starts_with("created")
                        || status.starts_with("exited")
                        || status.starts_with("dead"))
                    {
                        return Err(RuntimeError::InvalidCommand(
                            "cleanup_failed_candidate requires a Created, Exited, or Dead workload"
                                .to_owned(),
                        )
                        .into());
                    }
                    self.containers.remove(workload).await?;
                    actions_applied.push(json!({
                        "action": "cleanup_failed_candidate",
                        "container_id": workload.container_id,
                    }));
                }
                ReconcileWorkloadAction::RestoreRoute => {
                    let workload = workload.as_ref().ok_or(RuntimeError::WorkloadNotFound)?;
                    if !workload.status.to_ascii_lowercase().starts_with("up") {
                        return Err(RuntimeError::WorkloadNotRunning.into());
                    }
                    self.activate_workload(workload, reporter.as_ref()).await?;
                    actions_applied.push(json!({ "action": "restore_route" }));
                }
            }
        }
        Ok(CommandOutput::with_result(json!({
            "desired_state": desired_state,
            "actual_state": actual_state,
            "in_sync": desired_state == actual_state,
            "drift_reason": (desired_state != actual_state).then_some("workload_state_mismatch"),
            "container_id": workload.map(|workload| workload.container_id),
            "actions_applied": actions_applied,
        })))
    }

    async fn collect_stale_images(&self) -> Result<RuntimeImageCleanup, RuntimeExecutionError> {
        let cleanup = self
            .containers
            .cleanup_stale_images(self.image_retention)
            .await?;
        Ok(RuntimeImageCleanup {
            removed_images: cleanup.removed_images,
            reclaimed_bytes: cleanup.reclaimed_bytes,
        })
    }

    async fn cleanup_runtime(
        &self,
        request: CleanupRuntimeRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        if !request.approved {
            return Err(RuntimeError::InvalidCommand(
                "runtime cleanup requires explicit approval".to_owned(),
            )
            .into());
        }
        let mut cleaned_workspaces = 0;
        let mut removed_images = 0;
        let mut reclaimed_image_bytes = 0;
        let mut cleaned_routes = 0;
        for target in request.targets {
            if request.cancellation.is_cancelled() {
                return Err(RuntimeError::Cancelled.into());
            }
            match target {
                RuntimeCleanupTarget::StaleWorkspaces => {
                    cleaned_workspaces += self
                        .workspace
                        .cleanup_stale(self.workspace_gc_max_age)
                        .await?;
                }
                RuntimeCleanupTarget::StaleImages => {
                    let images = self
                        .containers
                        .cleanup_stale_images(self.image_retention)
                        .await?;
                    removed_images += images.removed_images;
                    reclaimed_image_bytes += images.reclaimed_bytes;
                }
                RuntimeCleanupTarget::StaleRoutes => {
                    let discovered = self.containers.detect_orphans().await?;
                    let known_routes = routable_routes(&discovered);
                    cleaned_routes += self
                        .routes
                        .cleanup_stale_routes(&known_routes, reporter.as_ref())
                        .await?;
                }
            }
        }
        Ok(CommandOutput::with_result(json!({
            "approved": true,
            "cleaned_workspaces": cleaned_workspaces,
            "cleaned_routes": cleaned_routes,
            "removed_images": removed_images,
            "reclaimed_image_bytes": reclaimed_image_bytes,
        })))
    }

    async fn shutdown(&self) -> Result<(), RuntimeExecutionError> {
        self.containers.shutdown().await;
        Ok(())
    }

    async fn inspect_project(
        &self,
        request: InspectProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_inspection(request, reporter)
            .await
            .map_err(Into::into)
    }

    async fn deploy_project(
        &self,
        request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_deployment(request, reporter)
            .await
            .map_err(Into::into)
    }

    async fn restart_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_restart(request, reporter)
            .await
            .map_err(Into::into)
    }

    async fn stop_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_stop(request, reporter, true)
            .await
            .map_err(Into::into)
    }

    async fn sleep_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_stop(request, reporter, false)
            .await
            .map_err(Into::into)
    }

    async fn wake_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_wake(request, reporter).await.map_err(Into::into)
    }

    async fn health_check(
        &self,
        request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_health_check(request).await.map_err(Into::into)
    }

    async fn refresh_route(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        self.run_refresh_route(request, reporter)
            .await
            .map_err(Into::into)
    }
}

/// An empty value is left out: nothing is lost, and BuildKit has no use for an
/// empty secret. `PORT` belongs to the runtime contract, not the build.
fn build_environment(
    payload: &DeployProjectPayload,
) -> Vec<(String, sakala_agent_core::ports::SecretString)> {
    payload
        .environment
        .iter()
        .filter(|(key, value)| key.as_str() != "PORT" && !value.is_empty())
        .map(|(key, value)| {
            (
                key.clone(),
                sakala_agent_core::ports::SecretString::new(value.clone()),
            )
        })
        .collect()
}

const MAX_COMMAND_CHARS: usize = 1_000;

/// An empty command means the project did not set one.
fn command_field(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
}

fn validate_command_field(name: &str, command: Option<&str>) -> Result<(), RuntimeError> {
    let Some(command) = command else {
        return Ok(());
    };
    if command.chars().count() > MAX_COMMAND_CHARS || command.contains(['\n', '\r', '\0']) {
        return Err(RuntimeError::InvalidCommand(format!(
            "{name} must be one line of at most {MAX_COMMAND_CHARS} characters"
        )));
    }
    Ok(())
}

fn validate_payload(payload: &DeployProjectPayload) -> Result<(), RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    if let Some(root_directory) = requested_root_directory(payload.root_directory.as_deref()) {
        validate_root_directory(root_directory)?;
    }
    validate_command_field("build_command", payload.build_command.as_deref())?;
    validate_command_field("start_command", payload.start_command.as_deref())?;
    if payload.container_port == 0 {
        return Err(RuntimeError::InvalidCommand(
            "container_port must be greater than zero".to_owned(),
        ));
    }
    if !valid_domain(&payload.domain) {
        return Err(RuntimeError::InvalidCommand(
            "domain must be a valid *.run.sakala.localhost, *.run.staging.sakala.dev, or *.run.sakala.dev hostname".to_owned(),
        ));
    }
    for (key, value) in &payload.environment {
        if !valid_env_key(key) || value.contains(['\n', '\r', '\0']) {
            return Err(RuntimeError::InvalidCommand(format!(
                "environment entry {key:?} is not safe for a Docker env file"
            )));
        }
    }
    // The agent exports PORT so the workload listens where the generated route
    // points. A deployment environment that disagrees would serve on one port
    // and be routed on another, so reject it rather than pick a winner.
    if let Some(port) = payload.environment.get("PORT") {
        let environment_port = port.parse::<u16>().ok();
        if environment_port != Some(payload.container_port) {
            return Err(RuntimeError::PortConflict {
                environment_port,
                container_port: payload.container_port,
            });
        }
    }
    Ok(())
}

fn inspection_source(
    payload: &InspectProjectPayload,
    credential: Option<sakala_agent_core::ports::RepositoryCredential>,
) -> Result<RepositorySource, RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    if let Some(root_directory) = requested_root_directory(payload.root_directory.as_deref()) {
        validate_root_directory(root_directory)?;
    }
    Ok(RepositorySource {
        repository_url: payload.repository_url.clone(),
        commit_sha: payload.commit_sha.clone(),
        credential,
    })
}

fn deployment_source(
    payload: &DeployProjectPayload,
    credential: Option<sakala_agent_core::ports::RepositoryCredential>,
) -> Result<RepositorySource, RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    Ok(RepositorySource {
        repository_url: payload.repository_url.clone(),
        commit_sha: payload.commit_sha.clone(),
        credential,
    })
}

fn validate_repository_source(repository_url: &str, commit_sha: &str) -> Result<(), RuntimeError> {
    let repository = Url::parse(repository_url).map_err(|error| {
        RuntimeError::InvalidCommand(format!("invalid repository URL: {error}"))
    })?;
    if repository.scheme() != "https"
        || repository.host_str() != Some("github.com")
        || !repository.username().is_empty()
        || repository.password().is_some()
        || repository.query().is_some()
        || repository.fragment().is_some()
    {
        return Err(RuntimeError::InvalidCommand(
            "repository URL must be a credential-free https://github.com URL".to_owned(),
        ));
    }
    let path_segments = repository
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if path_segments.len() != 2 {
        return Err(RuntimeError::InvalidCommand(
            "MVP repository URL must identify one GitHub owner and repository".to_owned(),
        ));
    }
    if commit_sha.len() != 40 || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RuntimeError::InvalidCommand(
            "commit_sha must be a complete 40-character Git SHA".to_owned(),
        ));
    }
    Ok(())
}

fn valid_domain(domain: &str) -> bool {
    let allowed_suffix = domain.ends_with(".run.sakala.localhost")
        || domain.ends_with(".run.staging.sakala.dev")
        || domain.ends_with(".run.sakala.dev");
    allowed_suffix
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_uppercase())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn system_log(message: String) -> DeploymentLog {
    DeploymentLog {
        stream: LogStream::System,
        message,
        recorded_at: OffsetDateTime::now_utc(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sakala_agent_protocol::DeployProjectPayload;

    use super::{valid_domain, validate_payload};

    fn payload() -> DeployProjectPayload {
        serde_json::from_value(serde_json::json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.run.staging.sakala.dev",
            "container_port": 3000,
        }))
        .expect("deploy payload")
    }

    #[test]
    fn accepts_staging_runtime_domains() {
        assert!(valid_domain("portfolio.run.staging.sakala.dev"));
    }

    #[test]
    fn rejects_domains_outside_runtime_zones() {
        assert!(!valid_domain("portfolio.staging.sakala.dev"));
        assert!(!valid_domain("portfolio.run.example.test"));
    }

    #[test]
    fn accepts_an_environment_port_matching_the_routed_port() {
        let mut payload = payload();
        payload.environment = BTreeMap::from([("PORT".to_owned(), "3000".to_owned())]);

        assert!(validate_payload(&payload).is_ok());
    }

    #[test]
    fn rejects_an_environment_port_that_contradicts_the_route() {
        let mut payload = payload();
        payload.environment = BTreeMap::from([("PORT".to_owned(), "8080".to_owned())]);

        let error = validate_payload(&payload).expect_err("conflicting PORT must be rejected");

        assert_eq!(error.code(), "runtime_port_conflict");
        assert!(error.to_string().contains("PORT=8080"));
        assert!(error.to_string().contains("must match container_port 3000"));
    }

    #[test]
    fn a_port_value_that_is_not_a_port_is_never_repeated_in_the_failure() {
        let mut payload = payload();
        payload.environment =
            BTreeMap::from([("PORT".to_owned(), "hunter2-not-a-port".to_owned())]);

        let error = validate_payload(&payload).expect_err("invalid PORT must be rejected");

        assert_eq!(error.code(), "runtime_port_conflict");
        assert!(error.to_string().contains("container_port 3000"));
        assert!(!error.to_string().contains("hunter2"));
    }
}
