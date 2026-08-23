use std::{
    collections::HashSet,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::{
    commands::CommandDispatcher,
    ports::{
        CleanupRuntimeRequest, CommandOutput, DeployProjectRequest, InspectProjectRequest,
        ReconcileWorkloadRequest, RepositoryCredential, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, RuntimeReporterFactory, SecretString, WorkloadLifecycleRequest,
    },
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, DeploymentEvent, DeploymentLog, DesiredWorkloadState,
    LogBounds, ReconcileWorkloadAction, RuntimeCleanupTarget,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, ProcessStream, RuntimeError,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
async fn auto_builder_deploys_a_root_dockerfile_and_writes_a_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let executor = DockerRuntimeExecutor::with_runner(config.clone(), runner.clone());

    let output = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("Dockerfile deployment should complete");

    assert_eq!(
        output.result["requested_resources"]["memory_mb"],
        json!(null)
    );
    assert_eq!(output.result["applied_resources"]["memory_mb"], 256);
    assert_eq!(output.result["applied_resources"]["cpu_millis"], 500);
    assert!(
        output.result.get("finalization_deferred").is_none(),
        "normal completion must not request control-plane repair"
    );

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command.args.iter().any(|argument| argument == "buildx")
            && command.args.iter().any(|argument| argument == "--load")
    }));
    assert!(!commands.iter().any(|command| command.program == "railpack"));
    let run = commands
        .iter()
        .find(|command| {
            command.program == "docker" && command.args.first().is_some_and(|arg| arg == "run")
        })
        .expect("docker run should execute");
    assert!(run.args.iter().any(|argument| argument == "256m"));
    assert!(run.args.iter().any(|argument| argument == "0.5"));
    assert!(run.args.iter().any(|argument| argument == "128"));
    assert!(
        run.args
            .iter()
            .any(|argument| argument == "dev.sakala.workload-kind=web")
    );
    assert!(
        run.args
            .iter()
            .any(|argument| argument == "dev.sakala.agent-id=local-agent-01")
    );
    assert!(
        run.args
            .iter()
            .any(|argument| argument == "dev.sakala.domain=portfolio.run.sakala.localhost")
    );
    assert!(
        run.args
            .iter()
            .any(|argument| argument == "dev.sakala.port=3000")
    );
    let build = commands
        .iter()
        .find(|command| {
            command.program == "docker" && command.args.iter().any(|argument| argument == "buildx")
        })
        .expect("docker build should execute");
    assert!(
        build.args.iter().any(
            |argument| argument == "dev.sakala.project-id=ff66ed4a-6303-4be6-8ef4-63c28b112680"
        )
    );
    assert!(build.args.iter().any(
        |argument| argument == "dev.sakala.deployment-id=4f1f21ef-730d-42d5-a46d-d965353cb993"
    ));
    assert!(
        config
            .caddy_sites_dir
            .join("ff66ed4a-6303-4be6-8ef4-63c28b112680.Caddyfile")
            .exists()
    );
    assert!(
        reporter
            .events
            .lock()
            .expect("event lock")
            .iter()
            .any(|event| event.event_type == "deployment.runtime.ready")
    );
}

#[tokio::test]
async fn docker_preflight_checks_required_runtime_dependencies() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::preflight(&executor)
        .await
        .expect("fake runtime dependencies should be checked");

    assert!(!report.has_fatal_failure());
    assert_eq!(report.checks.len(), 9);
    assert!(report.checks.iter().any(|check| check.name == "git"));
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "caddy-routing")
    );
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "runtime-network")
    );
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "workspace-disk")
    );
}

#[tokio::test]
async fn runtime_owns_bounded_host_telemetry_and_caches_dependency_versions() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let config = runtime_config(&temp);
    fs::create_dir_all(&config.workspace_root).expect("workspace should be available");
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let first = RuntimeExecutor::node_telemetry(&executor)
        .await
        .expect("telemetry snapshot should be available");
    assert_eq!(first.runtime_operational, Some(true));
    assert!(
        first.runtime_dependencies["docker"].is_null(),
        "cached version metadata must not define live readiness"
    );
    runner.live_caddy.store(false, Ordering::SeqCst);
    let second = RuntimeExecutor::node_telemetry(&executor)
        .await
        .expect("second telemetry snapshot should be available");

    assert!(first.disk_total_bytes.is_some());
    assert_eq!(second.runtime_operational, Some(false));
    let commands = runner.commands.lock().expect("command lock");
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.program == "git"
                && command.args.iter().any(|arg| arg == "--version"))
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.program == "docker"
                && command.args.first().is_some_and(|arg| arg == "info"))
            .count(),
        2,
        "live Docker readiness must not be cached"
    );
    assert!(
        commands
            .iter()
            .filter(|command| matches!(
                command.program.as_str(),
                "git" | "docker" | "railpack" | "df" | "du"
            ))
            .all(|command| command.timeout == Some(Duration::from_secs(2)))
    );
}

#[tokio::test]
async fn preflight_reports_each_missing_runtime_dependency_as_fatal() {
    for (program, required_argument, expected_check) in [
        ("git", None, "git"),
        ("docker", Some("version"), "docker"),
        ("docker", Some("buildx"), "docker-buildx"),
        ("railpack", None, "railpack"),
    ] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(UnavailableDependencyRunner {
            program,
            required_argument,
        });
        let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);
        let report = RuntimeExecutor::preflight(&executor)
            .await
            .expect("preflight should return a report even when a dependency is absent");
        assert!(report.has_fatal_failure());
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == expected_check && !check.ready && check.fatal)
        );
    }
}

#[tokio::test]
async fn preflight_rejects_a_workspace_path_that_cannot_be_created() {
    let temp = TempDir::new().expect("temp directory should be available");
    let workspace_file = temp.path().join("not-a-directory");
    fs::write(&workspace_file, "occupied").expect("workspace fixture should be written");
    let mut config = runtime_config(&temp);
    config.workspace_root = workspace_file;
    let executor = DockerRuntimeExecutor::with_runner(config, Arc::new(FakeRunner::new(true)));

    let report = RuntimeExecutor::preflight(&executor)
        .await
        .expect("preflight should return a report");

    assert!(report.has_fatal_failure());
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "workspace" && !check.ready && check.fatal)
    );
}

#[tokio::test]
async fn private_checkout_uses_ephemeral_askpass_without_credential_url_or_arguments() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("auto");
    command.payload["repository_access"] = json!("temporary_credential");
    let payload = command
        .deploy_payload()
        .expect("deployment payload should be valid");

    RuntimeExecutor::deploy_project(
        &executor,
        DeployProjectRequest {
            command_id: command.id,
            project_id: command.project_id.expect("project id"),
            deployment_id: command.deployment_id.expect("deployment id"),
            payload,
            repository_credential: Some(RepositoryCredential {
                username: "x-access-token".to_owned(),
                token: SecretString::new("ghs_installation_token"),
            }),
            cancellation: CancellationToken::new(),
        },
        reporter,
    )
    .await
    .expect("temporary credential should authorize checkout");

    let commands = runner.commands.lock().expect("command lock");
    let remote = commands
        .iter()
        .find(|command| command.program == "git" && command.args.iter().any(|arg| arg == "remote"))
        .expect("git remote should be configured");
    assert!(
        remote
            .args
            .iter()
            .any(|argument| argument == "https://github.com/gmedia/example-app.git")
    );
    assert!(!format!("{commands:?}").contains("ghs_installation_token"));
}

