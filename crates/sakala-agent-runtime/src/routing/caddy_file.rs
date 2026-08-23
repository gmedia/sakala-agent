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
    routing::{CaddyReloader, RouteManager, RouteSpec},
};
use sakala_agent_core::ports::RuntimeStaleRoute;

const MANAGED_ROUTE_PREFIX: &str = "# Managed by sakala-agent for project ";

pub struct CaddyFileRouteManager {
    sites_dir: PathBuf,
    reloader: Arc<dyn CaddyReloader>,
}

impl CaddyFileRouteManager {
    #[must_use]
    pub fn new(sites_dir: PathBuf, reloader: Arc<dyn CaddyReloader>) -> Self {
        Self {
            sites_dir,
            reloader,
        }
    }
}

struct RouteSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
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
        "{MANAGED_ROUTE_PREFIX}{}.\n{}:80 {{\n\treverse_proxy {}:{}\n}}\n",
        route.project_id, route.domain, route.upstream, route.port
    );

    fs::write(&temporary, content).await?;
    fs::rename(&temporary, &path).await?;

    Ok(RouteSnapshot { path, previous })
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

async fn remove_route(sites_dir: &Path, project_id: Uuid) -> Result<RouteSnapshot, RuntimeError> {
    let path = sites_dir.join(format!("{project_id}.Caddyfile"));
    let previous = match fs::read(&path).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if previous.is_some() {
        fs::remove_file(&path).await?;
    }
    Ok(RouteSnapshot { path, previous })
}

#[async_trait]
impl RouteManager for CaddyFileRouteManager {
    async fn discover_stale_routes(
        &self,
        known_projects: &HashSet<Uuid>,
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
            if !content.starts_with(&format!("{MANAGED_ROUTE_PREFIX}{project_id}."))
                || known_projects.contains(&project_id)
            {
                continue;
            }
            stale_routes.push(RuntimeStaleRoute {
                path: path.display().to_string(),
                project_id,
            });
        }
        Ok(stale_routes)
    }

    async fn activate(
        &self,
        route: &RouteSpec,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
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
        project_id: Uuid,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let snapshot = remove_route(&self.sites_dir, project_id).await?;
        if snapshot.previous.is_none() {
            return Ok(());
        }
        if let Err(error) = self.reloader.validate_and_reload(reporter).await {
            snapshot.restore().await?;
            self.reloader.reload_after_rollback().await;
            return Err(error);
        }
        Ok(())
    }
}
