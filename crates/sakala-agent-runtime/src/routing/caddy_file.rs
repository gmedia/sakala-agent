use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use tokio::fs;
use uuid::Uuid;

use crate::{
    RuntimeError, RuntimeReporter,
    routing::{
        CaddyReloader, KnownRoutes, RouteIdentity, RouteManager, RouteSpec, SleepingRouteSpec,
        sleeping_page,
    },
};
use sakala_agent_core::ports::RuntimeStaleRoute;

const MANAGED_ROUTE_PREFIX: &str = "# Managed by sakala-agent for project ";
const SLEEPING_MARKER: &str = " (sleeping)";

/// What the first line of a route file says about its owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteOwner {
    /// Written before routes named their deployment.
    Legacy,
    /// Proxies to the deployment's container.
    Serving(Uuid),
    /// Answers with the sleeping page while the deployment's container is stopped.
    Sleeping(Uuid),
}

impl RouteOwner {
    fn deployment_id(self) -> Option<Uuid> {
        match self {
            Self::Legacy => None,
            Self::Serving(deployment_id) | Self::Sleeping(deployment_id) => Some(deployment_id),
        }
    }
}

/// `None` when Sakala does not own the route of `project_id`, including when the
/// line names a deployment in a form this Agent does not write.
fn route_owner(content: &[u8], project_id: Uuid) -> Option<RouteOwner> {
    let first_line = content.split(|byte| *byte == b'\n').next()?;
    let first_line = std::str::from_utf8(first_line).ok()?;
    let rest = first_line
        .strip_prefix(MANAGED_ROUTE_PREFIX)?
        .strip_prefix(&project_id.to_string())?;
    if rest == "." {
        return Some(RouteOwner::Legacy);
    }
    let deployment = rest.strip_prefix(" deployment ")?.strip_suffix('.')?;
    match deployment.strip_suffix(SLEEPING_MARKER) {
        Some(deployment) => Uuid::parse_str(deployment).ok().map(RouteOwner::Sleeping),
        None => Uuid::parse_str(deployment).ok().map(RouteOwner::Serving),
    }
}

fn owner_line(project_id: Uuid, deployment_id: Uuid, sleeping: bool) -> String {
    let marker = if sleeping { SLEEPING_MARKER } else { "" };
    format!("{MANAGED_ROUTE_PREFIX}{project_id} deployment {deployment_id}{marker}.\n")
}

