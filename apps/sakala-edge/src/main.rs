use std::sync::Arc;

use anyhow::Context;
use sakala_agent_runtime::TokioProcessRunner;
use sakala_edge::{
    caddy::{COMMAND_TIMEOUT, CaddyControl},
    config,
    files::EdgeFiles,
    source::RouteSource,
    sync::EdgeSync,
};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::load()?;
    init_telemetry(&config.log_level)?;

    let source = RouteSource::new(&config.api_url, config.token.clone())
        .context("failed to build the HTTP client")?;
    info!(
        url = %source.url(),
        state_dir = %config.state_dir.display(),
        custom_domains_file = %config.custom_domains_file.display(),
        custom_domain_forbidden_zones = %config.policy.forbidden_zones,
        domain_suffixes = %config.policy.domain_suffixes,
        upstream_networks = %config.policy.upstreams,
        poll_interval_seconds = config.timing.poll_interval.as_secs(),
        "starting sakala-edge"
    );
    let caddy = CaddyControl::new(
        Arc::new(TokioProcessRunner::new(COMMAND_TIMEOUT)),
        config.validate_command,
        config.reload_command,
    );
    let sync = EdgeSync::new(
        source,
        config.policy,
        caddy,
        EdgeFiles::new(config.state_dir).with_custom_domains_path(config.custom_domains_file),
        config.timing,
    );

    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(sync.run(shutdown.clone()));
    wait_for_shutdown_signal().await?;
    info!("shutdown signal received");
    shutdown.cancel();
    worker.await.context("route sync task failed")?;
    Ok(())
}

/// systemd stops the service with SIGTERM; a terminal sends SIGINT.
async fn wait_for_shutdown_signal() -> anyhow::Result<()> {
    let mut terminate = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("failed to listen for Ctrl+C"),
        _ = terminate.recv() => Ok(()),
    }
}

fn init_telemetry(log_level: &str) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(log_level)
        .with_context(|| format!("invalid {} filter: {log_level}", config::LOG_LEVEL))?;
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing subscriber: {error}"))
}
