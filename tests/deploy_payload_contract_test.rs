use sakala_agent_protocol::{
    AgentCommand, CommandPoll, CommandType, CrashedWorkloadDetail, DeployProjectResult,
    FinalizationDeferredReason, HeartbeatPayload,
};

#[test]
fn api_deploy_project_fixture_deserializes_with_runtime_limits() {
    let command: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("API DeployProject fixture must deserialize");

    assert_eq!(command.command_type, CommandType::DeployProject);
    let payload = command
        .deploy_payload()
        .expect("DeployProject payload must be valid");
    assert_eq!(payload.timeouts.build_timeout_seconds, Some(600));
    assert_eq!(payload.timeouts.start_timeout_seconds, Some(120));
    assert_eq!(payload.timeouts.command_timeout_seconds, Some(900));
    assert_eq!(payload.log_bounds.max_line_length, Some(4096));
    assert_eq!(payload.log_bounds.max_batch_lines, Some(500));
    assert_eq!(payload.log_bounds.max_total_bytes, Some(10_485_760));
}

#[test]
fn api_lifecycle_fixtures_identify_one_managed_workload() {
    for (fixture, expected_type) in [
        (
            include_str!("../examples/commands/restart-project.json"),
            CommandType::RestartProject,
        ),
        (
            include_str!("../examples/commands/stop-project.json"),
            CommandType::StopProject,
        ),
    ] {
        let command: AgentCommand =
            serde_json::from_str(fixture).expect("API lifecycle fixture must deserialize");
        assert_eq!(command.command_type, expected_type);
        assert!(command.project_id.is_some());
        assert!(command.deployment_id.is_some());
        assert_eq!(command.payload, serde_json::json!({}));
    }
}

#[test]
fn deploy_completion_defaults_to_no_deferred_finalization_signal() {
    let result: DeployProjectResult = serde_json::from_value(serde_json::json!({
        "requested_resources": {
            "memory_mb": 256,
            "cpu_millis": 500,
            "pids_limit": 128
        },
        "applied_resources": {
            "memory_mb": 256,
            "cpu_millis": 500,
            "pids_limit": 128
        }
    }))
    .expect("legacy normal completion must remain compatible");

    assert!(!result.finalization_deferred);
    assert_eq!(result.finalization_deferred_reason, None);
    let wire = serde_json::to_value(result).expect("completion should serialize");
    assert!(wire.get("finalization_deferred").is_none());
    assert!(wire.get("finalization_deferred_reason").is_none());

    let deferred: DeployProjectResult = serde_json::from_value(serde_json::json!({
        "requested_resources": {},
        "applied_resources": {
            "memory_mb": 256,
            "cpu_millis": 500,
            "pids_limit": 128
        },
        "finalization_deferred": true,
        "finalization_deferred_reason": "grace_elapsed"
    }))
    .expect("deferred completion must match protocol");
    assert_eq!(
        deferred.finalization_deferred_reason,
        Some(FinalizationDeferredReason::GraceElapsed)
    );
}

#[test]
fn api_reconciliation_and_cleanup_fixtures_match_protocol_revision_four() {
    let reconciliation: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/reconcile-workload.json"))
            .expect("API ReconcileWorkload fixture must deserialize");
    let cleanup: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/cleanup-runtime.json"))
            .expect("API CleanupRuntime fixture must deserialize");

    assert_eq!(reconciliation.command_type, CommandType::ReconcileWorkload);
    assert_eq!(cleanup.command_type, CommandType::CleanupRuntime);
    assert_eq!(
        reconciliation
            .reconcile_workload_payload()
            .expect("reconciliation payload")
            .actions
            .len(),
        2
    );
    let cleanup = cleanup
        .cleanup_runtime_payload()
        .expect("cleanup payload must be valid");
    assert!(cleanup.approved);
    assert_eq!(cleanup.targets.len(), 3);
}

#[test]
fn crashed_workloads_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/crashed-workloads.json"))
            .expect("heartbeat fixture must deserialize");
    let details: Vec<CrashedWorkloadDetail> =
        serde_json::from_value(heartbeat.metadata["workloads"]["crashed_details"].clone())
            .expect("crashed details must match the DTO");

    assert_eq!(
        heartbeat.metadata["detail_counts"]["crashed_details"].as_u64(),
        Some(details.len() as u64)
    );
    assert_eq!(details[0].exit_code, Some(137));
    assert!(details[0].oom_killed);
    assert!(details[0].finished_at.is_some());
    assert_eq!(details[1].exit_code, None);
    assert!(!details[1].oom_killed);
    assert_eq!(details[1].finished_at, None);
    assert_eq!(details[0].restart_count, 8);
    assert!(!details[0].running);
    assert_eq!(details[1].restart_count, 3);
    assert!(details[1].running);

    // A DTO round trip keeps the exact wire shape the API validates.
    assert_eq!(
        serde_json::to_value(&details).expect("details serialize"),
        heartbeat.metadata["workloads"]["crashed_details"]
    );
}

#[test]
fn deploy_payload_without_build_config_stays_valid_and_unchanged_on_the_wire() {
    let command: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture must deserialize");
    let payload = command.deploy_payload().expect("payload must be valid");

    assert_eq!(payload.build_command, None);
    assert_eq!(payload.start_command, None);
    assert_eq!(payload.root_directory, None);
    let wire = serde_json::to_value(&payload).expect("payload serializes");
    for field in ["build_command", "start_command", "root_directory"] {
        assert!(wire.get(field).is_none(), "{field} must stay absent");
    }
}

#[test]
fn deploy_payload_build_config_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-build-config.json"
    ))
    .expect("fixture must deserialize");
    let payload = command.deploy_payload().expect("payload must be valid");

    assert_eq!(payload.container_port, 8080);
    assert_eq!(payload.root_directory.as_deref(), Some("apps/web"));
    assert_eq!(payload.build_command.as_deref(), Some("pnpm run build:web"));
    assert_eq!(
        payload.start_command.as_deref(),
        Some("node dist/server.js")
    );
}

#[test]
fn poll_response_reads_cancel_requested_and_treats_absence_as_empty() {
    let poll: CommandPoll = serde_json::from_str(include_str!(
        "../examples/commands/poll-cancel-requested.json"
    ))
    .expect("fixture must deserialize");
    assert!(poll.data.is_empty());
    assert_eq!(poll.cancel_requested.len(), 1);

    let poll: CommandPoll = serde_json::from_str(r#"{ "data": [] }"#).expect("no cancel field");
    assert!(poll.cancel_requested.is_empty());
}
