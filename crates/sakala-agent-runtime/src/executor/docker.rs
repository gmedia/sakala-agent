use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    CommandOutput, DeployProjectRequest, InspectProjectRequest, RuntimeExecutionError,
    RuntimeExecutor, RuntimeReconciliationReport, RuntimeReporter,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, DeployProjectPayload, DeployProjectResult, DeploymentEvent,
    DeploymentEventLevel, DeploymentLog, InspectProjectPayload, LogStream,
};
use serde_json::json;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{
    DockerRuntimeConfig, ProcessRunner, RuntimeError, TokioProcessRunner,
    builders::{BuildRequest, ImageBuildService, ImageBuilder},
    containers::{
        ContainerEngine, DockerContainerEngine, RunContainerRequest, container_name, image_name,
    },
    health::{DockerHealthChecker, HealthChecker},
    inspections::{ProjectInspector, RailpackProjectInspector},
    routing::{CaddyFileRouteManager, DockerExecCaddyReloader, RouteManager, RouteSpec},
    workspace::{DeploymentWorkspace, GitWorkspaceManager, RepositorySource, WorkspaceManager},
};

pub struct DockerRuntimeExecutor {
    workspace: Arc<dyn WorkspaceManager>,
    inspector: Arc<dyn ProjectInspector>,
    builder: Arc<dyn ImageBuilder>,
    containers: Arc<dyn ContainerEngine>,
    health: Arc<dyn HealthChecker>,
    routes: Arc<dyn RouteManager>,
    build_timeout: std::time::Duration,
}

impl DockerRuntimeExecutor {
    #[must_use]
    pub fn new(config: DockerRuntimeConfig) -> Self {
        let runner = Arc::new(TokioProcessRunner::new(config.command_timeout));
        Self::with_runner(config, runner)
    }

    #[must_use]
    pub fn with_runner(config: DockerRuntimeConfig, runner: Arc<dyn ProcessRunner>) -> Self {
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
            )),
            Arc::new(DockerHealthChecker::new(
                Arc::clone(&runner),
                config.health_attempts,
                config.health_interval,
            )),
            Arc::new(CaddyFileRouteManager::new(config.caddy_sites_dir, reloader)),
            config.build_timeout,
        )
    }

    #[must_use]
    pub fn with_services(
        workspace: Arc<dyn WorkspaceManager>,
        inspector: Arc<dyn ProjectInspector>,
        builder: Arc<dyn ImageBuilder>,
        containers: Arc<dyn ContainerEngine>,
        health: Arc<dyn HealthChecker>,
        routes: Arc<dyn RouteManager>,
        build_timeout: std::time::Duration,
    ) -> Self {
        Self {
            workspace,
            inspector,
            builder,
            containers,
            health,
            routes,
            build_timeout,
        }
    }

    async fn run_inspection(
        &self,
        request: InspectProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeError> {
        let reporter = reporter.as_ref();
        let source = inspection_source(&request.payload)?;

        emit_event(
            reporter,
            "project.inspection.started",
            "Checking out repository for project preview.",
            json!({ "commit_sha": source.commit_sha }),
        )
        .await?;
        let workspace = self
            .workspace
            .checkout(request.command_id, &source, reporter)
            .await?;
        let result = self.inspector.inspect(&workspace, &source, reporter).await;
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
        let applied_resources = self.containers.resolve_resources(payload.resources)?;
        let source = deployment_source(&payload)?;
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
            "deployment.checkout.started",
            "Cloning repository at the requested commit.",
            json!({ "commit_sha": payload.commit_sha }),
        )
        .await?;
        let workspace = self
            .workspace
            .checkout(request.command_id, &source, reporter.as_ref())
            .await?;
        let image = image_name(project_id, deployment_id, &payload.commit_sha);
        let container = container_name(project_id, deployment_id);
        let route_activated = AtomicBool::new(false);

        let result = self
            .deploy_inner(
                project_id,
                deployment_id,
                &payload,
                &workspace,
                &image,
                &container,
                applied_resources,
                &route_activated,
                Arc::clone(&reporter),
            )
            .await;

        if result.is_err() && !route_activated.load(Ordering::Acquire) {
            self.containers.cleanup_candidate(&container, &image).await;
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
        })
        .map_err(|error| {
            RuntimeError::Execution(format!("failed to serialize deployment result: {error}"))
        })?;
        Ok(CommandOutput::with_result(completion))
    }

    #[allow(clippy::too_many_arguments)]
    async fn deploy_inner(
        &self,
        project_id: Uuid,
        deployment_id: Uuid,
        payload: &DeployProjectPayload,
        workspace: &DeploymentWorkspace,
        image: &str,
        container: &str,
        applied_resources: AppliedRuntimeResources,
        route_activated: &AtomicBool,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<(), RuntimeError> {
        let reporter_ref = reporter.as_ref();
        emit_event(
            reporter_ref,
            "deployment.build.started",
            "Preparing application image.",
            json!({ "requested_builder": payload.builder }),
        )
        .await?;
        let build = tokio::time::timeout(
            self.build_timeout,
            self.builder.build(
                &BuildRequest {
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
            seconds: self.build_timeout.as_secs(),
        })??;

        emit_event(
            reporter_ref,
            "deployment.container.started",
            "Starting candidate application container.",
            json!({ "container": container, "image": image }),
        )
        .await?;
        self.containers
            .start(
                &RunContainerRequest {
                    project_id,
                    deployment_id,
                    name: container.to_owned(),
                    image: image.to_owned(),
                    workspace: workspace.root().to_owned(),
                    environment: payload.environment.clone(),
                    resources: applied_resources,
                },
                reporter_ref,
            )
            .await?;
        self.health.wait_until_ready(container).await?;
        self.routes
            .activate(
                &RouteSpec {
                    project_id,
                    domain: payload.domain.clone(),
                    upstream: container.to_owned(),
                    port: payload.container_port,
                },
                reporter_ref,
            )
            .await?;
        route_activated.store(true, Ordering::Release);

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
        if let Err(error) = self
            .containers
            .cleanup_previous(project_id, container, reporter_ref)
            .await
        {
            let _ = reporter
                .log(system_log(format!(
                    "previous container cleanup warning for project {project_id}: {error}"
                )))
                .await;
        }

        emit_event(
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
        .await?;
        self.containers.start_log_follower(container, reporter);
        Ok(())
    }
}

#[async_trait]
impl RuntimeExecutor for DockerRuntimeExecutor {
    async fn reconcile(&self) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        self.containers.detect_orphans().await.map_err(Into::into)
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
            "domain must be a valid *.run.sakala.localhost or *.run.sakala.dev hostname".to_owned(),
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

fn inspection_source(payload: &InspectProjectPayload) -> Result<RepositorySource, RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    Ok(RepositorySource {
        repository_url: payload.repository_url.clone(),
        commit_sha: payload.commit_sha.clone(),
    })
}

fn deployment_source(payload: &DeployProjectPayload) -> Result<RepositorySource, RuntimeError> {
    validate_repository_source(&payload.repository_url, &payload.commit_sha)?;
    Ok(RepositorySource {
        repository_url: payload.repository_url.clone(),
        commit_sha: payload.commit_sha.clone(),
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
            "MVP repository URL must be a credential-free public https://github.com URL".to_owned(),
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
    let allowed_suffix =
        domain.ends_with(".run.sakala.localhost") || domain.ends_with(".run.sakala.dev");
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
