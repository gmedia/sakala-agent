use sakala_agent_protocol::{HeartbeatPayload, NodeInfo, NodeStatus};
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
            agent_id: config.agent_id.clone(),
            hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_owned()),
            runtime_network: config.runtime_network.clone(),
            capabilities: vec!["noop-runtime".to_owned()],
        },
        sent_at: OffsetDateTime::now_utc(),
    }
}
