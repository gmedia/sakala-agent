use async_trait::async_trait;
use std::collections::HashSet;

use sakala_agent_core::ports::RuntimeStaleRoute;
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};

mod caddy_file;
mod docker_exec;

pub use caddy_file::CaddyFileRouteManager;
pub use docker_exec::DockerExecCaddyReloader;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSpec {
    pub project_id: Uuid,
    pub domain: String,
    pub upstream: String,
    pub port: u16,
}

#[async_trait]
pub trait RouteManager: Send + Sync {
    /// Finds route files owned by Sakala that no longer have a known managed
    /// workload. Discovery is intentionally non-destructive.
    async fn discover_stale_routes(
        &self,
        _known_projects: &HashSet<Uuid>,
    ) -> Result<Vec<RuntimeStaleRoute>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    async fn deactivate(
        &self,
        project_id: Uuid,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;
}

#[async_trait]
pub trait CaddyReloader: Send + Sync {
    async fn validate_and_reload(&self, reporter: &dyn RuntimeReporter)
    -> Result<(), RuntimeError>;

    async fn reload_after_rollback(&self);
}
