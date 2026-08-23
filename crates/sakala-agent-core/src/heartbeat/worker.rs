use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus, PROTOCOL_VERSION};
use serde_json::json;
use time::OffsetDateTime;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{AgentConfig, api::ApiClient};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let payload = payload(&config);

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

fn payload(config: &AgentConfig) -> HeartbeatPayload {
    HeartbeatPayload {
        status: NodeStatus::Ready,
        node: NodeInfo {
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_owned()),
            runtime_network: config.runtime_network.clone(),
            capabilities: config.capabilities.clone(),
        },
        metadata: json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
        }),
        sent_at: OffsetDateTime::now_utc(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sakala_agent_protocol::PROTOCOL_VERSION;

    use crate::AgentConfig;

    use super::payload;

    #[test]
    fn heartbeat_identifies_the_wire_contract_revision() {
        let config = AgentConfig::from_values(&HashMap::new())
            .expect("default agent config should be valid");
        let heartbeat = payload(&config);

        assert_eq!(heartbeat.metadata["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(heartbeat.metadata["version"], env!("CARGO_PKG_VERSION"));
    }
}