pub struct CaddyFileRouteManager {
    sites_dir: PathBuf,
    reloader: Arc<dyn CaddyReloader>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl CaddyFileRouteManager {
    #[must_use]
    pub fn new(sites_dir: PathBuf, reloader: Arc<dyn CaddyReloader>) -> Self {
        Self {
            sites_dir,
            reloader,
            mutation_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn replace_route(
        &self,
        project_id: Uuid,
        content: String,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let snapshot = write_route(&self.sites_dir, project_id, content).await?;
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(())
    }
}

struct RouteSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    changed: bool,
}

fn serving_route(route: &RouteSpec) -> String {
    format!(
        "{}{}:80 {{\n\treverse_proxy {}:{}\n}}\n",
        owner_line(route.project_id, route.deployment_id, false),
        route.domain,
        route.upstream,
        route.port
    )
}

fn sleeping_route(route: &SleepingRouteSpec) -> Result<String, RuntimeError> {
    Ok(format!(
        "{}{}",
        owner_line(route.project_id, route.deployment_id, true),
        sleeping_page::site_block(route)?
    ))
}

/// Replaces the project's route file through a rename, so Caddy never reads
/// half of one, and keeps the previous content for a rollback.
async fn write_route(
    sites_dir: &Path,
    project_id: Uuid,
    content: String,
) -> Result<RouteSnapshot, RuntimeError> {
    fs::create_dir_all(sites_dir).await?;
    let path = sites_dir.join(format!("{project_id}.Caddyfile"));
    let temporary = sites_dir.join(format!(".{project_id}.Caddyfile.tmp"));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    fs::write(&temporary, content).await?;
    fs::rename(&temporary, &path).await?;

    Ok(RouteSnapshot {
        path,
        previous,
        changed: true,
    })
}

impl RouteSnapshot {
    async fn restore(self) -> Result<(), RuntimeError> {
        match self.previous {
            Some(content) => fs::write(self.path, content).await?,
            None => match fs::remove_file(self.path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
        }

        Ok(())
    }
}

async fn remove_route(
    sites_dir: &Path,
    identity: RouteIdentity,
) -> Result<RouteSnapshot, RuntimeError> {
    let project_id = identity.project_id;
    let path = sites_dir.join(format!("{project_id}.Caddyfile"));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(content) = &previous {
        let Some(owner) = route_owner(content, project_id) else {
            tracing::warn!(route = %path.display(), "refusing to delete a route Sakala does not own");
            return Err(RuntimeError::Routing(format!(
                "refusing to delete the route of project {project_id} because it is not owned by Sakala"
            )));
        };
        // A serving or sleeping route of another deployment is newer than the
        // command, so it stays. `None` means "still legacy", not a wildcard: a
        // stale legacy discovery may race with a deployment-aware activation.
        if owner.deployment_id() != identity.deployment_id {
            return Ok(RouteSnapshot {
                path,
                previous,
                changed: false,
            });
        }
        fs::remove_file(&path).await?;
    }
    let changed = previous.is_some();
    Ok(RouteSnapshot {
        path,
        previous,
        changed,
    })
}

#[async_trait]
impl RouteManager for CaddyFileRouteManager {
    async fn discover_stale_routes(
        &self,
        known_routes: &KnownRoutes,
    ) -> Result<Vec<RuntimeStaleRoute>, RuntimeError> {
        let mut entries = match fs::read_dir(&self.sites_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut stale_routes = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(project) = name.strip_suffix(".Caddyfile") else {
                continue;
            };
            let Ok(project_id) = Uuid::parse_str(project) else {
                continue;
            };
            let content = fs::read(&path).await?;
            let Some(owner) = route_owner(&content, project_id) else {
                continue;
            };
            let identity = RouteIdentity {
                project_id,
                deployment_id: owner.deployment_id(),
            };
            let known = match owner {
                RouteOwner::Legacy => known_routes
                    .serving
                    .iter()
                    .any(|identity| identity.project_id == project_id),
                RouteOwner::Serving(_) => known_routes.serving.contains(&identity),
                RouteOwner::Sleeping(_) => known_routes.sleeping.contains(&identity),
            };
            if known {
                continue;
            }
            stale_routes.push(RuntimeStaleRoute {
                path: path.display().to_string(),
                project_id,
                deployment_id: identity.deployment_id,
            });
        }
        Ok(stale_routes)
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(route.project_id, serving_route(route), reporter)
            .await
    }

    async fn activate_sleeping(
        &self,
        route: &SleepingRouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        self.replace_route(route.project_id, sleeping_route(route)?, reporter)
            .await
    }

    async fn deactivate(
        &self,
        identity: RouteIdentity,
        reporter: &dyn RuntimeReporter,
    ) -> Result<bool, RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let snapshot = remove_route(&self.sites_dir, identity).await?;
        if !snapshot.changed {
            return Ok(false);
        }
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(true)
    }

    async fn cleanup_stale_routes(
        &self,
        known_routes: &KnownRoutes,
        reporter: &dyn RuntimeReporter,
    ) -> Result<usize, RuntimeError> {
        let stale = self.discover_stale_routes(known_routes).await?;
        let mut cleaned = 0;
        for route in stale {
            if self
                .deactivate(
                    RouteIdentity {
                        project_id: route.project_id,
                        deployment_id: route.deployment_id,
                    },
                    reporter,
                )
                .await?
            {
                cleaned += 1;
            }
        }
        Ok(cleaned)
    }
}
