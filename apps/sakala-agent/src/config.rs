use std::{collections::HashMap, env};

use clap::Parser;
use sakala_agent_core::AgentConfig;

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

    #[arg(long, env = "SAKALA_LOG_LEVEL")]
    log_level: Option<String>,
}

pub fn load() -> Result<AgentConfig, sakala_agent_core::CoreError> {
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
    insert(&mut values, "SAKALA_LOG_LEVEL", cli.log_level);

    AgentConfig::from_values(&values)
}

fn insert(values: &mut HashMap<String, String>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        values.insert(key.to_owned(), value);
    }
}
