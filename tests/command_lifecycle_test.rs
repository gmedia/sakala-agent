use sakala_agent_core::commands::lifecycle::can_transition;
use sakala_agent_protocol::{CommandStatus, CommandType};

#[test]
fn command_types_use_dashboard_json_names() {
    let json = serde_json::to_string(&CommandType::DeployProject).expect("type should serialize");
    let restored: CommandType = serde_json::from_str("\"RefreshRoute\"").expect("valid type");

    assert_eq!(json, "\"DeployProject\"");
    assert_eq!(restored, CommandType::RefreshRoute);
}

#[test]
fn command_statuses_use_dashboard_json_names_and_valid_lifecycle() {
    let status: CommandStatus = serde_json::from_str("\"Claimed\"").expect("valid status");

    assert_eq!(status, CommandStatus::Claimed);
    assert!(can_transition(
        CommandStatus::Pending,
        CommandStatus::Claimed
    ));
    assert!(can_transition(
        CommandStatus::Running,
        CommandStatus::Succeeded
    ));
    assert!(!can_transition(
        CommandStatus::Succeeded,
        CommandStatus::Running
    ));
}
