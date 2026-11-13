use async_trait::async_trait;
use std::collections::HashSet;

use sakala_agent_core::ports::RuntimeStaleRoute;
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};

mod caddy_file;
mod docker_exec;
mod sleeping_page;
pub(crate) mod traffic;

pub use caddy_file::CaddyFileRouteManager;
pub use docker_exec::{DockerExecCaddyMetrics, DockerExecCaddyReloader};
pub use sleeping_page::{SleepingRouteSpec, wake_url};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RouteIdentity {
    pub project_id: Uuid,
    pub deployment_id: Option<Uuid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSpec {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub upstream: String,
    pub port: u16,
}

/// The workloads a route may still belong to, by the kind of route.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KnownRoutes {
    /// Running workloads. A route that proxies to anything else is stale.
    pub serving: HashSet<RouteIdentity>,
    /// Workloads whose container still exists, in any state. A sleeping route
    /// stays while its container does, so the project can be woken.
    pub sleeping: HashSet<RouteIdentity>,
}

#[async_trait]
pub trait RouteManager: Send + Sync {
    /// Finds route files owned by Sakala that no longer have a known managed
    /// workload. Discovery is intentionally non-destructive.
    async fn discover_stale_routes(
        &self,
        _known_routes: &KnownRoutes,
    ) -> Result<Vec<RuntimeStaleRoute>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Replaces the project's route with a page that says the project is waking
    /// up and asks the control plane to wake it. Validated and reloaded like
    /// [`RouteManager::activate`], and restored when the router refuses it.
    async fn activate_sleeping(
        &self,
        route: &SleepingRouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Removes the project's route, serving or sleeping, when it belongs to
    /// `identity`.
    async fn deactivate(
        &self,
        identity: RouteIdentity,
        reporter: &dyn RuntimeReporter,
    ) -> Result<bool, RuntimeError>;

    /// Deletes only stale route files that pass the same Sakala ownership
    /// validation used by lifecycle deactivation.
    async fn cleanup_stale_routes(
        &self,
        known_routes: &KnownRoutes,
        reporter: &dyn RuntimeReporter,
    ) -> Result<usize, RuntimeError> {
        let _ = (known_routes, reporter);
        Ok(0)
    }
}

#[async_trait]
pub trait CaddyReloader: Send + Sync {
    async fn validate_and_reload(&self, reporter: &dyn RuntimeReporter)
    -> Result<(), RuntimeError>;

    async fn reload_after_rollback(&self);
}
