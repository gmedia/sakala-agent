use sakala_agent_protocol::{AgentCommand, CommandType};

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
