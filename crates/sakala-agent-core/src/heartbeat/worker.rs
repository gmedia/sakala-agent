use std::{
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus, PROTOCOL_VERSION};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{
    AgentConfig, NodeLifecycle, NodeLifecycleState,
    api::ApiClient,
    ports::{RuntimeExecutor, RuntimeReconciliationReport},
    scheduler::metrics::SchedulerMetrics,
};

pub struct HeartbeatRuntimeContext {
    pub node_lifecycle: Arc<NodeLifecycle>,
    pub runtime_driver: String,
    pub workspace_root: std::path::PathBuf,
    pub runtime: Arc<dyn RuntimeExecutor>,
    pub scheduler_metrics: Arc<SchedulerMetrics>,
    pub reconciliation: Arc<RwLock<RuntimeReconciliationReport>>,
    pub minimum_workspace_free_bytes: u64,
}

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    context: HeartbeatRuntimeContext,
    mut shutdown: watch::Receiver<bool>,
) {
    // Dependency versions are immutable for the lifetime of this process and can
    // be relatively expensive to obtain from Docker, so probe them only once.
    let dependencies = runtime_dependency_versions().await;
    loop {
        let payload = payload(&config, &context, &dependencies).await;

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

async fn payload(
    config: &AgentConfig,
    context: &HeartbeatRuntimeContext,
    dependencies: &serde_json::Value,
) -> HeartbeatPayload {
    let resources = node_resources(&context.workspace_root).await;
    let workloads = workload_statistics(context.runtime.as_ref()).await;
    let reconciliation = context
        .reconciliation
        .read()
        .map(|report| report.clone())
        .unwrap_or_default();
    HeartbeatPayload {
        status: match context.node_lifecycle.state() {
            NodeLifecycleState::Active => NodeStatus::Ready,
            NodeLifecycleState::Draining => NodeStatus::Draining,
            NodeLifecycleState::Drained => NodeStatus::Drained,
            NodeLifecycleState::Maintenance => NodeStatus::Maintenance,
        },
        node: NodeInfo {
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_owned()),
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
                "unhealthy_details": workloads.unhealthy_details.iter().map(|snapshot| json!({
                    "container_id": snapshot.workload.container_id,
                    "project_id": snapshot.workload.project_id,
                    "deployment_id": snapshot.workload.deployment_id,
                    "status": snapshot.workload.status,
                    "reason": snapshot.reason,
                })).collect::<Vec<_>>(),
            },
            "execution": {
                "active_commands": context.scheduler_metrics.active_commands(),
                "queued_local_commands": context.scheduler_metrics.queued_local_commands(),
                "capacity_waiting_commands": context.scheduler_metrics.capacity_waiting_commands(),
                "active_builds": workloads.active_builds,
                "maximum_concurrent_builds": workloads.maximum_concurrent_builds,
            },
            "reconciliation": {
                "inspected_containers": reconciliation.inspected_containers,
                "cleaned_workspaces": reconciliation.cleaned_workspaces,
                "reattached_log_followers": reconciliation.reattached_log_followers,
                "recovered_execution_records": reconciliation.recovered_execution_records,
                "recovered_workloads": reconciliation.workloads.iter().map(|workload| json!({
                    "container_id": workload.container_id,
                    "project_id": workload.project_id,
                    "deployment_id": workload.deployment_id,
                    "status": workload.status,
                })).collect::<Vec<_>>(),
                "orphans": reconciliation.orphans.iter().map(|orphan| json!({
                    "container_id": orphan.container_id,
                    "project_id": orphan.project_id,
                    "reason": orphan.reason,
                })).collect::<Vec<_>>(),
                "stale_routes": reconciliation.stale_routes.iter().map(|route| json!({
                    "path": route.path,
                    "project_id": route.project_id,
                })).collect::<Vec<_>>(),
                "stale_images": reconciliation.stale_images.iter().map(|image| json!({
                    "image_id": image.image_id,
                    "project_id": image.project_id,
                    "deployment_id": image.deployment_id,
                })).collect::<Vec<_>>(),
                "compatibility_issues": reconciliation.compatibility_issues.iter().map(|issue| json!({
                    "container_id": issue.container_id,
                    "project_id": issue.project_id,
                    "deployment_id": issue.deployment_id,
                    "reason": issue.reason,
                })).collect::<Vec<_>>(),
            },
            "runtime_dependencies": dependencies,
        }),
        sent_at: OffsetDateTime::now_utc(),
    }
}

fn disk_pressure_state(available_bytes: Option<u64>, minimum_free_bytes: u64) -> &'static str {
    match available_bytes {
        Some(available_bytes) if available_bytes < minimum_free_bytes => "critical",
        Some(_) => "normal",
        None => "unknown",
    }
}

async fn runtime_dependency_versions() -> serde_json::Value {
    let (git, docker, buildx, railpack) = tokio::join!(
        command_version("git", &["--version"]),
        command_version("docker", &["version", "--format", "{{.Server.Version}}"]),
        command_version("docker", &["buildx", "version"]),
        command_version("railpack", &["--version"]),
    );
    json!({
        "git": git,
        "docker": docker,
        "buildx": buildx,
        "railpack": railpack,
    })
}

async fn command_version(program: &str, arguments: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        probe_timeout(),
        tokio::process::Command::new(program)
            .args(arguments)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!version.is_empty()).then_some(version)
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
}