#[tokio::test]
async fn private_inspection_uses_the_same_ephemeral_credential_path() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let command = inspect_command();

    let output = RuntimeExecutor::inspect_project(
        &executor,
        InspectProjectRequest {
            command_id: command.id,
            payload: command
                .inspect_payload()
                .expect("inspection payload should be valid"),
            repository_credential: Some(RepositoryCredential {
                username: "x-access-token".to_owned(),
                token: SecretString::new("ghs_inspection_token"),
            }),
            cancellation: CancellationToken::new(),
        },
        reporter,
    )
    .await
    .expect("temporary credential should authorize inspection");

    assert_eq!(output.result["package_manager"], "pnpm");
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "railpack" && command.args.iter().any(|argument| argument == "info")
    }));
    assert!(!format!("{commands:?}").contains("ghs_inspection_token"));
}

#[tokio::test]
async fn control_plane_resource_request_is_applied_and_reported() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("auto");
    command.payload["resources"] = json!({
        "memory_mb": 384,
        "cpu_millis": 750,
        "pids_limit": 200
    });

    let output = dispatch(executor, &command, Arc::clone(&reporter))
        .await
        .expect("resource request within node ceilings should complete");

    assert_eq!(output.result["requested_resources"]["memory_mb"], 384);
    assert_eq!(output.result["applied_resources"]["memory_mb"], 384);
    let commands = runner.commands.lock().expect("command lock");
    let run = commands
        .iter()
        .find(|command| {
            command.program == "docker" && command.args.first().is_some_and(|arg| arg == "run")
        })
        .expect("docker run should execute");
    assert!(run.args.iter().any(|argument| argument == "384m"));
    assert!(run.args.iter().any(|argument| argument == "0.75"));
    assert!(run.args.iter().any(|argument| argument == "200"));
}

#[tokio::test]
async fn resource_request_above_node_maximum_fails_before_process_execution() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("auto");
    command.payload["resources"] = json!({ "memory_mb": 1024 });

    let error = dispatch(executor, &command, Arc::clone(&reporter))
        .await
        .expect_err("resource request above node maximum should fail");

    assert!(error.to_string().contains("exceeds this node's maximum"));
    assert!(runner.commands.lock().expect("command lock").is_empty());
}

#[tokio::test]
async fn build_timeout_reports_a_stable_failure_and_cleans_candidate_artifacts() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_build_delay(Duration::from_secs(60)));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.timeout_safety.max_build_timeout = Duration::from_secs(1);
    let workspace = config.workspace_root.join(
        Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b")
            .expect("command UUID")
            .to_string(),
    );
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let error = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect_err("slow build must exceed its deadline");

    assert_eq!(error.code(), "runtime_timeout");
    assert!(!workspace.exists());
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn agent_restart_during_build_cleans_workspace_and_candidate_artifacts() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_build_delay(Duration::from_secs(60)));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let workspace = config.workspace_root.join(
        Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b")
            .expect("command UUID")
            .to_string(),
    );
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let cancellation = CancellationToken::new();
    let command = deploy_command("auto");
    let task_executor = Arc::clone(&executor);
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        CommandDispatcher::new(task_executor)
            .dispatch(&command, reporter, task_cancellation)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    let error = task
        .await
        .expect("deployment task should join")
        .expect_err("cancelled deployment should fail");

    assert_eq!(error.code(), "runtime_cancelled");
    assert!(!workspace.exists());
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn agent_restart_during_startup_cleans_unactivated_candidate() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_inspect_delay(Duration::from_secs(60)));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let workspace = config.workspace_root.join(
        Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b")
            .expect("command UUID")
            .to_string(),
    );
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let cancellation = CancellationToken::new();
    let command = deploy_command("auto");
    let task_executor = Arc::clone(&executor);
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        CommandDispatcher::new(task_executor)
            .dispatch(&command, reporter, task_cancellation)
            .await
    });

    for _ in 0..100 {
        let candidate_started =
            runner
                .commands
                .lock()
                .expect("command lock")
                .iter()
                .any(|command| {
                    command.program == "docker"
                        && command
                            .args
                            .first()
                            .is_some_and(|argument| argument == "run")
                });
        if candidate_started {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancellation.cancel();
    let error = task
        .await
        .expect("deployment task should join")
        .expect_err("startup interrupted by restart should fail");

    assert_eq!(error.code(), "runtime_cancelled");
    assert!(!workspace.exists());
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "run")
    }));
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn node_limits_concurrent_image_builds() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_build_delay(Duration::from_millis(50)));
    let mut config = runtime_config(&temp);
    config.max_concurrent_builds = 1;
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let first = deploy_command("auto");
    let mut second = deploy_command("auto");
    second.id = Uuid::new_v4();
    second.project_id = Some(Uuid::new_v4());
    second.deployment_id = Some(Uuid::new_v4());
    second.payload["domain"] = json!("second.run.sakala.localhost");

    let first_dispatcher = CommandDispatcher::new(executor.clone());
    let second_dispatcher = CommandDispatcher::new(executor);
    let (first_result, second_result) = tokio::join!(
        first_dispatcher.dispatch(
            &first,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        ),
        second_dispatcher.dispatch(
            &second,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        )
    );

    first_result.expect("first deployment should complete");
    second_result.expect("second deployment should complete");
    assert_eq!(runner.max_concurrent_builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_new_projects_cannot_exceed_container_admission_limit() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(AdmissionRunner::new());
    let mut config = runtime_config(&temp);
    config.max_active_containers = 1;
    config.max_concurrent_builds = 2;
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let first = deploy_command("auto");
    let mut second = deploy_command("auto");
    second.id = Uuid::new_v4();
    second.project_id = Some(Uuid::new_v4());
    second.deployment_id = Some(Uuid::new_v4());
    second.payload["domain"] = json!("second.run.sakala.localhost");
    let first_dispatcher = CommandDispatcher::new(executor.clone());
    let second_dispatcher = CommandDispatcher::new(executor);

    let (first_result, second_result) = tokio::join!(
        first_dispatcher.dispatch(
            &first,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        ),
        second_dispatcher.dispatch(
            &second,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        )
    );

    assert_eq!(
        usize::from(first_result.is_ok()) + usize::from(second_result.is_ok()),
        1
    );
    let failure = first_result
        .err()
        .or_else(|| second_result.err())
        .expect("one deployment must fail");
    assert_eq!(failure.code(), "runtime_capacity_exceeded");
    assert_eq!(runner.active.lock().expect("capacity lock").len(), 1);
}

