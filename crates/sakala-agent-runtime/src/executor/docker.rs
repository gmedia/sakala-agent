use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    CleanupRuntimeRequest, CommandOutput, DeployProjectRequest, InspectProjectRequest,
    NodeTelemetry, ReconcileWorkloadRequest, RuntimeCapacity, RuntimeCompatibilityIssue,
    RuntimeExecutionError, RuntimeExecutor, RuntimeHealthSnapshot, RuntimePreflightCheck,
    RuntimePreflightReport, RuntimeReconciliationReport, RuntimeReporter, RuntimeReporterFactory,
    WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, DeployProjectPayload, DeployProjectResult, DeploymentEvent,
    DeploymentEventLevel, DeploymentLog, InspectProjectPayload, LogStream, ReconcileWorkloadAction,
    RuntimeCleanupTarget,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::{Mutex, Semaphore};
use url::Url;
use uuid::Uuid;

use crate::{
    CommandSpec, DockerRuntimeConfig, ProcessRunner, RuntimeError, TokioProcessRunner,
    builders::{BuildRequest, ImageBuildService, ImageBuilder},
    containers::{
        ContainerEngine, DockerContainerEngine, ManagedWorkload, RunContainerRequest,
        container_name, image_name,
    },
    health::{DockerHealthChecker, HealthChecker},
    inspections::{ProjectInspector, RailpackProjectInspector},
    routing::{
        CaddyFileRouteManager, DockerExecCaddyReloader, RouteIdentity, RouteManager, RouteSpec,
    },
    telemetry::NodeTelemetryCollector,
    workspace::{DeploymentWorkspace, GitWorkspaceManager, RepositorySource, WorkspaceManager},
};

