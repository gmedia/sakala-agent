use std::{path::PathBuf, time::Duration};

use crate::containers::ResourceSafetyConfig;

/// Node-level deadline ceilings. Product policy comes from the command payload;
/// these values protect the runtime node from excessive requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutSafetyConfig {
    pub max_build_timeout: Duration,
    pub max_start_timeout: Duration,
    pub max_command_timeout: Duration,
}

impl Default for TimeoutSafetyConfig {
    fn default() -> Self {
        Self {
            max_build_timeout: Duration::from_secs(600),
            max_start_timeout: Duration::from_secs(120),
            max_command_timeout: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AppliedRuntimeTimeouts {
    pub build: Duration,
    pub start: Duration,
}

impl TimeoutSafetyConfig {
    pub(crate) fn resolve(
        self,
        requested: sakala_agent_protocol::RuntimeTimeoutLimits,
    ) -> Result<AppliedRuntimeTimeouts, crate::RuntimeError> {
        let build = resolve_timeout(
            "build_timeout_seconds",
            requested.build_timeout_seconds,
            self.max_build_timeout,
        )?;
        let start = resolve_timeout(
            "start_timeout_seconds",
            requested.start_timeout_seconds,
            self.max_start_timeout,
        )?;
        let command = resolve_timeout(
            "command_timeout_seconds",
            requested.command_timeout_seconds,
            self.max_command_timeout,
        )?;
        if build >= command || start >= command {
            return Err(crate::RuntimeError::InvalidCommand(
                "build and start timeouts must each be shorter than command_timeout_seconds"
                    .to_owned(),
            ));
        }
        Ok(AppliedRuntimeTimeouts { build, start })
    }
}

fn resolve_timeout(
    name: &str,
    requested: Option<u64>,
    maximum: Duration,
) -> Result<Duration, crate::RuntimeError> {
    let seconds = requested.unwrap_or(maximum.as_secs());
    if seconds == 0 {
        return Err(crate::RuntimeError::InvalidCommand(format!(
            "{name} must be greater than zero"
        )));
    }
    if seconds > maximum.as_secs() {
        return Err(crate::RuntimeError::InvalidCommand(format!(
            "{name} ({seconds}s) exceeds the node maximum of {}s",
            maximum.as_secs()
        )));
    }
    Ok(Duration::from_secs(seconds))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerRuntimeConfig {
    pub agent_id: String,
    pub workspace_root: PathBuf,
    pub workspace_gc_max_age: Duration,
    pub min_workspace_free_bytes: u64,
    pub runtime_network: String,
    pub caddy_sites_dir: PathBuf,
    pub caddy_container: String,
    pub railpack_frontend: String,
    pub resource_safety: ResourceSafetyConfig,
    pub timeout_safety: TimeoutSafetyConfig,
    pub max_concurrent_builds: usize,
    pub max_active_containers: u32,
    pub health_attempts: u32,
    pub health_interval: Duration,
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            agent_id: "local-agent-01".to_owned(),
            workspace_root: PathBuf::from("/var/lib/sakala/builds"),
            workspace_gc_max_age: Duration::from_secs(86_400),
            min_workspace_free_bytes: 1_024 * 1_024 * 1_024,
            runtime_network: "sakala-runtime".to_owned(),
            caddy_sites_dir: PathBuf::from("/var/lib/sakala/caddy/sites"),
            caddy_container: "sakala-caddy".to_owned(),
            railpack_frontend: "ghcr.io/railwayapp/railpack-frontend:v0.23.0".to_owned(),
            resource_safety: ResourceSafetyConfig::default(),
            timeout_safety: TimeoutSafetyConfig::default(),
            max_concurrent_builds: 1,
            max_active_containers: 20,
            health_attempts: 10,
            health_interval: Duration::from_secs(1),
        }
    }
}
