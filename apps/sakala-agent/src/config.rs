use std::{collections::HashMap, env, fmt, path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;
use sakala_agent_core::{
    AgentConfig, CoreError, EDGE_ADDRESS_ENV, MAX_CONCURRENT_JOBS_ENV, REGION_ENV,
    heartbeat::{
        metrics::{
            DEFAULT_METRICS_INTERVAL, MAX_METRICS_INTERVAL, METRICS_INTERVAL_ENV,
            MIN_METRICS_INTERVAL,
        },
        object_storage::ObjectStorageIdentity,
    },
    ports::SecretString,
};
use sakala_agent_protocol::{
    DatabaseEndpoint, IMAGE_REUSE_CAPABILITY, IMAGE_ROLLBACK_CAPABILITY,
    MANAGED_POSTGRES_CAPABILITY, MANAGED_S3_CAPABILITY, Region, SCHEDULED_JOBS_CAPABILITY,
    WORKER_PROCESSES_CAPABILITY,
};
use sakala_agent_runtime::{
    DEFAULT_MAX_REPLICAS_PER_PROJECT, DEFAULT_RELEASE_COMMAND_TIMEOUT,
    DEFAULT_WORKER_STARTUP_WINDOW, DEFAULT_WORKER_STOP_GRACE, DockerRuntimeConfig,
    MAX_REPLICAS_PER_PROJECT_CEILING, ProjectDomainSuffixes, ResourceSafetyConfig,
    TimeoutSafetyConfig,
    database::{DATABASE_ADMIN_DSN_ENV, PostgresAdminConfig},
    object_storage::{
        DEFAULT_OBJECT_STORAGE_CONTAINER, DEFAULT_OBJECT_STORAGE_DATA_DIR,
        DEFAULT_OBJECT_STORAGE_MIN_FREE_MB, DEFAULT_OBJECT_STORAGE_REGION, RustfsAdminConfig,
        check_container_name, check_public_endpoint,
    },
    routing::{check_public_api_url, check_public_console_url},
};

/// The control plane as visitors' browsers reach it, for browser-facing URLs
/// such as the sleeping page's wake request. Defaults to `SAKALA_API_URL`.
pub const PUBLIC_API_URL_ENV: &str = "SAKALA_PUBLIC_API_URL";

/// The Console as project owners' browsers reach it, which a sleeping page
/// links its owner to. Optional: without it the page names the Console only.
pub const PUBLIC_CONSOLE_URL_ENV: &str = "SAKALA_PUBLIC_CONSOLE_URL";

const RELEASE_COMMAND_TIMEOUT_ENV: &str = "SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS";

/// How long a worker must keep running after it started before a deployment
/// counts it as started (ADR-022).
const WORKER_STARTUP_ENV: &str = "SAKALA_WORKER_STARTUP_SECONDS";

/// How long a worker gets between `SIGTERM` and `SIGKILL` (ADR-022).
const WORKER_STOP_GRACE_ENV: &str = "SAKALA_WORKER_STOP_GRACE_SECONDS";

/// No longer used: a project with containers keeps its live and previous
/// images, whatever their age, and nothing else. Still read and checked, so a
/// configuration that sets it keeps loading, and the startup log says it is
/// ignored.
pub const IMAGE_GC_MAX_AGE_ENV: &str = "SAKALA_IMAGE_GC_MAX_AGE_SECONDS";

/// Turns on the database host role: `none` (default) or `postgres`.
const DATABASE_DRIVER_ENV: &str = "SAKALA_DATABASE_DRIVER";

/// Where runtime nodes reach the managed instance over the private network,
/// reported as `metadata.database.endpoint`.
const DATABASE_PUBLIC_HOST_ENV: &str = "SAKALA_DATABASE_PUBLIC_HOST";
const DATABASE_PUBLIC_PORT_ENV: &str = "SAKALA_DATABASE_PUBLIC_PORT";

/// PostgreSQL's own default port.
const DEFAULT_DATABASE_PUBLIC_PORT: u16 = 5432;

/// Which database engine the agent administers, independent of the runtime
/// driver: a dedicated database host runs `noop` with `postgres`, and a
/// single-node installation can run `docker` with `postgres` (ADR-020).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DatabaseDriver {
    #[default]
    None,
    Postgres,
}

