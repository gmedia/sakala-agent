use sakala_agent_protocol::{
    AgentCommand, CommandType, DeployProjectResult, FinalizationDeferredReason,
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
