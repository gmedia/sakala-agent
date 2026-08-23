use sakala_agent_core::commands::lifecycle::can_transition;
use sakala_agent_protocol::{
    CleanupRuntimePayload, CommandStatus, CommandType, DeployProjectPayload, DeploymentBuilder,
    DeploymentEvent, DeploymentLog, HeartbeatPayload, InspectProjectPayload,
    ReconcileWorkloadAction, ReconcileWorkloadPayload, RuntimeCleanupTarget,
};
use serde_json::json;

#[test]
fn command_types_use_control_plane_json_names() {
    let json = serde_json::to_string(&CommandType::DeployProject).expect("type should serialize");
    let restored: CommandType = serde_json::from_str("\"RefreshRoute\"").expect("valid type");

    assert_eq!(json, "\"DeployProject\"");
    assert_eq!(restored, CommandType::RefreshRoute);

    let inspect = serde_json::to_string(&CommandType::InspectProject)
        .expect("inspection type should serialize");
    assert_eq!(inspect, "\"InspectProject\"");
}

#[test]
fn reconciliation_and_cleanup_contracts_require_explicit_actions() {
    let reconciliation: ReconcileWorkloadPayload = serde_json::from_value(json!({
        "desired_state": "running",
        "actions": ["restart_log_follower", "restore_route"]
    }))
    .expect("reconciliation payload should deserialize");
    let cleanup: CleanupRuntimePayload = serde_json::from_value(json!({
        "approved": true,
        "targets": ["stale_workspaces", "stale_images", "stale_routes"]
    }))
    .expect("cleanup payload should deserialize");

    assert_eq!(
        reconciliation.actions,
        vec![
            ReconcileWorkloadAction::RestartLogFollower,
            ReconcileWorkloadAction::RestoreRoute,
        ]
    );
    assert!(cleanup.approved);
    assert_eq!(
        cleanup.targets,
        vec![
            RuntimeCleanupTarget::StaleWorkspaces,
            RuntimeCleanupTarget::StaleImages,
            RuntimeCleanupTarget::StaleRoutes,
        ]
    );
    assert_eq!(
        serde_json::to_string(&CommandType::CleanupRuntime)
            .expect("cleanup command should serialize"),
        "\"CleanupRuntime\""
    );
}

#[test]
fn inspect_payload_is_separate_from_deployment_configuration() {
    let payload: InspectProjectPayload = serde_json::from_value(json!({
        "repository_url": "https://github.com/gmedia/example-app.git",
        "commit_sha": "0123456789abcdef0123456789abcdef01234567"
    }))
    .expect("inspect payload should deserialize");

    assert_eq!(
        payload.repository_url,
        "https://github.com/gmedia/example-app.git"
    );
    assert_eq!(payload.commit_sha.len(), 40);
}

#[test]
fn deploy_payload_defaults_to_auto_builder_and_port_3000() {
    let payload: DeployProjectPayload = serde_json::from_value(json!({
        "repository_url": "https://github.com/gmedia/example-app.git",
        "commit_sha": "0123456789abcdef0123456789abcdef01234567",
        "domain": "demo.run.sakala.localhost"
    }))
    .expect("deploy payload should deserialize");

    assert_eq!(payload.builder, DeploymentBuilder::Auto);
    assert_eq!(payload.container_port, 3000);
    assert!(payload.environment.is_empty());
    assert_eq!(payload.resources.memory_mb, None);
}

#[test]
fn command_statuses_use_control_plane_json_names_and_valid_lifecycle() {
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

#[test]
fn report_payloads_match_the_sakala_api_contract() {
    let heartbeat: HeartbeatPayload = serde_json::from_value(json!({
        "status": "ready",
        "hostname": "runtime-01",
        "runtime_network": "sakala-runtime",
        "capabilities": ["noop-runtime"],
        "metadata": { "version": "0.1.0" },
        "sent_at": "2026-06-23T08:00:00Z"
    }))
    .expect("heartbeat payload should deserialize");
    let event: DeploymentEvent = serde_json::from_value(json!({
        "type": "runtime.noop.completed",
        "level": "info",
        "message": "Noop runtime completed.",
        "metadata": {},
        "occurred_at": "2026-06-23T08:00:01Z"
    }))
    .expect("event payload should deserialize");
    let log: DeploymentLog = serde_json::from_value(json!({
        "stream": "system",
        "message": "No host operation executed.",
        "recorded_at": "2026-06-23T08:00:02Z"
    }))
    .expect("log payload should deserialize");

    let heartbeat_json = serde_json::to_value(heartbeat).expect("heartbeat should serialize");
    let event_json = serde_json::to_value(event).expect("event should serialize");
    let log_json = serde_json::to_value(log).expect("log should serialize");

    assert_eq!(heartbeat_json["hostname"], "runtime-01");
    assert!(heartbeat_json.get("node").is_none());
    assert!(heartbeat_json.get("agent_id").is_none());
    assert_eq!(event_json["type"], "runtime.noop.completed");
    assert!(event_json.get("command_id").is_none());
    assert_eq!(log_json["message"], "No host operation executed.");
    assert!(log_json.get("line").is_none());
    assert!(log_json.get("command_id").is_none());
}