async fn workload_statistics(runtime: &dyn RuntimeExecutor) -> WorkloadStatistics {
    let capacity = tokio::time::timeout(probe_timeout(), runtime.capacity())
        .await
        .ok()
        .and_then(Result::ok);
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
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct NodeResources {
    uptime_seconds: Option<u64>,
    cpu_total: Option<usize>,
    cpu_load_1m: Option<f64>,
    memory_total_bytes: Option<u64>,
    memory_available_bytes: Option<u64>,
    disk_total_bytes: Option<u64>,
    disk_available_bytes: Option<u64>,
    workspace_used_bytes: Option<u64>,
}

async fn node_resources(workspace_root: &Path) -> NodeResources {
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .map(|contents| parse_meminfo(&contents))
        .unwrap_or_default();
    let disk = workspace_disk_resources(workspace_root).await;
    NodeResources {
        uptime_seconds: std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|contents| contents.split_whitespace().next()?.parse::<f64>().ok())
            .map(|seconds| seconds.max(0.0) as u64),
        cpu_total: std::thread::available_parallelism().ok().map(usize::from),
        cpu_load_1m: std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|contents| contents.split_whitespace().next()?.parse::<f64>().ok()),
        memory_total_bytes: memory.0,
        memory_available_bytes: memory.1,
        ..disk
    }
}

async fn workspace_disk_resources(workspace_root: &Path) -> NodeResources {
    let output = tokio::time::timeout(
        probe_timeout(),
        tokio::process::Command::new("df")
            .arg("-Pk")
            .arg(workspace_root)
            .output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return NodeResources::default();
    };
    if !output.status.success() {
        return NodeResources::default();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().filter(|line| !line.trim().is_empty()).nth(1) else {
        return NodeResources::default();
    };
    let fields = line.split_whitespace().collect::<Vec<_>>();
    let disk_total_bytes = fields
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1_024));
    let disk_available_bytes = fields
        .get(3)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1_024));
    let workspace_used_bytes = tokio::time::timeout(
        probe_timeout(),
        tokio::process::Command::new("du")
            .arg("-sk")
            .arg(workspace_root)
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .filter(|output| output.status.success())
    .and_then(|output| String::from_utf8(output.stdout).ok())
    .and_then(|output| output.split_whitespace().next()?.parse::<u64>().ok())
    .and_then(|value| value.checked_mul(1_024));
    NodeResources {
        disk_total_bytes,
        disk_available_bytes,
        workspace_used_bytes,
        ..NodeResources::default()
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

fn parse_meminfo(contents: &str) -> (Option<u64>, Option<u64>) {
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else { continue };
        let value = fields.next().and_then(|value| value.parse::<u64>().ok());
        match key {
            "MemTotal:" => total = value.and_then(|value| value.checked_mul(1_024)),
            "MemAvailable:" => available = value.and_then(|value| value.checked_mul(1_024)),
            _ => {}
        }
    }
    (total, available)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    use sakala_agent_protocol::PROTOCOL_VERSION;
    use uuid::Uuid;

    use crate::{
        AgentConfig, NodeLifecycle,
        ports::RuntimeExecutor,
        ports::{RuntimeReconciliationReport, RuntimeStaleRoute},
        scheduler::metrics::SchedulerMetrics,
    };

    use super::{
        HeartbeatRuntimeContext, disk_pressure_state, parse_meminfo, payload,
        workspace_disk_resources,
    };

    struct EmptyRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for EmptyRuntime {}

    #[tokio::test]
    async fn heartbeat_identifies_the_wire_contract_revision() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let stale_project = Uuid::new_v4();
        let context = HeartbeatRuntimeContext {
            node_lifecycle: Arc::new(NodeLifecycle::new()),
            runtime_driver: "noop".to_owned(),
            workspace_root: PathBuf::from("/tmp"),
            runtime: Arc::new(EmptyRuntime),
            scheduler_metrics: Arc::new(SchedulerMetrics::default()),
            reconciliation: Arc::new(RwLock::new(RuntimeReconciliationReport {
                stale_routes: vec![RuntimeStaleRoute {
                    path: "/var/lib/sakala/caddy/stale.Caddyfile".to_owned(),
                    project_id: stale_project,
                }],
                ..RuntimeReconciliationReport::default()
            })),
            minimum_workspace_free_bytes: 0,
        };
        let heartbeat = payload(&config, &context, &serde_json::json!({})).await;

        assert_eq!(heartbeat.metadata["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(heartbeat.metadata["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            heartbeat.metadata["reconciliation"]["stale_routes"][0]["project_id"],
            stale_project.to_string()
        );
    }

    #[test]
    fn parses_memory_values_without_exposing_host_specific_text() {
        assert_eq!(
            parse_meminfo("MemTotal:       1024 kB\nMemAvailable:    512 kB\n"),
            (Some(1_048_576), Some(524_288))
        );
    }

    #[test]
    fn classifies_workspace_disk_pressure_without_guessing_missing_capacity() {
        assert_eq!(disk_pressure_state(Some(99), 100), "critical");
        assert_eq!(disk_pressure_state(Some(100), 100), "normal");
        assert_eq!(disk_pressure_state(None, 100), "unknown");
    }

    #[tokio::test]
    async fn reads_disk_capacity_for_an_existing_workspace() {
        let resources = workspace_disk_resources(std::path::Path::new("/tmp")).await;
        assert!(resources.disk_total_bytes.is_some());
        assert!(resources.disk_available_bytes.is_some());
    }
}
