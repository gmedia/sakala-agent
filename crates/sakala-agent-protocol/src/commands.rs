use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Operation requested by the dashboard for execution on a runtime node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum CommandType {
    DeployProject,
    RestartProject,
    StopProject,
    SleepProject,
    WakeProject,
    HealthCheck,
    RefreshRoute,
}

/// State owned by the dashboard while an agent processes a command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum CommandStatus {
    Pending,
    Claimed,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}

/// Command record returned by the dashboard polling endpoint.
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
