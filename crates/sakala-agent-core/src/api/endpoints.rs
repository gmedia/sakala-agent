use uuid::Uuid;

pub const COMMANDS: &str = "/api/agent/v1/commands";
pub const HEARTBEAT: &str = "/api/agent/v1/heartbeat";

#[must_use]
pub fn repository_credential(command_id: Uuid) -> String {
    format!("/api/agent/v1/commands/{command_id}/repository-credential")
}

#[must_use]
pub fn command_action(command_id: Uuid, action: &str) -> String {
    format!("/api/agent/v1/commands/{command_id}/{action}")
}
