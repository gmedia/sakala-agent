use std::{collections::HashMap, env, fmt, str::FromStr, time::Duration};

use clap::Parser;
use sakala_agent_core::{
    AgentConfig, CoreError, EDGE_ADDRESS_ENV, REGION_ENV,
    heartbeat::metrics::{
        DEFAULT_METRICS_INTERVAL, MAX_METRICS_INTERVAL, METRICS_INTERVAL_ENV, MIN_METRICS_INTERVAL,
    },
};
use sakala_agent_runtime::{
    DEFAULT_MAX_REPLICAS_PER_PROJECT, DEFAULT_RELEASE_COMMAND_TIMEOUT, DockerRuntimeConfig,
    MAX_REPLICAS_PER_PROJECT_CEILING, ProjectDomainSuffixes, ResourceSafetyConfig,
    TimeoutSafetyConfig, routing::check_public_api_url,
};

/// The control plane as visitors' browsers reach it, for browser-facing URLs
/// such as the sleeping page's wake request. Defaults to `SAKALA_API_URL`.
pub const PUBLIC_API_URL_ENV: &str = "SAKALA_PUBLIC_API_URL";

const RELEASE_COMMAND_TIMEOUT_ENV: &str = "SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RuntimeDriver {
    #[default]
    Noop,
    Docker,
}

impl fmt::Display for RuntimeDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Noop => formatter.write_str("noop"),
            Self::Docker => formatter.write_str("docker"),
        }
    }
}

