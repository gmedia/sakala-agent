use std::{collections::HashMap, env, fmt, str::FromStr, time::Duration};

use sakala_agent_protocol::{EdgeAddress, NodePlacement, Region};

use crate::CoreError;

/// Where the node is, reported as `metadata.runtime.region`.
pub const REGION_ENV: &str = "SAKALA_AGENT_REGION";

/// Where Sakala Edge reaches the node's runtime router, reported as
/// `metadata.runtime.edge_address`.
pub const EDGE_ADDRESS_ENV: &str = "SAKALA_AGENT_EDGE_ADDRESS";

/// How many `RunJob` commands the node runs at once, in a pool of their own.
pub const MAX_CONCURRENT_JOBS_ENV: &str = "SAKALA_MAX_CONCURRENT_JOBS";

/// Default of [`MAX_CONCURRENT_JOBS_ENV`] (ADR-022).
pub const DEFAULT_MAX_CONCURRENT_JOBS: usize = 2;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentMode {
    #[default]
    Local,
    Connected,
}

impl fmt::Display for AgentMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => formatter.write_str("local"),
            Self::Connected => formatter.write_str("connected"),
        }
    }
}

impl FromStr for AgentMode {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "connected" => Ok(Self::Connected),
            _ => Err(CoreError::InvalidConfiguration(format!(
                "SAKALA_AGENT_MODE must be local or connected, received {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentConfig {
    pub mode: AgentMode,
    pub agent_id: String,
    pub agent_token: Option<String>,
    pub api_url: String,
    pub poll_interval_seconds: u64,
    pub heartbeat_interval_seconds: u64,
    pub command_timeout_seconds: u64,
    pub max_concurrent_commands: usize,
    /// Slots of `RunJob`, apart from `max_concurrent_commands`, so a long job
    /// never holds back a deploy.
    pub max_concurrent_jobs: usize,
    pub shutdown_grace_seconds: u64,
    pub runtime_network: String,
    pub capabilities: Vec<String>,
    /// `SAKALA_AGENT_REGION` and `SAKALA_AGENT_EDGE_ADDRESS`, already checked
    /// against the control plane's rules. Unset values stay `None`.
    pub placement: NodePlacement,
}

impl AgentConfig {
    pub fn from_env() -> Result<Self, CoreError> {
        let values = env::vars().collect::<HashMap<_, _>>();
        Self::from_values(&values)
    }

    pub fn from_values(values: &HashMap<String, String>) -> Result<Self, CoreError> {
        let mode = get(values, "SAKALA_AGENT_MODE", "local").parse()?;
        let agent_token = values
            .get("SAKALA_AGENT_TOKEN")
            .filter(|value| !value.trim().is_empty())
            .cloned();

        if mode == AgentMode::Connected
            && agent_token
                .as_deref()
                .is_none_or(|token| token == "change-me")
        {
            return Err(CoreError::InvalidConfiguration(
                "SAKALA_AGENT_TOKEN must contain a non-placeholder token in connected mode"
                    .to_owned(),
            ));
        }

        Ok(Self {
            mode,
            agent_id: get(values, "SAKALA_AGENT_ID", "local-agent-01"),
            agent_token,
            api_url: get(values, "SAKALA_API_URL", "http://localhost:8000"),
            poll_interval_seconds: positive_number(values, "SAKALA_POLL_INTERVAL_SECONDS", 3)?,
            heartbeat_interval_seconds: positive_number(
                values,
                "SAKALA_HEARTBEAT_INTERVAL_SECONDS",
                10,
            )?,
            command_timeout_seconds: positive_number(
                values,
                "SAKALA_COMMAND_TIMEOUT_SECONDS",
                900,
            )?,
            max_concurrent_commands: positive_usize(values, "SAKALA_MAX_CONCURRENT_COMMANDS", 4)?,
            max_concurrent_jobs: positive_usize(
                values,
                MAX_CONCURRENT_JOBS_ENV,
                DEFAULT_MAX_CONCURRENT_JOBS,
            )?,
            shutdown_grace_seconds: positive_number(values, "SAKALA_SHUTDOWN_GRACE_SECONDS", 30)?,
            runtime_network: get(values, "SAKALA_RUNTIME_NETWORK", "sakala-runtime"),
            capabilities: vec!["noop-runtime".to_owned()],
            placement: placement(values)?,
        })
    }

    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_seconds)
    }

    #[must_use]
    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_seconds)
    }

    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.command_timeout_seconds)
    }

    #[must_use]
    pub fn shutdown_grace(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_seconds)
    }
}

/// Reads the optional placement values. An unset or empty variable means "not
/// configured". Anything else must pass the API's own rule, because the API
/// rejects the whole heartbeat over one malformed value: the agent refuses to
/// start rather than send heartbeats that never land.
fn placement(values: &HashMap<String, String>) -> Result<NodePlacement, CoreError> {
    Ok(NodePlacement {
        region: optional(values, REGION_ENV, Region::parse)?,
        edge_address: optional(values, EDGE_ADDRESS_ENV, EdgeAddress::parse)?,
    })
}

fn optional<T, E: fmt::Display>(
    values: &HashMap<String, String>,
    key: &str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<Option<T>, CoreError> {
    let Some(value) = values.get(key).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    parse(value).map(Some).map_err(|reason| {
        CoreError::InvalidConfiguration(format!("{key} {reason}, received {value:?}"))
    })
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

fn get(values: &HashMap<String, String>, key: &str, default: &str) -> String {
    values
        .get(key)
        .cloned()
        .unwrap_or_else(|| default.to_owned())
}

fn positive_number(
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
