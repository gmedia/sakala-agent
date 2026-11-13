use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use sakala_agent_core::ports::{RuntimeExecutionError, RuntimeReporter};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};
use sakala_agent_runtime::{
    CommandSpec, ProcessOutput, ProcessOutputSink, ProcessRunner, ProcessStream, RuntimeError,
    routing::{
        CaddyFileRouteManager, CaddyReloader, DockerExecCaddyReloader, KnownRoutes, RouteIdentity,
        RouteManager, RouteSpec, SleepingRoutePage, SleepingRouteSpec, wake_url,
    },
};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn caddy_file_route_restores_previous_content_when_reload_fails() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680")
        .expect("project UUID should be valid");
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    tokio::fs::write(&route_path, "previous route\n")
        .await
        .expect("previous route should be written");
    let reloader = Arc::new(FailingReloader::default());
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), reloader.clone());

    let error = manager
        .activate(&route(project_id), &NoopReporter)
        .await
        .expect_err("reload failure should fail route activation");

    assert!(error.to_string().contains("reload rejected"));
    assert_eq!(
        tokio::fs::read_to_string(route_path)
            .await
            .expect("route should remain readable"),
        "previous route\n"
    );
    assert_eq!(reloader.rollback_reloads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn caddy_file_route_restores_deleted_route_when_deactivation_reload_fails() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    let deployment_id = Uuid::new_v4();
    let managed_route =
        format!("# Managed by sakala-agent for project {project_id} deployment {deployment_id}.\n");
    tokio::fs::write(&route_path, &managed_route)
        .await
        .expect("route should be written");
    let reloader = Arc::new(FailingReloader::default());
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), reloader.clone());

    manager
        .deactivate(
            RouteIdentity {
                project_id,
                deployment_id: Some(deployment_id),
            },
            &NoopReporter,
        )
        .await
        .expect_err("reload failure should fail route deactivation");

    assert_eq!(
        tokio::fs::read_to_string(route_path)
            .await
            .expect("route should be restored"),
        managed_route
    );
    assert_eq!(reloader.rollback_reloads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn route_deactivation_refuses_file_not_owned_by_sakala() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    tokio::fs::write(&route_path, "# managed by another system\n")
        .await
        .expect("unmanaged route should be written");
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), Arc::new(SuccessfulReloader));

    let error = manager
        .deactivate(
            RouteIdentity {
                project_id,
                deployment_id: Some(Uuid::new_v4()),
            },
            &NoopReporter,
        )
        .await
        .expect_err("unmanaged route deletion must be rejected");

    assert!(error.to_string().contains("not owned by Sakala"));
    // The node's own paths stay in the Agent's log.
    assert!(
        !error
            .to_string()
            .contains(temp.path().to_string_lossy().as_ref()),
        "{error}"
    );
    assert!(route_path.exists());
}

#[tokio::test]
async fn stale_deployment_cannot_deactivate_the_current_project_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let current_deployment = Uuid::new_v4();
    let stale_deployment = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    let current_route = format!(
        "# Managed by sakala-agent for project {project_id} deployment {current_deployment}.\nexample.test:80 {{}}\n"
    );
    tokio::fs::write(&route_path, &current_route)
        .await
        .expect("current route should be written");
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), Arc::new(SuccessfulReloader));

    manager
        .deactivate(
            RouteIdentity {
                project_id,
                deployment_id: Some(stale_deployment),
            },
            &NoopReporter,
        )
        .await
        .expect("stale deactivation should be a safe no-op");

    assert_eq!(
        tokio::fs::read_to_string(route_path)
            .await
            .expect("current route must remain readable"),
        current_route
    );
}

#[tokio::test]
async fn stale_legacy_discovery_cannot_delete_a_modern_route_generation() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let current_deployment = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    let current_route = format!(
        "# Managed by sakala-agent for project {project_id} deployment {current_deployment}.\nexample.test:80 {{}}\n"
    );
    tokio::fs::write(&route_path, &current_route)
        .await
        .expect("modern route should be written");
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), Arc::new(SuccessfulReloader));

    let changed = manager
        .deactivate(
            RouteIdentity {
                project_id,
                deployment_id: None,
            },
            &NoopReporter,
        )
        .await
        .expect("legacy cleanup should be a safe no-op after route replacement");

    assert!(!changed);
    assert_eq!(
        tokio::fs::read_to_string(route_path)
            .await
            .expect("modern route must remain"),
        current_route
    );
}