impl FromStr for RuntimeDriver {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "noop" => Ok(Self::Noop),
            "docker" => Ok(Self::Docker),
            _ => Err(CoreError::InvalidConfiguration(format!(
                "SAKALA_RUNTIME_DRIVER must be noop or docker, received {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub agent: AgentConfig,
    pub runtime_driver: RuntimeDriver,
    pub docker_runtime: DockerRuntimeConfig,
    pub runtime_health_interval: Duration,
    /// How often the image GC removes stale Sakala images.
    pub image_gc_interval: Duration,
    /// How often heartbeats sample `workloads.metrics`.
    pub metrics_interval: Duration,
    pub log_level: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "sakala-agent",
    about = "Sakala runtime-node executor",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[arg(long, env = "SAKALA_AGENT_MODE")]
    mode: Option<String>,

    #[arg(long, env = "SAKALA_AGENT_ID")]
    agent_id: Option<String>,

    #[arg(long, env = "SAKALA_AGENT_TOKEN", hide_env_values = true)]
    agent_token: Option<String>,

    #[arg(long, env = "SAKALA_API_URL")]
    api_url: Option<String>,

    #[arg(long, env = PUBLIC_API_URL_ENV)]
    public_api_url: Option<String>,

    #[arg(long, env = "SAKALA_POLL_INTERVAL_SECONDS")]
    poll_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_HEARTBEAT_INTERVAL_SECONDS")]
    heartbeat_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_COMMAND_TIMEOUT_SECONDS")]
    command_timeout_seconds: Option<String>,

    #[arg(long, env = "SAKALA_MAX_CONCURRENT_COMMANDS")]
    max_concurrent_commands: Option<String>,

    #[arg(long, env = "SAKALA_SHUTDOWN_GRACE_SECONDS")]
    shutdown_grace_seconds: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_NETWORK")]
    runtime_network: Option<String>,

    #[arg(long, env = REGION_ENV)]
    agent_region: Option<String>,

    #[arg(long, env = EDGE_ADDRESS_ENV)]
    agent_edge_address: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_DRIVER")]
    runtime_driver: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_WORKSPACE")]
    runtime_workspace: Option<String>,

    #[arg(long, env = "SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS")]
    workspace_gc_max_age_seconds: Option<String>,

    #[arg(long, env = "SAKALA_IMAGE_GC_MAX_AGE_SECONDS")]
    image_gc_max_age_seconds: Option<String>,

    #[arg(long, env = "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS")]
    orphaned_image_grace_seconds: Option<String>,

    #[arg(long, env = "SAKALA_IMAGE_GC_INTERVAL_SECONDS")]
    image_gc_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS")]
    runtime_health_interval_seconds: Option<String>,

    #[arg(long, env = METRICS_INTERVAL_ENV)]
    metrics_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_MIN_WORKSPACE_FREE_MB")]
    min_workspace_free_mb: Option<String>,

    #[arg(long, env = "SAKALA_CADDY_SITES_DIR")]
    caddy_sites_dir: Option<String>,

    #[arg(long, env = "SAKALA_CADDY_CONTAINER")]
    caddy_container: Option<String>,

    #[arg(long, env = "SAKALA_RAILPACK_FRONTEND")]
    railpack_frontend: Option<String>,

    #[arg(long, env = "SAKALA_RAILPACK_IMAGE")]
    railpack_image: Option<String>,

    #[arg(long, env = "SAKALA_RAILPACK_NETWORK")]
    railpack_network: Option<String>,

    #[arg(long, env = "SAKALA_CONTAINER_RUNTIME")]
    container_runtime: Option<String>,

    #[arg(long, env = "SAKALA_CONTAINER_DNS")]
    container_dns: Option<String>,

    #[arg(long, env = "SAKALA_PROJECT_DOMAIN_SUFFIXES")]
    project_domain_suffixes: Option<String>,

    #[arg(long, env = "SAKALA_BUILD_TIMEOUT_SECONDS")]
    build_timeout_seconds: Option<String>,

    #[arg(long, env = "SAKALA_START_TIMEOUT_SECONDS")]
    start_timeout_seconds: Option<String>,

    #[arg(long, env = RELEASE_COMMAND_TIMEOUT_ENV)]
    release_command_timeout_seconds: Option<String>,

    #[arg(long, env = "SAKALA_MAX_CONCURRENT_BUILDS")]
    max_concurrent_builds: Option<String>,

    #[arg(long, env = "SAKALA_MAX_ACTIVE_CONTAINERS")]
    max_active_containers: Option<String>,

    #[arg(long, env = "SAKALA_MAX_REPLICAS_PER_PROJECT")]
    max_replicas_per_project: Option<String>,

    #[arg(long, env = "SAKALA_DEFAULT_CONTAINER_MEMORY_MB")]
    default_container_memory_mb: Option<String>,

    #[arg(long, env = "SAKALA_MAX_CONTAINER_MEMORY_MB")]
    max_container_memory_mb: Option<String>,

    #[arg(long, env = "SAKALA_DEFAULT_CONTAINER_CPU_MILLIS")]
    default_container_cpu_millis: Option<String>,

    #[arg(long, env = "SAKALA_MAX_CONTAINER_CPU_MILLIS")]
    max_container_cpu_millis: Option<String>,

    #[arg(long, env = "SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT")]
    default_container_pids_limit: Option<String>,

    #[arg(long, env = "SAKALA_MAX_CONTAINER_PIDS_LIMIT")]
    max_container_pids_limit: Option<String>,

    #[arg(long, env = "SAKALA_LOG_LEVEL")]
    log_level: Option<String>,
}

pub fn load() -> Result<AppConfig, CoreError> {
    let cli = Cli::parse();
    let mut values = env::vars().collect::<HashMap<_, _>>();

    insert(&mut values, "SAKALA_AGENT_MODE", cli.mode);
    insert(&mut values, "SAKALA_AGENT_ID", cli.agent_id);
    insert(&mut values, "SAKALA_AGENT_TOKEN", cli.agent_token);
    insert(&mut values, "SAKALA_API_URL", cli.api_url);
    insert(&mut values, PUBLIC_API_URL_ENV, cli.public_api_url);
    insert(
        &mut values,
        "SAKALA_POLL_INTERVAL_SECONDS",
        cli.poll_interval_seconds,
    );
    insert(
        &mut values,
        "SAKALA_HEARTBEAT_INTERVAL_SECONDS",
        cli.heartbeat_interval_seconds,
    );
    insert(
        &mut values,
        "SAKALA_COMMAND_TIMEOUT_SECONDS",
        cli.command_timeout_seconds,
    );
    insert(
        &mut values,
        "SAKALA_MAX_CONCURRENT_COMMANDS",
        cli.max_concurrent_commands,
    );
    insert(
        &mut values,
        "SAKALA_SHUTDOWN_GRACE_SECONDS",
        cli.shutdown_grace_seconds,
    );
    insert(&mut values, "SAKALA_RUNTIME_NETWORK", cli.runtime_network);
    insert(&mut values, REGION_ENV, cli.agent_region);
    insert(&mut values, EDGE_ADDRESS_ENV, cli.agent_edge_address);
    insert(&mut values, "SAKALA_RUNTIME_DRIVER", cli.runtime_driver);
    insert(
        &mut values,
        "SAKALA_RUNTIME_WORKSPACE",
        cli.runtime_workspace,
    );
    insert(
        &mut values,
        "SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS",
        cli.workspace_gc_max_age_seconds,
    );
    insert(
        &mut values,
        "SAKALA_IMAGE_GC_MAX_AGE_SECONDS",
        cli.image_gc_max_age_seconds,
    );
    insert(
        &mut values,
        "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS",
        cli.orphaned_image_grace_seconds,
    );
    insert(
        &mut values,
        "SAKALA_IMAGE_GC_INTERVAL_SECONDS",
        cli.image_gc_interval_seconds,
    );
    insert(
        &mut values,
        "SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS",
        cli.runtime_health_interval_seconds,
    );
    insert(
        &mut values,
        METRICS_INTERVAL_ENV,
        cli.metrics_interval_seconds,
    );
    insert(
        &mut values,
        "SAKALA_MIN_WORKSPACE_FREE_MB",
        cli.min_workspace_free_mb,
    );
    insert(&mut values, "SAKALA_CADDY_SITES_DIR", cli.caddy_sites_dir);
    insert(&mut values, "SAKALA_CADDY_CONTAINER", cli.caddy_container);
    insert(
        &mut values,
        "SAKALA_RAILPACK_FRONTEND",
        cli.railpack_frontend,
    );
    insert(&mut values, "SAKALA_RAILPACK_IMAGE", cli.railpack_image);
    insert(&mut values, "SAKALA_RAILPACK_NETWORK", cli.railpack_network);
    insert(
        &mut values,
        "SAKALA_CONTAINER_RUNTIME",
        cli.container_runtime,
    );
    insert(&mut values, "SAKALA_CONTAINER_DNS", cli.container_dns);
    insert(
        &mut values,
        "SAKALA_PROJECT_DOMAIN_SUFFIXES",
        cli.project_domain_suffixes,
    );
    insert(
        &mut values,
        "SAKALA_BUILD_TIMEOUT_SECONDS",
        cli.build_timeout_seconds,
    );
    insert(
        &mut values,
        "SAKALA_START_TIMEOUT_SECONDS",
        cli.start_timeout_seconds,
    );
    insert(
        &mut values,
        RELEASE_COMMAND_TIMEOUT_ENV,
        cli.release_command_timeout_seconds,
    );
    insert(
        &mut values,
        "SAKALA_MAX_CONCURRENT_BUILDS",
        cli.max_concurrent_builds,
    );
    insert(
        &mut values,
        "SAKALA_MAX_ACTIVE_CONTAINERS",
        cli.max_active_containers,
    );
    insert(
        &mut values,
        "SAKALA_MAX_REPLICAS_PER_PROJECT",
        cli.max_replicas_per_project,
    );
    insert(
        &mut values,
        "SAKALA_DEFAULT_CONTAINER_MEMORY_MB",
        cli.default_container_memory_mb,
    );
    insert(
        &mut values,
        "SAKALA_MAX_CONTAINER_MEMORY_MB",
        cli.max_container_memory_mb,
    );
    insert(
        &mut values,
        "SAKALA_DEFAULT_CONTAINER_CPU_MILLIS",
        cli.default_container_cpu_millis,
    );
    insert(
        &mut values,
        "SAKALA_MAX_CONTAINER_CPU_MILLIS",
        cli.max_container_cpu_millis,
    );
    insert(
        &mut values,
        "SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT",
        cli.default_container_pids_limit,
    );
    insert(
        &mut values,
        "SAKALA_MAX_CONTAINER_PIDS_LIMIT",
        cli.max_container_pids_limit,
    );
    insert(&mut values, "SAKALA_LOG_LEVEL", cli.log_level);

    from_values(&values)
}

fn from_values(values: &HashMap<String, String>) -> Result<AppConfig, CoreError> {
    let runtime_driver = get(values, "SAKALA_RUNTIME_DRIVER", "noop").parse()?;
    let mut agent = AgentConfig::from_values(values)?;
    agent.capabilities = capabilities(runtime_driver);
    let resource_safety = resource_safety(values)?;
    let min_workspace_free_bytes = positive_u64(values, "SAKALA_MIN_WORKSPACE_FREE_MB", 1_024)?
        .checked_mul(1_024 * 1_024)
        .ok_or_else(|| {
            CoreError::InvalidConfiguration(
                "SAKALA_MIN_WORKSPACE_FREE_MB is too large to represent".to_owned(),
            )
        })?;
    let build_timeout_seconds = positive_u64(values, "SAKALA_BUILD_TIMEOUT_SECONDS", 600)?;
    let start_timeout_seconds = positive_u64(values, "SAKALA_START_TIMEOUT_SECONDS", 120)?;
    if build_timeout_seconds >= agent.command_timeout_seconds
        || start_timeout_seconds >= agent.command_timeout_seconds
    {
        return Err(CoreError::InvalidConfiguration(format!(
            "SAKALA_BUILD_TIMEOUT_SECONDS ({build_timeout_seconds}) and SAKALA_START_TIMEOUT_SECONDS ({start_timeout_seconds}) must be shorter than SAKALA_COMMAND_TIMEOUT_SECONDS ({})",
            agent.command_timeout_seconds
        )));
    }

    let release_command_timeout = release_command_timeout(values, agent.command_timeout_seconds)?;

    // A deploy builds its image before it creates the container, so an image
    // younger than one command can still be about to run.
    let orphaned_image_grace_seconds =
        positive_u64(values, "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS", 86_400)?;
    if orphaned_image_grace_seconds < agent.command_timeout_seconds {
        return Err(CoreError::InvalidConfiguration(format!(
            "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS ({orphaned_image_grace_seconds}) must be at least SAKALA_COMMAND_TIMEOUT_SECONDS ({})",
            agent.command_timeout_seconds
        )));
    }

    Ok(AppConfig {
        docker_runtime: DockerRuntimeConfig {
            agent_id: agent.agent_id.clone(),
            workspace_root: get(values, "SAKALA_RUNTIME_WORKSPACE", "/var/lib/sakala/builds")
                .into(),
            workspace_gc_max_age: std::time::Duration::from_secs(positive_u64(
                values,
                "SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS",
                86_400,
            )?),
            image_gc_max_age: std::time::Duration::from_secs(positive_u64(
                values,
                "SAKALA_IMAGE_GC_MAX_AGE_SECONDS",
                604_800,
            )?),
            orphaned_image_grace: std::time::Duration::from_secs(orphaned_image_grace_seconds),
            min_workspace_free_bytes,
            runtime_network: agent.runtime_network.clone(),
            caddy_sites_dir: get(
                values,
                "SAKALA_CADDY_SITES_DIR",
                "/var/lib/sakala/caddy/sites",
            )
            .into(),
            caddy_container: get(values, "SAKALA_CADDY_CONTAINER", "sakala-caddy"),
            railpack_frontend: get(
                values,
                "SAKALA_RAILPACK_FRONTEND",
                "ghcr.io/railwayapp/railpack-frontend:v0.23.0",
            ),
            railpack_image: get(values, "SAKALA_RAILPACK_IMAGE", ""),
            railpack_network: get(values, "SAKALA_RAILPACK_NETWORK", "sakala-railpack"),
            container_runtime: container_runtime(values)?,
            container_dns: container_dns(values)?,
            project_domain_suffixes: project_domain_suffixes(values)?,
            resource_safety,
            timeout_safety: TimeoutSafetyConfig {
                max_build_timeout: std::time::Duration::from_secs(build_timeout_seconds),
                max_start_timeout: std::time::Duration::from_secs(start_timeout_seconds),
                max_command_timeout: agent.command_timeout(),
            },
            max_concurrent_builds: positive_usize(values, "SAKALA_MAX_CONCURRENT_BUILDS", 1)?,
            max_active_containers: positive_u32(values, "SAKALA_MAX_ACTIVE_CONTAINERS", 20)?,
            max_replicas_per_project: max_replicas_per_project(values)?,
            release_command_timeout,
            public_api_url: public_api_url(values, &agent.api_url)?,
            ..DockerRuntimeConfig::default()
        },
        runtime_health_interval: Duration::from_secs(positive_u64(
            values,
            "SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS",
            30,
        )?),
        image_gc_interval: Duration::from_secs(positive_u64(
            values,
            "SAKALA_IMAGE_GC_INTERVAL_SECONDS",
            3_600,
        )?),
        metrics_interval: metrics_interval(values)?,
        agent,
        runtime_driver,
        log_level: get(values, "SAKALA_LOG_LEVEL", "info"),
    })
}

fn resource_safety(values: &HashMap<String, String>) -> Result<ResourceSafetyConfig, CoreError> {
    let config = ResourceSafetyConfig {
        default_memory_mb: positive_u64(values, "SAKALA_DEFAULT_CONTAINER_MEMORY_MB", 256)?,
        max_memory_mb: positive_u64(values, "SAKALA_MAX_CONTAINER_MEMORY_MB", 512)?,
        default_cpu_millis: positive_u32(values, "SAKALA_DEFAULT_CONTAINER_CPU_MILLIS", 500)?,
        max_cpu_millis: positive_u32(values, "SAKALA_MAX_CONTAINER_CPU_MILLIS", 1_000)?,
        default_pids_limit: positive_u32(values, "SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT", 128)?,
        max_pids_limit: positive_u32(values, "SAKALA_MAX_CONTAINER_PIDS_LIMIT", 256)?,
    };

    for (name, default, maximum) in [
        (
            "container memory",
            config.default_memory_mb,
            config.max_memory_mb,
        ),
        (
            "container CPU",
            u64::from(config.default_cpu_millis),
            u64::from(config.max_cpu_millis),
        ),
        (
            "container process",
            u64::from(config.default_pids_limit),
            u64::from(config.max_pids_limit),
        ),
    ] {
        if default > maximum {
            return Err(CoreError::InvalidConfiguration(format!(
                "default {name} limit ({default}) cannot exceed node maximum ({maximum})"
            )));
        }
    }

    Ok(config)
}

/// A release command runs inside the deployment command, so it must end
/// before that command's deadline, like a build or a start.
fn release_command_timeout(
    values: &HashMap<String, String>,
    command_timeout_seconds: u64,
) -> Result<Duration, CoreError> {
    let seconds = positive_u64(
        values,
        RELEASE_COMMAND_TIMEOUT_ENV,
        DEFAULT_RELEASE_COMMAND_TIMEOUT.as_secs(),
    )?;
    if seconds >= command_timeout_seconds {
        return Err(CoreError::InvalidConfiguration(format!(
            "{RELEASE_COMMAND_TIMEOUT_ENV} ({seconds}) must be shorter than SAKALA_COMMAND_TIMEOUT_SECONDS ({command_timeout_seconds})"
        )));
    }
    Ok(Duration::from_secs(seconds))
}

/// One sample keeps Docker busy for about two seconds and the control plane
/// aggregates per minute, so the interval stays between the two bounds.
fn metrics_interval(values: &HashMap<String, String>) -> Result<Duration, CoreError> {
    let seconds = positive_u64(
        values,
        METRICS_INTERVAL_ENV,
        DEFAULT_METRICS_INTERVAL.as_secs(),
    )?;
    let interval = Duration::from_secs(seconds);
    if !(MIN_METRICS_INTERVAL..=MAX_METRICS_INTERVAL).contains(&interval) {
        return Err(CoreError::InvalidConfiguration(format!(
            "{METRICS_INTERVAL_ENV} must be between {} and {} seconds, received {seconds}",
            MIN_METRICS_INTERVAL.as_secs(),
            MAX_METRICS_INTERVAL.as_secs()
        )));
    }
    Ok(interval)
}

fn capabilities(runtime_driver: RuntimeDriver) -> Vec<String> {
    match runtime_driver {
        RuntimeDriver::Noop => vec!["noop-runtime".to_owned()],
        RuntimeDriver::Docker => [
            "docker-runtime",
            "project-inspection",
            "dockerfile-build",
            "railpack-info",
            "railpack-build",
            "caddy-file-routing",
            // Sleeping routes, wake capacity checks, and `workloads.traffic`.
            "idle-sleep",
            // `DeployProject.replicas`, balanced routes, and `workloads.replicas`.
            "replicas",
            // `DeployProject.release_command`, run once before the new version starts.
            "release-command",
            // `workloads.metrics`: CPU, memory, and restarts per workload container.
            "workload-metrics",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

/// An empty value means unset, which leaves the choice to Docker. Other values become
/// a process argument, so they are limited to what a runtime name can be.
fn container_runtime(values: &HashMap<String, String>) -> Result<Option<String>, CoreError> {
    let Some(name) = values
        .get("SAKALA_CONTAINER_RUNTIME")
        .filter(|name| !name.is_empty())
    else {
        return Ok(None);
    };
    let valid = name.starts_with(|first: char| first.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character));
    if valid {
        Ok(Some(name.clone()))
    } else {
        Err(CoreError::InvalidConfiguration(
            "SAKALA_CONTAINER_RUNTIME may only hold letters, digits, '.', '_', and '-', and must start with a letter or digit".to_owned(),
        ))
    }
}

fn container_dns(values: &HashMap<String, String>) -> Result<Vec<std::net::IpAddr>, CoreError> {
    values
        .get("SAKALA_CONTAINER_DNS")
        .map(String::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|server| !server.is_empty())
        .map(|server| {
            server.parse().map_err(|_| {
                CoreError::InvalidConfiguration(format!(
                    "SAKALA_CONTAINER_DNS holds {server:?}, which is not an IP address"
                ))
            })
        })
        .collect()
}

/// Unset or empty keeps the hosted Sakala suffixes. Anything else must be a valid
/// list, so a typo stops the agent instead of quietly routing a different zone.
fn project_domain_suffixes(
    values: &HashMap<String, String>,
) -> Result<ProjectDomainSuffixes, CoreError> {
    match values
        .get("SAKALA_PROJECT_DOMAIN_SUFFIXES")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        None => Ok(ProjectDomainSuffixes::default()),
        Some(value) => ProjectDomainSuffixes::parse(value).map_err(|reason| {
            CoreError::InvalidConfiguration(format!("SAKALA_PROJECT_DOMAIN_SUFFIXES {reason}"))
        }),
    }
}

fn get(values: &HashMap<String, String>, key: &str, default: &str) -> String {
    values
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_owned())
}

/// `SAKALA_PUBLIC_API_URL` when set, checked at start so a typo stops the agent
/// instead of breaking every sleeping page later; otherwise `SAKALA_API_URL`,
/// as before the setting existed. An empty value counts as unset.
fn public_api_url(values: &HashMap<String, String>, api_url: &str) -> Result<String, CoreError> {
    let Some(value) = values
        .get(PUBLIC_API_URL_ENV)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return Ok(api_url.to_owned());
    };
    check_public_api_url(value).map_err(|reason| {
        CoreError::InvalidConfiguration(format!("{PUBLIC_API_URL_ENV} {reason}"))
    })?;
    Ok(value.to_owned())
}

fn max_replicas_per_project(values: &HashMap<String, String>) -> Result<u32, CoreError> {
    const KEY: &str = "SAKALA_MAX_REPLICAS_PER_PROJECT";
    let maximum = positive_u32(values, KEY, DEFAULT_MAX_REPLICAS_PER_PROJECT)?;
    if maximum > MAX_REPLICAS_PER_PROJECT_CEILING {
        return Err(CoreError::InvalidConfiguration(format!(
            "{KEY} must be at most {MAX_REPLICAS_PER_PROJECT_CEILING}"
        )));
    }
    Ok(maximum)
}

fn positive_u32(
    values: &HashMap<String, String>,
    key: &str,
    default: u32,
) -> Result<u32, CoreError> {
    let value = values
        .get(key)
        .map_or_else(|| default.to_string(), Clone::clone)
        .parse::<u32>()
        .map_err(|_| CoreError::InvalidConfiguration(format!("{key} must be a number")))?;

    if value == 0 {
        return Err(CoreError::InvalidConfiguration(format!(
            "{key} must be greater than zero"
        )));
    }

    Ok(value)
}

fn positive_u64(
    values: &HashMap<String, String>,
    key: &str,
    default: u64,
) -> Result<u64, CoreError> {
    let value = values
        .get(key)
        .map_or_else(|| default.to_string(), Clone::clone)
        .parse::<u64>()
        .map_err(|_| CoreError::InvalidConfiguration(format!("{key} must be a number")))?;

    if value == 0 {
        return Err(CoreError::InvalidConfiguration(format!(
            "{key} must be greater than zero"
        )));
    }

    Ok(value)
}

fn positive_usize(
    values: &HashMap<String, String>,
    key: &str,
    default: usize,
) -> Result<usize, CoreError> {
    let value = values
        .get(key)
        .map_or_else(|| default.to_string(), Clone::clone)
        .parse::<usize>()
        .map_err(|_| CoreError::InvalidConfiguration(format!("{key} must be a number")))?;

    if value == 0 {
        return Err(CoreError::InvalidConfiguration(format!(
            "{key} must be greater than zero"
        )));
    }

    Ok(value)
}

fn insert(values: &mut HashMap<String, String>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        values.insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_binary_version_for_operator_compatibility_checks() {
        let error = Cli::try_parse_from(["sakala-agent", "--version"])
            .expect_err("version flag should stop normal command parsing");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn the_container_runtime_is_optional_and_cannot_become_another_argument() {
        let load = |value: &str| {
            from_values(&HashMap::from([(
                "SAKALA_CONTAINER_RUNTIME".to_owned(),
                value.to_owned(),
            )]))
        };

        let unset = from_values(&HashMap::new()).expect("config");
        assert_eq!(unset.docker_runtime.container_runtime, None);
        assert_eq!(
            load("").expect("config").docker_runtime.container_runtime,
            None
        );
        assert_eq!(
            load("runsc")
                .expect("config")
                .docker_runtime
                .container_runtime
                .as_deref(),
            Some("runsc")
        );
        for invalid in ["--privileged", "run sc", "runsc;id", "../runsc", "-x"] {
            assert!(load(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn container_dns_takes_only_ip_addresses() {
        let load = |value: &str| {
            from_values(&HashMap::from([(
                "SAKALA_CONTAINER_DNS".to_owned(),
                value.to_owned(),
            )]))
        };

        assert!(
            from_values(&HashMap::new())
                .expect("config")
                .docker_runtime
                .container_dns
                .is_empty()
        );
        assert_eq!(
            load("1.1.1.1, 2606:4700:4700::1111")
                .expect("config")
                .docker_runtime
                .container_dns
                .len(),
            2
        );
        for invalid in ["dns.example.com", "1.1.1.1;id", "1.1.1"] {
            assert!(load(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn project_domain_suffixes_default_to_hosted_zones_and_reject_bad_values() {
        let load = |value: &str| {
            from_values(&HashMap::from([(
                "SAKALA_PROJECT_DOMAIN_SUFFIXES".to_owned(),
                value.to_owned(),
            )]))
        };
        let suffixes = |config: AppConfig| config.docker_runtime.project_domain_suffixes;

        let hosted = ProjectDomainSuffixes::default();
        assert_eq!(
            suffixes(from_values(&HashMap::new()).expect("config")),
            hosted
        );
        assert_eq!(suffixes(load("  ").expect("config")), hosted);
        assert_eq!(
            suffixes(load("run.example.ac.id, apps.example.org").expect("config")).as_slice(),
            ["run.example.ac.id", "apps.example.org"]
        );
        for invalid in [",", "*.run.example.ac.id", "localhost", "Run.Example.ac.id"] {
            let error = load(invalid).expect_err(invalid).to_string();
            assert!(
                error.contains("SAKALA_PROJECT_DOMAIN_SUFFIXES"),
                "{invalid}: {error}"
            );
        }
    }

    #[test]
    fn placement_is_unset_by_default_and_empty_means_unset() {
        let unset = from_values(&HashMap::new()).expect("config");
        assert!(unset.agent.placement.is_empty());

        let empty = from_values(&HashMap::from([
            (REGION_ENV.to_owned(), String::new()),
            (EDGE_ADDRESS_ENV.to_owned(), String::new()),
        ]))
        .expect("config");
        assert!(empty.agent.placement.is_empty());
    }

    #[test]
    fn placement_accepts_values_the_api_accepts() {
        let config = from_values(&HashMap::from([
            (REGION_ENV.to_owned(), "id-jog-1".to_owned()),
            (EDGE_ADDRESS_ENV.to_owned(), "10.0.47.201:8180".to_owned()),
        ]))
        .expect("config");
        let placement = config.agent.placement;
        assert_eq!(
            placement.region.as_ref().map(|region| region.as_str()),
            Some("id-jog-1")
        );
        assert_eq!(
            placement
                .edge_address
                .as_ref()
                .map(|address| address.as_str()),
            Some("10.0.47.201:8180")
        );

        for address in ["[fd00::21]:8180", "runtime-01.internal:8180"] {
            let config = from_values(&HashMap::from([(
                EDGE_ADDRESS_ENV.to_owned(),
                address.to_owned(),
            )]))
            .expect(address);
            assert_eq!(
                config
                    .agent
                    .placement
                    .edge_address
                    .as_ref()
                    .map(|value| value.as_str()),
                Some(address)
            );
        }
    }

    #[test]
    fn placement_refuses_to_start_with_a_value_the_api_rejects() {
        let too_long_region = "a".repeat(sakala_agent_protocol::REGION_MAX_LENGTH + 1);
        for (key, value) in [
            (REGION_ENV, "JKT"),
            (REGION_ENV, "id--jog"),
            (REGION_ENV, " jkt"),
            (REGION_ENV, too_long_region.as_str()),
            (EDGE_ADDRESS_ENV, "10.0.47.201"),
            (EDGE_ADDRESS_ENV, "http://10.0.47.201:8180"),
            (EDGE_ADDRESS_ENV, "10.0.47.201:0"),
            (EDGE_ADDRESS_ENV, "10.0.47.201:65536"),
            (EDGE_ADDRESS_ENV, "fd00::21:8180"),
            (EDGE_ADDRESS_ENV, "10.0.47.256:8180"),
        ] {
            let error = from_values(&HashMap::from([(key.to_owned(), value.to_owned())]))
                .expect_err(value)
                .to_string();
            assert!(error.contains(key), "{key}={value:?}: {error}");
            assert!(
                error.contains(&format!("{value:?}")),
                "{key}={value:?}: {error}"
            );
        }
    }

    #[test]
    fn defaults_to_noop_runtime() {
        let config = from_values(&HashMap::new()).expect("default app config should load");

        assert_eq!(config.runtime_driver, RuntimeDriver::Noop);
        assert_eq!(config.agent.capabilities, ["noop-runtime"]);
        assert_eq!(config.docker_runtime.runtime_network, "sakala-runtime");
        assert_eq!(config.docker_runtime.resource_safety.default_memory_mb, 256);
        assert_eq!(config.docker_runtime.resource_safety.max_memory_mb, 512);
        assert_eq!(config.agent.max_concurrent_commands, 4);
        assert_eq!(config.docker_runtime.max_concurrent_builds, 1);
        assert_eq!(config.runtime_health_interval, Duration::from_secs(30));
        assert_eq!(config.image_gc_interval, Duration::from_secs(3_600));
        assert_eq!(
            config.docker_runtime.orphaned_image_grace,
            Duration::from_secs(86_400)
        );
        assert_eq!(
            config.docker_runtime.min_workspace_free_bytes,
            1_024 * 1_024 * 1_024
        );
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn docker_runtime_advertises_real_capabilities() {
        let values = HashMap::from([("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned())]);
        let config = from_values(&values).expect("Docker app config should load");

        assert_eq!(config.runtime_driver, RuntimeDriver::Docker);
        assert!(
            config
                .agent
                .capabilities
                .contains(&"project-inspection".to_owned())
        );
        assert!(
            config
                .agent
                .capabilities
                .contains(&"railpack-info".to_owned())
        );
        assert!(config.agent.capabilities.contains(&"idle-sleep".to_owned()));
        assert!(config.agent.capabilities.contains(&"replicas".to_owned()));
    }

    #[test]
    fn replicas_are_advertised_only_by_the_docker_runtime() {
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        assert!(!noop.agent.capabilities.contains(&"replicas".to_owned()));
    }

    #[test]
    fn release_commands_are_advertised_only_by_the_docker_runtime() {
        let docker = HashMap::from([("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned())]);
        let docker = from_values(&docker).expect("Docker app config should load");
        assert!(
            docker
                .agent
                .capabilities
                .contains(&"release-command".to_owned())
        );
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        assert!(
            !noop
                .agent
                .capabilities
                .contains(&"release-command".to_owned())
        );
    }

    #[test]
    fn the_release_command_timeout_defaults_to_ten_minutes_and_ends_before_the_command() {
        let timeout = |values: &[(&str, &str)]| {
            let values = values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<HashMap<_, _>>();
            from_values(&values).map(|config| config.docker_runtime.release_command_timeout)
        };
        assert_eq!(
            timeout(&[]).expect("default"),
            Duration::from_secs(600),
            "the documented default"
        );
        assert_eq!(
            timeout(&[(RELEASE_COMMAND_TIMEOUT_ENV, "30")]).expect("custom"),
            Duration::from_secs(30)
        );
        for invalid in ["0", "-1", "ten", "900"] {
            assert!(
                timeout(&[(RELEASE_COMMAND_TIMEOUT_ENV, invalid)]).is_err(),
                "{invalid}"
            );
        }
        let error = timeout(&[
            (RELEASE_COMMAND_TIMEOUT_ENV, "600"),
            ("SAKALA_COMMAND_TIMEOUT_SECONDS", "600"),
        ])
        .expect_err("must end before the command");
        assert!(
            error
                .to_string()
                .contains("must be shorter than SAKALA_COMMAND_TIMEOUT_SECONDS"),
            "{error}"
        );
    }

    #[test]
    fn the_replica_limit_defaults_to_four_and_stays_inside_the_ceiling() {
        let replicas = |value: Option<&str>| {
            let mut values = HashMap::new();
            if let Some(value) = value {
                values.insert(
                    "SAKALA_MAX_REPLICAS_PER_PROJECT".to_owned(),
                    value.to_owned(),
                );
            }
            from_values(&values).map(|config| config.docker_runtime.max_replicas_per_project)
        };
        assert_eq!(replicas(None).expect("default"), 4);
        assert_eq!(replicas(Some("1")).expect("one"), 1);
        assert_eq!(replicas(Some("64")).expect("ceiling"), 64);
        for invalid in ["0", "65", "-1", "four"] {
            assert!(replicas(Some(invalid)).is_err(), "{invalid}");
        }
    }

    #[test]
    fn idle_sleep_is_advertised_only_by_the_docker_runtime() {
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        assert!(!noop.agent.capabilities.contains(&"idle-sleep".to_owned()));

        let values = HashMap::from([
            ("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned()),
            (
                "SAKALA_API_URL".to_owned(),
                "https://api.staging.sakala.dev".to_owned(),
            ),
        ]);
        let docker = from_values(&values).expect("Docker app config should load");
        assert!(docker.agent.capabilities.contains(&"idle-sleep".to_owned()));
    }

    #[test]
    fn the_sleeping_page_uses_the_api_url_unless_a_public_one_is_set() {
        let public_api_url = |public: Option<&str>| {
            let mut values = HashMap::from([(
                "SAKALA_API_URL".to_owned(),
                "http://127.0.0.1:8100".to_owned(),
            )]);
            if let Some(public) = public {
                values.insert(PUBLIC_API_URL_ENV.to_owned(), public.to_owned());
            }
            from_values(&values).map(|config| config.docker_runtime.public_api_url)
        };

        // Unset or empty: the API URL the agent uses, as before.
        assert_eq!(
            public_api_url(None).expect("default"),
            "http://127.0.0.1:8100"
        );
        assert_eq!(
            public_api_url(Some("  ")).expect("empty"),
            "http://127.0.0.1:8100"
        );
        for public in [
            "https://api.example.ac.id",
            "https://example.ac.id/sakala/",
            "http://192.0.2.10:8000",
        ] {
            assert_eq!(public_api_url(Some(public)).expect(public), public);
        }
        for invalid in [
            "api.example.ac.id",
            "ftp://api.example.ac.id",
            "https://user:secret@api.example.ac.id",
            "https://api.example.ac.id/?a=1",
            "https://api.example.ac.id/#x",
            "https://api.example.ac.id/it's",
        ] {
            let error = public_api_url(Some(invalid))
                .expect_err(invalid)
                .to_string();
            assert!(error.contains(PUBLIC_API_URL_ENV), "{invalid}: {error}");
        }
    }

    #[test]
    fn rejects_default_resource_limit_above_node_maximum() {
        let values = HashMap::from([
            (
                "SAKALA_DEFAULT_CONTAINER_MEMORY_MB".to_owned(),
                "1024".to_owned(),
            ),
            (
                "SAKALA_MAX_CONTAINER_MEMORY_MB".to_owned(),
                "512".to_owned(),
            ),
        ]);

        let error = from_values(&values).expect_err("invalid safety config should fail");
        assert!(error.to_string().contains("cannot exceed node maximum"));
    }

    #[test]
    fn rejects_build_timeout_that_cannot_leave_time_for_cleanup() {
        let values = HashMap::from([
            ("SAKALA_BUILD_TIMEOUT_SECONDS".to_owned(), "900".to_owned()),
            (
                "SAKALA_COMMAND_TIMEOUT_SECONDS".to_owned(),
                "900".to_owned(),
            ),
        ]);

        let error = from_values(&values).expect_err("build deadline must precede command deadline");
        assert!(error.to_string().contains("must be shorter"));
    }

    #[test]
    fn accepts_independent_command_and_build_concurrency_limits() {
        let values = HashMap::from([
            ("SAKALA_MAX_CONCURRENT_COMMANDS".to_owned(), "6".to_owned()),
            ("SAKALA_MAX_CONCURRENT_BUILDS".to_owned(), "2".to_owned()),
        ]);

        let config = from_values(&values).expect("concurrency limits should be valid");
        assert_eq!(config.agent.max_concurrent_commands, 6);
        assert_eq!(config.docker_runtime.max_concurrent_builds, 2);
    }

    #[test]
    fn configures_the_image_gc_interval_and_orphaned_image_grace() {
        let values = HashMap::from([
            (
                "SAKALA_IMAGE_GC_INTERVAL_SECONDS".to_owned(),
                "600".to_owned(),
            ),
            (
                "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS".to_owned(),
                "7200".to_owned(),
            ),
        ]);
        let config = from_values(&values).expect("image GC settings should be valid");
        assert_eq!(config.image_gc_interval, Duration::from_secs(600));
        assert_eq!(
            config.docker_runtime.orphaned_image_grace,
            Duration::from_secs(7_200)
        );

        for (key, value, message) in [
            ("SAKALA_IMAGE_GC_INTERVAL_SECONDS", "0", "greater than zero"),
            (
                "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS",
                "0",
                "greater than zero",
            ),
            (
                "SAKALA_ORPHANED_IMAGE_GRACE_SECONDS",
                "60",
                "at least SAKALA_COMMAND_TIMEOUT_SECONDS",
            ),
        ] {
            let values = HashMap::from([(key.to_owned(), value.to_owned())]);
            let error = from_values(&values).expect_err("invalid image GC setting");
            assert!(
                error.to_string().contains(message),
                "{key}={value}: {error}"
            );
        }
    }

    #[test]
    fn rejects_zero_runtime_health_interval() {
        let values = HashMap::from([(
            "SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS".to_owned(),
            "0".to_owned(),
        )]);

        let error = from_values(&values).expect_err("zero interval must be rejected");
        assert!(error.to_string().contains("must be greater than zero"));
    }

    #[test]
    fn metrics_interval_defaults_to_a_minute_and_stays_within_its_bounds() {
        let config = from_values(&HashMap::new()).expect("defaults are valid");
        assert_eq!(config.metrics_interval, Duration::from_secs(60));

        for accepted in ["10", "3600"] {
            let values = HashMap::from([(METRICS_INTERVAL_ENV.to_owned(), accepted.to_owned())]);
            let config = from_values(&values).expect("a bound itself is accepted");
            assert_eq!(config.metrics_interval.as_secs().to_string(), accepted);
        }
        for (rejected, message) in [
            ("0", "must be greater than zero"),
            ("9", "between 10 and 3600 seconds"),
            ("3601", "between 10 and 3600 seconds"),
            ("60s", "must be a number"),
        ] {
            let values = HashMap::from([(METRICS_INTERVAL_ENV.to_owned(), rejected.to_owned())]);
            let error = from_values(&values).expect_err("out-of-range interval must be rejected");
            assert!(error.to_string().contains(message), "{rejected}: {error}");
        }
    }

    #[test]
    fn only_the_docker_runtime_advertises_workload_metrics() {
        let docker = from_values(&HashMap::from([(
            "SAKALA_RUNTIME_DRIVER".to_owned(),
            "docker".to_owned(),
        )]))
        .expect("docker config is valid");
        let noop = from_values(&HashMap::new()).expect("noop config is valid");

        assert!(
            docker
                .agent
                .capabilities
                .contains(&"workload-metrics".to_owned())
        );
        assert!(
            !noop
                .agent
                .capabilities
                .contains(&"workload-metrics".to_owned())
        );
    }

    #[test]
    fn configures_workspace_disk_floor_in_mebibytes() {
        let values = HashMap::from([("SAKALA_MIN_WORKSPACE_FREE_MB".to_owned(), "256".to_owned())]);

        let config = from_values(&values).expect("disk floor should be valid");
        assert_eq!(
            config.docker_runtime.min_workspace_free_bytes,
            256 * 1_024 * 1_024
        );
    }
}
