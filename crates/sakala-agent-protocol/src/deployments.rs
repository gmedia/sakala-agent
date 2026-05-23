use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentEventLevel {
    Info,
    Warning,
    Error,
}

/// Lifecycle event reported while a command is executing.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeploymentEvent {
    pub command_id: Uuid,
    pub level: DeploymentEventLevel,
    pub message: String,
    #[serde(default)]
    pub metadata: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}