impl FromStr for DatabaseDriver {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Self::None),
            "postgres" => Ok(Self::Postgres),
            _ => Err(CoreError::InvalidConfiguration(format!(
                "{DATABASE_DRIVER_ENV} must be none or postgres, received {value}"
            ))),
        }
    }
}

/// The database host role's settings, present only with
/// `SAKALA_DATABASE_DRIVER=postgres`.
#[derive(Clone, Debug)]
pub struct DatabaseHostConfig {
    pub admin: PostgresAdminConfig,
    pub endpoint: DatabaseEndpoint,
}

/// Turns on the object storage host role: `none` (default) or `rustfs`.
const OBJECT_STORAGE_DRIVER_ENV: &str = "SAKALA_OBJECT_STORAGE_DRIVER";
/// The container running RustFS, which holds the root key files.
const OBJECT_STORAGE_CONTAINER_ENV: &str = "SAKALA_OBJECT_STORAGE_CONTAINER";
/// Where tenants and browsers reach the instance, reported as
/// `metadata.object_storage.endpoint`.
const OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV: &str = "SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT";
/// The S3 signing region, equal to RustFS's `RUSTFS_REGION`.
const OBJECT_STORAGE_REGION_ENV: &str = "SAKALA_OBJECT_STORAGE_REGION";
/// The most all bucket quotas may add up to, in MiB.
const OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV: &str = "SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB";
/// Disk to keep free when every bucket is full, in MiB.
const OBJECT_STORAGE_MIN_FREE_MB_ENV: &str = "SAKALA_OBJECT_STORAGE_MIN_FREE_MB";
/// The instance's data directory, read for free space.
const OBJECT_STORAGE_DATA_DIR_ENV: &str = "SAKALA_OBJECT_STORAGE_DATA_DIR";

/// Which object storage engine the agent administers, independent of the
/// runtime and database drivers (ADR-021).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObjectStorageDriver {
    #[default]
    None,
    Rustfs,
}

impl FromStr for ObjectStorageDriver {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Self::None),
            "rustfs" => Ok(Self::Rustfs),
            _ => Err(CoreError::InvalidConfiguration(format!(
                "{OBJECT_STORAGE_DRIVER_ENV} must be none or rustfs, received {value}"
            ))),
        }
    }
}

