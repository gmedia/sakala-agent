use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
    pub stream: LogStream,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
}
