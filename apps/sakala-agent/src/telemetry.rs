use anyhow::Context;
use tracing_subscriber::EnvFilter;

pub fn init(log_level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(log_level)
        .with_context(|| format!("invalid SAKALA_LOG_LEVEL filter: {log_level}"))?;

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing subscriber: {error}"))
}
