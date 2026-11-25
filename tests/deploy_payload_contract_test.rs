use sakala_agent_protocol::{
    AgentCommand, BucketName, CommandPoll, CommandType, CrashedWorkloadDetail, DatabaseEndpoint,
    DatabaseExtension, DeployProjectResult, FinalizationDeferredReason, HeartbeatPayload,
    MANAGED_POSTGRES_CAPABILITY, MANAGED_S3_CAPABILITY, MANAGED_VALKEY_CAPABILITY,
    ManagedBucketUsage, ManagedDatabaseSize, ManagedName, ManagedValkeyUsage, NodePlacement,
    ObjectStorageMetadata, RequestedImageReuse, RequestedProcesses, RequestedReplicas,
    SCHEDULED_JOBS_CAPABILITY, SleepingPage, VALKEY_NAME_PREFIX, ValkeyHostMetadata, ValkeyMode,
    WORKER_PROCESSES_CAPABILITY, WorkloadMetrics, WorkloadReplicas, WorkloadTraffic,
    valkey_resource_name,
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

#[test]
fn deploy_payload_processes_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-processes.json"
    ))
    .expect("DeployProject fixture with processes must deserialize");
    let payload = command.deploy_payload().expect("payload");

    let workers = payload.processes.workers();
    assert_eq!(workers.len(), 2);
    assert_eq!(workers[0].name, "queue");
    assert_eq!(workers[0].replicas, 1);
    assert_eq!(workers[0].resources.memory_mb, Some(192));
    assert_eq!(workers[0].resources.pids_limit, None);
    assert_eq!(
        workers[1].replicas, 0,
        "a definition kept without containers"
    );

    // A round trip keeps the exact list the API sent, `null` limits included.
    assert_eq!(
        serde_json::to_value(&payload).expect("serialize")["processes"],
        command.payload["processes"]
    );
}

#[test]
fn a_deploy_payload_without_processes_stays_unchanged_on_the_wire() {
    let command: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture");
    let payload = command.deploy_payload().expect("payload");

    assert_eq!(payload.processes, RequestedProcesses::Absent);
    assert!(
        serde_json::to_value(&payload)
            .expect("serialize")
            .get("processes")
            .is_none()
    );
}

#[test]
fn run_job_fixture_matches_the_contract() {
    let poll: CommandPoll = serde_json::from_str(include_str!("../examples/commands/run-job.json"))
        .expect("poll response with RunJob must deserialize");
    let command = &poll.data[0];

    assert_eq!(command.command_type, CommandType::RunJob);
    assert!(command.project_id.is_some());
    assert!(command.deployment_id.is_some());
    let payload = command.run_job_payload().expect("RunJob payload");
    assert_eq!(payload.command, "php artisan schedule:run");
    assert_eq!(payload.timeout_seconds, Some(300));
    assert!(matches!(
        payload.reuse_image_from_deployment_id,
        RequestedImageReuse::Deployment(_)
    ));
    assert_eq!(payload.container_port, 8000);
    assert_eq!(payload.log_bounds.max_total_bytes, Some(10_485_760));
    assert_eq!(
        serde_json::to_value(&payload).expect("serialize"),
        command.payload
    );
}

#[test]
fn run_job_is_the_last_command_type_and_keeps_its_wire_name() {
    assert_eq!(
        serde_json::to_value(CommandType::RunJob).expect("serialize"),
        "RunJob"
    );
    assert_eq!(WORKER_PROCESSES_CAPABILITY, "worker-processes");
    assert_eq!(SCHEDULED_JOBS_CAPABILITY, "scheduled-jobs");
}

