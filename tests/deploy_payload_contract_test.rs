use sakala_agent_protocol::{
    AgentCommand, BucketName, CommandPoll, CommandType, CrashedWorkloadDetail, DatabaseEndpoint,
    DatabaseExtension, DeployProjectResult, FinalizationDeferredReason, HeartbeatPayload,
    MANAGED_POSTGRES_CAPABILITY, MANAGED_S3_CAPABILITY, ManagedBucketUsage, ManagedDatabaseSize,
    ManagedName, NodePlacement, ObjectStorageMetadata, RequestedImageReuse, RequestedReplicas,
    SleepingPage, WorkloadMetrics, WorkloadReplicas, WorkloadTraffic,
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
fn api_sleep_fixture_asks_for_the_unavailable_page() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/sleep-project-unavailable.json"
    ))
    .expect("API SleepProject fixture must deserialize");

    assert_eq!(command.command_type, CommandType::SleepProject);
    assert!(command.project_id.is_some());
    assert!(command.deployment_id.is_some());
    assert_eq!(
        command
            .sleep_project_payload()
            .expect("SleepProject payload must be valid")
            .page,
        SleepingPage::Unavailable
    );
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
fn workload_traffic_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/workload-traffic.json"))
            .expect("heartbeat fixture must deserialize");
    let traffic: Vec<WorkloadTraffic> =
        serde_json::from_value(heartbeat.metadata["workloads"]["traffic"].clone())
            .expect("traffic must match the DTO");

    assert_eq!(
        heartbeat.metadata["detail_counts"]["traffic"].as_u64(),
        Some(traffic.len() as u64)
    );
    assert!(
        heartbeat
            .node
            .capabilities
            .iter()
            .any(|c| c == "idle-sleep")
    );
    assert!(traffic[0].last_request_at > traffic[1].last_request_at);
    assert!(traffic[0].last_request_at < heartbeat.sent_at);

    // A DTO round trip keeps the exact wire shape the API validates.
    assert_eq!(
        serde_json::to_value(&traffic).expect("traffic serializes"),
        heartbeat.metadata["workloads"]["traffic"]
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
fn deploy_payload_release_command_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-release-command.json"
    ))
    .expect("fixture must deserialize");
    let payload = command.deploy_payload().expect("payload must be valid");

    assert_eq!(
        payload.release_command.as_deref(),
        Some("php artisan migrate --force")
    );
    assert_eq!(
        serde_json::to_value(&payload).expect("payload serializes")["release_command"],
        "php artisan migrate --force"
    );

    let plain: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture must deserialize");
    let plain = plain.deploy_payload().expect("payload must be valid");
    assert_eq!(plain.release_command, None);
    assert!(
        serde_json::to_value(&plain)
            .expect("payload serializes")
            .get("release_command")
            .is_none(),
        "absent stays absent"
    );
}

#[test]
fn deploy_payload_image_reuse_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-image-reuse.json"
    ))
    .expect("fixture must deserialize");
    let payload = command.deploy_payload().expect("payload must be valid");

    let source = uuid::Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID");
    assert_eq!(
        payload.reuse_image_from_deployment_id,
        RequestedImageReuse::Deployment(source)
    );
    assert_eq!(
        serde_json::to_value(&payload).expect("payload serializes")["reuse_image_from_deployment_id"],
        "4f1f21ef-730d-42d5-a46d-d965353cb993"
    );

    let plain: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture must deserialize");
    let plain = plain.deploy_payload().expect("payload must be valid");
    assert_eq!(
        plain.reuse_image_from_deployment_id,
        RequestedImageReuse::Absent
    );
    assert!(
        serde_json::to_value(&plain)
            .expect("payload serializes")
            .get("reuse_image_from_deployment_id")
            .is_none(),
        "absent stays absent"
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

#[test]
fn deploy_payload_replicas_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-replicas.json"
    ))
    .expect("fixture must deserialize");
    let payload = command.deploy_payload().expect("payload must be valid");

    assert!(matches!(payload.replicas, RequestedReplicas::Count(_)));
    assert_eq!(payload.replicas.count().get(), 3);
    assert_eq!(
        serde_json::to_value(&payload).expect("payload serializes")["replicas"],
        3
    );

    let single: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture must deserialize");
    let single = single.deploy_payload().expect("payload must be valid");
    assert_eq!(single.replicas, RequestedReplicas::Absent);
    assert!(
        serde_json::to_value(&single)
            .expect("payload serializes")
            .get("replicas")
            .is_none()
    );
}

