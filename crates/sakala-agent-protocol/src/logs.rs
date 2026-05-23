use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
    System,
}

/// Redacted output associated with a deployment command.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeploymentLog {
    pub command_id: Uuid,
    pub stream: LogStream,
    pub line: String,
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
}
