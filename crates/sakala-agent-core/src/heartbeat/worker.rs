use std::sync::Arc;

use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus, PROTOCOL_VERSION};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{AgentConfig, NodeLifecycle, NodeLifecycleState, api::ApiClient};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    node_lifecycle: Arc<NodeLifecycle>,
    runtime_driver: String,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let payload = payload(&config, node_lifecycle.state(), &runtime_driver);

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

fn payload(
    config: &AgentConfig,
    lifecycle_state: NodeLifecycleState,
    runtime_driver: &str,
) -> HeartbeatPayload {
    let resources = node_resources();
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
                "memory_total_bytes": resources.memory_total_bytes,
                "memory_available_bytes": resources.memory_available_bytes,
            },
        }),
        sent_at: OffsetDateTime::now_utc(),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NodeResources {
    uptime_seconds: Option<u64>,
    cpu_total: Option<usize>,
    memory_total_bytes: Option<u64>,
    memory_available_bytes: Option<u64>,
}

fn node_resources() -> NodeResources {
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .map(|contents| parse_meminfo(&contents))
        .unwrap_or_default();
    NodeResources {
        uptime_seconds: std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|contents| contents.split_whitespace().next()?.parse::<f64>().ok())
            .map(|seconds| seconds.max(0.0) as u64),
        cpu_total: std::thread::available_parallelism().ok().map(usize::from),
        memory_total_bytes: memory.0,
        memory_available_bytes: memory.1,
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

    use crate::{AgentConfig, NodeLifecycleState};

    use super::{parse_meminfo, payload};

    #[test]
    fn heartbeat_identifies_the_wire_contract_revision() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(&config, NodeLifecycleState::Active, "noop");

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
}