#[tokio::test]
async fn build_permit_is_released_before_container_readiness() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_build_delay(Duration::from_millis(50))
            .with_inspect_delay(Duration::from_millis(200)),
    );
    let mut config = runtime_config(&temp);
    config.max_concurrent_builds = 1;
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner));
    let first = deploy_command("auto");
    let mut second = deploy_command("auto");
    second.id = Uuid::new_v4();
    second.project_id = Some(Uuid::new_v4());
    second.deployment_id = Some(Uuid::new_v4());
    second.payload["domain"] = json!("second.run.sakala.localhost");
    let started = tokio::time::Instant::now();
    let first_dispatcher = CommandDispatcher::new(executor.clone());
    let second_dispatcher = CommandDispatcher::new(executor);

    let (first_result, second_result) = tokio::join!(
        first_dispatcher.dispatch(
            &first,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        ),
        second_dispatcher.dispatch(
            &second,
            Arc::new(RecordingReporter::default()),
            CancellationToken::new()
        )
    );

    first_result.expect("first deployment should complete");
    second_result.expect("second deployment should complete");
    assert!(started.elapsed() < Duration::from_millis(425));
}

#[tokio::test]
async fn requested_build_timeout_is_used_when_shorter_than_node_maximum() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_build_delay(Duration::from_secs(2)));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.timeout_safety.max_build_timeout = Duration::from_secs(10);
    let executor = DockerRuntimeExecutor::with_runner(config, runner);
    let mut command = deploy_command("auto");
    command.payload["timeouts"] = json!({
        "build_timeout_seconds": 1,
        "start_timeout_seconds": 5,
        "command_timeout_seconds": 10
    });

    let error = dispatch(executor, &command, reporter)
        .await
        .expect_err("requested build deadline must be enforced");

    assert_eq!(error.code(), "runtime_timeout");
    assert!(
        error
            .to_string()
            .contains("deployment-build exceeded its 1s timeout")
    );
}

#[tokio::test]
async fn timeout_above_node_maximum_is_rejected_before_runtime_processes_start() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.timeout_safety.max_build_timeout = Duration::from_secs(10);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());
    let mut command = deploy_command("auto");
    command.payload["timeouts"] = json!({
        "build_timeout_seconds": 11,
        "start_timeout_seconds": 5,
        "command_timeout_seconds": 20
    });

    let error = dispatch(executor, &command, reporter)
        .await
        .expect_err("timeout above node maximum must be rejected");

    assert_eq!(error.code(), "invalid_runtime_command");
    assert!(
        error
            .to_string()
            .contains("build_timeout_seconds (11s) exceeds")
    );
    assert!(runner.commands.lock().expect("command lock").is_empty());
}

#[tokio::test]
async fn requested_start_timeout_covers_health_readiness() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_inspect_delay(Duration::from_secs(2)));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.timeout_safety.max_start_timeout = Duration::from_secs(10);
    let executor = DockerRuntimeExecutor::with_runner(config, runner);
    let mut command = deploy_command("auto");
    command.payload["timeouts"] = json!({
        "build_timeout_seconds": 5,
        "start_timeout_seconds": 1,
        "command_timeout_seconds": 10
    });

    let error = dispatch(executor, &command, reporter)
        .await
        .expect_err("requested start deadline must cover health readiness");

    assert_eq!(error.code(), "runtime_timeout");
    assert!(
        error
            .to_string()
            .contains("deployment-start exceeded its 1s timeout")
    );
}

#[tokio::test]
async fn auto_builder_falls_back_to_railpack_when_dockerfile_is_absent() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("Railpack fallback should complete");

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "railpack" && command.args.iter().any(|argument| argument == "prepare")
    }));
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().starts_with("BUILDKIT_SYNTAX="))
    }));
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command.args.iter().any(|argument| {
                argument == "dev.sakala.deployment-id=4f1f21ef-730d-42d5-a46d-d965353cb993"
            })
    }));
}

#[tokio::test]
async fn project_preview_uses_railpack_info_without_preparing_or_building() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = dispatch(executor, &inspect_command(), Arc::clone(&reporter))
        .await
        .expect("project inspection should complete");

    assert_eq!(output.result["package_manager"], "pnpm");
    assert_eq!(output.result["env_example_found"], true);
    assert_eq!(output.result["railpack"]["provider"], "node");

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "railpack"
            && command.args.iter().any(|argument| argument == "info")
            && command.args.iter().any(|argument| argument == "json")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "railpack" && command.args.iter().any(|argument| argument == "prepare")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "docker" && command.args.iter().any(|argument| argument == "buildx")
    }));
}

#[tokio::test]
async fn executor_rejects_non_github_repository_before_starting_a_process() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("auto");
    command.payload["repository_url"] = json!("https://git.example.internal/project.git");

    let error = dispatch(executor, &command, Arc::clone(&reporter))
        .await
        .expect_err("unsupported repository host must be rejected");

    assert!(
        error
            .to_string()
            .contains("credential-free https://github.com")
    );
    assert!(runner.commands.lock().expect("command lock").is_empty());
}

#[tokio::test]
async fn git_checkout_failure_uses_a_specific_repository_error_code() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FailingGitRunner);
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let error = dispatch(executor, &deploy_command("auto"), reporter)
        .await
        .expect_err("failed fetch should stop deployment");

    assert_eq!(error.code(), "repository_checkout_failed");
}

#[tokio::test]
async fn activated_route_is_not_cleaned_up_when_ready_reporting_fails() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(FailingReadyReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("ready event failure after cutover must not fail deployment");

    assert_eq!(output.result["applied_resources"]["memory_mb"], 256);
    let commands = runner.commands.lock().expect("command lock");
    assert!(!commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
            && command
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().starts_with("sakala-app-"))
    }));
}

#[tokio::test]
async fn cancellation_after_route_cutover_finishes_committed_deployment() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        runtime_config(&temp),
        runner.clone(),
    ));
    let cancellation = CancellationToken::new();
    let reporter = Arc::new(CutoverCancellingReporter {
        cancellation: cancellation.clone(),
    });
    let command = deploy_command("auto");

    let output = CommandDispatcher::new(executor)
        .dispatch(&command, reporter, cancellation)
        .await
        .expect("post-cutover cancellation must finish as committed");

    assert_eq!(output.result["applied_resources"]["memory_mb"], 256);
    let commands = runner.commands.lock().expect("command lock");
    assert!(!commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
            && command.args.iter().any(|argument| {
                argument
                    .to_string_lossy()
                    .starts_with("sakala-app-ff66ed4a")
            })
    }));
}

#[tokio::test]
async fn node_capacity_rejects_a_new_project_but_allows_replacement() {
    let temp = TempDir::new().expect("temp directory should be available");
    let existing_project = "ff66ed4a-6303-4be6-8ef4-63c28b112680";
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(format!(
        "{existing_project}\n11111111-1111-4111-8111-111111111111\n"
    )));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.max_active_containers = 2;
    let executor = DockerRuntimeExecutor::with_runner(config.clone(), runner.clone());

    dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("redeploying an existing project must retain a replacement slot");

    let mut new_project = deploy_command("auto");
    new_project.project_id =
        Some(Uuid::parse_str("22222222-2222-4222-8222-222222222222").expect("project UUID"));
    let executor = DockerRuntimeExecutor::with_runner(config, runner);
    let error = dispatch(executor, &new_project, Arc::clone(&reporter))
        .await
        .expect_err("new project must respect node capacity");

    assert_eq!(error.code(), "runtime_capacity_exceeded");
}

#[tokio::test]
async fn successful_redeploy_removes_only_stopped_previous_containers() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("previous-container\n")
            .with_previous_container_inspection("false\t/old-deployment\n"),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(executor, &deploy_command("auto"), reporter)
        .await
        .expect("redeployment should complete");

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
            && command
                .args
                .iter()
                .any(|argument| argument == "previous-container")
            && !command.args.iter().any(|argument| argument == "--force")
    }));
}

