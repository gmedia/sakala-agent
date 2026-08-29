use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus, PROTOCOL_VERSION};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{
    AgentConfig, NodeLifecycle, NodeLifecycleState,
    api::ApiClient,
    ports::{NodeTelemetry, RuntimeExecutor, RuntimeReconciliationReport},
    scheduler::metrics::SchedulerMetrics,
};

const MAX_HEARTBEAT_DETAIL_ITEMS: usize = 50;

pub struct HeartbeatRuntimeContext {
    pub node_lifecycle: Arc<NodeLifecycle>,
    pub runtime_driver: String,
    pub runtime: Arc<dyn RuntimeExecutor>,
    pub scheduler_metrics: Arc<SchedulerMetrics>,
    pub reconciliation: Arc<RwLock<RuntimeReconciliationReport>>,
    pub startup_reconciliation_at: OffsetDateTime,
    pub minimum_workspace_free_bytes: u64,
}

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    context: HeartbeatRuntimeContext,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let payload = payload(&config, &context).await;

        if let Some(client) = &client {
            if let Err(error) = client.heartbeat(&payload).await {
                warn!(%error, "failed to send control-plane heartbeat");
            }
        } else {
            info!(
                agent_id = %config.agent_id,
                mode = %config.mode,
                "local heartbeat tick"
            );
        }

        tokio::select! {
            () = sleep(config.heartbeat_interval()) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    info!("heartbeat worker stopped");
}

async fn payload(config: &AgentConfig, context: &HeartbeatRuntimeContext) -> HeartbeatPayload {
    let (resources, telemetry_available) = match context.runtime.node_telemetry().await {
        Ok(resources) => (resources, true),
        Err(_) => (NodeTelemetry::default(), false),
    };
    let workloads = workload_statistics(context.runtime.as_ref()).await;
    let reconciliation = context
        .reconciliation
        .read()
        .map(|report| report.clone())
        .unwrap_or_default();
    HeartbeatPayload {
        status: match context.node_lifecycle.state() {
            NodeLifecycleState::Active
                if !telemetry_available
                    || resources.runtime_operational == Some(false)
                    || !workloads.capacity_available
                    || !workloads.health_available
                    || disk_pressure_state(
                        resources.disk_available_bytes,
                        context.minimum_workspace_free_bytes,
                    ) == "critical" =>
            {
                NodeStatus::Degraded
            }
            NodeLifecycleState::Active => NodeStatus::Ready,
            NodeLifecycleState::Draining => NodeStatus::Draining,
            NodeLifecycleState::Drained => NodeStatus::Drained,
            NodeLifecycleState::Maintenance => NodeStatus::Maintenance,
        },
        node: NodeInfo {
            hostname: resources
                .hostname
                .clone()
                .unwrap_or_else(|| "unknown-host".to_owned()),
            runtime_network: config.runtime_network.clone(),
            capabilities: config.capabilities.clone(),
        },
        metadata: json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
            "lifecycle_state": format!("{:?}", context.node_lifecycle.state()).to_ascii_lowercase(),
            "runtime_driver": context.runtime_driver,
            "uptime_seconds": resources.uptime_seconds,
            "resources": {
                "cpu_total": resources.cpu_total,
                "cpu_load_1m": resources.cpu_load_1m,
                "memory_total_bytes": resources.memory_total_bytes,
                "memory_available_bytes": resources.memory_available_bytes,
                "disk_total_bytes": resources.disk_total_bytes,
                "disk_available_bytes": resources.disk_available_bytes,
                "workspace_used_bytes": resources.workspace_used_bytes,
            },
            "disk_pressure": {
                "state": disk_pressure_state(resources.disk_available_bytes, context.minimum_workspace_free_bytes),
                "minimum_workspace_free_bytes": context.minimum_workspace_free_bytes,
                "available_workspace_bytes": resources.disk_available_bytes,
            },
            "workloads": {
                "active": workloads.active,
                "starting": workloads.starting,
                "unhealthy": workloads.unhealthy,
                "stopped": workloads.stopped,
                "unhealthy_details": bounded_values(&workloads.unhealthy_details, |snapshot| json!({
                    "container_id": snapshot.workload.container_id,
                    "project_id": snapshot.workload.project_id,
                    "deployment_id": snapshot.workload.deployment_id,
                    "status": snapshot.workload.status,
                    "reason": snapshot.reason,
                })),
            },
            "execution": {
                "active_commands": context.scheduler_metrics.active_commands(),
                "queued_local_commands": context.scheduler_metrics.queued_local_commands(),
                "capacity_waiting_commands": context.scheduler_metrics.capacity_waiting_commands(),
                "active_builds": workloads.active_builds,
                "maximum_concurrent_builds": workloads.maximum_concurrent_builds,
            },
            "startup_reconciliation": {
                "captured_at": context.startup_reconciliation_at,
                "inspected_containers": reconciliation.inspected_containers,
                "cleaned_workspaces": reconciliation.cleaned_workspaces,
                "reattached_log_followers": reconciliation.reattached_log_followers,
                "recovered_execution_records": reconciliation.recovered_execution_records,
                "recovered_workloads": bounded_values(&reconciliation.workloads, |workload| json!({
                    "container_id": workload.container_id,
                    "project_id": workload.project_id,
                    "deployment_id": workload.deployment_id,
                    "status": workload.status,
                })),
                "orphans": bounded_values(&reconciliation.orphans, |orphan| json!({
                    "container_id": orphan.container_id,
                    "project_id": orphan.project_id,
                    "reason": orphan.reason,
                })),
                "stale_routes": bounded_values(&reconciliation.stale_routes, |route| json!({
                    "path": route.path,
                    "project_id": route.project_id,
                })),
                "stale_images": bounded_values(&reconciliation.stale_images, |image| json!({
                    "image_id": image.image_id,
                    "project_id": image.project_id,
                    "deployment_id": image.deployment_id,
                })),
                "compatibility_issues": bounded_values(&reconciliation.compatibility_issues, |issue| json!({
                    "container_id": issue.container_id,
                    "project_id": issue.project_id,
                    "deployment_id": issue.deployment_id,
                    "reason": issue.reason,
                })),
            },
            "detail_counts": {
                "unhealthy_details": workloads.unhealthy_details.len(),
                "recovered_workloads": reconciliation.workloads.len(),
                "orphans": reconciliation.orphans.len(),
                "stale_routes": reconciliation.stale_routes.len(),
                "stale_images": reconciliation.stale_images.len(),
                "compatibility_issues": reconciliation.compatibility_issues.len(),
            },
            "runtime_dependencies": resources.runtime_dependencies,
        }),
        sent_at: OffsetDateTime::now_utc(),
    }
}

