use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{CommandStatus, DeployProjectPayload, InspectProjectPayload};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredWorkloadState {
    Running,
    Stopped,
    Missing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileWorkloadAction {
    RestartLogFollower,
    CleanupFailedCandidate,
    RestoreRoute,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcileWorkloadPayload {
    pub desired_state: DesiredWorkloadState,
    #[serde(default)]
    pub actions: Vec<ReconcileWorkloadAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCleanupTarget {
    StaleWorkspaces,
    StaleImages,
    StaleRoutes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CleanupRuntimePayload {
    /// Explicit destructive-action gate supplied by the control plane.
    pub approved: bool,
    pub targets: Vec<RuntimeCleanupTarget>,
}

/// Operation requested by the control plane for execution on a runtime node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum CommandType {
    InspectProject,
    DeployProject,
    RestartProject,
    StopProject,
    SleepProject,
    WakeProject,
    HealthCheck,
    RefreshRoute,
    ReconcileWorkload,
    CleanupRuntime,
    DrainNode,
    ResumeNode,
}

/// Command record returned by the control-plane polling endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentCommand {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub command_type: CommandType,
    pub status: CommandStatus,
    pub project_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    #[serde(default)]
    pub payload: Value,
}

impl AgentCommand {
    pub fn deploy_payload(&self) -> Result<DeployProjectPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn inspect_payload(&self) -> Result<InspectProjectPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn reconcile_workload_payload(
        &self,
    ) -> Result<ReconcileWorkloadPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn cleanup_runtime_payload(&self) -> Result<CleanupRuntimePayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

/// Body sent when an agent completes a command successfully.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CompleteCommandPayload {
    #[serde(default)]
    pub result: Value,
}