#[tokio::test]
async fn successful_redeploy_stops_and_removes_a_running_previous_container() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("previous-container\n")
            .with_previous_container_inspection("true\t/old-deployment\n"),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(executor, &deploy_command("auto"), reporter)
        .await
        .expect("redeployment should complete");

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "stop")
            && command
                .args
                .iter()
                .any(|argument| argument == "previous-container")
            && command.args.iter().any(|argument| argument == "--time")
    }));
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
            && command
                .args
                .iter()
                .any(|argument| argument == "previous-container")
    }));
}

#[tokio::test]
async fn deployment_refuses_build_when_workspace_disk_is_below_local_floor() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_df(
        "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000 9900 100 99% /\n",
    ));
    let reporter = Arc::new(RecordingReporter::default());
    let mut config = runtime_config(&temp);
    config.min_workspace_free_bytes = 200 * 1_024;
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let error = dispatch(executor, &deploy_command("auto"), reporter)
        .await
        .expect_err("deployment must not begin with critically low workspace disk");

    assert_eq!(error.code(), "runtime_disk_pressure");
    assert!(
        !runner
            .commands
            .lock()
            .expect("command lock")
            .iter()
            .any(|command| {
                command.program == "git" && command.args.iter().any(|argument| argument == "init")
            })
    );
}

#[tokio::test]
async fn reconciliation_detects_stopped_or_incompletely_labeled_containers() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "deadbeef\tExited (1) 2 minutes ago\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\ncandidate\tCreated\t11111111-1111-4111-8111-111111111111\t22222222-2222-4222-8222-222222222222\nmissing\tUp 10 minutes\t\t\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = sakala_agent_core::ports::RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation scan should complete");

    assert_eq!(report.inspected_containers, 3);
    assert_eq!(report.orphans.len(), 3);
    assert!(
        report
            .orphans
            .iter()
            .any(|orphan| orphan.reason == "stale stopped deployment container")
    );
    assert!(
        report
            .orphans
            .iter()
            .any(|orphan| orphan.reason == "dangling candidate container was never started")
    );
}

#[tokio::test]
async fn reconciliation_discovers_valid_managed_workloads() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "running	Up 10 minutes	ff66ed4a-6303-4be6-8ef4-63c28b112680	4f1f21ef-730d-42d5-a46d-d965353cb993\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation scan should complete");

    assert!(report.orphans.is_empty());
    assert_eq!(report.workloads.len(), 1);
    assert_eq!(report.workloads[0].container_id, "running");
    assert_eq!(report.workloads[0].status, "Up 10 minutes");
}

#[tokio::test]
async fn agent_restart_recovers_healthy_workload_and_orphan_visibility() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "healthy\tUp 10 minutes (healthy)\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\ncandidate\tCreated\t11111111-1111-4111-8111-111111111111\t22222222-2222-4222-8222-222222222222\n",
    ));

    let recovered = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let report = RuntimeExecutor::reconcile(&recovered)
        .await
        .expect("new agent process should reconcile existing runtime state");
    let health = RuntimeExecutor::health_snapshot(&recovered)
        .await
        .expect("new agent process should recheck recovered workload health");

    assert_eq!(report.workloads.len(), 1);
    assert!(
        report
            .orphans
            .iter()
            .any(|orphan| orphan.container_id == "candidate")
    );
    assert_eq!(health.len(), 2);
    assert!(
        health
            .iter()
            .any(|snapshot| snapshot.workload.container_id == "healthy" && snapshot.ready)
    );
    assert!(!runner.commands.lock().expect("command lock").iter().any(|command| {
        command.program == "docker"
            && matches!(command.args.first().map(|value| value.to_string_lossy()), Some(ref action) if action == "run" || action == "rm" || action == "start")
    }));
}

#[tokio::test]
async fn agent_restart_restores_bounded_log_follower_without_duplicates() {
    let temp = TempDir::new().expect("temp directory should be available");
    let command_id = Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b")
        .expect("command UUID should be valid");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "healthy\tUp 10 minutes (healthy)\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(format!(
                "healthy\tUp 10 minutes (healthy)\tportfolio.run.sakala.localhost\t3000\t{command_id}\t1024\t20\t65536\n"
            ))
            .with_follow_delay(Duration::from_secs(60)),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let factory = Arc::new(RecordingReporterFactory::default());

    let first = RuntimeExecutor::recover(&executor, Some(factory.clone()))
        .await
        .expect("restart recovery should reattach the log follower");
    let second = RuntimeExecutor::recover(&executor, Some(factory.clone()))
        .await
        .expect("repeated recovery should remain idempotent");

    assert_eq!(first.recovered_execution_records, 1);
    assert_eq!(first.reattached_log_followers, 1);
    assert_eq!(second.recovered_execution_records, 1);
    assert_eq!(second.reattached_log_followers, 0);
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    let followers = runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .filter(|command| {
            command.program == "docker"
                && command.args.iter().any(|argument| argument == "--follow")
        })
        .count();
    assert_eq!(followers, 1);
    RuntimeExecutor::shutdown(&executor)
        .await
        .expect("recovered follower should stop during shutdown");
}

#[tokio::test]
async fn legacy_container_metadata_is_reported_without_aborting_recovery() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "legacy\tUp 10 minutes\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(
                "legacy\tUp 10 minutes\t\t\t\t\t\t\n",
            ),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::recover(
        &executor,
        Some(Arc::new(RecordingReporterFactory::default())),
    )
    .await
    .expect("legacy metadata must not fail global recovery");

    assert_eq!(report.workloads.len(), 1);
    assert_eq!(report.compatibility_issues.len(), 1);
    assert!(
        report.compatibility_issues[0]
            .reason
            .contains("redeploy is required")
    );
    assert_eq!(report.reattached_log_followers, 0);
}

#[tokio::test]
async fn exited_workload_does_not_keep_a_route_alive() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680")
        .expect("project UUID should be valid");
    let config = runtime_config(&temp);
    fs::create_dir_all(&config.caddy_sites_dir).expect("route directory should be created");
    fs::write(
        config
            .caddy_sites_dir
            .join(format!("{project_id}.Caddyfile")),
        format!("# Managed by sakala-agent for project {project_id}.\nexample.test:80 {{}}\n"),
    )
    .expect("managed route should be written");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "deadbeef\tExited (1) 2 minutes ago\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(config, runner);

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation should complete");

    assert_eq!(report.stale_routes.len(), 1);
    assert_eq!(report.stale_routes[0].project_id, project_id);
}