fn bounded_values<T>(items: &[T], mapper: impl Fn(&T) -> Value) -> Vec<Value> {
    items
        .iter()
        .take(MAX_HEARTBEAT_DETAIL_ITEMS)
        .map(mapper)
        .collect()
}

fn disk_pressure_state(available_bytes: Option<u64>, minimum_free_bytes: u64) -> &'static str {
    match available_bytes {
        Some(available_bytes) if available_bytes < minimum_free_bytes => "critical",
        Some(_) => "normal",
        None => "unknown",
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WorkloadStatistics {
    active: Option<usize>,
    stopped: Option<usize>,
    starting: Option<usize>,
    unhealthy: Option<usize>,
    active_builds: Option<usize>,
    maximum_concurrent_builds: Option<usize>,
    unhealthy_details: Vec<crate::ports::RuntimeHealthSnapshot>,
    capacity_available: bool,
    health_available: bool,
}

async fn workload_statistics(runtime: &dyn RuntimeExecutor) -> WorkloadStatistics {
    let capacity = tokio::time::timeout(probe_timeout(), runtime.capacity())
        .await
        .ok()
        .and_then(Result::ok);
    let capacity_available = capacity.is_some();
    let active = capacity.as_ref().and_then(|value| value.active_workloads);
    let stopped = capacity.as_ref().and_then(|value| value.stopped_workloads);
    let active_builds = capacity.as_ref().and_then(|value| value.active_builds);
    let maximum_concurrent_builds = capacity
        .as_ref()
        .and_then(|value| value.maximum_concurrent_builds);
    let Ok(Ok(snapshots)) = tokio::time::timeout(probe_timeout(), runtime.health_snapshot()).await
    else {
        return WorkloadStatistics {
            active,
            stopped,
            active_builds,
            maximum_concurrent_builds,
            capacity_available,
            ..WorkloadStatistics::default()
        };
    };
    let starting = snapshots
        .iter()
        .filter(|snapshot| {
            snapshot
                .workload
                .status
                .to_ascii_lowercase()
                .contains("starting")
        })
        .count();
    let unhealthy_details = snapshots
        .into_iter()
        .filter(|snapshot| {
            !snapshot.ready
                && !snapshot
                    .workload
                    .status
                    .to_ascii_lowercase()
                    .contains("starting")
        })
        .collect::<Vec<_>>();
    WorkloadStatistics {
        active,
        stopped,
        active_builds,
        maximum_concurrent_builds,
        starting: Some(starting),
        unhealthy: Some(unhealthy_details.len()),
        unhealthy_details,
        capacity_available,
        health_available: true,
    }
}

fn probe_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(100)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(2)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };

    use sakala_agent_protocol::{NodeStatus, PROTOCOL_VERSION};
    use time::OffsetDateTime;
    use uuid::Uuid;

    use crate::{
        AgentConfig, NodeLifecycle,
        ports::{NodeTelemetry, RuntimeExecutionError, RuntimeExecutor},
        ports::{
            RuntimeCompatibilityIssue, RuntimeHealthSnapshot, RuntimeOrphan,
            RuntimeReconciliationReport, RuntimeStaleImage, RuntimeStaleRoute, RuntimeWorkload,
        },
        scheduler::metrics::SchedulerMetrics,
    };

    use super::{HeartbeatRuntimeContext, disk_pressure_state, payload};

    struct EmptyRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for EmptyRuntime {}

    struct FailedTelemetryRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for FailedTelemetryRuntime {
        async fn node_telemetry(&self) -> Result<NodeTelemetry, RuntimeExecutionError> {
            Err(RuntimeExecutionError::new(
                "runtime_telemetry_failed",
                "telemetry unavailable",
            ))
        }
    }

    struct HealthSnapshotRuntime {
        snapshots: Vec<RuntimeHealthSnapshot>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for HealthSnapshotRuntime {
        async fn health_snapshot(
            &self,
        ) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeExecutionError> {
            Ok(self.snapshots.clone())
        }
    }

    fn heartbeat_context(
        runtime: Arc<dyn RuntimeExecutor>,
        reconciliation: RuntimeReconciliationReport,
    ) -> HeartbeatRuntimeContext {
        HeartbeatRuntimeContext {
            node_lifecycle: Arc::new(NodeLifecycle::new()),
            runtime_driver: "noop".to_owned(),
            runtime,
            scheduler_metrics: Arc::new(SchedulerMetrics::default()),
            reconciliation: Arc::new(RwLock::new(reconciliation)),
            startup_reconciliation_at: OffsetDateTime::now_utc(),
            minimum_workspace_free_bytes: 0,
        }
    }

    #[tokio::test]
    async fn heartbeat_identifies_the_wire_contract_revision() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let stale_project = Uuid::new_v4();
        let context = HeartbeatRuntimeContext {
            node_lifecycle: Arc::new(NodeLifecycle::new()),
            runtime_driver: "noop".to_owned(),
            runtime: Arc::new(EmptyRuntime),
            scheduler_metrics: Arc::new(SchedulerMetrics::default()),
            reconciliation: Arc::new(RwLock::new(RuntimeReconciliationReport {
                stale_routes: vec![RuntimeStaleRoute {
                    path: "/var/lib/sakala/caddy/stale.Caddyfile".to_owned(),
                    project_id: stale_project,
                    deployment_id: None,
                }],
                ..RuntimeReconciliationReport::default()
            })),
            startup_reconciliation_at: OffsetDateTime::now_utc(),
            minimum_workspace_free_bytes: 0,
        };
        let heartbeat = payload(&config, &context).await;

        assert_eq!(heartbeat.metadata["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(heartbeat.metadata["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["stale_routes"][0]["project_id"],
            stale_project.to_string()
        );
        let detail_counts = &heartbeat.metadata["detail_counts"];
        assert_eq!(detail_counts["unhealthy_details"].as_u64(), Some(0));
        assert_eq!(detail_counts["recovered_workloads"].as_u64(), Some(0));
        assert_eq!(detail_counts["orphans"].as_u64(), Some(0));
        assert_eq!(detail_counts["stale_routes"].as_u64(), Some(1));
        assert_eq!(detail_counts["stale_images"].as_u64(), Some(0));
        assert_eq!(detail_counts["compatibility_issues"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn heartbeat_reports_zero_counts_for_empty_detail_collections() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(
            &config,
            &heartbeat_context(
                Arc::new(EmptyRuntime),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;

        let detail_counts = &heartbeat.metadata["detail_counts"];
        for key in [
            "unhealthy_details",
            "recovered_workloads",
            "orphans",
            "stale_routes",
            "stale_images",
            "compatibility_issues",
        ] {
            assert_eq!(detail_counts[key].as_u64(), Some(0), "count for {key}");
        }
    }

    #[tokio::test]
    async fn heartbeat_preserves_detail_collections_within_bound() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let project_id = Uuid::new_v4();
        let deployment_id = Uuid::new_v4();
        let workload = RuntimeWorkload {
            container_id: "container-00".to_owned(),
            project_id,
            deployment_id,
            status: "running".to_owned(),
        };
        let reconciliation = RuntimeReconciliationReport {
            workloads: vec![workload.clone()],
            orphans: vec![RuntimeOrphan {
                container_id: "orphan-00".to_owned(),
                project_id: Some(project_id),
                reason: "unmanaged".to_owned(),
            }],
            stale_routes: vec![RuntimeStaleRoute {
                path: "/var/lib/sakala/caddy/stale-00.Caddyfile".to_owned(),
                project_id,
                deployment_id: Some(deployment_id),
            }],
            stale_images: vec![RuntimeStaleImage {
                image_id: "image-00".to_owned(),
                project_id: Some(project_id),
                deployment_id: Some(deployment_id),
            }],
            compatibility_issues: vec![RuntimeCompatibilityIssue {
                container_id: "compatibility-00".to_owned(),
                project_id,
                deployment_id,
                reason: "legacy_labels".to_owned(),
            }],
            ..RuntimeReconciliationReport::default()
        };
        let runtime = HealthSnapshotRuntime {
            snapshots: vec![RuntimeHealthSnapshot {
                workload,
                ready: false,
                reason: Some("health check failed".to_owned()),
            }],
        };
        let heartbeat = payload(
            &config,
            &heartbeat_context(Arc::new(runtime), reconciliation),
        )
        .await;

        let detail_counts = &heartbeat.metadata["detail_counts"];
        for key in [
            "unhealthy_details",
            "recovered_workloads",
            "orphans",
            "stale_routes",
            "stale_images",
            "compatibility_issues",
        ] {
            assert_eq!(detail_counts[key].as_u64(), Some(1), "count for {key}");
        }
        assert_eq!(
            heartbeat.metadata["workloads"]["unhealthy_details"]
                .as_array()
                .expect("unhealthy details should be an array")
                .len(),
            1
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["recovered_workloads"]
                .as_array()
                .expect("recovered workloads should be an array")
                .len(),
            1
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["orphans"]
                .as_array()
                .expect("orphans should be an array")
                .len(),
            1
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["stale_routes"]
                .as_array()
                .expect("stale routes should be an array")
                .len(),
            1
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["stale_images"]
                .as_array()
                .expect("stale images should be an array")
                .len(),
            1
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["compatibility_issues"]
                .as_array()
                .expect("compatibility issues should be an array")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn heartbeat_bounds_detail_collections_and_reports_original_counts() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let project_id = Uuid::new_v4();
        let deployment_id = Uuid::new_v4();
        let workloads: Vec<_> = (0..51)
            .map(|index| RuntimeWorkload {
                container_id: format!("container-{index:02}"),
                project_id,
                deployment_id,
                status: "running".to_owned(),
            })
            .collect();
        let snapshots = workloads
            .iter()
            .cloned()
            .map(|workload| RuntimeHealthSnapshot {
                workload,
                ready: false,
                reason: Some("health check failed".to_owned()),
            })
            .collect();
        let reconciliation = RuntimeReconciliationReport {
            workloads,
            orphans: (0..51)
                .map(|index| RuntimeOrphan {
                    container_id: format!("orphan-{index:02}"),
                    project_id: Some(project_id),
                    reason: "unmanaged".to_owned(),
                })
                .collect(),
            stale_routes: (0..51)
                .map(|index| RuntimeStaleRoute {
                    path: format!("/var/lib/sakala/caddy/stale-{index:02}.Caddyfile"),
                    project_id,
                    deployment_id: Some(deployment_id),
                })
                .collect(),
            stale_images: (0..51)
                .map(|index| RuntimeStaleImage {
                    image_id: format!("image-{index:02}"),
                    project_id: Some(project_id),
                    deployment_id: Some(deployment_id),
                })
                .collect(),
            compatibility_issues: (0..51)
                .map(|index| RuntimeCompatibilityIssue {
                    container_id: format!("compatibility-{index:02}"),
                    project_id,
                    deployment_id,
                    reason: "legacy_labels".to_owned(),
                })
                .collect(),
            ..RuntimeReconciliationReport::default()
        };
        let heartbeat = payload(
            &config,
            &heartbeat_context(
                Arc::new(HealthSnapshotRuntime { snapshots }),
                reconciliation,
            ),
        )
        .await;

        let detail_counts = &heartbeat.metadata["detail_counts"];
        for key in [
            "unhealthy_details",
            "recovered_workloads",
            "orphans",
            "stale_routes",
            "stale_images",
            "compatibility_issues",
        ] {
            assert_eq!(detail_counts[key].as_u64(), Some(51), "count for {key}");
        }

        let unhealthy_details = heartbeat.metadata["workloads"]["unhealthy_details"]
            .as_array()
            .expect("unhealthy details should be an array");
        assert_eq!(unhealthy_details.len(), 50);
        assert_eq!(unhealthy_details[0]["container_id"], "container-00");
        assert_eq!(unhealthy_details[49]["container_id"], "container-49");

        let recovered_workloads =
            heartbeat.metadata["startup_reconciliation"]["recovered_workloads"]
                .as_array()
                .expect("recovered workloads should be an array");
        assert_eq!(recovered_workloads.len(), 50);
        assert_eq!(recovered_workloads[0]["container_id"], "container-00");
        assert_eq!(recovered_workloads[49]["container_id"], "container-49");

        let orphans = heartbeat.metadata["startup_reconciliation"]["orphans"]
            .as_array()
            .expect("orphans should be an array");
        assert_eq!(orphans.len(), 50);
        assert_eq!(orphans[0]["container_id"], "orphan-00");
        assert_eq!(orphans[49]["container_id"], "orphan-49");

        let stale_routes = heartbeat.metadata["startup_reconciliation"]["stale_routes"]
            .as_array()
            .expect("stale routes should be an array");
        assert_eq!(stale_routes.len(), 50);
        assert_eq!(
            stale_routes[0]["path"],
            "/var/lib/sakala/caddy/stale-00.Caddyfile"
        );
        assert_eq!(
            stale_routes[49]["path"],
            "/var/lib/sakala/caddy/stale-49.Caddyfile"
        );

        let stale_images = heartbeat.metadata["startup_reconciliation"]["stale_images"]
            .as_array()
            .expect("stale images should be an array");
        assert_eq!(stale_images.len(), 50);
        assert_eq!(stale_images[0]["image_id"], "image-00");
        assert_eq!(stale_images[49]["image_id"], "image-49");

        let compatibility_issues =
            heartbeat.metadata["startup_reconciliation"]["compatibility_issues"]
                .as_array()
                .expect("compatibility issues should be an array");
        assert_eq!(compatibility_issues.len(), 50);
        assert_eq!(compatibility_issues[0]["container_id"], "compatibility-00");
        assert_eq!(compatibility_issues[49]["container_id"], "compatibility-49");
    }

    #[test]
    fn classifies_workspace_disk_pressure_without_guessing_missing_capacity() {
        assert_eq!(disk_pressure_state(Some(99), 100), "critical");
        assert_eq!(disk_pressure_state(Some(100), 100), "normal");
        assert_eq!(disk_pressure_state(None, 100), "unknown");
    }

    #[tokio::test]
    async fn active_node_is_degraded_when_runtime_telemetry_fails() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let context = HeartbeatRuntimeContext {
            node_lifecycle: Arc::new(NodeLifecycle::new()),
            runtime_driver: "docker".to_owned(),
            runtime: Arc::new(FailedTelemetryRuntime),
            scheduler_metrics: Arc::new(SchedulerMetrics::default()),
            reconciliation: Arc::new(RwLock::new(RuntimeReconciliationReport::default())),
            startup_reconciliation_at: OffsetDateTime::now_utc(),
            minimum_workspace_free_bytes: 0,
        };

        assert_eq!(
            payload(&config, &context).await.status,
            NodeStatus::Degraded
        );
    }
}