#[test]
fn worker_heartbeat_fixture_matches_the_heartbeat_contract() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/worker-processes.json"))
            .expect("heartbeat fixture must deserialize");
    for capability in [WORKER_PROCESSES_CAPABILITY, SCHEDULED_JOBS_CAPABILITY] {
        assert!(heartbeat.node.capabilities.iter().any(|c| c == capability));
    }
    let crashed: Vec<CrashedWorkloadDetail> =
        serde_json::from_value(heartbeat.metadata["workloads"]["crashed_details"].clone())
            .expect("crashed details");
    assert_eq!(crashed[0].process.as_deref(), Some("queue"));
    let metrics: Vec<WorkloadMetrics> =
        serde_json::from_value(heartbeat.metadata["workloads"]["metrics"].clone())
            .expect("metrics");
    assert_eq!(metrics[0].process, None, "the web replica");
    assert_eq!(metrics[1].process.as_deref(), Some("queue"));
    assert_eq!(
        heartbeat.metadata["workloads"]["unhealthy_details"][0]["process"],
        "queue"
    );

    // A round trip keeps the wire shape: no `process` key for the web replica.
    assert_eq!(
        serde_json::to_value(&metrics).expect("serialize"),
        heartbeat.metadata["workloads"]["metrics"]
    );
    assert_eq!(
        serde_json::to_value(&crashed).expect("serialize"),
        heartbeat.metadata["workloads"]["crashed_details"]
    );
}

#[test]
fn api_valkey_command_fixtures_carry_valid_payloads_without_workload_identity() {
    let poll: CommandPoll =
        serde_json::from_str(include_str!("../examples/commands/valkey-commands.json"))
            .expect("Valkey command fixture must deserialize");

    let types: Vec<_> = poll
        .data
        .iter()
        .map(|command| command.command_type)
        .collect();
    assert_eq!(
        types,
        [
            CommandType::CreateValkey,
            CommandType::RotateValkeyPassword,
            CommandType::ResizeValkey,
            CommandType::LockValkey,
            CommandType::UnlockValkey,
            CommandType::DeleteValkey,
        ]
    );
    for command in &poll.data {
        assert!(command.project_id.is_none());
        assert!(command.deployment_id.is_none());
    }
    let create = poll.data[0]
        .create_valkey_payload()
        .expect("CreateValkey payload must be valid");
    assert_eq!(
        create.valkey_id.to_string(),
        "0b5c3f9e-1d2a-4e8b-9c7d-6a5f4e3d2c1b"
    );
    assert_eq!(create.mode, ValkeyMode::Persistent);
    assert_eq!(create.maxmemory_mb.get(), 64);
    poll.data[1]
        .rotate_valkey_password_payload()
        .expect("RotateValkeyPassword payload must be valid");
    assert_eq!(
        poll.data[2]
            .resize_valkey_payload()
            .expect("ResizeValkey payload must be valid")
            .maxmemory_mb
            .get(),
        128
    );
    for command in &poll.data[3..] {
        command
            .valkey_target_payload()
            .expect("LockValkey, UnlockValkey, and DeleteValkey payloads must be valid");
    }
}

#[test]
fn valkey_command_types_have_their_wire_names() {
    let names: Vec<_> = [
        CommandType::CreateValkey,
        CommandType::DeleteValkey,
        CommandType::RotateValkeyPassword,
        CommandType::LockValkey,
        CommandType::UnlockValkey,
        CommandType::ResizeValkey,
    ]
    .into_iter()
    .map(|command_type| serde_json::to_value(command_type).expect("serialize"))
    .collect();
    assert_eq!(
        names,
        [
            "CreateValkey",
            "DeleteValkey",
            "RotateValkeyPassword",
            "LockValkey",
            "UnlockValkey",
            "ResizeValkey",
        ]
    );
    assert_eq!(MANAGED_VALKEY_CAPABILITY, "managed-valkey");
}

#[test]
fn deploy_payload_valkey_fixture_matches_the_contract() {
    let command: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-valkey.json"
    ))
    .expect("DeployProject fixture with valkey must deserialize");
    let payload = command.deploy_payload().expect("payload");

    let valkey = payload.valkey.as_ref().expect("valkey");
    assert_eq!(
        valkey.network,
        valkey_resource_name(valkey.valkey_id().expect("store ID"))
    );
    assert!(valkey.network.starts_with(VALKEY_NAME_PREFIX));
    assert_eq!(valkey.host.to_string(), "10.213.0.2");
    // The variables come from the control plane; the agent only joins the
    // network. REDIS_URL carries no database number.
    assert_eq!(payload.environment["REDIS_HOST"], valkey.host.to_string());
    assert!(payload.environment["REDIS_URL"].ends_with("@10.213.0.2:6379"));
    assert_eq!(
        serde_json::to_value(&payload).expect("serialize")["valkey"],
        command.payload["valkey"]
    );
}