#[tokio::test]
async fn approved_image_cleanup_prunes_only_retained_sakala_dangling_images() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_image_prune_output("Total reclaimed space: 2MB\n"));
    let mut config = runtime_config(&temp);
    config.image_gc_max_age = Duration::from_secs(3_600);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let output = RuntimeExecutor::cleanup_runtime(
        &executor,
        CleanupRuntimeRequest {
            command_id: Uuid::new_v4(),
            approved: true,
            targets: vec![RuntimeCleanupTarget::StaleImages],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("approved cleanup should complete");

    assert_eq!(output.result["reclaimed_image_bytes"], 2 * 1_024 * 1_024);
    let commands = runner.commands.lock().expect("command lock");
    let prune = commands
        .iter()
        .find(|command| {
            command.program == "docker"
                && command
                    .args
                    .first()
                    .is_some_and(|argument| argument == "image")
                && command.args.iter().any(|argument| argument == "prune")
        })
        .expect("Sakala-only image prune should run");
    assert!(
        prune
            .args
            .iter()
            .any(|argument| argument == "label=dev.sakala.managed=true")
    );
    assert!(prune.args.iter().any(|argument| argument == "until=3600s"));
    assert!(!prune.args.iter().any(|argument| argument == "-a"));
}

#[tokio::test]
async fn successful_image_prune_is_not_failed_by_cosmetic_telemetry_format() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_image_prune_output("Total reclaimed space: unknown\n"));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let output = RuntimeExecutor::cleanup_runtime(
        &executor,
        CleanupRuntimeRequest {
            command_id: Uuid::new_v4(),
            approved: true,
            targets: vec![RuntimeCleanupTarget::StaleImages],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("successful prune must not fail on cosmetic output parsing");

    assert_eq!(output.result["reclaimed_image_bytes"], 0);
}

#[tokio::test]
async fn reconciliation_reports_stale_images_before_sakala_only_prune() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let deployment_id = Uuid::new_v4();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_image_list_output(format!("sha256:stale\t{project_id}\t{deployment_id}\n"))
            .with_image_prune_output("Total reclaimed space: 1KB\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation should inventory images before cleanup");

    assert_eq!(report.stale_images.len(), 1);
    assert_eq!(report.stale_images[0].image_id, "sha256:stale");
    assert_eq!(report.stale_images[0].project_id, Some(project_id));
    assert_eq!(report.stale_images[0].deployment_id, Some(deployment_id));
    assert_eq!(report.reclaimed_image_bytes, 0);
}

#[tokio::test]
async fn reconciliation_does_not_report_recent_dangling_image_as_stale() {
    let temp = TempDir::new().expect("temp directory should be available");
    let created = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current time should format");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_image_list_output("sha256:recent\t\t\n")
            .with_image_created_output(format!("{created}\n")),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation should inspect recent image age");

    assert!(report.stale_images.is_empty());
}

#[tokio::test]
async fn approved_cleanup_runs_only_requested_sakala_targets() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_image_prune_output("Total reclaimed space: 2KB\n"));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::cleanup_runtime(
        &executor,
        CleanupRuntimeRequest {
            command_id: Uuid::new_v4(),
            approved: true,
            targets: vec![RuntimeCleanupTarget::StaleImages],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("approved image cleanup should succeed");

    assert_eq!(output.result["reclaimed_image_bytes"], 2 * 1_024);
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command.args.iter().any(|argument| argument == "prune")
            && command
                .args
                .iter()
                .any(|argument| argument == "label=dev.sakala.managed=true")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "docker" && command.args.iter().any(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn reconciliation_applies_only_explicit_safe_workload_actions() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID");
    let deployment_id =
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("candidate\tCreated\tportfolio.run.sakala.localhost\t3000\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id,
            deployment_id,
            desired_state: DesiredWorkloadState::Missing,
            actions: vec![ReconcileWorkloadAction::CleanupFailedCandidate],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("explicit failed candidate cleanup should succeed");

    assert_eq!(
        output.result["actions_applied"][0]["action"],
        "cleanup_failed_candidate"
    );
    assert!(
        runner
            .commands
            .lock()
            .expect("command lock")
            .iter()
            .any(|command| {
                command.program == "docker"
                    && command
                        .args
                        .first()
                        .is_some_and(|argument| argument == "rm")
                    && command.args.iter().any(|argument| argument == "candidate")
                    && !command.args.iter().any(|argument| argument == "--force")
            })
    );
}

#[tokio::test]
async fn reconciliation_restores_known_route_only_when_explicitly_instructed() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID");
    let deployment_id =
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID");
    let config = runtime_config(&temp);
    let route_path = config
        .caddy_sites_dir
        .join(format!("{project_id}.Caddyfile"));
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "running\tUp 10 minutes (healthy)\tportfolio.run.sakala.localhost\t3000\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(config, runner);

    let output = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id,
            deployment_id,
            desired_state: DesiredWorkloadState::Running,
            actions: vec![ReconcileWorkloadAction::RestoreRoute],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("explicit route restoration should succeed");

    assert_eq!(
        output.result["actions_applied"][0]["action"],
        "restore_route"
    );
    assert!(route_path.exists());
    assert!(
        fs::read_to_string(route_path)
            .expect("route should be readable")
            .starts_with(&format!(
                "# Managed by sakala-agent for project {project_id} deployment {deployment_id}."
            ))
    );
}

#[tokio::test]
async fn reconciliation_reports_drift_without_mutating_the_workload() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("workload\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id: "ff66ed4a-6303-4be6-8ef4-63c28b112680"
                .parse()
                .expect("project id"),
            deployment_id: "4f1f21ef-730d-42d5-a46d-d965353cb993"
                .parse()
                .expect("deployment id"),
            desired_state: DesiredWorkloadState::Stopped,
            actions: Vec::new(),
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("reconciliation should report drift");

    assert_eq!(output.result["desired_state"], "stopped");
    assert_eq!(output.result["actual_state"], "running");
    assert_eq!(output.result["in_sync"], false);
    assert!(runner.commands.lock().expect("command lock").iter().all(|command| {
        !(command.program == "docker"
            && matches!(command.args.first().map(|value| value.to_string_lossy()), Some(ref action) if action == "start" || action == "stop" || action == "rm"))
    }));
}

#[tokio::test]
async fn reconciliation_reports_missing_workload_without_creating_one() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id: "ff66ed4a-6303-4be6-8ef4-63c28b112680"
                .parse()
                .expect("project id"),
            deployment_id: "4f1f21ef-730d-42d5-a46d-d965353cb993"
                .parse()
                .expect("deployment id"),
            desired_state: DesiredWorkloadState::Running,
            actions: Vec::new(),
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("missing workload should be reported");

    assert_eq!(output.result["actual_state"], "missing");
    assert_eq!(output.result["in_sync"], false);
    assert!(
        !runner
            .commands
            .lock()
            .expect("command lock")
            .iter()
            .any(|command| {
                command.program == "docker"
                    && command
                        .args
                        .first()
                        .is_some_and(|argument| argument == "run")
            })
    );
}

#[tokio::test]
async fn runtime_health_snapshot_only_checks_active_workloads_and_marks_unhealthy_state() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "healthy\tUp 10 minutes (healthy)\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\nunhealthy\tUp 1 minute (unhealthy)\t11111111-1111-4111-8111-111111111111\t22222222-2222-4222-8222-222222222222\ninvalid\tUp 1 minute\t\t\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let snapshots = RuntimeExecutor::health_snapshot(&executor)
        .await
        .expect("health snapshot should complete");

    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.workload.container_id == "healthy" && snapshot.ready && snapshot.reason.is_none()
    }));
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.workload.container_id == "unhealthy"
            && !snapshot.ready
            && snapshot
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unhealthy"))
    }));
}

