use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{NodeInfo, NodeStatus};

/// Periodic presence report sent by an agent to the dashboard.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HeartbeatPayload {
    pub status: NodeStatus,
    pub node: NodeInfo,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
}
