use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use sakala_agent_protocol::{
    CrashedWorkloadDetail, HeartbeatPayload, NodeInfo, NodePlacement, NodeStatus, PROTOCOL_VERSION,
    WorkloadReplicas, WorkloadTraffic,
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};
use uuid::Uuid;

use super::{
    databases::DatabaseHeartbeat,
    metrics::{WorkloadMetricsSampler, insert_metrics},
};
use crate::{
    AgentConfig, NodeLifecycle, NodeLifecycleState,
    api::ApiClient,
    ports::{NodeTelemetry, RuntimeExecutor, RuntimeReconciliationReport},
    scheduler::metrics::SchedulerMetrics,
};

const MAX_HEARTBEAT_DETAIL_ITEMS: usize = 50;

/// `metadata.runtime`: the node's configured region and edge address.
const RUNTIME_METADATA_KEY: &str = "runtime";

/// `metadata.workloads.maximum_active_workloads`: the node's
/// `SAKALA_MAX_ACTIVE_CONTAINERS`, for capacity-aware placement.
const MAXIMUM_ACTIVE_WORKLOADS_METADATA_KEY: &str = "maximum_active_workloads";

pub struct HeartbeatRuntimeContext {
    pub node_lifecycle: Arc<NodeLifecycle>,
    pub runtime_driver: String,
    pub runtime: Arc<dyn RuntimeExecutor>,
    pub scheduler_metrics: Arc<SchedulerMetrics>,
    pub reconciliation: Arc<RwLock<RuntimeReconciliationReport>>,
    pub startup_reconciliation_at: OffsetDateTime,
    pub minimum_workspace_free_bytes: u64,
    /// Bound of `workloads.traffic` and `workloads.replicas`, the node's
    /// `SAKALA_MAX_ACTIVE_CONTAINERS`: at most that many workloads can be
    /// running and served at once.
    pub max_traffic_items: usize,
    /// Takes `workloads.metrics` samples, at most once per its interval.
    /// `None` on a runtime without the `workload-metrics` capability, whose
    /// heartbeats never carry the list.
    pub metrics: Option<WorkloadMetricsSampler>,
    /// `metadata.database` and `metadata.databases` on a database host, `None`
    /// on a node without `SAKALA_DATABASE_DRIVER`.
    pub database: Option<DatabaseHeartbeat>,
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
    // `docker stats` takes about two seconds, so the sample runs next to the
    // health probes instead of after them.
    let sample_metrics = async {
        match &context.metrics {
            Some(sampler) => sampler.sample_if_due(context.runtime.as_ref()).await,
            None => None,
        }
    };
    let (workloads, metrics) = tokio::join!(
        workload_statistics(context.runtime.as_ref()),
        sample_metrics,
    );
    let traffic = tokio::time::timeout(probe_timeout(), context.runtime.workload_traffic())
        .await
        .ok()
        .flatten();
    let reconciliation = context
        .reconciliation
        .read()
        .map(|report| report.clone())
        .unwrap_or_default();
    let mut metadata = json!({
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
            "maximum_replicas_per_project": workloads.maximum_replicas_per_project,
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
            // `sent_at` goes through the typed payload's rfc3339 serializer;
            // this timestamp lives inside the untyped metadata map, where
            // OffsetDateTime would otherwise serialize as a tuple the API
            // rejects as an invalid date.
            "captured_at": context
                .startup_reconciliation_at
                .format(&Rfc3339)
                .expect("UTC timestamp formats as RFC 3339"),
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
            "sleeping_workloads": bounded_values(&reconciliation.sleeping_workloads, |workload| json!({
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
                "deployment_id": route.deployment_id,
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
            "sleeping_workloads": reconciliation.sleeping_workloads.len(),
            "orphans": reconciliation.orphans.len(),
            "stale_routes": reconciliation.stale_routes.len(),
            "stale_images": reconciliation.stale_images.len(),
            "compatibility_issues": reconciliation.compatibility_issues.len(),
        },
        "runtime_dependencies": resources.runtime_dependencies,
    });
    insert_crashed_details(&mut metadata, workloads.crashed_details.as_deref());
    insert_traffic(&mut metadata, traffic.as_deref(), context.max_traffic_items);
    insert_replicas(
        &mut metadata,
        &workloads.replicas,
        context.max_traffic_items,
    );
    if let Some(sampler) = &context.metrics {
        insert_metrics(&mut metadata, metrics.as_deref(), sampler.max_items());
    }
    insert_maximum_active_workloads(&mut metadata, workloads.maximum_active_workloads);
    insert_placement(&mut metadata, &config.placement);
    let database_available = match &context.database {
        Some(database) => database.insert(&mut metadata).await,
        None => true,
    };
    HeartbeatPayload {
        status: match context.node_lifecycle.state() {
            NodeLifecycleState::Active
                if !telemetry_available
                    || !database_available
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
        metadata,
        sent_at: OffsetDateTime::now_utc(),
    }
}

/// Adds the crash report only when it is known. An absent list means "unknown"
/// to the control plane, while a present list that omits a workload means that
/// workload is no longer crashed, so a failed probe must never send an empty one.
fn insert_crashed_details(metadata: &mut Value, details: Option<&[CrashedWorkloadDetail]>) {
    let Some(details) = details else { return };
    metadata["workloads"]["crashed_details"] = Value::Array(bounded_values(details, |detail| {
        serde_json::to_value(detail).unwrap_or(Value::Null)
    }));
    metadata["detail_counts"]["crashed_details"] = json!(details.len());
}

/// Adds the last request per workload only when the router's counters were read.
/// An absent list means unknown, and the control plane must not read it as idle.
/// A present list only ever moves activity forward: a workload missing from it
/// has had no request counted since the router started, not "no request now".
fn insert_traffic(metadata: &mut Value, traffic: Option<&[WorkloadTraffic]>, bound: usize) {
    let Some(traffic) = traffic else { return };
    metadata["workloads"]["traffic"] = Value::Array(
        traffic
            .iter()
            .take(bound)
            .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
            .collect(),
    );
    metadata["detail_counts"]["traffic"] = json!(traffic.len());
}

/// Adds the replica counts of running deployments with more than one replica.
/// Both keys are left out when there is none, including when the health
/// snapshot failed, so an absent list says nothing about any deployment.
fn insert_replicas(metadata: &mut Value, replicas: &[WorkloadReplicas], bound: usize) {
    if replicas.is_empty() {
        return;
    }
    metadata["workloads"]["replicas"] = Value::Array(
        replicas
            .iter()
            .take(bound)
            .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
            .collect(),
    );
    metadata["detail_counts"]["replicas"] = json!(replicas.len());
}

/// Adds the node's container limit when the runtime reports one. An unknown
/// limit is left out rather than sent as `null`, so the API keeps treating the
/// node as one without a known capacity.
fn insert_maximum_active_workloads(metadata: &mut Value, maximum: Option<usize>) {
    let Some(maximum) = maximum else { return };
    metadata["workloads"][MAXIMUM_ACTIVE_WORKLOADS_METADATA_KEY] = json!(maximum);
}

/// Adds `metadata.runtime` with the configured region and edge address. Each
/// key is left out when unset, and the object is left out when both are, so
/// the API never receives an empty value. The values were checked against the
/// API's rules at startup.
fn insert_placement(metadata: &mut Value, placement: &NodePlacement) {
    if placement.is_empty() {
        return;
    }
    // Two strings always serialize; on the impossible failure the key stays
    // out, because the API rejects `runtime: null` with the whole heartbeat.
    if let Ok(runtime) = serde_json::to_value(placement) {
        metadata[RUNTIME_METADATA_KEY] = runtime;
    }
}

/// Groups the snapshot of running containers by deployment. A deployment of
/// one replica is left out: its single container already speaks for it.
fn deployment_replicas(snapshots: &[crate::ports::RuntimeHealthSnapshot]) -> Vec<WorkloadReplicas> {
    let mut deployments = BTreeMap::<(Uuid, Uuid), WorkloadReplicas>::new();
    for snapshot in snapshots {
        let workload = &snapshot.workload;
        let entry = deployments
            .entry((workload.project_id, workload.deployment_id))
            .or_insert_with(|| WorkloadReplicas {
                project_id: workload.project_id,
                deployment_id: workload.deployment_id,
                desired: snapshot.desired_replicas.get(),
                ready: 0,
            });
        entry.desired = entry.desired.max(snapshot.desired_replicas.get());
        entry.ready += u32::from(snapshot.ready);
    }
    deployments
        .into_values()
        .filter(|deployment| deployment.desired > 1)
        .collect()
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
    maximum_replicas_per_project: Option<u32>,
    maximum_active_workloads: Option<usize>,
    unhealthy_details: Vec<crate::ports::RuntimeHealthSnapshot>,
    crashed_details: Option<Vec<CrashedWorkloadDetail>>,
    replicas: Vec<WorkloadReplicas>,
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
    let maximum_replicas_per_project = capacity
        .as_ref()
        .and_then(|value| value.maximum_replicas_per_project);
    let maximum_active_workloads = capacity
        .as_ref()
        .and_then(|value| value.maximum_active_workloads);
    let health_unavailable = || WorkloadStatistics {
        active,
        stopped,
        active_builds,
        maximum_concurrent_builds,
        maximum_replicas_per_project,
        maximum_active_workloads,
        capacity_available,
        ..WorkloadStatistics::default()
    };
    let Ok(Ok(snapshots)) = tokio::time::timeout(probe_timeout(), runtime.health_snapshot()).await
    else {
        return health_unavailable();
    };
    let Ok(Ok(crashed_details)) =
        tokio::time::timeout(probe_timeout(), runtime.crashed_workloads(&snapshots)).await
    else {
        return health_unavailable();
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
    let replicas = deployment_replicas(&snapshots);
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
        maximum_replicas_per_project,
        maximum_active_workloads,
        starting: Some(starting),
        unhealthy: Some(unhealthy_details.len()),
        unhealthy_details,
        crashed_details: Some(crashed_details),
        replicas,
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
        num::NonZeroU32,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use sakala_agent_protocol::{
        CrashedWorkloadDetail, NodeStatus, PROTOCOL_VERSION, WorkloadTraffic,
    };
    use serde_json::json;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};
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

    use super::{HeartbeatRuntimeContext, WorkloadMetricsSampler, disk_pressure_state, payload};

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

    struct CrashedRuntime {
        details: Vec<CrashedWorkloadDetail>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for CrashedRuntime {
        async fn crashed_workloads(
            &self,
            _snapshots: &[RuntimeHealthSnapshot],
        ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeExecutionError> {
            Ok(self.details.clone())
        }
    }

    struct FailedCrashQueryRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for FailedCrashQueryRuntime {
        async fn crashed_workloads(
            &self,
            _snapshots: &[RuntimeHealthSnapshot],
        ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeExecutionError> {
            Err(RuntimeExecutionError::new(
                "runtime_container_failed",
                "docker unavailable",
            ))
        }
    }

    struct TrafficRuntime {
        traffic: Option<Vec<WorkloadTraffic>>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for TrafficRuntime {
        async fn workload_traffic(&self) -> Option<Vec<WorkloadTraffic>> {
            self.traffic.clone()
        }
    }

    fn traffic_item(project_id: Uuid) -> WorkloadTraffic {
        WorkloadTraffic {
            project_id,
            deployment_id: Uuid::new_v4(),
            last_request_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    struct FailedHealthRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for FailedHealthRuntime {
        async fn health_snapshot(
            &self,
        ) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeExecutionError> {
            Err(RuntimeExecutionError::new(
                "runtime_container_failed",
                "docker unavailable",
            ))
        }
    }

    fn crash_detail(project_id: Uuid) -> CrashedWorkloadDetail {
        CrashedWorkloadDetail {
            project_id,
            deployment_id: Uuid::new_v4(),
            exit_code: Some(137),
            oom_killed: true,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
            restart_count: 4,
            running: true,
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
            max_traffic_items: 20,
            metrics: Some(WorkloadMetricsSampler::new(Duration::from_secs(60), 20)),
            database: None,
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
            max_traffic_items: 20,
            metrics: Some(WorkloadMetricsSampler::new(Duration::from_secs(60), 20)),
            database: None,
        };
        let heartbeat = payload(&config, &context).await;

        assert_eq!(heartbeat.metadata["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(heartbeat.metadata["version"], env!("CARGO_PKG_VERSION"));
        let captured_at = heartbeat.metadata["startup_reconciliation"]["captured_at"]
            .as_str()
            .expect("captured_at is an RFC 3339 string, not a time tuple");
        assert_eq!(
            OffsetDateTime::parse(captured_at, &Rfc3339).expect("captured_at parses as RFC 3339"),
            context.startup_reconciliation_at
        );
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["stale_routes"][0]["project_id"],
            stale_project.to_string()
        );
        assert!(
            heartbeat.metadata["startup_reconciliation"]["stale_routes"][0]["deployment_id"]
                .is_null()
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
            sleeping_workloads: vec![RuntimeWorkload {
                container_id: "sleeping-00".to_owned(),
                status: "Exited (143) 2 hours ago".to_owned(),
                ..workload.clone()
            }],
            orphans: vec![RuntimeOrphan {
                container_id: "orphan-00".to_owned(),
                project_id: Some(project_id),
                deployment_id: None,
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
                desired_replicas: NonZeroU32::MIN,
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
            "sleeping_workloads",
            "orphans",
            "stale_routes",
            "stale_images",
            "compatibility_issues",
        ] {
            assert_eq!(detail_counts[key].as_u64(), Some(1), "count for {key}");
        }
        assert_eq!(
            heartbeat.metadata["startup_reconciliation"]["sleeping_workloads"],
            json!([{
                "container_id": "sleeping-00",
                "project_id": project_id,
                "deployment_id": deployment_id,
                "status": "Exited (143) 2 hours ago",
            }])
        );
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
                desired_replicas: NonZeroU32::MIN,
            })
            .collect();
        let reconciliation = RuntimeReconciliationReport {
            workloads,
            orphans: (0..51)
                .map(|index| RuntimeOrphan {
                    container_id: format!("orphan-{index:02}"),
                    project_id: Some(project_id),
                    deployment_id: None,
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
        assert_eq!(stale_routes[0]["deployment_id"], deployment_id.to_string());
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

    #[tokio::test]
    async fn heartbeat_bounds_crashed_details_and_keeps_the_original_count() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let projects = (0..51).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        let runtime = CrashedRuntime {
            details: projects.iter().copied().map(crash_detail).collect(),
        };
        let heartbeat = payload(
            &config,
            &heartbeat_context(Arc::new(runtime), RuntimeReconciliationReport::default()),
        )
        .await;

        assert_eq!(
            heartbeat.metadata["detail_counts"]["crashed_details"].as_u64(),
            Some(51)
        );
        let details = heartbeat.metadata["workloads"]["crashed_details"]
            .as_array()
            .expect("crashed details should be an array");
        assert_eq!(details.len(), 50);
        assert_eq!(details[0]["project_id"], projects[0].to_string());
        assert_eq!(details[49]["project_id"], projects[49].to_string());
        assert_eq!(details[0]["exit_code"], 137);
        assert_eq!(details[0]["oom_killed"], true);
        assert_eq!(details[0]["finished_at"], "1970-01-01T00:00:00Z");
        assert_eq!(details[0]["restart_count"], 4);
        assert_eq!(details[0]["running"], true);
        assert_eq!(
            details[0].as_object().expect("detail object").len(),
            7,
            "the wire shape carries nothing beyond the agreed fields"
        );
    }

    #[tokio::test]
    async fn a_node_without_crashes_reports_an_empty_list_and_a_zero_count() {
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

        assert_eq!(
            heartbeat.metadata["workloads"]["crashed_details"],
            json!([])
        );
        assert_eq!(
            heartbeat.metadata["detail_counts"]["crashed_details"].as_u64(),
            Some(0)
        );
    }

    #[tokio::test]
    async fn an_unknown_crash_state_omits_the_list_instead_of_claiming_no_crashes() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let runtimes: [Arc<dyn RuntimeExecutor>; 2] = [
            Arc::new(FailedHealthRuntime),
            Arc::new(FailedCrashQueryRuntime),
        ];
        for runtime in runtimes {
            let heartbeat = payload(
                &config,
                &heartbeat_context(runtime, RuntimeReconciliationReport::default()),
            )
            .await;

            assert_eq!(heartbeat.status, NodeStatus::Degraded);
            assert!(heartbeat.metadata["workloads"]["unhealthy"].is_null());
            assert!(
                heartbeat.metadata["workloads"]
                    .get("crashed_details")
                    .is_none()
            );
            assert!(
                heartbeat.metadata["detail_counts"]
                    .get("crashed_details")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn heartbeat_reports_the_last_request_of_each_served_workload() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let project_id = Uuid::new_v4();
        let item = traffic_item(project_id);
        let heartbeat = payload(
            &config,
            &heartbeat_context(
                Arc::new(TrafficRuntime {
                    traffic: Some(vec![item.clone()]),
                }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;

        assert_eq!(
            heartbeat.metadata["workloads"]["traffic"],
            json!([{
                "project_id": project_id,
                "deployment_id": item.deployment_id,
                "last_request_at": "1970-01-01T00:00:00Z",
            }])
        );
        assert_eq!(
            heartbeat.metadata["detail_counts"]["traffic"].as_u64(),
            Some(1)
        );
    }

    #[tokio::test]
    async fn traffic_is_bounded_by_the_node_container_limit_and_keeps_the_count() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let projects = (0..7).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        let mut context = heartbeat_context(
            Arc::new(TrafficRuntime {
                traffic: Some(projects.iter().copied().map(traffic_item).collect()),
            }),
            RuntimeReconciliationReport::default(),
        );
        context.max_traffic_items = 5;

        let heartbeat = payload(&config, &context).await;

        let traffic = heartbeat.metadata["workloads"]["traffic"]
            .as_array()
            .expect("traffic should be an array");
        assert_eq!(traffic.len(), 5);
        assert_eq!(traffic[0]["project_id"], projects[0].to_string());
        assert_eq!(traffic[4]["project_id"], projects[4].to_string());
        assert_eq!(
            heartbeat.metadata["detail_counts"]["traffic"].as_u64(),
            Some(7)
        );
    }

    #[tokio::test]
    async fn unknown_traffic_omits_both_keys_and_known_silence_sends_an_empty_list() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let unknown = payload(
            &config,
            &heartbeat_context(
                Arc::new(EmptyRuntime),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        assert!(unknown.metadata["workloads"].get("traffic").is_none());
        assert!(unknown.metadata["detail_counts"].get("traffic").is_none());

        let silent = payload(
            &config,
            &heartbeat_context(
                Arc::new(TrafficRuntime {
                    traffic: Some(Vec::new()),
                }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        assert_eq!(silent.metadata["workloads"]["traffic"], json!([]));
        assert_eq!(
            silent.metadata["detail_counts"]["traffic"].as_u64(),
            Some(0)
        );
    }

    struct ReplicaRuntime {
        snapshots: Vec<RuntimeHealthSnapshot>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for ReplicaRuntime {
        async fn capacity(&self) -> Result<crate::ports::RuntimeCapacity, RuntimeExecutionError> {
            Ok(crate::ports::RuntimeCapacity {
                maximum_replicas_per_project: Some(4),
                ..crate::ports::RuntimeCapacity::default()
            })
        }

        async fn health_snapshot(
            &self,
        ) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeExecutionError> {
            Ok(self.snapshots.clone())
        }
    }

    fn replica(
        project_id: Uuid,
        deployment_id: Uuid,
        desired: u32,
        ready: bool,
    ) -> RuntimeHealthSnapshot {
        RuntimeHealthSnapshot {
            workload: RuntimeWorkload {
                container_id: Uuid::new_v4().to_string(),
                project_id,
                deployment_id,
                status: if ready {
                    "Up 1 minute"
                } else {
                    "Restarting (1)"
                }
                .to_owned(),
            },
            ready,
            reason: None,
            desired_replicas: NonZeroU32::new(desired).expect("non-zero"),
        }
    }

    #[tokio::test]
    async fn heartbeat_reports_ready_replicas_of_deployments_with_several() {
        let config = AgentConfig::from_values(&HashMap::new()).expect("config");
        let (balanced, balanced_deployment) = (Uuid::new_v4(), Uuid::new_v4());
        let (single, single_deployment) = (Uuid::new_v4(), Uuid::new_v4());
        let runtime = ReplicaRuntime {
            snapshots: vec![
                replica(balanced, balanced_deployment, 3, true),
                replica(balanced, balanced_deployment, 3, false),
                replica(balanced, balanced_deployment, 3, true),
                replica(single, single_deployment, 1, true),
            ],
        };
        let heartbeat = payload(
            &config,
            &heartbeat_context(Arc::new(runtime), RuntimeReconciliationReport::default()),
        )
        .await;

        assert_eq!(
            heartbeat.metadata["workloads"]["replicas"],
            json!([{
                "project_id": balanced,
                "deployment_id": balanced_deployment,
                "desired": 3,
                "ready": 2,
            }])
        );
        assert_eq!(heartbeat.metadata["detail_counts"]["replicas"], 1);
        assert_eq!(
            heartbeat.metadata["workloads"]["maximum_replicas_per_project"],
            4
        );
    }

    #[tokio::test]
    async fn replicas_are_bounded_like_traffic_and_omitted_when_there_are_none() {
        let config = AgentConfig::from_values(&HashMap::new()).expect("config");
        let snapshots = (0..7)
            .flat_map(|_| {
                let (project_id, deployment_id) = (Uuid::new_v4(), Uuid::new_v4());
                [
                    replica(project_id, deployment_id, 2, true),
                    replica(project_id, deployment_id, 2, true),
                ]
            })
            .collect();
        let mut context = heartbeat_context(
            Arc::new(ReplicaRuntime { snapshots }),
            RuntimeReconciliationReport::default(),
        );
        context.max_traffic_items = 5;
        let bounded = payload(&config, &context).await;
        assert_eq!(
            bounded.metadata["workloads"]["replicas"]
                .as_array()
                .expect("replicas")
                .len(),
            5
        );
        assert_eq!(bounded.metadata["detail_counts"]["replicas"], 7);

        let single = ReplicaRuntime {
            snapshots: vec![replica(Uuid::new_v4(), Uuid::new_v4(), 1, true)],
        };
        let none = payload(
            &config,
            &heartbeat_context(Arc::new(single), RuntimeReconciliationReport::default()),
        )
        .await;
        assert!(none.metadata["workloads"].get("replicas").is_none());
        assert!(none.metadata["detail_counts"].get("replicas").is_none());

        let unknown = payload(
            &config,
            &heartbeat_context(
                Arc::new(FailedHealthRuntime),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        assert!(unknown.metadata["workloads"].get("replicas").is_none());
        assert!(
            unknown.metadata["workloads"]["maximum_replicas_per_project"].is_null(),
            "a driver without replicas reports no limit"
        );
    }

    struct CapacityRuntime {
        maximum_active_workloads: Option<usize>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for CapacityRuntime {
        async fn capacity(&self) -> Result<crate::ports::RuntimeCapacity, RuntimeExecutionError> {
            Ok(crate::ports::RuntimeCapacity {
                maximum_active_workloads: self.maximum_active_workloads,
                ..crate::ports::RuntimeCapacity::default()
            })
        }
    }

    #[tokio::test]
    async fn heartbeat_reports_the_node_container_limit_only_when_known() {
        let config = AgentConfig::from_values(&HashMap::new()).expect("config");
        let known = payload(
            &config,
            &heartbeat_context(
                Arc::new(CapacityRuntime {
                    maximum_active_workloads: Some(20),
                }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        assert_eq!(
            known.metadata["workloads"]["maximum_active_workloads"],
            json!(20)
        );

        let zero = payload(
            &config,
            &heartbeat_context(
                Arc::new(CapacityRuntime {
                    maximum_active_workloads: Some(0),
                }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        assert_eq!(
            zero.metadata["workloads"]["maximum_active_workloads"],
            json!(0)
        );

        for runtime in [
            Arc::new(CapacityRuntime {
                maximum_active_workloads: None,
            }) as Arc<dyn RuntimeExecutor>,
            Arc::new(EmptyRuntime),
            Arc::new(FailedHealthRuntime),
        ] {
            let unknown = payload(
                &config,
                &heartbeat_context(runtime, RuntimeReconciliationReport::default()),
            )
            .await;
            assert!(
                unknown.metadata["workloads"]
                    .get("maximum_active_workloads")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn heartbeat_reports_configured_placement_and_omits_the_rest() {
        let context = || {
            heartbeat_context(
                Arc::new(EmptyRuntime),
                RuntimeReconciliationReport::default(),
            )
        };
        let unset = AgentConfig::from_values(&HashMap::new()).expect("config");
        assert!(
            payload(&unset, &context())
                .await
                .metadata
                .get("runtime")
                .is_none(),
            "an unconfigured node sends no runtime object at all"
        );

        let both = AgentConfig::from_values(&HashMap::from([
            (crate::REGION_ENV.to_owned(), "id-jog-1".to_owned()),
            (
                crate::EDGE_ADDRESS_ENV.to_owned(),
                "10.0.47.201:8180".to_owned(),
            ),
        ]))
        .expect("config");
        assert_eq!(
            payload(&both, &context()).await.metadata["runtime"],
            json!({ "region": "id-jog-1", "edge_address": "10.0.47.201:8180" })
        );

        let region_only = AgentConfig::from_values(&HashMap::from([(
            crate::REGION_ENV.to_owned(),
            "jkt".to_owned(),
        )]))
        .expect("config");
        assert_eq!(
            payload(&region_only, &context()).await.metadata["runtime"],
            json!({ "region": "jkt" })
        );

        let address_only = AgentConfig::from_values(&HashMap::from([(
            crate::EDGE_ADDRESS_ENV.to_owned(),
            "[fd00::21]:8180".to_owned(),
        )]))
        .expect("config");
        assert_eq!(
            payload(&address_only, &context()).await.metadata["runtime"],
            json!({ "edge_address": "[fd00::21]:8180" })
        );
    }

    #[tokio::test]
    async fn every_timestamp_on_the_wire_is_an_rfc3339_string() {
        // Typed fields such as `sent_at` declare the rfc3339 serializer, but
        // values inside the untyped metadata map fall back to `time`'s default
        // tuple form, which the control plane rejects as an invalid date. Walk
        // the serialized payload so any future `*_at` field is covered too.
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(
            &config,
            &heartbeat_context(
                Arc::new(CrashedRuntime {
                    details: vec![crash_detail(Uuid::new_v4())],
                }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;
        let wire = serde_json::to_value(&heartbeat).expect("heartbeat should serialize");

        let mut timestamps = Vec::new();
        collect_timestamp_fields(&wire, &mut timestamps);
        assert!(
            timestamps.iter().any(|(key, _)| key == "captured_at"),
            "captured_at should be part of the wire payload"
        );
        assert!(
            timestamps.iter().any(|(key, _)| key == "finished_at"),
            "finished_at should be part of the wire payload"
        );
        for (key, value) in timestamps {
            let text = value
                .as_str()
                .unwrap_or_else(|| panic!("{key} must be a string, got {value}"));
            OffsetDateTime::parse(text, &Rfc3339)
                .unwrap_or_else(|error| panic!("{key} must be RFC 3339: {text} ({error})"));
        }
    }

    struct MetricsRuntime {
        result: Result<Vec<sakala_agent_protocol::WorkloadMetrics>, ()>,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for MetricsRuntime {
        async fn workload_metrics(
            &self,
        ) -> Result<Vec<sakala_agent_protocol::WorkloadMetrics>, RuntimeExecutionError> {
            self.result
                .clone()
                .map_err(|()| RuntimeExecutionError::new("runtime_container_failed", "no stats"))
        }
    }

    #[tokio::test]
    async fn heartbeat_sends_a_fresh_metrics_sample_once_and_omits_it_until_the_next() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let project_id = Uuid::new_v4();
        let deployment_id = Uuid::new_v4();
        let sampled_at = OffsetDateTime::parse("2026-11-18T09:15:00Z", &Rfc3339).expect("time");
        let context = heartbeat_context(
            Arc::new(MetricsRuntime {
                result: Ok(vec![sakala_agent_protocol::WorkloadMetrics {
                    project_id,
                    deployment_id,
                    replica: 1,
                    sampled_at,
                    cpu_percent: 112.5,
                    memory_bytes: 37_538_611,
                    memory_limit_bytes: Some(268_435_456),
                    restart_count: 2,
                }]),
            }),
            RuntimeReconciliationReport::default(),
        );

        let fresh = payload(&config, &context).await;
        assert_eq!(
            fresh.metadata["workloads"]["metrics"],
            json!([{
                "project_id": project_id,
                "deployment_id": deployment_id,
                "replica": 1,
                "sampled_at": "2026-11-18T09:15:00Z",
                "cpu_percent": 112.5,
                "memory_bytes": 37_538_611,
                "memory_limit_bytes": 268_435_456,
                "restart_count": 2,
            }])
        );
        assert_eq!(fresh.metadata["detail_counts"]["metrics"], 1);

        // The interval (60 seconds in this context) has not passed yet.
        let between = payload(&config, &context).await;
        assert!(between.metadata["workloads"].get("metrics").is_none());
        assert!(between.metadata["detail_counts"].get("metrics").is_none());
    }

    struct DatabaseSizes {
        result: Result<Vec<sakala_agent_protocol::ManagedDatabaseSize>, ()>,
    }

    #[async_trait::async_trait]
    impl crate::ports::DatabaseAdmin for DatabaseSizes {
        async fn database_sizes(
            &self,
        ) -> Result<Vec<sakala_agent_protocol::ManagedDatabaseSize>, RuntimeExecutionError>
        {
            self.result
                .clone()
                .map_err(|()| RuntimeExecutionError::new("database_admin_unavailable", "refused"))
        }
    }

    fn database_context(
        result: Result<Vec<sakala_agent_protocol::ManagedDatabaseSize>, ()>,
    ) -> HeartbeatRuntimeContext {
        let mut context = heartbeat_context(
            Arc::new(EmptyRuntime),
            RuntimeReconciliationReport::default(),
        );
        context.database = Some(super::DatabaseHeartbeat::new(
            Arc::new(DatabaseSizes { result }),
            sakala_agent_protocol::DatabaseEndpoint::new("10.0.47.10", 5432).expect("endpoint"),
        ));
        context
    }

    #[tokio::test]
    async fn a_database_host_reports_its_endpoint_and_database_sizes() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let context = database_context(Ok(vec![sakala_agent_protocol::ManagedDatabaseSize {
            name: "p_0123456789abcdef".to_owned(),
            size_bytes: 7_952_063,
        }]));

        let heartbeat = payload(&config, &context).await;

        assert_eq!(heartbeat.status, NodeStatus::Ready);
        assert_eq!(
            heartbeat.metadata["database"],
            json!({ "endpoint": { "host": "10.0.47.10", "port": 5432 }, "available": true })
        );
        assert_eq!(
            heartbeat.metadata["databases"],
            json!([{ "name": "p_0123456789abcdef", "size_bytes": 7_952_063 }])
        );
        assert_eq!(heartbeat.metadata["detail_counts"]["databases"], 1);
    }

    #[tokio::test]
    async fn a_database_host_that_cannot_read_its_instance_is_degraded() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");

        let heartbeat = payload(&config, &database_context(Err(()))).await;

        assert_eq!(heartbeat.status, NodeStatus::Degraded);
        assert_eq!(heartbeat.metadata["database"]["available"], false);
        assert!(heartbeat.metadata.get("databases").is_none());
        assert!(
            heartbeat.metadata["detail_counts"]
                .get("databases")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_runtime_node_sends_no_database_keys() {
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

        assert!(heartbeat.metadata.get("database").is_none());
        assert!(heartbeat.metadata.get("databases").is_none());
    }

    #[tokio::test]
    async fn a_runtime_without_the_capability_never_samples_metrics() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let mut context = heartbeat_context(
            Arc::new(MetricsRuntime {
                result: Ok(Vec::new()),
            }),
            RuntimeReconciliationReport::default(),
        );
        context.metrics = None;

        let heartbeat = payload(&config, &context).await;

        assert!(heartbeat.metadata["workloads"].get("metrics").is_none());
        assert!(heartbeat.metadata["detail_counts"].get("metrics").is_none());
    }

    #[tokio::test]
    async fn a_failed_metrics_sample_leaves_both_keys_out_and_keeps_the_node_ready() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(
            &config,
            &heartbeat_context(
                Arc::new(MetricsRuntime { result: Err(()) }),
                RuntimeReconciliationReport::default(),
            ),
        )
        .await;

        assert!(heartbeat.metadata["workloads"].get("metrics").is_none());
        assert!(heartbeat.metadata["detail_counts"].get("metrics").is_none());
        assert_eq!(heartbeat.status, NodeStatus::Ready);
    }

    fn collect_timestamp_fields(
        value: &serde_json::Value,
        found: &mut Vec<(String, serde_json::Value)>,
    ) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if key.ends_with("_at") {
                        found.push((key.clone(), value.clone()));
                    }
                    collect_timestamp_fields(value, found);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_timestamp_fields(item, found);
                }
            }
            _ => {}
        }
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
            max_traffic_items: 20,
            metrics: Some(WorkloadMetricsSampler::new(Duration::from_secs(60), 20)),
            database: None,
        };

        assert_eq!(
            payload(&config, &context).await.status,
            NodeStatus::Degraded
        );
    }
}