#[test]
fn workload_replicas_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/workload-replicas.json"))
            .expect("heartbeat fixture must deserialize");
    let replicas: Vec<WorkloadReplicas> =
        serde_json::from_value(heartbeat.metadata["workloads"]["replicas"].clone())
            .expect("replicas must match the DTO");

    assert_eq!(
        heartbeat.metadata["detail_counts"]["replicas"].as_u64(),
        Some(replicas.len() as u64)
    );
    assert!(heartbeat.node.capabilities.iter().any(|c| c == "replicas"));
    assert!(
        replicas
            .iter()
            .all(|item| item.desired > 1 && item.ready <= item.desired)
    );
    assert_eq!(
        heartbeat.metadata["workloads"]["maximum_replicas_per_project"],
        4
    );
    assert_eq!(
        serde_json::to_value(&replicas).expect("replicas serialize"),
        heartbeat.metadata["workloads"]["replicas"]
    );
}

#[test]
fn node_placement_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/node-placement.json"))
            .expect("heartbeat fixture must deserialize");
    // Parsing through the DTO applies the API's region and edge address rules.
    let placement: NodePlacement = serde_json::from_value(heartbeat.metadata["runtime"].clone())
        .expect("runtime must match the DTO");

    assert_eq!(
        placement.region.as_ref().map(|region| region.as_str()),
        Some("id-jog-1")
    );
    assert_eq!(
        placement
            .edge_address
            .as_ref()
            .map(|address| address.as_str()),
        Some("10.0.47.201:8180")
    );
    assert_eq!(
        heartbeat.metadata["workloads"]["maximum_active_workloads"].as_u64(),
        Some(20)
    );
    assert_eq!(
        serde_json::to_value(&placement).expect("placement serializes"),
        heartbeat.metadata["runtime"]
    );
}

#[test]
fn workload_metrics_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/workload-metrics.json"))
            .expect("heartbeat fixture must deserialize");
    let metrics: Vec<WorkloadMetrics> =
        serde_json::from_value(heartbeat.metadata["workloads"]["metrics"].clone())
            .expect("metrics must match the DTO");

    assert_eq!(
        heartbeat.metadata["detail_counts"]["metrics"].as_u64(),
        Some(metrics.len() as u64)
    );
    assert!(
        heartbeat
            .node
            .capabilities
            .iter()
            .any(|c| c == "workload-metrics")
    );
    // One sample: every entry shares its time, which precedes the heartbeat.
    assert!(
        metrics
            .iter()
            .all(|item| item.sampled_at == metrics[0].sampled_at)
    );
    assert!(metrics[0].sampled_at < heartbeat.sent_at);
    assert!(metrics.iter().all(|item| item.cpu_percent >= 0.0));
    // Percent of one core, so a busy multi-threaded replica passes 100.
    assert!(metrics.iter().any(|item| item.cpu_percent > 100.0));
    assert!(metrics.iter().any(|item| item.memory_limit_bytes.is_none()));
    // Each replica of a deployment has its own entry.
    assert_eq!(
        metrics
            .iter()
            .filter(|item| item.deployment_id == metrics[1].deployment_id)
            .map(|item| item.replica)
            .collect::<Vec<_>>(),
        [0, 1]
    );

    // A DTO round trip keeps the exact wire shape, `null` limit included.
    assert_eq!(
        serde_json::to_value(&metrics).expect("metrics serialize"),
        heartbeat.metadata["workloads"]["metrics"]
    );
}

