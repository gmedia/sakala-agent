use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

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

/// One entry of `metadata.workloads.crashed_details`: a managed workload Docker
/// is restarting right now, or one that keeps crashing and has not stayed up for
/// the agent's stable window since its last crash.
///
/// A single restart that ended with a running app is not listed, and neither is
/// any `exited` container, because Docker cannot tell a stop from a crash there.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CrashedWorkloadDetail {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    /// Exit code of the last run, `null` when Docker no longer reports one.
    pub exit_code: Option<i32>,
    /// Whether the kernel killed the last run for memory, as far as Docker shows.
    pub oom_killed: bool,
    /// When the last run ended, `null` when Docker has no usable timestamp.
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    /// How many times Docker restarted it since it was last started on purpose.
    pub restart_count: u32,
    /// Whether a process is running at this moment, between two crashes.
    pub running: bool,
}
