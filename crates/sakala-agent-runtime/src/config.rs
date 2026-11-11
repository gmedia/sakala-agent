use std::{path::PathBuf, time::Duration};

use crate::{containers::ResourceSafetyConfig, domains::ProjectDomainSuffixes};

/// How long a crash-looping workload must stay up before it stops being reported
/// as crashed. Long enough that an app crashing every few minutes reads as crashed
/// throughout, and short enough that an app that recovered clears soon.
pub const DEFAULT_CRASH_STABLE_WINDOW: Duration = Duration::from_secs(300);

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
    pub image_gc_max_age: Duration,
    /// Images of a project without any container are removed once older than this.
    pub orphaned_image_grace: Duration,
    pub min_workspace_free_bytes: u64,
    pub runtime_network: String,
    pub caddy_sites_dir: PathBuf,
    pub caddy_container: String,
    pub railpack_frontend: String,
    /// Image that runs Railpack, pinned by digest. Empty means not configured, and
    /// then Railpack builds and inspections are refused.
    pub railpack_image: String,
    /// Network of the Railpack sandbox. The agent creates it when it is missing.
    pub railpack_network: String,
    /// OCI runtime for managed workloads, such as `runsc`, as registered with
    /// Docker. `None` uses Docker's default. Builds and the Railpack sandbox are not
    /// affected. A workload that cannot start under it fails; nothing falls back.
    pub container_runtime: Option<String>,
    /// Name servers for workloads, written to a resolv.conf that is mounted into them.
    /// Empty leaves DNS to Docker, which `runsc` cannot use on a user-defined network:
    /// the embedded server lives on the host side of its network stack.
    pub container_dns: Vec<std::net::IpAddr>,
    /// Suffixes a deployment domain must sit under, one label deep. The default
    /// is the hosted Sakala zones; a self-hosted node names its own.
    pub project_domain_suffixes: ProjectDomainSuffixes,
    pub resource_safety: ResourceSafetyConfig,
    pub timeout_safety: TimeoutSafetyConfig,
    pub max_concurrent_builds: usize,
    pub max_active_containers: u32,
    /// With `health_interval`, sets how long a `HealthCheck` command waits for
    /// the app: `health_attempts * health_interval`. Deployments and other
    /// lifecycle commands wait for their start deadline instead.
    pub health_attempts: u32,
    /// Pause between readiness attempts.
    pub health_interval: Duration,
    /// See [`DEFAULT_CRASH_STABLE_WINDOW`].
    pub crash_stable_window: Duration,
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            agent_id: "local-agent-01".to_owned(),
            workspace_root: PathBuf::from("/var/lib/sakala/builds"),
            workspace_gc_max_age: Duration::from_secs(86_400),
            image_gc_max_age: Duration::from_secs(604_800),
            orphaned_image_grace: Duration::from_secs(86_400),
            min_workspace_free_bytes: 1_024 * 1_024 * 1_024,
            runtime_network: "sakala-runtime".to_owned(),
            caddy_sites_dir: PathBuf::from("/var/lib/sakala/caddy/sites"),
            caddy_container: "sakala-caddy".to_owned(),
            railpack_frontend: "ghcr.io/railwayapp/railpack-frontend:v0.23.0".to_owned(),
            railpack_image: String::new(),
            railpack_network: "sakala-railpack".to_owned(),
            container_runtime: None,
            container_dns: Vec::new(),
            project_domain_suffixes: ProjectDomainSuffixes::default(),
            resource_safety: ResourceSafetyConfig::default(),
            timeout_safety: TimeoutSafetyConfig::default(),
            max_concurrent_builds: 1,
            max_active_containers: 20,
            health_attempts: 10,
            health_interval: Duration::from_secs(1),
            crash_stable_window: DEFAULT_CRASH_STABLE_WINDOW,
        }
    }
}