/// The object storage host role's settings, present only with
/// `SAKALA_OBJECT_STORAGE_DRIVER=rustfs`. Holds no credential: the root key
/// stays in the RustFS container.
#[derive(Clone, Debug)]
pub struct ObjectStorageHostConfig {
    pub admin: RustfsAdminConfig,
    pub identity: ObjectStorageIdentity,
}

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
    /// `None` unless the node is a database host.
    pub database: Option<DatabaseHostConfig>,
    /// `None` unless the node is an object storage host.
    pub object_storage: Option<ObjectStorageHostConfig>,
    pub log_level: String,
    /// Settings that are set but no longer have any effect, for one startup
    /// warning each.
    pub ignored_settings: Vec<&'static str>,
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

    #[arg(long, env = PUBLIC_CONSOLE_URL_ENV)]
    public_console_url: Option<String>,

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

    /// Deprecated and ignored; see [`IMAGE_GC_MAX_AGE_ENV`].
    #[arg(long, env = "SAKALA_IMAGE_GC_MAX_AGE_SECONDS", hide = true)]
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

    #[arg(long, env = WORKER_STARTUP_ENV)]
    worker_startup_seconds: Option<String>,

    #[arg(long, env = WORKER_STOP_GRACE_ENV)]
    worker_stop_grace_seconds: Option<String>,

    #[arg(long, env = MAX_CONCURRENT_JOBS_ENV)]
    max_concurrent_jobs: Option<String>,

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

    #[arg(long, env = DATABASE_DRIVER_ENV)]
    database_driver: Option<String>,

    #[arg(long, env = DATABASE_PUBLIC_HOST_ENV)]
    database_public_host: Option<String>,

    #[arg(long, env = DATABASE_PUBLIC_PORT_ENV)]
    database_public_port: Option<String>,

    // SAKALA_DATABASE_ADMIN_DSN has no flag on purpose: it holds the superuser
    // password, which must not appear in the process arguments.
    #[arg(long, env = OBJECT_STORAGE_DRIVER_ENV)]
    object_storage_driver: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_CONTAINER_ENV)]
    object_storage_container: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV)]
    object_storage_public_endpoint: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_REGION_ENV)]
    object_storage_region: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV)]
    object_storage_total_quota_mb: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_MIN_FREE_MB_ENV)]
    object_storage_min_free_mb: Option<String>,

    #[arg(long, env = OBJECT_STORAGE_DATA_DIR_ENV)]
    object_storage_data_dir: Option<String>,

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
    insert(&mut values, PUBLIC_CONSOLE_URL_ENV, cli.public_console_url);
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
        IMAGE_GC_MAX_AGE_ENV,
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
    insert(&mut values, WORKER_STARTUP_ENV, cli.worker_startup_seconds);
    insert(
        &mut values,
        WORKER_STOP_GRACE_ENV,
        cli.worker_stop_grace_seconds,
    );
    insert(
        &mut values,
        MAX_CONCURRENT_JOBS_ENV,
        cli.max_concurrent_jobs,
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
    insert(&mut values, DATABASE_DRIVER_ENV, cli.database_driver);
    insert(
        &mut values,
        DATABASE_PUBLIC_HOST_ENV,
        cli.database_public_host,
    );
    insert(
        &mut values,
        DATABASE_PUBLIC_PORT_ENV,
        cli.database_public_port,
    );
    for (key, value) in [
        (OBJECT_STORAGE_DRIVER_ENV, cli.object_storage_driver),
        (OBJECT_STORAGE_CONTAINER_ENV, cli.object_storage_container),
        (
            OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV,
            cli.object_storage_public_endpoint,
        ),
        (OBJECT_STORAGE_REGION_ENV, cli.object_storage_region),
        (
            OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV,
            cli.object_storage_total_quota_mb,
        ),
        (
            OBJECT_STORAGE_MIN_FREE_MB_ENV,
            cli.object_storage_min_free_mb,
        ),
        (OBJECT_STORAGE_DATA_DIR_ENV, cli.object_storage_data_dir),
    ] {
        insert(&mut values, key, value);
    }
    insert(&mut values, "SAKALA_LOG_LEVEL", cli.log_level);

    from_values(&values)
}

