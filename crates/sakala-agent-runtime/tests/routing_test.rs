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
    RuntimeError,
    routing::{CaddyFileRouteManager, CaddyReloader, RouteIdentity, RouteManager, RouteSpec},
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
        .discover_stale_routes(&HashSet::from([RouteIdentity {
            project_id: active_project,
            deployment_id: Some(Uuid::new_v4()),
        }]))
        .await
        .expect("route discovery should succeed");

    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].project_id, stale_project);
    assert_eq!(stale[0].path, stale_path.display().to_string());
    assert!(stale_path.exists(), "discovery must not delete route files");
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
