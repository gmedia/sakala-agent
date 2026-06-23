use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::{NodeInfo, NodeStatus};

/// Periodic presence report sent by an agent to the control plane.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HeartbeatPayload {
    pub status: NodeStatus,
    #[serde(flatten)]
    pub node: NodeInfo,
    #[serde(default)]
    pub metadata: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
}
