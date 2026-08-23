use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use tokio::fs;
use uuid::Uuid;

use crate::{
    RuntimeError, RuntimeReporter,
    routing::{CaddyReloader, RouteIdentity, RouteManager, RouteSpec},
};
use sakala_agent_core::ports::RuntimeStaleRoute;

const MANAGED_ROUTE_PREFIX: &str = "# Managed by sakala-agent for project ";

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
}

struct RouteSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    changed: bool,
}

async fn write_route(sites_dir: &Path, route: &RouteSpec) -> Result<RouteSnapshot, RuntimeError> {
    fs::create_dir_all(sites_dir).await?;
    let path = sites_dir.join(format!("{}.Caddyfile", route.project_id));
    let temporary = sites_dir.join(format!(".{}.Caddyfile.tmp", route.project_id));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let content = format!(
        "{MANAGED_ROUTE_PREFIX}{} deployment {}.\n{}:80 {{\n\treverse_proxy {}:{}\n}}\n",
        route.project_id, route.deployment_id, route.domain, route.upstream, route.port
    );

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
        let legacy_prefix = format!("{MANAGED_ROUTE_PREFIX}{project_id}.");
        let current_prefix = format!("{MANAGED_ROUTE_PREFIX}{project_id} deployment ");
        if !content.starts_with(legacy_prefix.as_bytes())
            && !content.starts_with(current_prefix.as_bytes())
        {
            return Err(RuntimeError::Routing(format!(
                "refusing to delete route {} because it is not owned by Sakala",
                path.display()
            )));
        }
        if let Some(deployment_id) = identity.deployment_id {
            let expected =
                format!("{MANAGED_ROUTE_PREFIX}{project_id} deployment {deployment_id}.");
            if !content.starts_with(expected.as_bytes()) {
                return Ok(RouteSnapshot {
                    path,
                    previous,
                    changed: false,
                });
            }
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
        known_routes: &HashSet<RouteIdentity>,
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
            let content = fs::read_to_string(&path).await?;
            let owned_prefix = format!("{MANAGED_ROUTE_PREFIX}{project_id}");
            if !content.starts_with(&owned_prefix) {
                continue;
            }
            let deployment_id = content
                .lines()
                .next()
                .and_then(|line| line.strip_prefix(&format!("{owned_prefix} deployment ")))
                .and_then(|value| value.strip_suffix('.'))
                .and_then(|value| Uuid::parse_str(value).ok());
            if deployment_id.is_some_and(|deployment_id| {
                known_routes.contains(&RouteIdentity {
                    project_id,
                    deployment_id: Some(deployment_id),
                })
            }) || deployment_id.is_none()
                && known_routes
                    .iter()
                    .any(|identity| identity.project_id == project_id)
            {
                continue;
            }
            stale_routes.push(RuntimeStaleRoute {
                path: path.display().to_string(),
                project_id,
                deployment_id,
            });
        }
        Ok(stale_routes)
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let snapshot = write_route(&self.sites_dir, route).await?;
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(())
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
        known_routes: &HashSet<RouteIdentity>,
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