pub struct DockerRuntimeExecutor {
    workspace: Arc<dyn WorkspaceManager>,
    inspector: Arc<dyn ProjectInspector>,
    builder: Arc<dyn ImageBuilder>,
    containers: Arc<dyn ContainerEngine>,
    health: Arc<dyn HealthChecker>,
    routes: Arc<dyn RouteManager>,
    timeout_safety: crate::TimeoutSafetyConfig,
    build_permits: Arc<Semaphore>,
    container_admission: Arc<Mutex<()>>,
    max_concurrent_builds: usize,
    workspace_gc_max_age: std::time::Duration,
    image_gc_max_age: std::time::Duration,
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
        };
        let agent_id = config.agent_id;
        let max_concurrent_builds = config.max_concurrent_builds;
        let workspace_gc_max_age = config.workspace_gc_max_age;
        let image_gc_max_age = config.image_gc_max_age;
        let min_workspace_free_bytes = config.min_workspace_free_bytes;
        let reloader = Arc::new(DockerExecCaddyReloader::new(
            Arc::clone(&runner),
            config.caddy_container,
        ));
        Self::with_services(
            Arc::new(GitWorkspaceManager::new(
                config.workspace_root,
                Arc::clone(&runner),
            )),
            Arc::new(RailpackProjectInspector::new(Arc::clone(&runner))),
            Arc::new(ImageBuildService::new(
                Arc::clone(&runner),
                config.railpack_frontend,
            )),
            Arc::new(DockerContainerEngine::new(
                Arc::clone(&runner),
                config.runtime_network,
                config.resource_safety,
                config.max_active_containers,
                agent_id,
            )),
            Arc::new(DockerHealthChecker::new(
                Arc::clone(&runner),
                config.health_attempts,
                config.health_interval,
            )),
            Arc::new(CaddyFileRouteManager::new(config.caddy_sites_dir, reloader)),
            config.timeout_safety,
            max_concurrent_builds,
            workspace_gc_max_age,
            image_gc_max_age,
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
        max_concurrent_builds: usize,
        workspace_gc_max_age: std::time::Duration,
        image_gc_max_age: std::time::Duration,
        min_workspace_free_bytes: u64,
        preflight: DockerPreflight,
    ) -> Self {
        let telemetry = NodeTelemetryCollector::new(
            Arc::clone(&preflight.runner),
            preflight.workspace_root.clone(),
            preflight.runtime_network.clone(),
            preflight.caddy_container.clone(),
        );
        Self {
            workspace,
            inspector,
            builder,
            containers,
            health,
            routes,
            timeout_safety,
            build_permits: Arc::new(Semaphore::new(max_concurrent_builds)),
            container_admission: Arc::new(Mutex::new(())),
            max_concurrent_builds,
            workspace_gc_max_age,
            image_gc_max_age,
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
        let result = tokio::select! {
            result = self.inspector.inspect(&workspace, &inspection_source, reporter) => result,
            () = request.cancellation.cancelled() => Err(RuntimeError::Cancelled),
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
            Arc::clone(&reporter),
        );
        tokio::pin!(deployment);
        let result = tokio::select! {
            biased;
            result = &mut deployment => result,
            () = request.cancellation.cancelled() => {
                if route_activated.load(Ordering::Acquire) {
                    deployment.await
                } else {
                    Err(RuntimeError::Cancelled)
                }
            },
        };

        if result.is_err()
            && !route_activated.load(Ordering::Acquire)
            && let Err(error) = self.containers.cleanup_candidate(&container, &image).await
        {
            let _ = reporter
                .log(system_log(format!(
                    "candidate cleanup warning for {container}: {error}"
                )))
                .await;
        }
        if let Err(error) = self.workspace.cleanup(&workspace).await {
            let _ = reporter
                .log(system_log(format!(
                    "workspace cleanup warning for {}: {error}",
                    workspace.root().display()
                )))
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
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<(), RuntimeError> {
        let reporter_ref = reporter.as_ref();
        let build_permit = Arc::clone(&self.build_permits)
            .acquire_owned()
            .await
            .map_err(|_| {
                RuntimeError::Execution("build concurrency scheduler is unavailable".to_owned())
            })?;
        emit_event(
            reporter_ref,
            "deployment.build.started",
            "Preparing application image.",
            json!({ "requested_builder": payload.builder }),
        )
        .await?;
        let build = tokio::time::timeout(
            applied_timeouts.build,
            self.builder.build(
                &BuildRequest {
                    project_id,
                    deployment_id,
                    workspace: workspace.root().to_owned(),
                    source: workspace.source().to_owned(),
                    image: image.to_owned(),
                    requested: payload.builder,
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
                    },
                    reporter_ref,
                )
                .await?;
            drop(admission);
            self.health.wait_until_ready(container).await
        })
        .await
        .map_err(|_| RuntimeError::Timeout {
            operation: "deployment-start".to_owned(),
            seconds: applied_timeouts.start.as_secs(),
        })??;
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

    async fn activate_workload(
        &self,
        workload: &ManagedWorkload,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.health.wait_until_ready(&workload.container_id).await?;
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
                return Ok(CommandOutput::with_result(
                    json!({ "status": "already_stopped" }),
                ));
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
        let event_type = if remove {
            "workload.stop.completed"
        } else {
            "workload.sleep.completed"
        };
        emit_event(
            reporter.as_ref(),
            event_type,
            if remove {
                "Workload stopped and container removed; image is retained."
            } else {
                "Workload stopped for sleep; container and image are retained."
            },
            json!({ "project_id": workload.project_id, "deployment_id": workload.deployment_id }),
        )
        .await?;
        Ok(CommandOutput::with_result(
            json!({ "status": if remove { "stopped" } else { "sleeping" } }),
        ))
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
            self.health.wait_until_ready(&workload.container_id).await
        } else {
            Err(RuntimeError::WorkloadNotRunning)
        };
        let ready = readiness.is_ok();
        let reason = readiness.err().map(|error| error.to_string());
        Ok(CommandOutput::with_result(json!({
            "container_id": workload.container_id,
            "running": running,
            "ready": ready,
            "docker_status": workload.status,
            "reason": reason,
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
            self.command_check("railpack", CommandSpec::new("railpack").arg("--version"))
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
        checks.push(directory_check("workspace", &self.workspace_root, true).await);
        checks.push(directory_check("caddy-sites", &self.caddy_sites_dir, true).await);

        Ok(RuntimePreflightReport { checks })
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
                detail: format!("command exited with status {:?}", output.code),
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
            .detect_stale_images(self.image_gc_max_age)
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
            let reporter = factory.reporter(command_id, workload.log_bounds);
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
                    let started = self
                        .containers
                        .start_log_follower(&workload.container_id, Arc::clone(&reporter));
                    actions_applied.push(json!({
                        "action": "restart_log_follower",
                        "started": started,
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
                    reclaimed_image_bytes += self
                        .containers
                        .cleanup_stale_images(self.image_gc_max_age)
                        .await?;
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

fn validate_payload(payload: &DeployProjectPayload) -> Result<(), RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
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
    Ok(())
}

fn inspection_source(
    payload: &InspectProjectPayload,
    credential: Option<sakala_agent_core::ports::RepositoryCredential>,
) -> Result<RepositorySource, RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
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

async fn emit_event(
    reporter: &dyn RuntimeReporter,
    event_type: &str,
    message: &str,
    metadata: serde_json::Value,
) -> Result<(), RuntimeError> {
    reporter
        .event(DeploymentEvent {
            event_type: event_type.to_owned(),
            level: DeploymentEventLevel::Info,
            message: message.to_owned(),
            metadata,
            occurred_at: OffsetDateTime::now_utc(),
        })
        .await
        .map_err(Into::into)
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
    use super::valid_domain;

    #[test]
    fn accepts_staging_runtime_domains() {
        assert!(valid_domain("portfolio.run.staging.sakala.dev"));
    }

    #[test]
    fn rejects_domains_outside_runtime_zones() {
        assert!(!valid_domain("portfolio.staging.sakala.dev"));
        assert!(!valid_domain("portfolio.run.example.test"));
    }
}