#[tokio::test]
async fn stale_route_discovery_only_reports_sakala_owned_routes_without_workloads() {
    let temp = TempDir::new().expect("temp directory should be available");
    let active_project = Uuid::new_v4();
    let stale_project = Uuid::new_v4();
    let unmanaged_project = Uuid::new_v4();
    let manager =
        CaddyFileRouteManager::new(temp.path().to_owned(), Arc::new(FailingReloader::default()));

    tokio::fs::write(
        temp.path().join(format!("{active_project}.Caddyfile")),
        format!("# Managed by sakala-agent for project {active_project}.\n"),
    )
    .await
    .expect("active route should be written");
    let stale_path = temp.path().join(format!("{stale_project}.Caddyfile"));
    tokio::fs::write(
        &stale_path,
        format!("# Managed by sakala-agent for project {stale_project}.\n"),
    )
    .await
    .expect("stale route should be written");
    tokio::fs::write(
        temp.path().join(format!("{unmanaged_project}.Caddyfile")),
        "# owned by another system\n",
    )
    .await
    .expect("unmanaged route should be written");

    let stale = manager
        .discover_stale_routes(&KnownRoutes {
            serving: HashSet::from([RouteIdentity {
                project_id: active_project,
                deployment_id: Some(Uuid::new_v4()),
            }]),
            ..KnownRoutes::default()
        })
        .await
        .expect("route discovery should succeed");

    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].project_id, stale_project);
    assert_eq!(stale[0].path, stale_path.display().to_string());
    assert!(stale_path.exists(), "discovery must not delete route files");
}

fn waking_page() -> SleepingRoutePage {
    SleepingRoutePage::Waking {
        wake_url: wake_url("https://api.sakala.dev", "portfolio").expect("wake URL"),
    }
}

fn sleeping(project_id: Uuid, deployment_id: Uuid) -> SleepingRouteSpec {
    sleeping_with(project_id, deployment_id, waking_page())
}

fn sleeping_with(
    project_id: Uuid,
    deployment_id: Uuid,
    page: SleepingRoutePage,
) -> SleepingRouteSpec {
    SleepingRouteSpec {
        project_id,
        deployment_id,
        domain: "portfolio.run.sakala.localhost".to_owned(),
        page,
    }
}

#[tokio::test]
async fn a_refused_sleeping_route_puts_the_previous_route_back() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    let serving = format!(
        "# Managed by sakala-agent for project {project_id} deployment {}.\n",
        Uuid::new_v4()
    );
    tokio::fs::write(&route_path, &serving)
        .await
        .expect("route should be written");
    let reloader = Arc::new(FailingReloader::default());
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), reloader.clone());

    manager
        .activate_sleeping(&sleeping(project_id, Uuid::new_v4()), &NoopReporter)
        .await
        .expect_err("reload failure should fail the sleeping route");

    assert_eq!(
        tokio::fs::read_to_string(route_path)
            .await
            .expect("route should be restored"),
        serving
    );
    assert_eq!(reloader.rollback_reloads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn sleeping_routes_follow_the_deployment_ownership_rules() {
    for page in [waking_page(), SleepingRoutePage::Unavailable] {
        assert_sleeping_route_ownership(page).await;
    }
}

/// Both pages share the `(sleeping)` owner line, so they follow the same rules.
async fn assert_sleeping_route_ownership(page: SleepingRoutePage) {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let deployment_id = Uuid::new_v4();
    let route_path = temp.path().join(format!("{project_id}.Caddyfile"));
    let manager = CaddyFileRouteManager::new(temp.path().to_owned(), Arc::new(SuccessfulReloader));
    manager
        .activate_sleeping(
            &sleeping_with(project_id, deployment_id, page),
            &NoopReporter,
        )
        .await
        .expect("sleeping route");
    let content = tokio::fs::read_to_string(&route_path).await.expect("route");
    assert!(
        content.starts_with(&format!(
            "# Managed by sakala-agent for project {project_id} deployment {deployment_id} (sleeping).\n"
        )),
        "{content}"
    );
    let identity = RouteIdentity {
        project_id,
        deployment_id: Some(deployment_id),
    };

    // Kept while its container exists in any state, stale once it is gone.
    let kept = manager
        .discover_stale_routes(&KnownRoutes {
            sleeping: HashSet::from([identity]),
            ..KnownRoutes::default()
        })
        .await
        .expect("discovery");
    assert!(kept.is_empty());
    let stale = manager
        .discover_stale_routes(&KnownRoutes {
            // Running is not enough: the sleeping route needs its own container.
            serving: HashSet::from([RouteIdentity {
                project_id,
                deployment_id: Some(Uuid::new_v4()),
            }]),
            ..KnownRoutes::default()
        })
        .await
        .expect("discovery");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].deployment_id, Some(deployment_id));

    // Neither another deployment nor a legacy identity can remove it.
    for other in [Some(Uuid::new_v4()), None] {
        assert!(
            !manager
                .deactivate(
                    RouteIdentity {
                        project_id,
                        deployment_id: other,
                    },
                    &NoopReporter,
                )
                .await
                .expect("safe no-op")
        );
        assert!(route_path.exists());
    }
    assert!(
        manager
            .deactivate(identity, &NoopReporter)
            .await
            .expect("its own deployment removes it")
    );
    assert!(!route_path.exists());
}