#[test]
fn run_job_valkey_fixture_matches_the_contract() {
    let poll: CommandPoll =
        serde_json::from_str(include_str!("../examples/commands/run-job-valkey.json"))
            .expect("poll response with RunJob must deserialize");
    let payload = poll.data[0].run_job_payload().expect("RunJob payload");

    assert_eq!(
        payload
            .valkey
            .as_ref()
            .map(|valkey| valkey.host.to_string()),
        Some("10.213.0.2".to_owned())
    );
    assert_eq!(
        serde_json::to_value(&payload).expect("serialize"),
        poll.data[0].payload
    );
}

#[test]
fn payloads_without_valkey_stay_unchanged_on_the_wire() {
    let deploy: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("fixture");
    let payload = deploy.deploy_payload().expect("payload");
    assert!(payload.valkey.is_none());
    assert!(
        serde_json::to_value(&payload)
            .expect("serialize")
            .get("valkey")
            .is_none()
    );
    let job: CommandPoll =
        serde_json::from_str(include_str!("../examples/commands/run-job.json")).expect("fixture");
    let payload = job.data[0].run_job_payload().expect("payload");
    assert!(payload.valkey.is_none());
}

#[test]
fn valkey_host_heartbeat_fixture_matches_the_protocol_types() {
    let heartbeat: HeartbeatPayload =
        serde_json::from_str(include_str!("../examples/heartbeat/valkey-host.json"))
            .expect("Valkey host heartbeat fixture must deserialize");

    assert!(
        heartbeat
            .node
            .capabilities
            .contains(&MANAGED_VALKEY_CAPABILITY.to_owned())
    );
    let host: ValkeyHostMetadata =
        serde_json::from_value(heartbeat.metadata["valkey"].clone()).expect("valkey");
    assert_eq!(host.capacity_mb, 384);
    assert!(host.available);
    let stores: Vec<ManagedValkeyUsage> =
        serde_json::from_value(heartbeat.metadata["valkeys"].clone()).expect("valkeys");
    assert_eq!(stores.len(), 2);
    assert_eq!(heartbeat.metadata["detail_counts"]["valkeys"], 2);
    assert_eq!(stores[0].mode, ValkeyMode::Persistent);
    assert_eq!(stores[0].aof_last_write_status.as_deref(), Some("ok"));
    assert_eq!(stores[1].status, "exited");
    assert_eq!(
        stores[1].used_memory, None,
        "readings of a stopped store are null"
    );
    // Every field is present, `null` included.
    for store in heartbeat.metadata["valkeys"].as_array().expect("list") {
        assert_eq!(store.as_object().expect("object").len(), 12);
    }
}

#[test]
fn valkey_failure_fixture_covers_every_valkey_failure_code() {
    use sakala_agent_protocol::valkey::error_codes;

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../examples/failures/valkey-failures.json"))
            .expect("Valkey failure fixture must parse");
    let codes: std::collections::BTreeSet<_> = fixture
        .as_array()
        .expect("list")
        .iter()
        .map(|entry| {
            serde_json::from_value::<CommandType>(entry["type"].clone()).expect("a command type");
            assert!(entry["body"]["error_message"].as_str().is_some());
            entry["body"]["error_code"]
                .as_str()
                .expect("code")
                .to_owned()
        })
        .collect();
    for code in [
        error_codes::VALKEY_MODE_INVALID,
        error_codes::VALKEY_MEMORY_INVALID,
        error_codes::VALKEY_PASSWORD_INVALID,
        error_codes::VALKEY_CAPACITY_EXCEEDED,
        error_codes::VALKEY_NOT_FOUND,
        error_codes::VALKEY_CONFLICT,
        error_codes::VALKEY_SIZE_BELOW_USAGE,
        error_codes::VALKEY_IN_USE,
        error_codes::VALKEY_UNAVAILABLE,
        error_codes::VALKEY_OPERATION_FAILED,
        error_codes::VALKEY_NETWORK_UNAVAILABLE,
        "unsupported_runtime_command",
        "invalid_runtime_command",
    ] {
        assert!(codes.contains(code), "{code} is missing from the fixture");
    }
}

