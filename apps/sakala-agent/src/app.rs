use std::sync::{Arc, RwLock};

use anyhow::Context;
use sakala_agent_core::{
    AgentMode, NodeLifecycle,
    api::ApiClient,
    heartbeat,
    ports::{RuntimeExecutor, RuntimePreflightReport, RuntimeReconciliationReport},
    reporting::ApiRuntimeReporterFactory,
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
    let workspace_root = config.docker_runtime.workspace_root.clone();
    let minimum_workspace_free_bytes = config.docker_runtime.min_workspace_free_bytes;
    let reconciliation = Arc::new(RwLock::new(RuntimeReconciliationReport::default()));
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
    let reporter_factory = client.clone().map(|client| {
        Arc::new(ApiRuntimeReporterFactory::new(client))
            as Arc<dyn sakala_agent_core::ports::RuntimeReporterFactory>
    });
    match runtime.recover(reporter_factory).await {
        Ok(report) => {
            if let Ok(mut snapshot) = reconciliation.write() {
                *snapshot = report.clone();
            }
            info!(
                inspected_containers = report.inspected_containers,
                discovered_workloads = report.workloads.len(),
                orphaned_containers = report.orphans.len(),
                cleaned_workspaces = report.cleaned_workspaces,
                reattached_log_followers = report.reattached_log_followers,
                recovered_execution_records = report.recovered_execution_records,
                compatibility_issues = report.compatibility_issues.len(),
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
    match runtime.health_snapshot().await {
        Ok(snapshots) => {
            let unhealthy = snapshots.iter().filter(|snapshot| !snapshot.ready).count();
            info!(
                inspected_workloads = snapshots.len(),
                unhealthy_workloads = unhealthy,
                "startup workload health verification completed"
            );
            for snapshot in snapshots.into_iter().filter(|snapshot| !snapshot.ready) {
                warn!(
                    container_id = %snapshot.workload.container_id,
                    project_id = %snapshot.workload.project_id,
                    deployment_id = %snapshot.workload.deployment_id,
                    status = %snapshot.workload.status,
                    reason = ?snapshot.reason,
                    "recovered workload is not ready"
                );
            }
        }
        Err(error) => warn!(%error, "startup workload health verification failed"),
    }
    let desired_lifecycle = match &client {
        Some(client) => client
            .node_lifecycle()
            .await
            .context("failed to restore authoritative node lifecycle state")?
            .desired_state
            .into(),
        None => sakala_agent_core::NodeLifecycleState::Active,
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let node_lifecycle = Arc::new(NodeLifecycle::with_state(desired_lifecycle));
    let scheduler_metrics = Arc::new(scheduler::metrics::SchedulerMetrics::default());

    let runtime_health_task = tokio::spawn(sakala_agent_core::health::worker::run(
        Arc::clone(&runtime),
        config.agent.agent_id.clone(),
        config.runtime_health_interval,
        shutdown_rx.clone(),
    ));
    let heartbeat_task = tokio::spawn(heartbeat::worker::run(
        config.agent.clone(),
        client.clone(),
        heartbeat::worker::HeartbeatRuntimeContext {
            node_lifecycle: Arc::clone(&node_lifecycle),
            runtime_driver: config.runtime_driver.to_string(),
            workspace_root,
            runtime: Arc::clone(&runtime),
            scheduler_metrics: Arc::clone(&scheduler_metrics),
            reconciliation: Arc::clone(&reconciliation),
            minimum_workspace_free_bytes,
        },
        shutdown_rx.clone(),
    ));
    let poller_task = tokio::spawn(scheduler::poller::run(
        config.agent,
        client,
        Arc::clone(&runtime),
        node_lifecycle,
        scheduler_metrics,
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
