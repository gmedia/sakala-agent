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

/// One entry of `metadata.workloads.traffic`: when the router last served a
/// request for a running managed workload, read from the router's own request
/// counters.
///
/// The value is never earlier than the agent's start for a host the router had
/// already counted at that moment, because counters cannot say when a request
/// arrived before the agent first read them. A workload without any request
/// counted by the router is not listed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadTraffic {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub last_request_at: OffsetDateTime,
}

/// One entry of `metadata.workloads.replicas`: how many containers of a running
/// deployment with more than one replica the runtime sees serving.
///
/// `ready` counts the replicas Docker reports running and not unhealthy. The
/// router stops sending requests to a replica it cannot reach, so the
/// deployment serves while `ready` is above zero.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkloadReplicas {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    /// The replica count the deployment was started with.
    pub desired: u32,
    pub ready: u32,
}

/// One entry of `metadata.workloads.metrics`: what one running container of a
/// managed workload used when the agent last sampled it. Each replica of a
/// deployment has its own entry.
///
/// The list is sampled at most once per `SAKALA_METRICS_INTERVAL_SECONDS` and
/// sent only in the heartbeat that follows a fresh sample, so every entry is a
/// new reading. A heartbeat without the list has no new reading, which must
/// never be read as zero usage.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkloadMetrics {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    /// Index of the container among the replicas of its deployment, from 0.
    /// A deployment without replicas has only replica 0.
    pub replica: u32,
    /// When Docker reported these values. Every entry of one sample shares it.
    #[serde(with = "time::serde::rfc3339")]
    pub sampled_at: OffsetDateTime,
    /// CPU used, in percent of one core, as `docker stats` computes it over
    /// its own short window. A container busy on two cores reports about 200.
    pub cpu_percent: f64,
    /// Memory the container's cgroup uses, as `docker stats` reports it.
    pub memory_bytes: u64,
    /// The container's memory limit, `null` when it runs without one.
    pub memory_limit_bytes: Option<u64>,
    /// Docker's `RestartCount`: how often Docker's restart policy restarted the
    /// container since it was last started on purpose. `docker start` and
    /// `docker restart`, which a wake or `RestartProject` use, reset it to 0.
    pub restart_count: u32,
}
