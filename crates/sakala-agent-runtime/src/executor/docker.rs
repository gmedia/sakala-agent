use std::{
    collections::HashSet,
    num::NonZeroU32,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use sakala_agent_core::logs::redactor::ValueRedactor;
use sakala_agent_core::ports::{
    CleanupRuntimeRequest, CommandOutput, DeployProjectRequest, InspectProjectRequest,
    NodeTelemetry, ReconcileWorkloadRequest, RuntimeCapacity, RuntimeCompatibilityIssue,
    RuntimeExecutionError, RuntimeExecutor, RuntimeHealthSnapshot, RuntimeImageCleanup,
    RuntimeNetworkReconciliation, RuntimePreflightCheck, RuntimePreflightReport,
    RuntimeReconciliationReport, RuntimeReplicaRestoration, RuntimeReporter,
    RuntimeReporterFactory, WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, DeployProjectPayload, DeployProjectResult,
    DeploymentBuilder, DeploymentLog, InspectProjectPayload, LogBounds, LogStream,
    ReconcileWorkloadAction, RuntimeCleanupTarget, SleepingPage, WorkloadTraffic,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::Instant,
};
use url::Url;
use uuid::Uuid;

use super::{
    project_locks::ProjectLocks,
    replica_restore::{ReplicaRestorer, RestorePolicy, RestoreTarget},
};
use crate::{
    CommandSpec, DockerRuntimeConfig, ProcessRunner, RuntimeError, TokioProcessRunner,
    builders::{BuildRequest, ImageBuildService, ImageBuilder},
    containers::{
        ContainerEngine, DockerContainerEngine, FailureOutput, ImageCleanup, ImageRetention,
        ManagedWorkload, ReleaseContainerRequest, ReleaseExit, ReplicaSlot, RunContainerRequest,
        image_name, release_container_name, replicas::resolve_replicas,
    },
    health::{DockerHealthChecker, HealthChecker, ReadinessRequest},
    inspections::{ProjectInspector, RailpackProjectInspector},
    logs::emit_event,
    railpack::{RailpackSandbox, is_pinned_image},
    routing::{
        CaddyFileRouteManager, DockerExecCaddyMetrics, DockerExecCaddyReloader, KnownRoutes,
        RouteIdentity, RouteManager, RouteSpec, SleepingRoutePage, SleepingRouteSpec,
        traffic::TrafficTracker, wake_url,
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
    release_command_timeout: std::time::Duration,
    build_permits: Arc<Semaphore>,
    container_admission: Arc<Mutex<()>>,
    max_concurrent_builds: usize,
    max_replicas_per_project: u32,
    workspace_gc_max_age: std::time::Duration,
    image_retention: ImageRetention,
    min_workspace_free_bytes: u64,
    project_domains: crate::ProjectDomainSuffixes,
    preflight: DockerPreflight,
    telemetry: NodeTelemetryCollector,
    api_url: String,
    router_metrics: DockerExecCaddyMetrics,
    traffic: std::sync::Mutex<TrafficTracker>,
    /// Keeps a router without readable counters to one warning, not one per heartbeat.
    traffic_unreadable: AtomicBool,
    /// Held by every project command, and tried by replica restore.
    project_locks: ProjectLocks,
    restorer: std::sync::Mutex<ReplicaRestorer>,
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
    project_domains: crate::ProjectDomainSuffixes,
    max_active_containers: u32,
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
            project_domains: config.project_domain_suffixes.clone(),
            max_active_containers: config.max_active_containers,
        };
        let container_dns = crate::containers::ContainerDns::new(
            &config.workspace_root,
            config.container_dns.clone(),
        );
        let agent_id = config.agent_id;
        let max_concurrent_builds = config.max_concurrent_builds;
        let max_replicas_per_project = config.max_replicas_per_project;
        let workspace_gc_max_age = config.workspace_gc_max_age;
        let image_retention = ImageRetention {
            max_age: config.image_gc_max_age,
            orphaned_grace: config.orphaned_image_grace,
        };
        let min_workspace_free_bytes = config.min_workspace_free_bytes;
        let health_check_window = config.health_interval * config.health_attempts;
        let release_command_timeout = config.release_command_timeout;
        let restore_policy = RestorePolicy::new(config.crash_stable_window);
        let router_container = config.caddy_container.clone();
        let api_url = config.api_url.clone();
        let reloader = Arc::new(DockerExecCaddyReloader::new(
            Arc::clone(&runner),
            config.caddy_container,
        ));
        Self::with_services(
            Arc::new(GitWorkspaceManager::new(
                config.workspace_root,
                Arc::clone(&runner),
            )),
            Arc::new(RailpackProjectInspector::new(
                Arc::new(RailpackSandbox::new(
                    Arc::clone(&runner),
                    config.railpack_image.clone(),
                    config.railpack_network.clone(),
                )),
                Arc::clone(&runner),
            )),
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
                router_container.clone(),
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
            release_command_timeout,
            max_concurrent_builds,
            max_replicas_per_project,
            workspace_gc_max_age,
            image_retention,
            min_workspace_free_bytes,
            preflight,
            api_url,
            restore_policy,
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
        release_command_timeout: std::time::Duration,
        max_concurrent_builds: usize,
        max_replicas_per_project: u32,
        workspace_gc_max_age: std::time::Duration,
        image_retention: ImageRetention,
        min_workspace_free_bytes: u64,
        preflight: DockerPreflight,
        api_url: String,
        restore_policy: RestorePolicy,
    ) -> Self {
        let telemetry = NodeTelemetryCollector::new(
            Arc::clone(&preflight.runner),
            preflight.workspace_root.clone(),
            preflight.runtime_network.clone(),
            preflight.caddy_container.clone(),
            preflight.railpack_image.clone(),
            preflight.container_runtime.clone(),
        );
        let router_metrics = DockerExecCaddyMetrics::new(
            Arc::clone(&preflight.runner),
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
            health_check_window,
            release_command_timeout,
            build_permits: Arc::new(Semaphore::new(max_concurrent_builds)),
            container_admission: Arc::new(Mutex::new(())),
            max_concurrent_builds,
            max_replicas_per_project,
            workspace_gc_max_age,
            image_retention,
            min_workspace_free_bytes,
            project_domains: preflight.project_domains.clone(),
            preflight,
            telemetry,
            api_url,
            router_metrics,
            traffic: std::sync::Mutex::new(TrafficTracker::new(OffsetDateTime::now_utc())),
            traffic_unreadable: AtomicBool::new(false),
            project_locks: ProjectLocks::default(),
            restorer: std::sync::Mutex::new(ReplicaRestorer::new(restore_policy)),
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
                "exposed_ports": inspection.exposed_ports,
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
        validate_payload(&payload, &self.project_domains)?;
        let replicas = resolve_replicas(payload.replicas, self.max_replicas_per_project)?;
        let applied_timeouts = self.timeout_safety.resolve(payload.timeouts)?;
        let applied_resources = self.containers.resolve_resources(payload.resources)?;
        let source = deployment_source(&payload, request.repository_credential)?;
        self.ensure_workspace_capacity().await?;
        self.containers
            .ensure_capacity(project_id, replicas)
            .await?;

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
        let candidates = ReplicaSlot::all(replicas)
            .map(|slot| (slot, slot.container_name(project_id, deployment_id)))
            .collect::<Vec<_>>();
        let containers = candidates
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let release = ReleaseContainer::new(
            Arc::clone(&self.containers),
            release_container_name(project_id, deployment_id),
        );
        let route_activated = AtomicBool::new(false);
        let candidate_started = AtomicBool::new(false);
        let cutover_started = AtomicBool::new(false);
        let failed_replica = OnceLock::new();

        let deployment = self.deploy_inner(
            request.command_id,
            project_id,
            deployment_id,
            &payload,
            &workspace,
            &image,
            &candidates,
            applied_resources,
            applied_timeouts,
            DeploymentProgress {
                route_activated: &route_activated,
                candidate_started: &candidate_started,
                cutover_started: &cutover_started,
                failed_replica: &failed_replica,
                release: &release,
            },
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

        // A release container never outlives its deployment command. The release
        // step removes it itself; this covers a cancellation in the middle of it.
        if let Err(error) = release.remove().await {
            let _ = reporter
                .log(system_log(format!(
                    "release container cleanup warning for {}: {error}",
                    release.name
                )))
                .await;
        }

        // The app's output only exists in the container, so it must be read before
        // cleanup removes it. A cancelled command is not an app failure, and its
        // shutdown grace is better spent on cleanup.
        let failed_before_cutover = result.is_err() && !route_activated.load(Ordering::Acquire);
        if failed_before_cutover
            && candidate_started.load(Ordering::Acquire)
            && !matches!(result, Err(RuntimeError::Cancelled))
        {
            // The replica that failed explains the failure; before readiness,
            // that is the first one.
            let failed = failed_replica.get().unwrap_or(&containers[0]);
            let output = self
                .report_candidate_output(failed, payload.log_bounds, reporter.as_ref())
                .await;
            result = result.map_err(|error| error.refine_crash(output.exec_denied));
        }
        if failed_before_cutover {
            if let Err(error) = self.containers.cleanup_candidate(&containers, &image).await {
                let _ = reporter
                    .log(system_log(format!(
                        "candidate cleanup warning for {}: {error}",
                        containers.join(", ")
                    )))
                    .await;
            }
            // A first deployment leaves the network with nobody but the router.
            self.release_network(project_id).await;
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
        candidates: &[(ReplicaSlot, String)],
        applied_resources: AppliedRuntimeResources,
        applied_timeouts: crate::config::AppliedRuntimeTimeouts,
        progress: DeploymentProgress<'_>,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<(), RuntimeError> {
        let reporter_ref = reporter.as_ref();
        let containers = candidates
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let replicas = replica_count(candidates.len());
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

        // Once per deployment, before anything of the new version starts: until
        // the route moves, the previous deployment keeps serving.
        if let Some(command) = command_field(payload.release_command.as_deref()) {
            self.run_release_command(
                &ReleaseContainerRequest {
                    command_id,
                    project_id,
                    deployment_id,
                    name: progress.release.name.clone(),
                    image: image.to_owned(),
                    workspace: workspace.root().to_owned(),
                    environment: payload.environment.clone(),
                    resources: applied_resources,
                    port: payload.container_port,
                    command,
                },
                replicas,
                progress.release,
                reporter_ref,
            )
            .await?;
        }

        emit_event(
            reporter_ref,
            "deployment.container.started",
            if containers.len() > 1 {
                "Starting candidate application containers."
            } else {
                "Starting candidate application container."
            },
            with_replicas(
                json!({ "container": containers[0], "image": image }),
                &containers,
            ),
        )
        .await?;
        let start_began = Instant::now();
        tokio::time::timeout(applied_timeouts.start, async {
            // The early capacity check is only a fast-fail optimization. This
            // serialized re-check is authoritative and closes concurrent admission races.
            let admission = self.container_admission.lock().await;
            self.containers
                .ensure_capacity(project_id, replicas)
                .await?;
            for (replica, name) in candidates {
                self.containers
                    .start(
                        &RunContainerRequest {
                            command_id,
                            project_id,
                            deployment_id,
                            name: name.clone(),
                            replica: *replica,
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
            }
            drop(admission);
            Ok::<(), RuntimeError>(())
        })
        .await
        .map_err(|_| RuntimeError::Timeout {
            operation: "deployment-start".to_owned(),
            seconds: applied_timeouts.start.as_secs(),
        })??;
        progress.candidate_started.store(true, Ordering::Release);
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
        // Every replica must answer before the route moves, under one deadline:
        // the old deployment keeps serving until the whole new set is ready.
        for name in &containers {
            if let Err(error) = self
                .health
                .wait_until_ready(&ReadinessRequest {
                    container: name.clone(),
                    port: payload.container_port,
                    started_at: start_began,
                    timeout: applied_timeouts.start,
                    expected_restarts: Some(0),
                    operation: "deployment-start",
                })
                .await
            {
                let _ = progress.failed_replica.set(name.clone());
                return Err(error);
            }
        }
        emit_event(
            reporter_ref,
            "deployment.route.started",
            "Application answered; activating its route.",
            json!({ "domain": payload.domain, "port": payload.container_port }),
        )
        .await?;
        progress.cutover_started.store(true, Ordering::Release);
        self.routes
            .activate(
                &RouteSpec {
                    project_id,
                    deployment_id,
                    domain: payload.domain.clone(),
                    upstreams: containers.clone(),
                    port: payload.container_port,
                },
                reporter_ref,
            )
            .await?;
        progress.route_activated.store(true, Ordering::Release);
        reporter_ref.mark_deployment_committed(CommandOutput::with_result(json!({
            "requested_resources": payload.resources,
            "applied_resources": applied_resources,
        })));

        for (replica, name) in candidates {
            if containers.len() > 1 {
                let _ = reporter
                    .log(system_log(format!(
                        "Startup output of replica {}:",
                        replica.index
                    )))
                    .await;
            }
            if let Err(error) = self
                .containers
                .report_startup_logs(name, reporter_ref)
                .await
            {
                let _ = reporter
                    .log(system_log(format!(
                        "startup log collection warning for {name}: {error}"
                    )))
                    .await;
            }
        }
        let finalization_error = match self
            .containers
            .cleanup_previous(project_id, &containers, reporter_ref)
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
            with_replicas(
                json!({
                    "builder": build.builder,
                    "container": containers[0],
                    "domain": payload.domain,
                    "image": image,
                    "resources": applied_resources,
                }),
                &containers,
            ),
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
        for name in &containers {
            let _ = self
                .containers
                .start_log_follower(name, Arc::clone(&reporter));
        }
        if let Some(error) = finalization_error {
            return Err(error);
        }
        Ok(())
    }

    /// Runs the deployment's release command in its own container and waits for
    /// it, then removes the container whatever happened. Only a zero exit lets
    /// the deployment go on to start its replicas.
    async fn run_release_command(
        &self,
        request: &ReleaseContainerRequest,
        replicas: NonZeroU32,
        release: &ReleaseContainer,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        emit_event(
            reporter,
            "deployment.release.started",
            "Running the release command before the new version starts.",
            json!({
                "container": request.name,
                "timeout_seconds": self.release_command_timeout.as_secs(),
            }),
        )
        .await?;
        let started = Instant::now();
        let outcome = self
            .execute_release(request, replicas, release, reporter)
            .await;
        if let Err(error) = release.remove().await {
            let _ = reporter
                .log(system_log(format!(
                    "release container cleanup warning for {}: {error}",
                    request.name
                )))
                .await;
        }
        let exit = outcome?;
        if !exit.succeeded() {
            return Err(RuntimeError::ReleaseCommandFailed {
                exit_code: exit.exit_code,
                out_of_memory_limit_mb: exit.oom_killed.then_some(request.resources.memory_mb),
            });
        }
        emit_event(
            reporter,
            "deployment.release.completed",
            "Release command finished; starting the new version.",
            json!({ "duration_ms": started.elapsed().as_millis() }),
        )
        .await
    }

    /// The release container is admitted like the replicas it precedes, under
    /// the same serialized check and for the deployment's whole replica count:
    /// a node that could not start the new version then refuses before the
    /// command changes anything, such as a database schema. While it runs it
    /// is one more container of this project, the way the previous replicas
    /// are during a rolling deploy, and other projects count it as one slot.
    async fn execute_release(
        &self,
        request: &ReleaseContainerRequest,
        replicas: NonZeroU32,
        release: &ReleaseContainer,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ReleaseExit, RuntimeError> {
        let admission = self.container_admission.lock().await;
        self.containers
            .ensure_capacity(request.project_id, replicas)
            .await?;
        // Before `docker run`, which can leave a created container behind
        // even when it fails.
        release.arm();
        self.containers.start_release(request, reporter).await?;
        drop(admission);
        tokio::time::timeout(
            self.release_command_timeout,
            self.containers.wait_release(&request.name, reporter),
        )
        .await
        .map_err(|_| RuntimeError::ReleaseCommandTimeout {
            seconds: self.release_command_timeout.as_secs(),
        })?
    }

    /// Every replica of the request's deployment, in replica order.
    async fn workloads(
        &self,
        request: &WorkloadLifecycleRequest,
    ) -> Result<Vec<ManagedWorkload>, RuntimeError> {
        let workloads = self
            .containers
            .workloads(request.project_id, request.deployment_id)
            .await?;
        if workloads.is_empty() {
            return Err(RuntimeError::WorkloadNotFound);
        }
        Ok(workloads)
    }

    /// A lifecycle command has no start deadline of its own in its payload.
    fn workload_readiness(
        &self,
        workload: &ManagedWorkload,
        timeout: std::time::Duration,
    ) -> ReadinessRequest {
        ReadinessRequest {
            container: workload.name.clone(),
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

    /// Best effort: a network left behind holds no state, and startup
    /// reconciliation removes it. The operator log says why it stayed.
    async fn release_network(&self, project_id: Uuid) {
        if let Err(error) = self.containers.release_network(project_id).await {
            tracing::warn!(%project_id, %error, "project network was not removed");
        }
    }

    /// Waits until one replica answers, then writes the route to all of them.
    /// The router keeps requests away from a replica it cannot reach, so the
    /// others may still be starting.
    async fn activate_workload(
        &self,
        workloads: &[ManagedWorkload],
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let first = &workloads[0];
        // A recreated router container has lost its project networks.
        self.containers.attach_router(first.project_id).await?;
        self.wait_until_any_ready(workloads, self.timeout_safety.max_start_timeout)
            .await?;
        self.routes
            .activate(
                &RouteSpec {
                    project_id: first.project_id,
                    deployment_id: first.deployment_id,
                    domain: first.domain.clone(),
                    upstreams: workloads
                        .iter()
                        .map(|workload| workload.name.clone())
                        .collect(),
                    port: first.port,
                },
                reporter,
            )
            .await
    }

    /// Succeeds as soon as one replica is ready. When none is, the failure of
    /// the lowest replica is returned, which for one replica is its own.
    async fn wait_until_any_ready(
        &self,
        workloads: &[ManagedWorkload],
        timeout: std::time::Duration,
    ) -> Result<(), RuntimeError> {
        if let [workload] = workloads {
            return self
                .health
                .wait_until_ready(&self.workload_readiness(workload, timeout))
                .await;
        }
        let mut checks = self.spawn_readiness(workloads, timeout);
        let mut first_failure: Option<(usize, RuntimeError)> = None;
        while let Some(joined) = checks.join_next().await {
            let (index, result) = joined.map_err(readiness_task_failed)?;
            match result {
                // Dropping the set cancels the checks still waiting.
                Ok(()) => return Ok(()),
                Err(error) => {
                    if first_failure
                        .as_ref()
                        .is_none_or(|(failed, _)| index < *failed)
                    {
                        first_failure = Some((index, error));
                    }
                }
            }
        }
        Err(first_failure.map_or(RuntimeError::WorkloadNotRunning, |(_, error)| error))
    }

    /// The readiness of every replica, in replica order.
    async fn readiness_of_each(
        &self,
        workloads: &[ManagedWorkload],
        timeout: std::time::Duration,
    ) -> Result<Vec<Result<(), RuntimeError>>, RuntimeError> {
        let mut checks = self.spawn_readiness(workloads, timeout);
        let mut results = (0..workloads.len()).map(|_| None).collect::<Vec<_>>();
        while let Some(joined) = checks.join_next().await {
            let (index, result) = joined.map_err(readiness_task_failed)?;
            results[index] = Some(result);
        }
        Ok(results
            .into_iter()
            .map(|result| result.unwrap_or(Err(RuntimeError::WorkloadNotRunning)))
            .collect())
    }

    /// Probes the replicas side by side, so a stuck one does not use up the
    /// deadline of the others. A replica that is not running fails at once.
    fn spawn_readiness(
        &self,
        workloads: &[ManagedWorkload],
        timeout: std::time::Duration,
    ) -> JoinSet<(usize, Result<(), RuntimeError>)> {
        let mut checks = JoinSet::new();
        for (index, workload) in workloads.iter().enumerate() {
            let health = Arc::clone(&self.health);
            let running = workload.is_running();
            let request = self.workload_readiness(workload, timeout);
            checks.spawn(async move {
                let result = if running {
                    health.wait_until_ready(&request).await
                } else {
                    Err(RuntimeError::WorkloadNotRunning)
                };
                (index, result)
            });
        }
        checks
    }

    async fn run_restart(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workloads = self.workloads(&request).await?;
        // A sleeping deployment stays asleep; a running one gets every replica
        // back, including one that stopped on its own.
        if !workloads.iter().any(ManagedWorkload::is_running) {
            return Err(RuntimeError::WorkloadNotRunning);
        }
        for workload in &workloads {
            self.containers.restart(workload, 10).await?;
        }
        let restarted = workloads
            .iter()
            .map(|workload| ManagedWorkload {
                status: RESTARTED_STATUS.to_owned(),
                ..workload.clone()
            })
            .collect::<Vec<_>>();
        self.activate_workload(&restarted, reporter.as_ref())
            .await?;
        let workload = &workloads[0];
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
        let workloads = self
            .containers
            .workloads(request.project_id, request.deployment_id)
            .await?;
        let Some(workload) = workloads.first() else {
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
                self.release_network(request.project_id).await;
                let mut result = json!({ "status": "already_stopped" });
                if request.remove_images {
                    self.remove_project_images(request.project_id, &mut result, reporter.as_ref())
                        .await?;
                }
                return Ok(CommandOutput::with_result(result));
            }
            return Err(RuntimeError::WorkloadNotFound);
        };
        self.containers.stop(&workloads, 10).await?;
        if remove {
            self.deactivate_route(workload, reporter.as_ref()).await?;
            for replica in &workloads {
                self.containers.remove(replica).await?;
            }
            self.release_network(workload.project_id).await;
        } else {
            self.sleep_route(workload, request.sleeping_page, reporter.as_ref())
                .await?;
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
                "Workload stopped for sleep; container and image are retained, and its domain shows the sleeping page."
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

    async fn deactivate_route(
        &self,
        workload: &ManagedWorkload,
        reporter: &dyn RuntimeReporter,
    ) -> Result<bool, RuntimeError> {
        self.routes
            .deactivate(
                RouteIdentity {
                    project_id: workload.project_id,
                    deployment_id: Some(workload.deployment_id),
                },
                reporter,
            )
            .await
    }

    /// Puts the sleeping page the control plane asked for on the project's
    /// domain in place of its route. When that page cannot be built or the
    /// router refuses it, the route is removed as before idle sleep existed, so
    /// the project still sleeps; the operator log says why its domain shows the
    /// router's 404 instead.
    async fn sleep_route(
        &self,
        workload: &ManagedWorkload,
        page: SleepingPage,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let page = match page {
            SleepingPage::Waking => self
                .project_domains
                .project_label(&workload.domain)
                .ok_or_else(|| {
                    RuntimeError::InvalidCommand(
                        "the workload's domain is not under an allowed project domain suffix"
                            .to_owned(),
                    )
                })
                .and_then(|slug| wake_url(&self.api_url, slug))
                .map(|wake_url| SleepingRoutePage::Waking { wake_url }),
            SleepingPage::Unavailable => Ok(SleepingRoutePage::Unavailable),
        };
        let sleeping = page.map(|page| SleepingRouteSpec {
            project_id: workload.project_id,
            deployment_id: workload.deployment_id,
            domain: workload.domain.clone(),
            page,
        });
        let result = match sleeping {
            Ok(route) => self.routes.activate_sleeping(&route, reporter).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(
                project_id = %workload.project_id,
                deployment_id = %workload.deployment_id,
                %error,
                "sleeping page was not installed; removing the project route instead"
            );
            self.deactivate_route(workload, reporter).await?;
        }
        Ok(())
    }

    /// Stops the containers this wake started but could not bring online, so
    /// they do not hold slots while the sleeping page stays up. The containers,
    /// image, and network stay for the next wake. A failure here is only logged:
    /// the wake already failed for its own reason, which is what gets reported.
    async fn stop_failed_wake(&self, started: &[ManagedWorkload]) {
        let Some(first) = started.first() else {
            return;
        };
        let started = started
            .iter()
            .map(|workload| ManagedWorkload {
                // The listing said stopped, but this wake has just started it.
                status: RESTARTED_STATUS.to_owned(),
                ..workload.clone()
            })
            .collect::<Vec<_>>();
        if let Err(error) = self.containers.stop(&started, 10).await {
            tracing::warn!(
                project_id = %first.project_id,
                deployment_id = %first.deployment_id,
                containers = %started
                    .iter()
                    .map(|workload| workload.container_id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                %error,
                "failed wake could not stop its containers again; they may still hold capacity slots"
            );
        }
    }

    async fn run_wake(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workloads = self.workloads(&request).await?;
        let workload = &workloads[0];
        // Same admission as a deployment: a full node refuses the wake, and the
        // sleeping page stays until there is room for every replica.
        let admission = self.container_admission.lock().await;
        self.containers
            .ensure_capacity(workload.project_id, replica_count(workloads.len()))
            .await?;
        let mut started = Vec::new();
        for replica in &workloads {
            if replica.is_running() {
                continue;
            }
            if let Err(error) = self.containers.start_existing(replica).await {
                drop(admission);
                self.stop_failed_wake(&started).await;
                return Err(error);
            }
            started.push(replica.clone());
        }
        drop(admission);
        let woken = workloads
            .iter()
            .map(|replica| ManagedWorkload {
                status: RESTARTED_STATUS.to_owned(),
                ..replica.clone()
            })
            .collect::<Vec<_>>();
        // Writes the serving route over the sleeping one in one rename, and puts
        // the sleeping page back when the router refuses the new route.
        if let Err(error) = self.activate_workload(&woken, reporter.as_ref()).await {
            self.stop_failed_wake(&started).await;
            return Err(error);
        }
        emit_event(
            reporter.as_ref(),
            "workload.wake.completed",
            "Sleeping workload is ready and its route is restored.",
            json!({ "project_id": workload.project_id, "deployment_id": workload.deployment_id }),
        )
        .await?;
        Ok(CommandOutput::with_result(json!({ "status": "ready" })))
    }

    /// One replica reports as before. Several report the first ready replica,
    /// or the first one when none is, plus every replica under `replicas`: the
    /// deployment is ready while one of them is, because the router serves it.
    async fn run_health_check(
        &self,
        request: WorkloadLifecycleRequest,
    ) -> Result<CommandOutput, RuntimeError> {
        let workloads = self.workloads(&request).await?;
        let results = self
            .readiness_of_each(&workloads, self.health_check_window)
            .await?;
        let checks = workloads
            .iter()
            .zip(&results)
            .map(|(workload, result)| {
                let failure = result.as_ref().err();
                json!({
                    "container_id": workload.container_id,
                    "running": workload.is_running(),
                    "ready": failure.is_none(),
                    "docker_status": workload.status,
                    "reason": failure.map(ToString::to_string),
                    "error_code": failure.map(RuntimeError::code),
                })
            })
            .collect::<Vec<_>>();
        let reported = results.iter().position(Result::is_ok).unwrap_or(0);
        let mut result = checks[reported].clone();
        if checks.len() > 1 {
            result["desired_replicas"] = json!(checks.len());
            result["ready_replicas"] =
                json!(results.iter().filter(|result| result.is_ok()).count());
            result["replicas"] = json!(checks);
        }
        Ok(CommandOutput::with_result(result))
    }

    async fn run_refresh_route(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let workloads = self.workloads(&request).await?;
        if !workloads.iter().any(ManagedWorkload::is_running) {
            return Err(RuntimeError::WorkloadNotRunning);
        }
        self.activate_workload(&workloads, reporter.as_ref())
            .await?;
        Ok(CommandOutput::with_result(json!({ "status": "ready" })))
    }

    /// One pass of replica restore; see [`super::replica_restore`] for which
    /// replicas qualify. The route is left as it is: it already lists every
    /// replica, and the router sends requests to one again once it answers.
    async fn run_restore_replicas(&self) -> Result<RuntimeReplicaRestoration, RuntimeError> {
        let listing = self.containers.replica_listing().await?;
        let targets = self.restorer.lock().expect("replica restorer lock").plan(
            &listing,
            Instant::now(),
            |project_id| self.project_locks.is_locked(project_id),
        );
        let mut outcome = RuntimeReplicaRestoration::default();
        for target in targets {
            // A command that started since the listing goes first.
            let Some(_project) = self.project_locks.try_lock(target.project_id) else {
                continue;
            };
            match self.restore_replica(&target).await {
                Ok(true) => outcome.restored += 1,
                Ok(false) => {}
                Err(error) => {
                    outcome.failed += 1;
                    tracing::warn!(
                        container_id = %target.container_id,
                        project_id = %target.project_id,
                        deployment_id = %target.deployment_id,
                        replica = target.replica.index,
                        attempt = target.attempt,
                        %error,
                        "stopped replica could not be restored; the agent retries after a delay"
                    );
                }
            }
        }
        Ok(outcome)
    }

    /// Starts one stopped replica with its own container, image, and settings,
    /// under the project lock the caller holds. Returns `false` when a fresh
    /// look shows there is nothing to do any more.
    async fn restore_replica(&self, target: &RestoreTarget) -> Result<bool, RuntimeError> {
        // The listing may be stale: look again now that no command can interfere.
        // A lookup that fails counts as an attempt, so it cannot repeat every pass.
        let workloads = match self
            .containers
            .workloads(target.project_id, target.deployment_id)
            .await
        {
            Ok(workloads) => workloads,
            Err(error) => {
                self.record_restore_attempt(target);
                return Err(error);
            }
        };
        let Some(replica) = workloads
            .iter()
            .find(|workload| workload.container_id == target.container_id)
        else {
            return Ok(false);
        };
        if replica.is_running() || !workloads.iter().any(ManagedWorkload::is_running) {
            return Ok(false);
        }
        self.record_restore_attempt(target);
        tracing::info!(
            container_id = %target.container_id,
            container = %replica.name,
            project_id = %target.project_id,
            deployment_id = %target.deployment_id,
            replica = target.replica.index,
            replicas = target.replica.count,
            attempt = target.attempt,
            status = %replica.status,
            "restoring a stopped replica of a serving deployment"
        );
        // Same admission as a deploy or wake. The deployment was admitted with
        // all its replicas, so asking for all of them again only refuses when
        // other projects took the freed slot in the meantime.
        let _admission = self.container_admission.lock().await;
        self.containers
            .ensure_capacity(target.project_id, replica.replica.count)
            .await?;
        self.containers.start_existing(replica).await?;
        Ok(true)
    }

    fn record_restore_attempt(&self, target: &RestoreTarget) {
        self.restorer
            .lock()
            .expect("replica restorer lock")
            .record_attempt(&target.container_id, Instant::now());
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
        checks.push(
            crate::containers::network::preflight_check(
                self.runner.as_ref(),
                self.max_active_containers,
            )
            .await,
        );
        checks.push(self.railpack_sandbox_check().await);
        if let Some(runtime) = &self.container_runtime {
            checks.extend(self.container_runtime_checks(runtime).await);
        }
        checks.push(workspace);
        checks.push(directory_check("caddy-sites", &self.caddy_sites_dir, true).await);
        checks.push(RuntimePreflightCheck {
            name: "project-domains".to_owned(),
            fatal: false,
            ready: true,
            detail: format!(
                "deployments may use one label under {} (SAKALA_PROJECT_DOMAIN_SUFFIXES)",
                self.project_domains
            ),
        });

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

/// A serving route needs a running workload. A sleeping route needs only its
/// container, which stays `exited` while the project sleeps and is listed with
/// the orphans for that reason.
fn known_routes(report: &RuntimeReconciliationReport) -> KnownRoutes {
    let serving = report
        .workloads
        .iter()
        .filter(|workload| workload.status.to_ascii_lowercase().starts_with("up"))
        .map(|workload| RouteIdentity {
            project_id: workload.project_id,
            deployment_id: Some(workload.deployment_id),
        })
        .collect();
    let sleeping = report
        .workloads
        .iter()
        .map(|workload| (Some(workload.project_id), Some(workload.deployment_id)))
        .chain(
            report
                .orphans
                .iter()
                .map(|orphan| (orphan.project_id, orphan.deployment_id)),
        )
        .filter_map(|identity| match identity {
            (Some(project_id), Some(deployment_id)) => Some(RouteIdentity {
                project_id,
                deployment_id: Some(deployment_id),
            }),
            _ => None,
        })
        .collect();
    KnownRoutes { serving, sleeping }
}

#[async_trait]
impl RuntimeExecutor for DockerRuntimeExecutor {
    async fn preflight(&self) -> Result<RuntimePreflightReport, RuntimeExecutionError> {
        self.preflight.run().await.map_err(Into::into)
    }

    async fn reconcile(&self) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        let mut report = self.containers.detect_orphans().await?;
        let known_routes = known_routes(&report);
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
        // A release container belongs to a deployment command, and none of
        // those survived the restart. Removed first, so its network can go too.
        match self.containers.remove_stale_releases().await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(
                removed,
                "release containers of interrupted deployments removed"
            ),
            Err(error) => {
                tracing::warn!(%error, "release containers of interrupted deployments could not be removed")
            }
        }
        // Unlike containers, a network holds no state, so one without containers
        // is removed here instead of only reported.
        if let Err(error) = self.containers.reconcile_networks(true).await {
            tracing::warn!(%error, "project network reconciliation failed");
        }
        for discovered in report.workloads.clone() {
            let workload = match self
                .containers
                .workloads(discovered.project_id, discovered.deployment_id)
                .await
            {
                Ok(workloads) => match workloads
                    .into_iter()
                    .find(|workload| workload.container_id == discovered.container_id)
                {
                    Some(workload) => workload,
                    None => continue,
                },
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

    async fn reconcile_networks(
        &self,
        remove_unused: bool,
    ) -> Result<RuntimeNetworkReconciliation, RuntimeExecutionError> {
        self.containers
            .reconcile_networks(remove_unused)
            .await
            .map_err(Into::into)
    }

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeExecutionError> {
        let mut capacity = self.containers.capacity().await?;
        capacity.active_builds = Some(
            self.max_concurrent_builds
                .saturating_sub(self.build_permits.available_permits()),
        );
        capacity.maximum_concurrent_builds = Some(self.max_concurrent_builds);
        capacity.maximum_replicas_per_project = Some(self.max_replicas_per_project);
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

    async fn restore_replicas(&self) -> Result<RuntimeReplicaRestoration, RuntimeExecutionError> {
        self.run_restore_replicas().await.map_err(Into::into)
    }

    async fn workload_traffic(&self) -> Option<Vec<WorkloadTraffic>> {
        let counters = match self.router_metrics.read().await {
            Ok(counters) => {
                if self.traffic_unreadable.swap(false, Ordering::Relaxed) {
                    tracing::info!("router request counters are readable again");
                }
                counters
            }
            Err(error) => {
                if !self.traffic_unreadable.swap(true, Ordering::Relaxed) {
                    tracing::warn!(%error, "router request counters are unreadable; heartbeats omit traffic");
                }
                return None;
            }
        };
        // Without per-host counters the router has said nothing about activity.
        let totals = crate::routing::traffic::request_totals_by_host(&counters)?;
        let last_requests = self
            .traffic
            .lock()
            .ok()?
            .observe(totals, OffsetDateTime::now_utc());
        let workloads = match self.containers.routed_workloads().await {
            Ok(workloads) => workloads,
            Err(error) => {
                tracing::debug!(%error, "routed workloads could not be listed for traffic");
                return None;
            }
        };
        // Every replica of a deployment is listed; the host is the deployment's.
        let mut seen = HashSet::new();
        Some(
            workloads
                .into_iter()
                .filter(|workload| seen.insert((workload.project_id, workload.deployment_id)))
                .filter_map(|workload| {
                    Some(WorkloadTraffic {
                        last_request_at: *last_requests.get(&workload.domain)?,
                        project_id: workload.project_id,
                        deployment_id: workload.deployment_id,
                    })
                })
                .collect(),
        )
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
        let _project = self.project_locks.lock(request.project_id).await;
        let workloads = self
            .containers
            .workloads(request.project_id, request.deployment_id)
            .await?;
        let actual_state = if workloads.is_empty() {
            "missing"
        } else if workloads.iter().any(ManagedWorkload::is_running) {
            "running"
        } else {
            "stopped"
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
                    let workload = workloads.first().ok_or(RuntimeError::WorkloadNotFound)?;
                    let running = workloads
                        .iter()
                        .filter(|workload| workload.is_running())
                        .collect::<Vec<_>>();
                    if running.is_empty() {
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
                    let mut started = false;
                    for replica in running {
                        started |= self.containers.start_log_follower(
                            &replica.container_id,
                            Arc::clone(&follower_reporter),
                        );
                    }
                    actions_applied.push(json!({
                        "action": "restart_log_follower",
                        "started": started,
                        "command_id": command_id,
                    }));
                }
                ReconcileWorkloadAction::CleanupFailedCandidate => {
                    let workload = workloads.first().ok_or(RuntimeError::WorkloadNotFound)?;
                    let never_serving = workloads.iter().all(|replica| {
                        let status = replica.status.to_ascii_lowercase();
                        status.starts_with("created")
                            || status.starts_with("exited")
                            || status.starts_with("dead")
                    });
                    if !never_serving {
                        return Err(RuntimeError::InvalidCommand(
                            "cleanup_failed_candidate requires a Created, Exited, or Dead workload"
                                .to_owned(),
                        )
                        .into());
                    }
                    for replica in &workloads {
                        self.containers.remove(replica).await?;
                    }
                    self.release_network(workload.project_id).await;
                    actions_applied.push(json!({
                        "action": "cleanup_failed_candidate",
                        "container_id": workload.container_id,
                    }));
                }
                ReconcileWorkloadAction::RestoreRoute => {
                    if workloads.is_empty() {
                        return Err(RuntimeError::WorkloadNotFound.into());
                    }
                    if !workloads.iter().any(ManagedWorkload::is_running) {
                        return Err(RuntimeError::WorkloadNotRunning.into());
                    }
                    self.activate_workload(&workloads, reporter.as_ref())
                        .await?;
                    actions_applied.push(json!({ "action": "restore_route" }));
                }
            }
        }
        Ok(CommandOutput::with_result(json!({
            "desired_state": desired_state,
            "actual_state": actual_state,
            "in_sync": desired_state == actual_state,
            "drift_reason": (desired_state != actual_state).then_some("workload_state_mismatch"),
            "container_id": workloads.first().map(|workload| workload.container_id.clone()),
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
                    let known_routes = known_routes(&discovered);
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
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_deployment(request, reporter)
            .await
            .map_err(Into::into)
    }

    async fn restart_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_restart(request, reporter)
            .await
            .map_err(Into::into)
    }

    async fn stop_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_stop(request, reporter, true)
            .await
            .map_err(Into::into)
    }

    async fn sleep_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_stop(request, reporter, false)
            .await
            .map_err(Into::into)
    }

    async fn wake_project(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_wake(request, reporter).await.map_err(Into::into)
    }

    async fn health_check(
        &self,
        request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_health_check(request).await.map_err(Into::into)
    }

    async fn refresh_route(
        &self,
        request: WorkloadLifecycleRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _project = self.project_locks.lock(request.project_id).await;
        self.run_refresh_route(request, reporter)
            .await
            .map_err(Into::into)
    }
}

/// The status a container listed as stopped has once a command started it
/// again; the listing is not read a second time.
const RESTARTED_STATUS: &str = "Up";

/// What a deployment has done so far, for the cleanup after it ends.
#[derive(Clone, Copy)]
struct DeploymentProgress<'a> {
    route_activated: &'a AtomicBool,
    candidate_started: &'a AtomicBool,
    cutover_started: &'a AtomicBool,
    /// The first replica that failed readiness, whose output explains why.
    failed_replica: &'a OnceLock<String>,
    release: &'a ReleaseContainer,
}

/// The release container of one deployment, removed on every path: by the
/// release step once the command ended or timed out, by the deployment's
/// cleanup after a cancellation, and in the background when the command is
/// dropped before either could run, such as after the shutdown grace.
struct ReleaseContainer {
    containers: Arc<dyn ContainerEngine>,
    name: String,
    /// Set from just before `docker run` until a removal succeeded.
    started: AtomicBool,
}

impl ReleaseContainer {
    fn new(containers: Arc<dyn ContainerEngine>, name: String) -> Self {
        Self {
            containers,
            name,
            started: AtomicBool::new(false),
        }
    }

    fn arm(&self) {
        self.started.store(true, Ordering::Release);
    }

    /// Does nothing when no release container was started, or it is gone.
    async fn remove(&self) -> Result<(), RuntimeError> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        self.containers.remove_release(&self.name).await?;
        self.started.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for ReleaseContainer {
    fn drop(&mut self) {
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(container = %self.name, "release container left behind; the next start removes it");
            return;
        };
        let containers = Arc::clone(&self.containers);
        let name = std::mem::take(&mut self.name);
        runtime.spawn(async move {
            if let Err(error) = containers.remove_release(&name).await {
                tracing::warn!(container = %name, %error, "release container could not be removed; the next start removes it");
            }
        });
    }
}

/// Adds every replica's container to an event of a deployment with several.
/// One replica keeps the event every earlier Agent sent.
fn with_replicas(mut details: serde_json::Value, containers: &[String]) -> serde_json::Value {
    if containers.len() > 1 {
        details["replicas"] = json!(containers.len());
        details["containers"] = json!(containers);
    }
    details
}

/// The replica count of a non-empty set of `len` containers.
fn replica_count(len: usize) -> NonZeroU32 {
    u32::try_from(len)
        .ok()
        .and_then(NonZeroU32::new)
        .unwrap_or(NonZeroU32::MIN)
}

fn readiness_task_failed(error: tokio::task::JoinError) -> RuntimeError {
    RuntimeError::Execution(format!("readiness check did not finish: {error}"))
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

/// Stricter than the other command fields, like the control plane: no ASCII
/// control character at all, so the line shown in the deployment log is the
/// line that runs.
fn validate_release_command(command: Option<&str>) -> Result<(), RuntimeError> {
    let Some(command) = command else {
        return Ok(());
    };
    if command.chars().count() > MAX_COMMAND_CHARS || command.chars().any(|c| c.is_ascii_control())
    {
        return Err(RuntimeError::InvalidCommand(format!(
            "release_command must be one line of at most {MAX_COMMAND_CHARS} characters without control characters"
        )));
    }
    Ok(())
}

fn validate_payload(
    payload: &DeployProjectPayload,
    project_domains: &crate::ProjectDomainSuffixes,
) -> Result<(), RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    if let Some(root_directory) = requested_root_directory(payload.root_directory.as_deref()) {
        validate_root_directory(root_directory)?;
    }
    validate_command_field("build_command", payload.build_command.as_deref())?;
    validate_command_field("start_command", payload.start_command.as_deref())?;
    validate_release_command(payload.release_command.as_deref())?;
    if payload.container_port == 0 {
        return Err(RuntimeError::InvalidCommand(
            "container_port must be greater than zero".to_owned(),
        ));
    }
    if !project_domains.accepts(&payload.domain) {
        return Err(RuntimeError::InvalidCommand(format!(
            "domain must be one lowercase DNS label under an allowed suffix ({project_domains}), such as portfolio.{}",
            project_domains.as_slice()[0]
        )));
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

    use super::validate_payload;
    use crate::ProjectDomainSuffixes;

    fn payload() -> DeployProjectPayload {
        serde_json::from_value(serde_json::json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.staging.sakala.app",
            "container_port": 3000,
        }))
        .expect("deploy payload")
    }

    fn validate(payload: &DeployProjectPayload) -> Result<(), crate::RuntimeError> {
        validate_payload(payload, &ProjectDomainSuffixes::default())
    }

    fn with_domain(domain: &str) -> DeployProjectPayload {
        let mut payload = payload();
        payload.domain = domain.to_owned();
        payload
    }

    #[test]
    fn accepts_staging_runtime_domains() {
        assert!(validate(&payload()).is_ok());
    }

    #[test]
    fn rejects_domains_outside_runtime_zones() {
        let allowed = format!("allowed suffix ({})", ProjectDomainSuffixes::default());
        for domain in [
            "portfolio.run.sakala.dev",
            "portfolio.run.staging.sakala.dev",
            "portfolio.run.example.test",
            "api.portfolio.sakala.app",
        ] {
            let error = validate(&with_domain(domain)).expect_err(domain);
            assert_eq!(error.code(), "invalid_runtime_command");
            assert!(error.to_string().contains(&allowed), "{error}");
        }
    }

    #[test]
    fn a_configured_suffix_admits_self_hosted_domains_only() {
        let suffixes = ProjectDomainSuffixes::parse("run.example.ac.id").expect("suffix");

        assert!(validate_payload(&with_domain("portfolio.run.example.ac.id"), &suffixes).is_ok());
        let error = validate_payload(&payload(), &suffixes).expect_err("hosted zone");
        assert!(
            error
                .to_string()
                .contains("(run.example.ac.id), such as portfolio.run.example.ac.id"),
            "{error}"
        );
    }

    fn with_release_command(command: &str) -> DeployProjectPayload {
        let mut payload = payload();
        payload.release_command = Some(command.to_owned());
        payload
    }

    #[test]
    fn accepts_a_release_command_of_one_line_up_to_the_limit() {
        for command in [
            "php artisan migrate --force",
            "npx prisma migrate deploy && node scripts/seed.js",
            &"a".repeat(1_000),
            &"é".repeat(1_000),
            "",
        ] {
            assert!(
                validate(&with_release_command(command)).is_ok(),
                "{command}"
            );
        }
    }

    #[test]
    fn rejects_a_release_command_that_is_too_long_or_holds_control_characters() {
        for command in [
            "a".repeat(1_001),
            "php artisan migrate\nrm -rf /".to_owned(),
            "php artisan migrate\r".to_owned(),
            "php\tartisan migrate".to_owned(),
            "migrate\0".to_owned(),
            "migrate\u{1b}[2J".to_owned(),
            "migrate\u{7f}".to_owned(),
        ] {
            let error = validate(&with_release_command(&command)).expect_err(&command);
            assert_eq!(error.code(), "invalid_runtime_command");
            assert!(
                error
                    .to_string()
                    .contains("release_command must be one line"),
                "{error}"
            );
        }
    }

    #[test]
    fn accepts_an_environment_port_matching_the_routed_port() {
        let mut payload = payload();
        payload.environment = BTreeMap::from([("PORT".to_owned(), "3000".to_owned())]);

        assert!(validate(&payload).is_ok());
    }

    #[test]
    fn rejects_an_environment_port_that_contradicts_the_route() {
        let mut payload = payload();
        payload.environment = BTreeMap::from([("PORT".to_owned(), "8080".to_owned())]);

        let error = validate(&payload).expect_err("conflicting PORT must be rejected");

        assert_eq!(error.code(), "runtime_port_conflict");
        assert!(error.to_string().contains("PORT=8080"));
        assert!(error.to_string().contains("must match container_port 3000"));
    }

    #[test]
    fn a_port_value_that_is_not_a_port_is_never_repeated_in_the_failure() {
        let mut payload = payload();
        payload.environment =
            BTreeMap::from([("PORT".to_owned(), "hunter2-not-a-port".to_owned())]);

        let error = validate(&payload).expect_err("invalid PORT must be rejected");

        assert_eq!(error.code(), "runtime_port_conflict");
        assert!(error.to_string().contains("container_port 3000"));
        assert!(!error.to_string().contains("hunter2"));
    }
}