#[tokio::test]
async fn sleep_stops_managed_workload_without_removing_the_container() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("sleeping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n"),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::sleep_project(&executor, lifecycle_request(), reporter)
        .await
        .expect("sleep should stop an existing workload");

    assert_eq!(output.result["status"], "sleeping");
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "stop")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn stop_removes_workload_container_but_not_its_image() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("stopping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n"),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::stop_project(&executor, lifecycle_request(), reporter)
        .await
        .expect("stop should remove an existing workload");

    assert_eq!(output.result["status"], "stopped");
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
    }));
    assert!(!commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "image")
            && command.args.get(1).is_some_and(|argument| argument == "rm")
    }));
}

#[tokio::test]
async fn stop_missing_workload_removes_stale_owned_route_idempotently() {
    let temp = TempDir::new().expect("temp directory should be available");
    let config = runtime_config(&temp);
    let request = lifecycle_request();
    fs::create_dir_all(&config.caddy_sites_dir).expect("route directory should be created");
    let route = config
        .caddy_sites_dir
        .join(format!("{}.Caddyfile", request.project_id));
    fs::write(
        &route,
        format!(
            "# Managed by sakala-agent for project {} deployment {}.\nexample.test:80 {{}}\n",
            request.project_id, request.deployment_id
        ),
    )
    .expect("managed route should be written");
    let executor = DockerRuntimeExecutor::with_runner(config, Arc::new(FakeRunner::new(true)));

    let output =
        RuntimeExecutor::stop_project(&executor, request, Arc::new(RecordingReporter::default()))
            .await
            .expect("stop should be idempotent when the workload is absent");

    assert_eq!(output.result["status"], "already_stopped");
    assert!(!route.exists());
}

#[tokio::test]
async fn stale_stop_command_cannot_remove_a_newer_deployment_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let config = runtime_config(&temp);
    let stale_request = lifecycle_request();
    let current_deployment = Uuid::new_v4();
    fs::create_dir_all(&config.caddy_sites_dir).expect("route directory should be created");
    let route = config
        .caddy_sites_dir
        .join(format!("{}.Caddyfile", stale_request.project_id));
    let current_route = format!(
        "# Managed by sakala-agent for project {} deployment {current_deployment}.\nexample.test:80 {{}}\n",
        stale_request.project_id
    );
    fs::write(&route, &current_route).expect("current route should be written");
    let executor = DockerRuntimeExecutor::with_runner(config, Arc::new(FakeRunner::new(true)));

    let output = RuntimeExecutor::stop_project(
        &executor,
        stale_request,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("stale stop should remain idempotent");

    assert_eq!(output.result["status"], "already_stopped");
    assert_eq!(
        fs::read_to_string(route).expect("current route must remain"),
        current_route
    );
}

#[tokio::test]
async fn sleep_missing_workload_reports_drift_instead_of_success() {
    let temp = TempDir::new().expect("temp directory should be available");
    let executor =
        DockerRuntimeExecutor::with_runner(runtime_config(&temp), Arc::new(FakeRunner::new(true)));

    let error = RuntimeExecutor::sleep_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("sleep cannot retain an absent container");

    assert_eq!(error.code(), "runtime_workload_not_found");
}

#[tokio::test]
async fn wake_starts_stopped_workload_and_restores_its_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "sleeping\tExited (0) 1 minute ago\tportfolio.run.sakala.localhost\t3000\n",
    ));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route_path = config
        .caddy_sites_dir
        .join("ff66ed4a-6303-4be6-8ef4-63c28b112680.Caddyfile");
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let output = RuntimeExecutor::wake_project(&executor, lifecycle_request(), reporter)
        .await
        .expect("wake should restore a sleeping workload");

    assert_eq!(output.result["status"], "ready");
    assert!(route_path.exists());
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "start")
    }));
}

#[tokio::test]
async fn restart_rechecks_readiness_and_revalidates_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_docker_ps(
            "running\tUp 1 minute (healthy)\tportfolio.run.sakala.localhost\t3000\n",
        ));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route_path = config
        .caddy_sites_dir
        .join("ff66ed4a-6303-4be6-8ef4-63c28b112680.Caddyfile");
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let output = RuntimeExecutor::restart_project(&executor, lifecycle_request(), reporter)
        .await
        .expect("restart should revalidate the workload route");

    assert_eq!(output.result["status"], "ready");
    assert!(route_path.exists());
    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "restart")
    }));
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "inspect")
    }));
}

#[tokio::test]
async fn explicit_health_check_returns_structured_unready_state_for_stopped_workload() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "stopped\tExited (0) 1 minute ago\tportfolio.run.sakala.localhost\t3000\n",
    ));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let output = RuntimeExecutor::health_check(&executor, lifecycle_request(), reporter)
        .await
        .expect("health check should report a stopped workload without failing the command");

    assert_eq!(output.result["running"], false);
    assert_eq!(output.result["ready"], false);
    assert_eq!(output.result["docker_status"], "Exited (0) 1 minute ago");
    assert!(
        output.result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not running"))
    );
}

#[tokio::test]
async fn ready_deployment_starts_log_follower_that_runtime_shutdown_can_stop() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        runtime_config(&temp),
        runner.clone(),
    ));
    let runtime: Arc<dyn RuntimeExecutor> = executor.clone();

    CommandDispatcher::new(runtime)
        .dispatch(&deploy_command("auto"), reporter, CancellationToken::new())
        .await
        .expect("deployment should start a log follower");
    tokio::task::yield_now().await;
    executor
        .shutdown()
        .await
        .expect("runtime shutdown should stop log followers");

    let commands = runner.commands.lock().expect("command lock");
    assert!(commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "logs")
            && command.args.iter().any(|argument| argument == "--follow")
            && command.timeout_disabled
    }));
}

