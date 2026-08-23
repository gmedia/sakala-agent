use std::{collections::HashMap, env, fmt, str::FromStr, time::Duration};

use crate::CoreError;

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
    pub runtime_network: String,
    pub capabilities: Vec<String>,
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
            runtime_network: get(values, "SAKALA_RUNTIME_NETWORK", "sakala-runtime"),
            capabilities: vec!["noop-runtime".to_owned()],
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
