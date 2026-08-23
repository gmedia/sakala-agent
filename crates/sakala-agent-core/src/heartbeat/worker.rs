use std::{path::Path, sync::Arc};

use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus, PROTOCOL_VERSION};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{
    AgentConfig, NodeLifecycle, NodeLifecycleState, api::ApiClient, ports::RuntimeExecutor,
};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    node_lifecycle: Arc<NodeLifecycle>,
    runtime_driver: String,
    workspace_root: std::path::PathBuf,
    runtime: Arc<dyn RuntimeExecutor>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let payload = payload(
            &config,
            node_lifecycle.state(),
            &runtime_driver,
            &workspace_root,
            runtime.as_ref(),
        )
        .await;

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
    lifecycle_state: NodeLifecycleState,
    runtime_driver: &str,
    workspace_root: &Path,
    runtime: &dyn RuntimeExecutor,
) -> HeartbeatPayload {
    let resources = node_resources(workspace_root).await;
    let workloads = workload_statistics(runtime).await;
    let dependencies = runtime_dependency_versions().await;
    HeartbeatPayload {
        status: match lifecycle_state {
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
            "lifecycle_state": format!("{lifecycle_state:?}").to_ascii_lowercase(),
            "runtime_driver": runtime_driver,
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
            "workloads": {
                "active": workloads.active,
                "starting": workloads.starting,
                "unhealthy": workloads.unhealthy,
            },
            "runtime_dependencies": dependencies,
        }),
        sent_at: OffsetDateTime::now_utc(),
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
    let output = tokio::process::Command::new(program)
        .args(arguments)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!version.is_empty()).then_some(version)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WorkloadStatistics {
    active: Option<usize>,
    starting: Option<usize>,
    unhealthy: Option<usize>,
}

async fn workload_statistics(runtime: &dyn RuntimeExecutor) -> WorkloadStatistics {
    let active = runtime
        .capacity()
        .await
        .ok()
        .and_then(|value| value.active_workloads);
    let Ok(snapshots) = runtime.health_snapshot().await else {
        return WorkloadStatistics {
            active,
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
    let unhealthy = snapshots
        .iter()
        .filter(|snapshot| {
            !snapshot.ready
                && !snapshot
                    .workload
                    .status
                    .to_ascii_lowercase()
                    .contains("starting")
        })
        .count();
    WorkloadStatistics {
        active,
        starting: Some(starting),
        unhealthy: Some(unhealthy),
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
    let output = tokio::process::Command::new("df")
        .arg("-Pk")
        .arg(workspace_root)
        .output()
        .await;
    let Ok(output) = output else {
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
    let workspace_used_bytes = tokio::process::Command::new("du")
        .arg("-sk")
        .arg(workspace_root)
        .output()
        .await
        .ok()
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
    use std::collections::HashMap;

    use sakala_agent_protocol::PROTOCOL_VERSION;

    use crate::{AgentConfig, NodeLifecycleState, ports::RuntimeExecutor};

    use super::{parse_meminfo, payload, workspace_disk_resources};

    struct EmptyRuntime;

    #[async_trait::async_trait]
    impl RuntimeExecutor for EmptyRuntime {}

    #[tokio::test]
    async fn heartbeat_identifies_the_wire_contract_revision() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(
            &config,
            NodeLifecycleState::Active,
            "noop",
            std::path::Path::new("/tmp"),
            &EmptyRuntime,
        )
        .await;

        assert_eq!(heartbeat.metadata["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(heartbeat.metadata["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn parses_memory_values_without_exposing_host_specific_text() {
        assert_eq!(
            parse_meminfo("MemTotal:       1024 kB\nMemAvailable:    512 kB\n"),
            (Some(1_048_576), Some(524_288))
        );
    }

    #[tokio::test]
    async fn reads_disk_capacity_for_an_existing_workspace() {
        let resources = workspace_disk_resources(std::path::Path::new("/tmp")).await;
        assert!(resources.disk_total_bytes.is_some());
        assert!(resources.disk_available_bytes.is_some());
    }
}