fn route(project_id: Uuid) -> RouteSpec {
    RouteSpec {
        project_id,
        deployment_id: Uuid::new_v4(),
        domain: "portfolio.run.sakala.localhost".to_owned(),
        upstream: "sakala-app-project-deployment".to_owned(),
        port: 3000,
    }
}

/// What `caddy validate` prints on a node: the shared config path and a warning
/// that names another tenant's site file.
const CADDY_OUTPUT: [&str; 2] = [
    r#"{"level":"info","ts":1762822000.1,"msg":"using config from file","file":"/etc/caddy/Caddyfile"}"#,
    r#"{"level":"warn","ts":1762822000.2,"msg":"Caddyfile input is not formatted","adapter":"caddyfile","file":"/etc/caddy/sites/0e4f2c1a-other-tenant.Caddyfile","line":3}"#,
];

struct CaddyRunner {
    success: bool,
}

#[async_trait]
impl ProcessRunner for CaddyRunner {
    async fn run(
        &self,
        _spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        for line in CADDY_OUTPUT {
            sink.line(ProcessStream::Stderr, line).await?;
        }
        Ok(ProcessOutput {
            success: self.success,
            code: Some(i32::from(!self.success)),
            stderr: CADDY_OUTPUT.join("\n"),
            ..ProcessOutput::default()
        })
    }
}

#[derive(Default)]
struct RecordingReporter {
    logs: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl RuntimeReporter for RecordingReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        self.logs.lock().expect("log lock").push(log.message);
        Ok(())
    }
}

#[tokio::test]
async fn caddy_output_never_reaches_the_deployment_log() {
    for success in [true, false] {
        let reloader = DockerExecCaddyReloader::new(
            Arc::new(CaddyRunner { success }),
            "sakala-caddy".to_owned(),
        );
        let reporter = RecordingReporter::default();

        let result = reloader.validate_and_reload(&reporter).await;

        assert!(
            reporter.logs.lock().expect("log lock").is_empty(),
            "Caddy's own output describes the shared router"
        );
        assert_eq!(result.is_ok(), success);
        if let Err(error) = result {
            let message = error.to_string();
            assert_eq!(error.code(), "runtime_routing_failed");
            assert!(message.contains("exit code 1"), "{message}");
            for detail in ["/etc/caddy", "Caddyfile", "other-tenant", "caddy-"] {
                assert!(!message.contains(detail), "{message}");
            }
        }
    }
}

#[derive(Default)]
struct FailingReloader {
    rollback_reloads: AtomicUsize,
}

#[async_trait]
impl CaddyReloader for FailingReloader {
    async fn validate_and_reload(
        &self,
        _reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Execution("reload rejected".to_owned()))
    }

    async fn reload_after_rollback(&self) {
        self.rollback_reloads.fetch_add(1, Ordering::Relaxed);
    }
}

struct NoopReporter;

struct SuccessfulReloader;

#[async_trait]
impl CaddyReloader for SuccessfulReloader {
    async fn validate_and_reload(
        &self,
        _reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn reload_after_rollback(&self) {}
}

#[async_trait]
impl RuntimeReporter for NoopReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}
