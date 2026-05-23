use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    Ready,
    Busy,
    Degraded,
}

/// Runtime-node details sent with agent heartbeat messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeInfo {
    pub agent_id: String,
    pub hostname: String,
    pub runtime_network: String,
    pub capabilities: Vec<String>,
}
