use std::sync::Arc;

use anyhow::Context;
use sakala_agent_core::{AgentMode, api::ApiClient, heartbeat, ports::RuntimeExecutor, scheduler};
use sakala_agent_runtime::{DockerRuntimeExecutor, NoopRuntimeExecutor};
use tokio::sync::watch;
use tracing::{info, warn};

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
    match runtime.reconcile().await {
        Ok(report) => {
            info!(
                inspected_containers = report.inspected_containers,
                orphaned_containers = report.orphans.len(),
                "runtime reconciliation scan completed"
            );
            for orphan in report.orphans {
                warn!(
                    container_id = %orphan.container_id,
                    project_id = ?orphan.project_id,
                    reason = %orphan.reason,
                    "orphaned Sakala container detected; automatic deletion is disabled"
                );
            }
        }
        Err(error) => warn!(%error, "runtime reconciliation scan failed"),
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let heartbeat_task = tokio::spawn(heartbeat::worker::run(
        config.agent.clone(),
        client.clone(),
        shutdown_rx.clone(),
    ));
    let poller_task = tokio::spawn(scheduler::poller::run(
        config.agent,
        client,
        Arc::clone(&runtime),
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
    runtime
        .shutdown()
        .await
        .context("failed to shut down runtime background tasks")?;
    info!("Sakala agent shutdown complete");

    Ok(())
}
