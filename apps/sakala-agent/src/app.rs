use std::sync::Arc;

use anyhow::Context;
use sakala_agent_core::{AgentConfig, AgentMode, dashboard::DashboardClient, heartbeat, scheduler};
use sakala_agent_runtime::{NoopRuntimeExecutor, RuntimeExecutor};
use tokio::sync::watch;
use tracing::info;

pub async fn run(config: AgentConfig) -> anyhow::Result<()> {
    info!(
        agent_id = %config.agent_id,
        mode = %config.mode,
        dashboard_url = %config.dashboard_url,
        runtime_network = %config.runtime_network,
        "starting Sakala agent with noop runtime executor"
    );

    let client = match config.mode {
        AgentMode::Local => None,
        AgentMode::Connected => Some(DashboardClient::from_config(&config)?),
    };
    let runtime: Arc<dyn RuntimeExecutor> = Arc::new(NoopRuntimeExecutor);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let heartbeat_task = tokio::spawn(heartbeat::worker::run(
        config.clone(),
        client.clone(),
        shutdown_rx.clone(),
    ));
    let poller_task = tokio::spawn(scheduler::poller::run(config, client, runtime, shutdown_rx));

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl+C")?;
    info!("shutdown signal received");
    shutdown_tx
        .send(true)
        .context("failed to notify agent workers of shutdown")?;

    heartbeat_task
        .await
        .context("heartbeat worker task failed")?;
    poller_task.await.context("command poller task failed")?;
    info!("Sakala agent shutdown complete");

    Ok(())
}
