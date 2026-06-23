use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use tokio::fs;

use crate::{
    RuntimeError, RuntimeReporter,
    routing::{CaddyReloader, RouteManager, RouteSpec},
};

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
        "# Managed by sakala-agent for project {}.\n{}:80 {{\n\treverse_proxy {}:{}\n}}\n",
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

#[async_trait]
impl RouteManager for CaddyFileRouteManager {
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
}
