use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{RuntimeExecutionError, RuntimeReporter};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};
use sakala_agent_runtime::{
    RuntimeError,
    routing::{CaddyFileRouteManager, CaddyReloader, RouteManager, RouteSpec},
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

fn route(project_id: Uuid) -> RouteSpec {
    RouteSpec {
        project_id,
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

#[async_trait]
impl RuntimeReporter for NoopReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}