fn from_values(values: &HashMap<String, String>) -> Result<AppConfig, CoreError> {
    let runtime_driver = get(values, "SAKALA_RUNTIME_DRIVER", "noop").parse()?;
    let mut agent = AgentConfig::from_values(values)?;
    agent.capabilities = capabilities(runtime_driver);
    let database = database_host(values)?;
    if database.is_some() {
        agent
            .capabilities
            .push(MANAGED_POSTGRES_CAPABILITY.to_owned());
    }
    let object_storage = object_storage_host(values, agent.command_timeout())?;
    if object_storage.is_some() {
        agent.capabilities.push(MANAGED_S3_CAPABILITY.to_owned());
    }
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
    let worker_startup_window = shorter_than_command(
        values,
        WORKER_STARTUP_ENV,
        DEFAULT_WORKER_STARTUP_WINDOW,
        agent.command_timeout_seconds,
    )?;
    let worker_stop_grace = shorter_than_command(
        values,
        WORKER_STOP_GRACE_ENV,
        DEFAULT_WORKER_STOP_GRACE,
        agent.command_timeout_seconds,
    )?;

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

    let ignored_settings = if values.contains_key(IMAGE_GC_MAX_AGE_ENV) {
        // Checked as before, so a value that stopped an older Agent still
        // stops this one, but not used any more. The default is never read.
        positive_u64(values, IMAGE_GC_MAX_AGE_ENV, u64::default())?;
        vec![IMAGE_GC_MAX_AGE_ENV]
    } else {
        Vec::new()
    };
    Ok(AppConfig {
        ignored_settings,
        docker_runtime: DockerRuntimeConfig {
            agent_id: agent.agent_id.clone(),
            workspace_root: get(values, "SAKALA_RUNTIME_WORKSPACE", "/var/lib/sakala/builds")
                .into(),
            workspace_gc_max_age: std::time::Duration::from_secs(positive_u64(
                values,
                "SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS",
                86_400,
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
            worker_startup_window,
            worker_stop_grace,
            public_api_url: public_api_url(values, &agent.api_url)?,
            public_console_url: public_console_url(values)?,
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
        database,
        object_storage,
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
    shorter_than_command(
        values,
        RELEASE_COMMAND_TIMEOUT_ENV,
        DEFAULT_RELEASE_COMMAND_TIMEOUT,
        command_timeout_seconds,
    )
}

/// A setting that runs inside a deployment command, such as the release
/// command's deadline or a worker's startup window and stop grace, must end
/// before that command's deadline.
fn shorter_than_command(
    values: &HashMap<String, String>,
    key: &str,
    default: Duration,
    command_timeout_seconds: u64,
) -> Result<Duration, CoreError> {
    let seconds = positive_u64(values, key, default.as_secs())?;
    if seconds >= command_timeout_seconds {
        return Err(CoreError::InvalidConfiguration(format!(
            "{key} ({seconds}) must be shorter than SAKALA_COMMAND_TIMEOUT_SECONDS ({command_timeout_seconds})"
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
            // `DeployProject.reuse_image_from_deployment_id`: start from an image
            // built earlier instead of building.
            IMAGE_REUSE_CAPABILITY,
            // The live and the previous image of every project stay on the
            // node, so a rollback to the previous deployment needs no build.
            IMAGE_ROLLBACK_CAPABILITY,
            // `DeployProject.processes`: worker containers next to the web
            // replicas, and `process` on logs, metrics, and crash details.
            WORKER_PROCESSES_CAPABILITY,
            // `RunJob`: one container of the live image, in its own pool.
            SCHEDULED_JOBS_CAPABILITY,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

/// Reads the database host role. With `postgres`, the admin DSN and the public
/// host are required and checked now, so a typo stops the agent instead of
/// failing every database command later.
fn database_host(
    values: &HashMap<String, String>,
) -> Result<Option<DatabaseHostConfig>, CoreError> {
    let driver: DatabaseDriver = get(values, DATABASE_DRIVER_ENV, "none").trim().parse()?;
    if driver == DatabaseDriver::None {
        return Ok(None);
    }
    let required = |key: &str| {
        values
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CoreError::InvalidConfiguration(format!(
                    "{key} is required when {DATABASE_DRIVER_ENV}=postgres"
                ))
            })
    };
    let admin = PostgresAdminConfig::parse(&SecretString::new(required(DATABASE_ADMIN_DSN_ENV)?))
        .map_err(CoreError::InvalidConfiguration)?;
    let host = required(DATABASE_PUBLIC_HOST_ENV)?;
    let port = match values
        .get(DATABASE_PUBLIC_PORT_ENV)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        None => DEFAULT_DATABASE_PUBLIC_PORT,
        Some(port) => port.parse::<u16>().map_err(|_| {
            CoreError::InvalidConfiguration(format!(
                "{DATABASE_PUBLIC_PORT_ENV} must be a port from 1 to 65535, received {port:?}"
            ))
        })?,
    };
    let endpoint = DatabaseEndpoint::new(host, port).map_err(|reason| {
        let key = match reason {
            sakala_agent_protocol::databases::InvalidDatabaseEndpoint::Host => {
                DATABASE_PUBLIC_HOST_ENV
            }
            sakala_agent_protocol::databases::InvalidDatabaseEndpoint::Port => {
                DATABASE_PUBLIC_PORT_ENV
            }
        };
        CoreError::InvalidConfiguration(format!("{key} {reason}"))
    })?;
    Ok(Some(DatabaseHostConfig { admin, endpoint }))
}

/// Reads the object storage host role. With `rustfs`, the public endpoint and
/// the total quota are required and every value is checked now, so a typo stops
/// the agent instead of failing every bucket command later.
fn object_storage_host(
    values: &HashMap<String, String>,
    command_timeout: Duration,
) -> Result<Option<ObjectStorageHostConfig>, CoreError> {
    let driver: ObjectStorageDriver = get(values, OBJECT_STORAGE_DRIVER_ENV, "none")
        .trim()
        .parse()?;
    if driver == ObjectStorageDriver::None {
        return Ok(None);
    }
    let value = |key: &str| {
        values
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    };
    let invalid =
        |key: &str, reason: &str| CoreError::InvalidConfiguration(format!("{key} {reason}"));
    let required = |key: &str| {
        value(key).ok_or_else(|| {
            CoreError::InvalidConfiguration(format!(
                "{key} is required when {OBJECT_STORAGE_DRIVER_ENV}=rustfs"
            ))
        })
    };

    let container = value(OBJECT_STORAGE_CONTAINER_ENV).unwrap_or(DEFAULT_OBJECT_STORAGE_CONTAINER);
    check_container_name(container)
        .map_err(|reason| invalid(OBJECT_STORAGE_CONTAINER_ENV, reason))?;
    let endpoint = check_public_endpoint(required(OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV)?)
        .map_err(|reason| invalid(OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV, reason))?;
    let region =
        Region::parse(value(OBJECT_STORAGE_REGION_ENV).unwrap_or(DEFAULT_OBJECT_STORAGE_REGION))
            .map_err(|reason| invalid(OBJECT_STORAGE_REGION_ENV, &reason.to_string()))?;
    let total_quota_mb = required(OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV)?
        .parse::<u64>()
        .ok()
        .filter(|total| *total > 0)
        .ok_or_else(|| {
            invalid(
                OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV,
                "must be a whole number above 0",
            )
        })?;
    let min_free_mb = match value(OBJECT_STORAGE_MIN_FREE_MB_ENV) {
        None => DEFAULT_OBJECT_STORAGE_MIN_FREE_MB,
        Some(min_free) => min_free
            .parse::<u64>()
            .map_err(|_| invalid(OBJECT_STORAGE_MIN_FREE_MB_ENV, "must be a whole number"))?,
    };
    let data_dir = PathBuf::from(
        value(OBJECT_STORAGE_DATA_DIR_ENV).unwrap_or(DEFAULT_OBJECT_STORAGE_DATA_DIR),
    );
    if !data_dir.is_absolute() {
        return Err(invalid(
            OBJECT_STORAGE_DATA_DIR_ENV,
            "must be an absolute path",
        ));
    }
    Ok(Some(ObjectStorageHostConfig {
        admin: RustfsAdminConfig {
            container: container.to_owned(),
            data_dir,
            total_quota_mb,
            min_free_mb,
            command_timeout,
        },
        identity: ObjectStorageIdentity {
            endpoint,
            region,
            capacity_mb: total_quota_mb,
        },
    }))
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

/// `SAKALA_PUBLIC_CONSOLE_URL` when set, checked at start like
/// `SAKALA_PUBLIC_API_URL`. An empty value counts as unset.
fn public_console_url(values: &HashMap<String, String>) -> Result<Option<String>, CoreError> {
    let Some(value) = values
        .get(PUBLIC_CONSOLE_URL_ENV)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    check_public_console_url(value).map_err(|reason| {
        CoreError::InvalidConfiguration(format!("{PUBLIC_CONSOLE_URL_ENV} {reason}"))
    })?;
    Ok(Some(value.to_owned()))
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
    fn workers_and_jobs_are_advertised_only_by_the_docker_runtime() {
        let docker = HashMap::from([("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned())]);
        let docker = from_values(&docker).expect("Docker app config should load");
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        for capability in ["worker-processes", "scheduled-jobs"] {
            assert!(
                docker.agent.capabilities.contains(&capability.to_owned()),
                "{capability}"
            );
            assert!(
                !noop.agent.capabilities.contains(&capability.to_owned()),
                "{capability}"
            );
        }
    }

    #[test]
    fn worker_and_job_settings_have_the_adr_starting_values() {
        let config = from_values(&HashMap::new()).expect("default app config should load");
        assert_eq!(config.agent.max_concurrent_jobs, 2);
        assert_eq!(
            config.docker_runtime.worker_startup_window,
            Duration::from_secs(10)
        );
        assert_eq!(
            config.docker_runtime.worker_stop_grace,
            Duration::from_secs(30)
        );

        let config = from_values(&HashMap::from([
            ("SAKALA_MAX_CONCURRENT_JOBS".to_owned(), "5".to_owned()),
            ("SAKALA_WORKER_STARTUP_SECONDS".to_owned(), "3".to_owned()),
            (
                "SAKALA_WORKER_STOP_GRACE_SECONDS".to_owned(),
                "60".to_owned(),
            ),
        ]))
        .expect("custom values load");
        assert_eq!(config.agent.max_concurrent_jobs, 5);
        assert_eq!(config.agent.max_concurrent_commands, 4, "independent");
        assert_eq!(
            config.docker_runtime.worker_startup_window,
            Duration::from_secs(3)
        );
        assert_eq!(
            config.docker_runtime.worker_stop_grace,
            Duration::from_secs(60)
        );

        for (key, value, expected) in [
            ("SAKALA_MAX_CONCURRENT_JOBS", "0", "greater than zero"),
            ("SAKALA_WORKER_STARTUP_SECONDS", "0", "greater than zero"),
            ("SAKALA_WORKER_STARTUP_SECONDS", "900", "must be shorter"),
            (
                "SAKALA_WORKER_STOP_GRACE_SECONDS",
                "1000",
                "must be shorter",
            ),
        ] {
            let error =
                from_values(&HashMap::from([(key.to_owned(), value.to_owned())])).expect_err(key);
            assert!(
                error.to_string().contains(expected),
                "{key}={value}: {error}"
            );
        }
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
    fn image_reuse_is_advertised_only_by_the_docker_runtime() {
        let docker = HashMap::from([("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned())]);
        let docker = from_values(&docker).expect("Docker app config should load");
        assert!(
            docker
                .agent
                .capabilities
                .contains(&IMAGE_REUSE_CAPABILITY.to_owned())
        );
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        assert!(
            !noop
                .agent
                .capabilities
                .contains(&IMAGE_REUSE_CAPABILITY.to_owned())
        );
    }

    #[test]
    fn image_rollback_is_advertised_only_by_the_docker_runtime() {
        let docker = HashMap::from([("SAKALA_RUNTIME_DRIVER".to_owned(), "docker".to_owned())]);
        let docker = from_values(&docker).expect("Docker app config should load");
        assert!(
            docker
                .agent
                .capabilities
                .contains(&IMAGE_ROLLBACK_CAPABILITY.to_owned())
        );
        let noop = from_values(&HashMap::new()).expect("default app config should load");
        assert!(
            !noop
                .agent
                .capabilities
                .contains(&IMAGE_ROLLBACK_CAPABILITY.to_owned())
        );
    }

    #[test]
    fn the_image_gc_max_age_still_loads_but_is_reported_as_ignored() {
        let config = from_values(&HashMap::new()).expect("default app config should load");
        assert!(config.ignored_settings.is_empty());

        let set = HashMap::from([(IMAGE_GC_MAX_AGE_ENV.to_owned(), "604800".to_owned())]);
        let config = from_values(&set).expect("a configuration that sets it keeps loading");
        assert_eq!(config.ignored_settings, [IMAGE_GC_MAX_AGE_ENV]);

        for invalid in ["0", "seven days"] {
            let values = HashMap::from([(IMAGE_GC_MAX_AGE_ENV.to_owned(), invalid.to_owned())]);
            assert!(from_values(&values).is_err(), "{invalid} is still refused");
        }
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
    fn the_sleeping_page_links_the_console_only_when_a_valid_url_is_set() {
        let public_console_url = |console: Option<&str>| {
            let mut values = HashMap::new();
            if let Some(console) = console {
                values.insert(PUBLIC_CONSOLE_URL_ENV.to_owned(), console.to_owned());
            }
            from_values(&values).map(|config| config.docker_runtime.public_console_url)
        };

        assert_eq!(public_console_url(None).expect("unset"), None);
        assert_eq!(public_console_url(Some("  ")).expect("empty"), None);
        for console in ["https://app.sakala.dev", "https://example.ac.id/console/"] {
            assert_eq!(
                public_console_url(Some(console)).expect(console).as_deref(),
                Some(console)
            );
        }
        for invalid in [
            "app.sakala.dev",
            "javascript:alert(1)",
            "https://app.sakala.dev/?next=/",
            "https://app.sakala.dev/it's",
        ] {
            let error = public_console_url(Some(invalid))
                .expect_err(invalid)
                .to_string();
            assert!(error.contains(PUBLIC_CONSOLE_URL_ENV), "{invalid}: {error}");
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

    fn database_values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut values = HashMap::from([
            (DATABASE_DRIVER_ENV.to_owned(), "postgres".to_owned()),
            (
                DATABASE_ADMIN_DSN_ENV.to_owned(),
                "host=/var/run/postgresql user=postgres password=admin-secret-value".to_owned(),
            ),
            (DATABASE_PUBLIC_HOST_ENV.to_owned(), "10.0.47.10".to_owned()),
        ]);
        for (key, value) in pairs {
            values.insert((*key).to_owned(), (*value).to_owned());
        }
        values
    }

    #[test]
    fn the_database_role_is_off_by_default_and_none_means_off() {
        for values in [
            HashMap::new(),
            HashMap::from([(DATABASE_DRIVER_ENV.to_owned(), "none".to_owned())]),
            HashMap::from([(DATABASE_DRIVER_ENV.to_owned(), String::new())]),
        ] {
            let config = from_values(&values).expect("config");
            assert!(config.database.is_none());
            assert!(
                !config
                    .agent
                    .capabilities
                    .contains(&MANAGED_POSTGRES_CAPABILITY.to_owned())
            );
        }
        assert!(
            from_values(&HashMap::from([(
                DATABASE_DRIVER_ENV.to_owned(),
                "mysql".to_owned()
            )]))
            .is_err()
        );
    }

    #[test]
    fn a_database_host_advertises_managed_postgres_next_to_its_runtime() {
        let noop = from_values(&database_values(&[])).expect("config");
        assert_eq!(
            noop.agent.capabilities,
            ["noop-runtime", MANAGED_POSTGRES_CAPABILITY]
        );
        let database = noop.database.expect("database role");
        assert_eq!(
            database.endpoint,
            DatabaseEndpoint::new("10.0.47.10", 5432).expect("endpoint")
        );

        let docker =
            from_values(&database_values(&[("SAKALA_RUNTIME_DRIVER", "docker")])).expect("config");
        assert!(
            docker
                .agent
                .capabilities
                .contains(&"docker-runtime".to_owned())
        );
        assert_eq!(
            docker.agent.capabilities.last().map(String::as_str),
            Some(MANAGED_POSTGRES_CAPABILITY)
        );

        let port =
            from_values(&database_values(&[(DATABASE_PUBLIC_PORT_ENV, "6432")])).expect("config");
        assert_eq!(port.database.expect("database").endpoint.port, 6432);
    }

    #[test]
    fn a_database_host_refuses_to_start_on_missing_or_invalid_values() {
        for (key, value) in [
            (DATABASE_ADMIN_DSN_ENV, ""),
            (DATABASE_PUBLIC_HOST_ENV, " "),
            (DATABASE_PUBLIC_HOST_ENV, "db 01"),
            (DATABASE_PUBLIC_PORT_ENV, "0"),
            (DATABASE_PUBLIC_PORT_ENV, "65536"),
            (DATABASE_PUBLIC_PORT_ENV, "postgres"),
            (
                DATABASE_ADMIN_DSN_ENV,
                "postgresql://postgres:admin-secret-value@10.0.47.10/postgres",
            ),
        ] {
            let error = from_values(&database_values(&[(key, value)]))
                .expect_err("invalid database host config")
                .to_string();
            assert!(error.contains(key), "{key}={value:?}: {error}");
            assert!(!error.contains("admin-secret-value"), "{error}");
        }
    }

    #[test]
    fn the_admin_dsn_never_reaches_debug_output() {
        let config = from_values(&database_values(&[])).expect("config");
        let printed = format!("{config:?}");
        assert!(!printed.contains("admin-secret-value"), "{printed}");
    }

    fn object_storage_values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut values = HashMap::from([
            (OBJECT_STORAGE_DRIVER_ENV.to_owned(), "rustfs".to_owned()),
            (
                OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV.to_owned(),
                "https://s3.sakala.app".to_owned(),
            ),
            (
                OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV.to_owned(),
                "5120".to_owned(),
            ),
        ]);
        for (key, value) in pairs {
            values.insert((*key).to_owned(), (*value).to_owned());
        }
        values
    }

    #[test]
    fn the_object_storage_role_is_off_by_default_and_none_means_off() {
        for values in [
            HashMap::new(),
            HashMap::from([(OBJECT_STORAGE_DRIVER_ENV.to_owned(), "none".to_owned())]),
            HashMap::from([(OBJECT_STORAGE_DRIVER_ENV.to_owned(), String::new())]),
        ] {
            let config = from_values(&values).expect("config");
            assert!(config.object_storage.is_none());
            assert!(
                !config
                    .agent
                    .capabilities
                    .contains(&MANAGED_S3_CAPABILITY.to_owned())
            );
        }
        assert!(
            from_values(&HashMap::from([(
                OBJECT_STORAGE_DRIVER_ENV.to_owned(),
                "minio".to_owned()
            )]))
            .is_err()
        );
    }

    #[test]
    fn an_object_storage_host_advertises_managed_s3_next_to_its_runtime() {
        let noop = from_values(&object_storage_values(&[])).expect("config");
        assert_eq!(
            noop.agent.capabilities,
            ["noop-runtime", MANAGED_S3_CAPABILITY]
        );
        let object_storage = noop.object_storage.expect("object storage role");
        assert_eq!(
            object_storage.identity,
            ObjectStorageIdentity {
                endpoint: "https://s3.sakala.app".to_owned(),
                region: Region::parse("us-east-1").expect("region"),
                capacity_mb: 5_120,
            }
        );
        assert_eq!(
            object_storage.admin.container,
            "sakala-object-storage-rustfs-1"
        );
        assert_eq!(
            object_storage.admin.data_dir,
            PathBuf::from("/var/lib/sakala/object-storage")
        );
        assert_eq!(object_storage.admin.total_quota_mb, 5_120);
        assert_eq!(object_storage.admin.min_free_mb, 1_024);
        assert_eq!(
            object_storage.admin.command_timeout,
            noop.agent.command_timeout()
        );

        let docker = from_values(&object_storage_values(&[
            ("SAKALA_RUNTIME_DRIVER", "docker"),
            (DATABASE_DRIVER_ENV, "postgres"),
            (
                DATABASE_ADMIN_DSN_ENV,
                "host=/var/run/postgresql user=postgres",
            ),
            (DATABASE_PUBLIC_HOST_ENV, "10.0.47.10"),
        ]))
        .expect("config");
        assert!(
            docker
                .agent
                .capabilities
                .contains(&"docker-runtime".to_owned())
        );
        assert_eq!(
            &docker.agent.capabilities[docker.agent.capabilities.len() - 2..],
            [MANAGED_POSTGRES_CAPABILITY, MANAGED_S3_CAPABILITY]
        );

        let custom = from_values(&object_storage_values(&[
            (OBJECT_STORAGE_CONTAINER_ENV, "rustfs"),
            (OBJECT_STORAGE_REGION_ENV, "id-jog-1"),
            (OBJECT_STORAGE_MIN_FREE_MB_ENV, "0"),
            (OBJECT_STORAGE_DATA_DIR_ENV, "/srv/rustfs"),
            (OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV, "http://127.0.0.1:9000"),
        ]))
        .expect("config")
        .object_storage
        .expect("object storage role");
        assert_eq!(custom.admin.container, "rustfs");
        assert_eq!(custom.identity.region.as_str(), "id-jog-1");
        assert_eq!(custom.identity.endpoint, "http://127.0.0.1:9000");
        assert_eq!(custom.admin.min_free_mb, 0);
        assert_eq!(custom.admin.data_dir, PathBuf::from("/srv/rustfs"));
    }

    #[test]
    fn an_object_storage_host_refuses_to_start_on_missing_or_invalid_values() {
        for (key, value) in [
            (OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV, ""),
            (OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV, "http://s3.sakala.app"),
            (
                OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV,
                "https://s3.sakala.app/bucket",
            ),
            (
                OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV,
                "https://s3.sakala.app?x=1",
            ),
            (
                OBJECT_STORAGE_PUBLIC_ENDPOINT_ENV,
                "https://key:secret@s3.sakala.app",
            ),
            (OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV, ""),
            (OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV, "0"),
            (OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV, "-5"),
            (OBJECT_STORAGE_TOTAL_QUOTA_MB_ENV, "5 GB"),
            (OBJECT_STORAGE_MIN_FREE_MB_ENV, "-1"),
            (OBJECT_STORAGE_CONTAINER_ENV, "-e"),
            (OBJECT_STORAGE_CONTAINER_ENV, "rustfs;id"),
            (OBJECT_STORAGE_REGION_ENV, "US-East-1"),
            (OBJECT_STORAGE_DATA_DIR_ENV, "data"),
        ] {
            let error = from_values(&object_storage_values(&[(key, value)]))
                .expect_err("invalid object storage host config")
                .to_string();
            assert!(error.contains(key), "{key}={value:?}: {error}");
            assert!(!error.contains("secret"), "{error}");
        }
    }
}