#[test]
fn custom_domain_examples_match_the_wire_contract() {
    use sakala_agent_protocol::{
        CUSTOM_DOMAINS_CAPABILITY, ProjectDomainsUpdate, RequestedCustomDomains,
        UpdateProjectDomainsResult,
    };

    assert_eq!(CUSTOM_DOMAINS_CAPABILITY, "custom-domains");
    let deploy: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/deploy-project-custom-domains.json"
    ))
    .expect("deploy example");
    assert_eq!(
        deploy.deploy_payload().expect("payload").custom_domains,
        RequestedCustomDomains::List(vec!["www.contoh.com".to_owned(), "contoh.com".to_owned()])
    );
    // The plain deployment example keeps whatever the route has.
    let plain: AgentCommand =
        serde_json::from_str(include_str!("../examples/commands/deploy-project.json"))
            .expect("deploy example");
    assert_eq!(
        plain.deploy_payload().expect("payload").custom_domains,
        RequestedCustomDomains::Absent
    );

    let wake: AgentCommand = serde_json::from_str(include_str!(
        "../examples/commands/wake-project-custom-domains.json"
    ))
    .expect("wake example");
    assert_eq!(wake.command_type, CommandType::WakeProject);
    assert_eq!(
        wake.custom_domains_payload()
            .expect("payload")
            .custom_domains,
        RequestedCustomDomains::List(vec!["www.contoh.com".to_owned()])
    );

    let update: serde_json::Value = serde_json::from_str(include_str!(
        "../examples/commands/update-project-domains.json"
    ))
    .expect("update example");
    let command: AgentCommand =
        serde_json::from_value(update["command"].clone()).expect("update command");
    assert_eq!(command.command_type, CommandType::UpdateProjectDomains);
    assert_eq!(
        command
            .update_project_domains_payload()
            .expect("payload")
            .custom_domains,
        ["www.contoh.com", "contoh.com"]
    );
    let result: UpdateProjectDomainsResult =
        serde_json::from_value(update["complete"]["result"].clone()).expect("result");
    assert_eq!(result.status, ProjectDomainsUpdate::Updated);
    assert_eq!(result.custom_domains, ["contoh.com", "www.contoh.com"]);
    assert_eq!(update["event"]["event_type"], "workload.domains.updated");
}

#[test]
fn custom_domain_failure_examples_hold_every_code_with_the_agents_own_messages() {
    use sakala_agent_protocol::custom_domains::error_codes;
    use sakala_agent_runtime::{CustomDomainPolicy, RuntimeError};

    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../examples/failures/custom-domain-failures.json"
    ))
    .expect("custom domain failure fixture must parse");
    let entries = fixture.as_array().expect("list");
    let policy = CustomDomainPolicy::default();
    for entry in entries {
        serde_json::from_value::<CommandType>(entry["type"].clone()).expect("a command type");
        let code = entry["body"]["error_code"].as_str().expect("code");
        let message = entry["body"]["error_message"].as_str().expect("message");
        // The messages are what this build sends, not paraphrases.
        let expected = match code {
            error_codes::CUSTOM_DOMAIN_INVALID => {
                let domain = message
                    .split('"')
                    .nth(1)
                    .expect("a quoted domain in the message");
                let rejection = policy.check(domain).expect_err("the example is refused");
                RuntimeError::CustomDomainInvalid {
                    domain: format!("{domain:?}"),
                    reason: rejection.reason().to_owned(),
                }
                .to_string()
            }
            error_codes::CUSTOM_DOMAIN_TAKEN => RuntimeError::CustomDomainTaken {
                domain: "www.contoh.com".to_owned(),
            }
            .to_string(),
            _ => message.to_owned(),
        };
        assert_eq!(message, expected, "{code}");
    }
    let codes = entries
        .iter()
        .map(|entry| entry["body"]["error_code"].as_str().expect("code"))
        .collect::<std::collections::BTreeSet<_>>();
    for code in [
        error_codes::CUSTOM_DOMAIN_INVALID,
        error_codes::CUSTOM_DOMAIN_TAKEN,
        "invalid_runtime_command",
        "unsupported_runtime_command",
    ] {
        assert!(codes.contains(code), "{code} is missing from the fixture");
    }
}
