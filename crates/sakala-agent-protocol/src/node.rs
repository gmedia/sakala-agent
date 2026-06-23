use serde::{Deserialize, Serialize};

/// Runtime-node details sent with agent heartbeat messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeInfo {
    pub hostname: String,
    pub runtime_network: String,
    pub capabilities: Vec<String>,
}