#[test]
fn api_database_command_fixtures_carry_valid_payloads_without_workload_identity() {
    let poll: CommandPoll =
        serde_json::from_str(include_str!("../examples/commands/database-commands.json"))
            .expect("database command fixture must deserialize");

    let types: Vec<_> = poll
        .data
        .iter()
        .map(|command| command.command_type)
        .collect();
    assert_eq!(
        types,
        [
            CommandType::CreateDatabase,
            CommandType::RotateDatabasePassword,
            CommandType::LockDatabase,
            CommandType::UnlockDatabase,
            CommandType::InstallDatabaseExtension,
            CommandType::DropDatabase,
        ]
    );
    for command in &poll.data {
        assert!(command.project_id.is_none());
        assert!(command.deployment_id.is_none());
    }
    let create = poll.data[0]
        .create_database_payload()
        .expect("CreateDatabase payload must be valid");
    assert_eq!(create.database.as_str(), "p_8k2m4q7x9z1c3v5b6n0d");
    assert_eq!(create.role.as_str(), "p_r4t7y2u9i1o3p5a8s6d0");
    poll.data[1]
        .rotate_database_password_payload()
        .expect("RotateDatabasePassword payload must be valid");
    poll.data[2]
        .database_target_payload()
        .expect("LockDatabase payload must be valid");
    poll.data[3]
        .database_target_payload()
        .expect("UnlockDatabase payload must be valid");
    assert_eq!(
        poll.data[4]
            .install_database_extension_payload()
            .expect("InstallDatabaseExtension payload must be valid")
            .extension,
        DatabaseExtension::Vector
    );
    poll.data[5]
        .drop_database_payload()
        .expect("DropDatabase payload must be valid");
}

#[test]
fn database_host_heartbeat_fixture_matches_the_protocol_types() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/database-host.json"))
            .expect("database host heartbeat fixture must deserialize");

    assert!(
        heartbeat
            .node
            .capabilities
            .contains(&MANAGED_POSTGRES_CAPABILITY.to_owned())
    );
    let endpoint: DatabaseEndpoint =
        serde_json::from_value(heartbeat.metadata["database"]["endpoint"].clone())
            .expect("endpoint");
    assert_eq!(
        endpoint,
        DatabaseEndpoint::new("10.0.47.10", 5432).expect("valid endpoint")
    );
    let databases: Vec<ManagedDatabaseSize> =
        serde_json::from_value(heartbeat.metadata["databases"].clone()).expect("databases");
    assert_eq!(databases.len(), 2);
    assert_eq!(heartbeat.metadata["detail_counts"]["databases"], 2);
    for database in databases {
        ManagedName::parse(&database.name).expect("managed name");
    }
}

#[test]
fn api_bucket_command_fixtures_carry_valid_payloads_without_workload_identity() {
    let poll: CommandPoll =
        serde_json::from_str(include_str!("../examples/commands/bucket-commands.json"))
            .expect("bucket command fixture must deserialize");

    let types: Vec<_> = poll
        .data
        .iter()
        .map(|command| command.command_type)
        .collect();
    assert_eq!(
        types,
        [
            CommandType::CreateBucket,
            CommandType::RotateBucketKey,
            CommandType::SetBucketQuota,
            CommandType::LockBucket,
            CommandType::UnlockBucket,
            CommandType::DeleteBucket,
        ]
    );
    for command in &poll.data {
        assert!(command.project_id.is_none());
        assert!(command.deployment_id.is_none());
    }
    let create = poll.data[0]
        .create_bucket_payload()
        .expect("CreateBucket payload must be valid");
    assert_eq!(create.bucket.as_str(), "b-8k2m4q7x9z1c3v5b6n0d");
    assert_eq!(create.access_key_id.as_str(), "SK7Q2M9X4K1C8V5B3N0D");
    assert_eq!(create.quota_mb.get(), 256);
    poll.data[1]
        .rotate_bucket_key_payload()
        .expect("RotateBucketKey payload must be valid");
    assert_eq!(
        poll.data[2]
            .set_bucket_quota_payload()
            .expect("SetBucketQuota payload must be valid")
            .quota_mb
            .get(),
        512
    );
    for command in &poll.data[3..] {
        command
            .bucket_key_payload()
            .expect("LockBucket, UnlockBucket, and DeleteBucket payloads must be valid");
    }
}