#[tokio::test]
async fn repeated_redeploy_soak_keeps_runtime_artifacts_bounded() {
    const ITERATIONS: usize = 100;
    const MEMORY_GROWTH_LIMIT_BYTES: u64 = 64 * 1_024 * 1_024;

    let temp = TempDir::new().expect("temp directory should be available");
    let mut config = runtime_config(&temp);
    config.image_gc_max_age = Duration::from_secs(1);
    let workspace_root = config.workspace_root.clone();
    let sites_dir = config.caddy_sites_dir.clone();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("previous-container\n")
            .with_previous_container_inspection("false\t/previous\n")
            .with_image_list_output("sha256:stale\t\t\n")
            .with_image_prune_output("Total reclaimed space: 1KB\n"),
    );
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let memory_before = resident_memory_bytes();

    for iteration in 0..ITERATIONS {
        let mut command = deploy_command("auto");
        command.id = Uuid::new_v4();
        command.deployment_id = Some(Uuid::new_v4());
        command.payload["domain"] = json!(format!("soak-{iteration}.run.sakala.localhost"));
        CommandDispatcher::new(executor.clone())
            .dispatch(
                &command,
                Arc::new(RecordingReporter::default()),
                CancellationToken::new(),
            )
            .await
            .expect("soak deployment should complete");
    }
    let reconciliation = RuntimeExecutor::reconcile(executor.as_ref())
        .await
        .expect("soak reconciliation should complete");
    let cleanup = RuntimeExecutor::cleanup_runtime(
        executor.as_ref(),
        CleanupRuntimeRequest {
            command_id: Uuid::new_v4(),
            approved: true,
            targets: vec![RuntimeCleanupTarget::StaleImages],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("soak image cleanup should complete");
    RuntimeExecutor::shutdown(executor.as_ref())
        .await
        .expect("soak runtime should shut down cleanly");

    let commands = runner.commands.lock().expect("command lock");
    let runs = commands
        .iter()
        .filter(|command| {
            command.program == "docker"
                && command
                    .args
                    .first()
                    .is_some_and(|argument| argument == "run")
        })
        .count();
    let followers = commands
        .iter()
        .filter(|command| {
            command.program == "docker"
                && command.args.iter().any(|argument| argument == "--follow")
        })
        .count();
    let retired_containers = commands
        .iter()
        .filter(|command| {
            command.program == "docker"
                && command
                    .args
                    .first()
                    .is_some_and(|argument| argument == "rm")
                && command
                    .args
                    .iter()
                    .any(|argument| argument == "previous-container")
        })
        .count();
    drop(commands);

    assert_eq!(runs, ITERATIONS);
    assert_eq!(
        followers, ITERATIONS,
        "one follower is allowed per deployment"
    );
    assert_eq!(retired_containers, ITERATIONS);
    assert_eq!(reconciliation.stale_images.len(), 1);
    assert_eq!(cleanup.result["reclaimed_image_bytes"], 1_024);
    assert_eq!(directory_entry_count(&workspace_root), 0);
    assert_eq!(regular_file_count(&sites_dir), 1);
    if let (Some(before), Some(after)) = (memory_before, resident_memory_bytes()) {
        assert!(
            after.saturating_sub(before) <= MEMORY_GROWTH_LIMIT_BYTES,
            "resident memory grew from {before} to {after} bytes"
        );
    }
}

async fn dispatch<R>(
    executor: DockerRuntimeExecutor,
    command: &AgentCommand,
    reporter: Arc<R>,
) -> Result<CommandOutput, RuntimeExecutionError>
where
    R: RuntimeReporter + 'static,
{
    CommandDispatcher::new(Arc::new(executor))
        .dispatch(command, reporter, CancellationToken::new())
        .await
}

fn runtime_config(temp: &TempDir) -> DockerRuntimeConfig {
    DockerRuntimeConfig {
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        health_attempts: 1,
        health_interval: Duration::ZERO,
        ..DockerRuntimeConfig::default()
    }
}

fn resident_memory_bytes() -> Option<u64> {
    let pages = fs::read_to_string("/proc/self/statm")
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()?;
    pages.checked_mul(4_096)
}

fn regular_file_count(root: &std::path::Path) -> usize {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count()
}

fn directory_entry_count(root: &std::path::Path) -> usize {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .count()
}

fn deploy_command(builder: &str) -> AgentCommand {
    AgentCommand {
        id: Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b").expect("command UUID"),
        command_type: CommandType::DeployProject,
        status: CommandStatus::Pending,
        project_id: Some(
            Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID"),
        ),
        deployment_id: Some(
            Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID"),
        ),
        payload: json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.run.sakala.localhost",
            "container_port": 3000,
            "builder": builder,
            "environment": { "APP_ENV": "production" }
        }),
    }
}

fn inspect_command() -> AgentCommand {
    AgentCommand {
        id: Uuid::parse_str("4ee40ba8-c613-455c-8d97-376b7a522994").expect("command UUID"),
        command_type: CommandType::InspectProject,
        status: CommandStatus::Pending,
        project_id: None,
        deployment_id: None,
        payload: json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567"
        }),
    }
}

fn lifecycle_request() -> WorkloadLifecycleRequest {
    WorkloadLifecycleRequest {
        command_id: Uuid::new_v4(),
        project_id: Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID"),
        deployment_id: Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993")
            .expect("deployment UUID"),
        cancellation: CancellationToken::new(),
    }
}

struct FakeRunner {
    dockerfile: bool,
    commands: Mutex<Vec<CommandSpec>>,
    docker_ps_stdout: String,
    workload_lookup_stdout: Option<String>,
    df_stdout: String,
    build_delay: Option<Duration>,
    inspect_delay: Option<Duration>,
    previous_container_inspection: Option<String>,
    image_prune_stdout: String,
    image_list_stdout: String,
    image_created_stdout: String,
    follow_delay: Option<Duration>,
    live_caddy: AtomicBool,
    active_builds: AtomicUsize,
    max_concurrent_builds: AtomicUsize,
}

struct AdmissionRunner {
    inner: FakeRunner,
    active: Mutex<HashSet<String>>,
}

impl AdmissionRunner {
    fn new() -> Self {
        Self {
            inner: FakeRunner::new(true).with_build_delay(Duration::from_millis(25)),
            active: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl ProcessRunner for AdmissionRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let capacity_probe = spec.program == "docker"
            && spec.args.first().is_some_and(|argument| argument == "ps")
            && !spec.args.iter().any(|argument| argument == "--all")
            && spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().contains("dev.sakala.project-id"));
        if capacity_probe {
            self.inner
                .commands
                .lock()
                .expect("command lock")
                .push(spec.clone());
            let stdout = self
                .active
                .lock()
                .expect("capacity lock")
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                stdout,
                stderr: String::new(),
            });
        }

        let output = self.inner.run(spec, sink).await?;
        if output.success
            && spec.program == "docker"
            && spec.args.first().is_some_and(|argument| argument == "run")
            && let Some(project) = spec.args.iter().find_map(|argument| {
                argument
                    .to_string_lossy()
                    .strip_prefix("dev.sakala.project-id=")
                    .map(str::to_owned)
            })
        {
            self.active.lock().expect("capacity lock").insert(project);
        }
        Ok(output)
    }
}

struct FailingGitRunner;

struct UnavailableDependencyRunner {
    program: &'static str,
    required_argument: Option<&'static str>,
}

#[async_trait]
impl ProcessRunner for UnavailableDependencyRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let unavailable = spec.program == self.program
            && self
                .required_argument
                .is_none_or(|argument| spec.args.iter().any(|value| value == argument));
        Ok(ProcessOutput {
            success: !unavailable,
            code: (!unavailable).then_some(0).or(Some(127)),
            stdout: if spec.program == "df" {
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000000 1000 9999000 1% /\n".to_owned()
            } else {
                String::new()
            },
            stderr: String::new(),
        })
    }
}

#[async_trait]
impl ProcessRunner for FailingGitRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        Ok(ProcessOutput {
            success: spec.program != "git",
            code: (spec.program != "git").then_some(0).or(Some(1)),
            stdout: if spec.program == "df" {
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000000 1000 9999000 1% /\n".to_owned()
            } else {
                String::new()
            },
            stderr: String::new(),
        })
    }
}

impl FakeRunner {
    fn new(dockerfile: bool) -> Self {
        Self {
            dockerfile,
            commands: Mutex::new(Vec::new()),
            docker_ps_stdout: String::new(),
            workload_lookup_stdout: None,
            df_stdout: "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000000 1000 9999000 1% /\n".to_owned(),
            build_delay: None,
            inspect_delay: None,
            previous_container_inspection: None,
            image_prune_stdout: String::new(),
            image_list_stdout: String::new(),
            image_created_stdout: "2020-01-01T00:00:00Z\n".to_owned(),
            follow_delay: None,
            live_caddy: AtomicBool::new(true),
            active_builds: AtomicUsize::new(0),
            max_concurrent_builds: AtomicUsize::new(0),
        }
    }

