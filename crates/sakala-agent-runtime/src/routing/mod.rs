use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

use sakala_agent_core::ports::RuntimeStaleRoute;
use sakala_agent_protocol::ProjectDomainsUpdate;
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};

mod caddy_file;
mod docker_exec;
mod sleeping_page;
pub(crate) mod traffic;

pub use caddy_file::CaddyFileRouteManager;
pub use docker_exec::{DockerExecCaddyMetrics, DockerExecCaddyReloader};
pub use sleeping_page::{
    SleepingRoutePage, SleepingRouteSpec, check_public_api_url, check_public_console_url, wake_url,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RouteIdentity {
    pub project_id: Uuid,
    pub deployment_id: Option<Uuid>,
}

/// The port every route listens on in the router. TLS ends at the host Caddy
/// or Sakala Edge in front of it.
pub(crate) const ROUTER_PORT: u16 = 80;

/// The site address line of a project route: its own domain first, then its
/// custom domains (ADR-024), all on the router's port. The first address is
/// the project's domain, which is how a route file read back tells it apart.
pub(crate) fn site_addresses(domain: &str, custom_domains: &[String]) -> String {
    std::iter::once(domain)
        .chain(custom_domains.iter().map(String::as_str))
        .map(|host| format!("{host}:{ROUTER_PORT}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A serving route: the project's domain, proxied to its deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSpec {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    /// Further hosts the route serves, checked, sorted, and each once. No other
    /// project's route may hold one of them.
    pub custom_domains: Vec<String>,
    /// The container name of every replica, in replica order. One name gives
    /// the plain proxy every earlier Agent wrote; several are balanced.
    pub upstreams: Vec<String>,
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

    /// The deployments whose route is the sleeping page: the project was put to
    /// sleep with that deployment active, and its stopped containers are kept
    /// for the wake. Read-only.
    async fn sleeping_routes(&self) -> Result<HashSet<RouteIdentity>, RuntimeError> {
        Ok(HashSet::new())
    }

    /// The custom domains the project's route serves now, serving or
    /// sleeping. `None` when the project has no route Sakala owns. Read-only.
    async fn custom_domains(&self, _project_id: Uuid) -> Result<Option<Vec<String>>, RuntimeError> {
        Ok(None)
    }

    /// The custom domains of every project route that has any. Read-only.
    async fn custom_domains_by_project(&self) -> Result<HashMap<Uuid, Vec<String>>, RuntimeError> {
        Ok(HashMap::new())
    }

    /// Makes the project's route, whichever kind it is, serve exactly
    /// `custom_domains` besides the project's domain. Validated and reloaded
    /// like [`RouteManager::activate`], and restored when the router refuses it.
    async fn update_custom_domains(
        &self,
        _project_id: Uuid,
        _custom_domains: &[String],
        _reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectDomainsUpdate, RuntimeError> {
        Err(RuntimeError::Routing(
            "this route manager cannot change custom domains".to_owned(),
        ))
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Replaces the project's route with a sleeping page: one that says the
    /// project is waking up and asks the control plane to wake it, or one that
    /// says it is not available. Validated and reloaded like
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
