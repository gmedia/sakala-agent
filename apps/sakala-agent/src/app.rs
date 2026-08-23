use std::sync::Arc;

use anyhow::Context;
use sakala_agent_core::{
    AgentMode,
    api::ApiClient,
    heartbeat,
    ports::{RuntimeExecutor, RuntimePreflightReport},
    scheduler,
};
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
    let preflight = runtime
        .preflight()
        .await
        .context("runtime preflight failed")?;
    log_preflight(&preflight);
    if preflight.has_fatal_failure() {
        anyhow::bail!("runtime preflight found one or more fatal dependency failures");
    }
    match runtime.reconcile().await {
        Ok(report) => {
            info!(
                inspected_containers = report.inspected_containers,
                discovered_workloads = report.workloads.len(),
                orphaned_containers = report.orphans.len(),
                cleaned_workspaces = report.cleaned_workspaces,
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

    let runtime_health_task = tokio::spawn(sakala_agent_core::health::worker::run(
        Arc::clone(&runtime),
        config.agent.agent_id.clone(),
        config.runtime_health_interval,
        shutdown_rx.clone(),
    ));
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
    runtime_health_task
        .await
        .context("runtime health worker task failed")?;
    runtime
        .shutdown()
        .await
        .context("failed to shut down runtime background tasks")?;
    info!("Sakala agent shutdown complete");

    Ok(())
}

fn log_preflight(report: &RuntimePreflightReport) {
    for check in &report.checks {
        if check.ready {
            info!(check = %check.name, detail = %check.detail, "runtime preflight check passed");
        } else if check.fatal {
            warn!(check = %check.name, detail = %check.detail, "runtime preflight check failed");
        } else {
            warn!(check = %check.name, detail = %check.detail, "runtime preflight warning");
        }
    }
}