    fn with_docker_ps(mut self, stdout: impl Into<String>) -> Self {
        self.docker_ps_stdout = stdout.into();
        self
    }

    fn with_workload_lookup(mut self, stdout: impl Into<String>) -> Self {
        self.workload_lookup_stdout = Some(stdout.into());
        self
    }

    fn with_follow_delay(mut self, delay: Duration) -> Self {
        self.follow_delay = Some(delay);
        self
    }

    fn with_df(mut self, stdout: impl Into<String>) -> Self {
        self.df_stdout = stdout.into();
        self
    }

    fn with_build_delay(mut self, delay: Duration) -> Self {
        self.build_delay = Some(delay);
        self
    }

    fn with_inspect_delay(mut self, delay: Duration) -> Self {
        self.inspect_delay = Some(delay);
        self
    }

    fn with_previous_container_inspection(mut self, stdout: impl Into<String>) -> Self {
        self.previous_container_inspection = Some(stdout.into());
        self
    }

    fn with_image_prune_output(mut self, stdout: impl Into<String>) -> Self {
        self.image_prune_stdout = stdout.into();
        self
    }

    fn with_image_list_output(mut self, stdout: impl Into<String>) -> Self {
        self.image_list_stdout = stdout.into();
        self
    }

    fn with_image_created_output(mut self, stdout: impl Into<String>) -> Self {
        self.image_created_stdout = stdout.into();
        self
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        self.commands
            .lock()
            .expect("command lock")
            .push(spec.clone());

        let live_caddy_probe = spec.program == "docker"
            && spec
                .args
                .first()
                .is_some_and(|argument| argument == "inspect")
            && spec
                .args
                .iter()
                .any(|argument| argument == "{{.State.Running}}");
        if live_caddy_probe && !self.live_caddy.load(Ordering::SeqCst) {
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "Caddy is not running".to_owned(),
            });
        }

        if spec.program == "docker"
            && spec.args.iter().any(|argument| argument == "buildx")
            && let Some(delay) = self.build_delay
        {
            let active = self.active_builds.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent_builds
                .fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            self.active_builds.fetch_sub(1, Ordering::SeqCst);
        }
        if spec.program == "docker"
            && spec.args.iter().any(|argument| argument == "inspect")
            && let Some(delay) = self.inspect_delay
        {
            tokio::time::sleep(delay).await;
        }
        if spec.program == "docker"
            && spec.args.iter().any(|argument| argument == "--follow")
            && let Some(delay) = self.follow_delay
        {
            tokio::time::sleep(delay).await;
        }

        if spec.program == "git" && spec.args.iter().any(|argument| argument == "init") {
            let source = spec.args.last().expect("git init target");
            fs::create_dir_all(source).expect("fake repository should be created");
            if self.dockerfile {
                fs::write(
                    std::path::Path::new(source).join("Dockerfile"),
                    "FROM scratch\n",
                )
                .expect("fake Dockerfile should be created");
            }
            fs::write(std::path::Path::new(source).join("package.json"), "{}\n")
                .expect("fake package manifest should be created");
            fs::write(
                std::path::Path::new(source).join("pnpm-lock.yaml"),
                "lockfileVersion: 9\n",
            )
            .expect("fake lockfile should be created");
            fs::write(
                std::path::Path::new(source).join(".env.example"),
                "APP_URL=\n",
            )
            .expect("fake env example should be created");
        }

        if spec.program == "railpack" && spec.args.iter().any(|argument| argument == "info") {
            let output_index = spec
                .args
                .iter()
                .position(|argument| argument == "--out")
                .expect("railpack info should define an output path");
            fs::write(
                &spec.args[output_index + 1],
                r#"{"provider":"node","version":"22"}"#,
            )
            .expect("fake Railpack info should be written");
        }

        let stdout = if spec.program == "docker" && spec.args.first().is_some_and(|arg| arg == "ps")
        {
            if spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().contains("dev.sakala.command-id"))
            {
                self.workload_lookup_stdout
                    .as_deref()
                    .unwrap_or(&self.docker_ps_stdout)
            } else {
                self.docker_ps_stdout.as_str()
            }
        } else if live_caddy_probe {
            "true\n"
        } else if spec.program == "docker"
            && spec
                .args
                .iter()
                .any(|argument| argument == "{{.State.Running}}\t{{.Name}}")
        {
            self.previous_container_inspection
                .as_deref()
                .unwrap_or("true\t/running\n")
        } else if spec.program == "docker"
            && spec
                .args
                .first()
                .is_some_and(|argument| argument == "image")
            && spec.args.iter().any(|argument| argument == "inspect")
        {
            &self.image_created_stdout
        } else if spec.program == "docker" && spec.args.iter().any(|argument| argument == "inspect")
        {
            "running\n"
        } else if spec.program == "docker"
            && spec
                .args
                .first()
                .is_some_and(|argument| argument == "image")
            && spec.args.iter().any(|argument| argument == "prune")
        {
            &self.image_prune_stdout
        } else if spec.program == "docker"
            && spec
                .args
                .first()
                .is_some_and(|argument| argument == "image")
            && spec.args.iter().any(|argument| argument == "ls")
        {
            &self.image_list_stdout
        } else if spec.program == "docker" && spec.args.iter().any(|argument| argument == "logs") {
            "application listening\n"
        } else if spec.program == "df" {
            &self.df_stdout
        } else {
            ""
        };
        for line in stdout.lines() {
            sink.line(ProcessStream::Stdout, line).await?;
        }

        Ok(ProcessOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }
}

#[derive(Default)]
struct RecordingReporter {
    events: Mutex<Vec<DeploymentEvent>>,
    logs: Mutex<Vec<DeploymentLog>>,
}

#[derive(Default)]
struct RecordingReporterFactory {
    created: AtomicUsize,
}

impl RuntimeReporterFactory for RecordingReporterFactory {
    fn reporter(&self, _command_id: Uuid, _log_bounds: LogBounds) -> Arc<dyn RuntimeReporter> {
        self.created.fetch_add(1, Ordering::SeqCst);
        Arc::new(RecordingReporter::default())
    }
}

#[async_trait]
impl RuntimeReporter for RecordingReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        self.events.lock().expect("event lock").push(event);
        Ok(())
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        self.logs.lock().expect("log lock").push(log);
        Ok(())
    }
}

#[derive(Default)]
struct FailingReadyReporter {
    logs: Mutex<Vec<DeploymentLog>>,
}

struct CutoverCancellingReporter {
    cancellation: CancellationToken,
}

#[async_trait]
impl RuntimeReporter for CutoverCancellingReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        if event.event_type == "deployment.runtime.ready" {
            self.cancellation.cancel();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

#[async_trait]
impl RuntimeReporter for FailingReadyReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        if event.event_type == "deployment.runtime.ready" {
            return Err(RuntimeExecutionError::reporting("ready event rejected"));
        }
        Ok(())
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        self.logs.lock().expect("log lock").push(log);
        Ok(())
    }
}