#[test]
fn bucket_command_types_have_the_api_wire_names_in_the_api_order() {
    let names: Vec<_> = [
        CommandType::CreateBucket,
        CommandType::DeleteBucket,
        CommandType::RotateBucketKey,
        CommandType::SetBucketQuota,
        CommandType::LockBucket,
        CommandType::UnlockBucket,
    ]
    .into_iter()
    .map(|command_type| serde_json::to_value(command_type).expect("serialize"))
    .collect();
    assert_eq!(
        names,
        [
            "CreateBucket",
            "DeleteBucket",
            "RotateBucketKey",
            "SetBucketQuota",
            "LockBucket",
            "UnlockBucket",
        ]
    );
}

#[test]
fn object_storage_host_heartbeat_fixture_matches_the_protocol_types() {
    let heartbeat: HeartbeatPayload = serde_json::from_str(include_str!(
        "../examples/heartbeat/object-storage-host.json"
    ))
    .expect("object storage host heartbeat fixture must deserialize");

    assert!(
        heartbeat
            .node
            .capabilities
            .contains(&MANAGED_S3_CAPABILITY.to_owned())
    );
    let object_storage: ObjectStorageMetadata =
        serde_json::from_value(heartbeat.metadata["object_storage"].clone())
            .expect("object_storage");
    assert_eq!(object_storage.endpoint, "https://s3.sakala.app");
    assert_eq!(
        sakala_agent_runtime::object_storage::check_public_endpoint(&object_storage.endpoint),
        Ok(object_storage.endpoint.clone())
    );
    assert_eq!(object_storage.region.as_str(), "us-east-1");
    assert_eq!(object_storage.capacity_mb, 5_120);
    assert!(object_storage.available);
    let buckets: Vec<ManagedBucketUsage> =
        serde_json::from_value(heartbeat.metadata["buckets"].clone()).expect("buckets");
    assert_eq!(buckets.len(), 2);
    assert_eq!(heartbeat.metadata["detail_counts"]["buckets"], 2);
    for bucket in buckets {
        BucketName::parse(&bucket.name).expect("managed bucket name");
    }
}

#[test]
fn bucket_failure_fixture_covers_every_bucket_failure_code() {
    use sakala_agent_protocol::buckets::error_codes;

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../examples/failures/bucket-failures.json"))
            .expect("bucket failure fixture must parse");
    let codes: std::collections::BTreeSet<_> = fixture
        .as_array()
        .expect("list")
        .iter()
        .map(|entry| {
            serde_json::from_value::<CommandType>(entry["type"].clone())
                .expect("a bucket command type");
            assert!(entry["body"]["error_message"].as_str().is_some());
            entry["body"]["error_code"]
                .as_str()
                .expect("code")
                .to_owned()
        })
        .collect();
    for code in [
        error_codes::BUCKET_NAME_INVALID,
        error_codes::BUCKET_KEY_INVALID,
        error_codes::BUCKET_QUOTA_INVALID,
        error_codes::BUCKET_CAPACITY_EXCEEDED,
        error_codes::BUCKET_ADMIN_UNAVAILABLE,
        error_codes::BUCKET_NOT_FOUND,
        error_codes::BUCKET_CONFLICT,
        error_codes::BUCKET_OPERATION_FAILED,
        error_codes::BUCKET_OUTCOME_UNKNOWN,
        "unsupported_runtime_command",
    ] {
        assert!(codes.contains(code), "{code} is missing from the fixture");
    }
    assert_eq!(
        error_codes::BUCKET_OUTCOME_UNKNOWN,
        "bucket_outcome_unknown"
    );
}
