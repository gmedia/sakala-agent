use serde::{Deserialize, Serialize};

/// Runtime-node details sent with agent heartbeat messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeInfo {
    pub hostname: String,
    pub runtime_network: String,
    pub capabilities: Vec<String>,
}

/// Desired lifecycle state returned by the control plane during agent bootstrap.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredNodeLifecycleState {
    Active,
    Draining,
    Drained,
    Maintenance,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeLifecyclePayload {
    pub desired_state: DesiredNodeLifecycleState,
}
