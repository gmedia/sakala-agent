use std::{path::PathBuf, time::Duration};

use crate::containers::ResourceSafetyConfig;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerRuntimeConfig {
    pub workspace_root: PathBuf,
    pub runtime_network: String,
    pub caddy_sites_dir: PathBuf,
    pub caddy_container: String,
    pub railpack_frontend: String,
    pub resource_safety: ResourceSafetyConfig,
    pub health_attempts: u32,
    pub health_interval: Duration,
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("/var/lib/sakala/builds"),
            runtime_network: "sakala-runtime".to_owned(),
            caddy_sites_dir: PathBuf::from("/var/lib/sakala/caddy/sites"),
            caddy_container: "sakala-caddy".to_owned(),
            railpack_frontend: "ghcr.io/railwayapp/railpack-frontend:v0.23.0".to_owned(),
            resource_safety: ResourceSafetyConfig::default(),
            health_attempts: 10,
            health_interval: Duration::from_secs(1),
        }
    }
}
