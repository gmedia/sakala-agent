use async_trait::async_trait;
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
    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;
}

#[async_trait]
pub trait CaddyReloader: Send + Sync {
    async fn validate_and_reload(&self, reporter: &dyn RuntimeReporter)
    -> Result<(), RuntimeError>;

    async fn reload_after_rollback(&self);
}
