use std::sync::Arc;

use anyhow::Context;
use sakala_agent_core::{AgentMode, api::ApiClient, heartbeat, ports::RuntimeExecutor, scheduler};
use sakala_agent_runtime::{DockerRuntimeExecutor, NoopRuntimeExecutor};
use tokio::sync::watch;
use tracing::info;

use crate::config::{AppConfig, RuntimeDriver};

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    info!(
        agent_id = %config.agent.agent_id,
        mode = %config.agent.mode,
        api_url = %config.agent.api_url,
        runtime_network = %config.agent.runtime_network,
        runtime_driver = %config.runtime_driver,
        "starting Sakala agent"
    );

    let client = match config.agent.mode {
        AgentMode::Local => None,
        AgentMode::Connected => Some(ApiClient::from_config(&config.agent)?),
    };
    let runtime: Arc<dyn RuntimeExecutor> = match config.runtime_driver {
        RuntimeDriver::Noop => Arc::new(NoopRuntimeExecutor),
        RuntimeDriver::Docker => Arc::new(DockerRuntimeExecutor::new(config.docker_runtime)),
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let heartbeat_task = tokio::spawn(heartbeat::worker::run(
        config.agent.clone(),
        client.clone(),
        shutdown_rx.clone(),
    ));
    let poller_task = tokio::spawn(scheduler::poller::run(
        config.agent,
        client,
        runtime,
        shutdown_rx,
    ));

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
