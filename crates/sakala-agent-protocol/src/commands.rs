use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{CommandStatus, DeployProjectPayload, InspectProjectPayload};

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
}

/// Body sent when an agent completes a command successfully.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CompleteCommandPayload {
    #[serde(default)]
    pub result: Value,
}
