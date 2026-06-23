use std::{collections::HashMap, env, fmt, str::FromStr};

use clap::Parser;
use sakala_agent_core::{AgentConfig, CoreError};
use sakala_agent_runtime::{DockerRuntimeConfig, ResourceSafetyConfig};

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
    pub log_level: String,
}

#[derive(Debug, Parser)]
#[command(name = "sakala-agent", about = "Sakala runtime-node executor")]
struct Cli {
    #[arg(long, env = "SAKALA_AGENT_MODE")]
    mode: Option<String>,

    #[arg(long, env = "SAKALA_AGENT_ID")]
    agent_id: Option<String>,

    #[arg(long, env = "SAKALA_AGENT_TOKEN", hide_env_values = true)]
    agent_token: Option<String>,

    #[arg(long, env = "SAKALA_API_URL")]
    api_url: Option<String>,

    #[arg(long, env = "SAKALA_POLL_INTERVAL_SECONDS")]
    poll_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_HEARTBEAT_INTERVAL_SECONDS")]
    heartbeat_interval_seconds: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_NETWORK")]
    runtime_network: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_DRIVER")]
    runtime_driver: Option<String>,

    #[arg(long, env = "SAKALA_RUNTIME_WORKSPACE")]
    runtime_workspace: Option<String>,

    #[arg(long, env = "SAKALA_CADDY_SITES_DIR")]
    caddy_sites_dir: Option<String>,

    #[arg(long, env = "SAKALA_CADDY_CONTAINER")]
    caddy_container: Option<String>,

    #[arg(long, env = "SAKALA_RAILPACK_FRONTEND")]
    railpack_frontend: Option<String>,

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
    insert(&mut values, "SAKALA_RUNTIME_NETWORK", cli.runtime_network);
    insert(&mut values, "SAKALA_RUNTIME_DRIVER", cli.runtime_driver);
    insert(
        &mut values,
        "SAKALA_RUNTIME_WORKSPACE",
        cli.runtime_workspace,
    );
    insert(&mut values, "SAKALA_CADDY_SITES_DIR", cli.caddy_sites_dir);
    insert(&mut values, "SAKALA_CADDY_CONTAINER", cli.caddy_container);
    insert(
        &mut values,
        "SAKALA_RAILPACK_FRONTEND",
        cli.railpack_frontend,
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

    Ok(AppConfig {
        docker_runtime: DockerRuntimeConfig {
            workspace_root: get(values, "SAKALA_RUNTIME_WORKSPACE", "/var/lib/sakala/builds")
                .into(),
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
            resource_safety,
            ..DockerRuntimeConfig::default()
        },
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
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

fn get(values: &HashMap<String, String>, key: &str, default: &str) -> String {
    values
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_owned())
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

fn insert(values: &mut HashMap<String, String>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        values.insert(key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_noop_runtime() {
        let config = from_values(&HashMap::new()).expect("default app config should load");

        assert_eq!(config.runtime_driver, RuntimeDriver::Noop);
        assert_eq!(config.agent.capabilities, ["noop-runtime"]);
        assert_eq!(config.docker_runtime.runtime_network, "sakala-runtime");
        assert_eq!(config.docker_runtime.resource_safety.default_memory_mb, 256);
        assert_eq!(config.docker_runtime.resource_safety.max_memory_mb, 512);
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
}
