use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use sakala_agent_core::{
    commands::CommandDispatcher,
    logs::redactor::ValueRedactor,
    ports::{
        CleanupRuntimeRequest, CommandOutput, DeployProjectRequest, InspectProjectRequest,
        ReconcileWorkloadRequest, RepositoryCredential, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, RuntimeReporterFactory, SecretString, WorkloadLifecycleRequest,
    },
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, CrashedWorkloadDetail, DeploymentEvent,
    DeploymentLog, DesiredWorkloadState, LogBounds, LogStream, ReconcileWorkloadAction,
    RuntimeCleanupTarget,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, ProcessStream, RuntimeError,
};
use serde_json::json;
use tempfile::TempDir;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
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
    assert_eq!(report.checks.len(), 12);
    let domains = report
        .checks
        .iter()
        .find(|check| check.name == "project-domains")
        .expect("configured project domains are reported");
    assert!(domains.ready && !domains.fatal);
    assert!(
        domains
            .detail
            .contains("run.sakala.localhost, run.staging.sakala.dev, run.sakala.dev"),
        "{}",
        domains.detail
    );
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "project-networks" && !check.fatal && check.ready)
    );
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "railpack-sandbox" && check.fatal && check.ready)
    );
    assert!(!report.checks.iter().any(|check| check.name == "railpack"));
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "caddy-http-probe" && !check.fatal)
    );
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
        is_sandbox(command) && command.args.iter().any(|argument| argument == "info")
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
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());
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
        is_sandbox(command) && command.args.iter().any(|argument| argument == "prepare")
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
        is_sandbox(command)
            && command.args.iter().any(|argument| argument == "info")
            && command.args.iter().any(|argument| argument == "json")
    }));
    assert!(!commands.iter().any(|command| {
        is_sandbox(command) && command.args.iter().any(|argument| argument == "prepare")
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
async fn failed_previous_cleanup_becomes_a_post_commit_finalization_error() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let deployment_id = Uuid::new_v4();
    let mut command = deploy_command("auto");
    command.project_id = Some(project_id);
    command.deployment_id = Some(deployment_id);
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("previous-container\n")
            .with_previous_container_inspection("true\t/old-deployment\n")
            .with_failed_previous_stop(),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = config
        .caddy_sites_dir
        .join(format!("{project_id}.Caddyfile"));
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let error = dispatch(executor, &command, Arc::clone(&reporter))
        .await
        .expect_err("failed cleanup must enter post-commit repair machinery");
    tokio::task::yield_now().await;

    assert_eq!(error.code(), "runtime_container_failed");
    assert!(reporter.deployment_committed());
    assert_eq!(
        reporter
            .committed_output()
            .expect("cutover result must be retained")
            .result["applied_resources"]["memory_mb"],
        256
    );
    assert!(route.exists(), "the committed route must stay active");
    assert!(
        reporter
            .events
            .lock()
            .expect("event lock")
            .iter()
            .any(|event| event.event_type == "deployment.runtime.ready"),
        "ready reporting must still be attempted before propagating cleanup failure"
    );
    assert!(
        reporter
            .logs
            .lock()
            .expect("log lock")
            .iter()
            .any(|log| log.message.contains("previous container cleanup warning"))
    );
    assert!(
        runner
            .commands
            .lock()
            .expect("command lock")
            .iter()
            .any(|process| process.program == "docker"
                && process
                    .args
                    .first()
                    .is_some_and(|argument| argument == "logs")
                && process.args.iter().any(|argument| argument == "--follow")),
        "log follower must be started before cleanup failure is propagated"
    );
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
    // The follower runs as a spawned task; give it a moment to issue its
    // `docker logs --follow` before counting.
    tokio::time::sleep(Duration::from_millis(50)).await;
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

fn image_removals(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .filter(|command| {
            command.program == "docker"
                && command
                    .args
                    .first()
                    .is_some_and(|argument| argument == "image")
                && command.args.get(1).is_some_and(|argument| argument == "rm")
        })
        .map(|command| string_arguments(command)[2..].to_vec())
        .collect()
}

async fn clean_images(executor: &DockerRuntimeExecutor) -> CommandOutput {
    RuntimeExecutor::cleanup_runtime(
        executor,
        CleanupRuntimeRequest {
            command_id: Uuid::new_v4(),
            approved: true,
            targets: vec![RuntimeCleanupTarget::StaleImages],
            cancellation: CancellationToken::new(),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("approved cleanup should complete")
}

/// Every deployment image keeps its own tag, so none is ever dangling and
/// `docker image prune` without `-a` removed nothing at all.
#[tokio::test]
async fn approved_image_cleanup_removes_old_project_images_but_keeps_active_and_previous() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project = Uuid::new_v4();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("active-container\n")
            .with_images(&[
                ("sha256:oldest", project, "2020-01-01T00:00:00Z", 1_000),
                ("sha256:previous", project, "2020-01-02T00:00:00Z", 2_000),
                ("sha256:active", project, "2020-01-03T00:00:00Z", 3_000),
            ])
            .with_image_users("sha256:active\n"),
    );
    let mut config = runtime_config(&temp);
    config.image_gc_max_age = Duration::from_secs(3_600);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let output = clean_images(&executor).await;

    assert_eq!(output.result["removed_images"], 1);
    assert_eq!(output.result["reclaimed_image_bytes"], 1_000);
    assert_eq!(
        image_removals(&runner),
        [[format!("sakala/project-{project}:sha256:oldest-tag")]]
    );
    let commands = runner.commands.lock().expect("command lock");
    let listing = commands
        .iter()
        .find(|command| {
            command.program == "docker" && command.args.iter().any(|argument| argument == "ls")
        })
        .expect("image listing");
    let listing = string_arguments(listing);
    assert!(listing.contains(&"label=dev.sakala.managed=true".to_owned()));
    assert!(!listing.iter().any(|argument| argument.contains(".Label")));
    assert!(!commands.iter().any(|command| {
        command
            .args
            .iter()
            .any(|argument| argument == "prune" || argument == "--force")
    }));
}

#[tokio::test]
async fn reconciliation_reports_the_images_cleanup_would_remove_without_removing_them() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_images(&[
                ("sha256:stale", project_id, "2020-01-01T00:00:00Z", 1_024),
                ("sha256:newest", project_id, "2020-01-02T00:00:00Z", 1_024),
            ])
            .with_project_container(project_id),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation should inventory images before cleanup");

    assert_eq!(report.stale_images.len(), 1);
    assert_eq!(report.stale_images[0].image_id, "sha256:stale");
    assert_eq!(report.stale_images[0].project_id, Some(project_id));
    assert_eq!(report.reclaimed_image_bytes, 0);
    assert!(image_removals(&runner).is_empty());
}

#[tokio::test]
async fn reconciliation_does_not_report_a_recent_image_as_stale() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::new_v4();
    let created = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current time should format");
    let runner = Arc::new(FakeRunner::new(true).with_images(&[
        ("sha256:recent", project_id, &created, 1),
        ("sha256:newest", project_id, &created, 1),
    ]));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation should inspect recent image age");

    assert!(report.stale_images.is_empty());
}

#[tokio::test]
async fn approved_cleanup_runs_only_requested_sakala_targets() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project = Uuid::new_v4();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_images(&[
                ("sha256:old", project, "2020-01-01T00:00:00Z", 2_048),
                ("sha256:new", project, "2020-01-02T00:00:00Z", 2_048),
            ])
            .with_project_container(project),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = clean_images(&executor).await;

    assert_eq!(output.result["reclaimed_image_bytes"], 2_048);
    assert_eq!(output.result["cleaned_routes"], 0);
    assert_eq!(output.result["cleaned_workspaces"], 0);
    let commands = runner.commands.lock().expect("command lock");
    assert!(!commands.iter().any(|command| {
        command.program == "docker"
            && command
                .args
                .first()
                .is_some_and(|argument| argument == "rm")
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
            reporter_factory: None,
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
async fn restarted_log_follower_reports_under_the_original_deploy_command() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID");
    let deployment_id =
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID");
    let deploy_command_id =
        Uuid::parse_str("b3c8cb55-3bc8-4725-a004-e69d9917d40b").expect("deploy command UUID");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "healthy\tUp 10 minutes (healthy)\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(format!(
                "healthy\tUp 10 minutes (healthy)\tportfolio.run.sakala.localhost\t3000\t{deploy_command_id}\t1024\t20\t65536\n"
            ))
            .with_follow_lines(&["app listening on :3000"])
            .with_follow_delay(Duration::from_secs(60)),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);
    let factory = Arc::new(RecordingReporterFactory::default());
    let reconcile_reporter = Arc::new(RecordingReporter::default());

    let output = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id,
            deployment_id,
            desired_state: DesiredWorkloadState::Running,
            actions: vec![ReconcileWorkloadAction::RestartLogFollower],
            cancellation: CancellationToken::new(),
            reporter_factory: Some(factory.clone()),
        },
        reconcile_reporter.clone(),
    )
    .await
    .expect("explicit follower restart should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        output.result["actions_applied"][0],
        json!({
            "action": "restart_log_follower",
            "started": true,
            "command_id": deploy_command_id,
        })
    );
    let reporters = factory.reporters.lock().expect("factory lock").clone();
    assert_eq!(
        reporters.len(),
        1,
        "one reporter bound to the deploy command"
    );
    let (command_id, log_bounds, follower_reporter) = &reporters[0];
    assert_eq!(*command_id, deploy_command_id);
    assert_eq!(
        *log_bounds,
        LogBounds {
            max_line_length: Some(1024),
            max_batch_lines: Some(20),
            max_total_bytes: Some(65536),
        }
    );
    let follower_logs = follower_reporter.logs.lock().expect("log lock").clone();
    assert_eq!(follower_logs.len(), 1);
    assert_eq!(follower_logs[0].message, "[runtime] app listening on :3000");
    assert!(
        reconcile_reporter.logs.lock().expect("log lock").is_empty(),
        "the reconcile command reporter must never carry follower output"
    );
    RuntimeExecutor::shutdown(&executor)
        .await
        .expect("follower should stop during shutdown");
}

#[tokio::test]
async fn restart_log_follower_is_refused_without_a_deploy_command_identity() {
    let temp = TempDir::new().expect("temp directory should be available");
    let project_id = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID");
    let deployment_id =
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "legacy\tUp 10 minutes (healthy)\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(
                "legacy\tUp 10 minutes (healthy)\tportfolio.run.sakala.localhost\t3000\t\t\t\t\n",
            ),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = RuntimeExecutor::reconcile_workload(
        &executor,
        ReconcileWorkloadRequest {
            project_id,
            deployment_id,
            desired_state: DesiredWorkloadState::Running,
            actions: vec![ReconcileWorkloadAction::RestartLogFollower],
            cancellation: CancellationToken::new(),
            reporter_factory: Some(Arc::new(RecordingReporterFactory::default())),
        },
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("legacy workload cannot restart a follower under an unknown command");

    assert_eq!(error.code(), "invalid_runtime_command");
    assert!(
        !runner
            .commands
            .lock()
            .expect("command lock")
            .iter()
            .any(|command| { command.args.iter().any(|argument| argument == "--follow") })
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
            reporter_factory: None,
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
            reporter_factory: None,
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
            reporter_factory: None,
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

fn position(runner: &FakeRunner, matches: impl Fn(&[String]) -> bool) -> Option<usize> {
    runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .position(|command| command.program == "docker" && matches(&string_arguments(command)))
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current time should format")
}

/// The control plane sets `remove_images` when the project was deleted. Without
/// it the active and previous images stayed on the node forever.
#[tokio::test]
async fn stopping_a_deleted_project_removes_all_its_images_after_the_container() {
    let temp = TempDir::new().expect("temp directory should be available");
    let mut request = lifecycle_request();
    request.remove_images = true;
    let project = request.project_id;
    let other = Uuid::new_v4();
    let fresh = now_rfc3339();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_workload_lookup("stopping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n")
            .with_images(&[
                ("sha256:active", project, "2020-01-02T00:00:00Z", 3_000),
                ("sha256:fresh", project, &fresh, 2_000),
                ("sha256:other", other, "2020-01-01T00:00:00Z", 1),
            ]),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::stop_project(&executor, request, reporter)
        .await
        .expect("stop should remove the workload and the images");

    assert_eq!(output.result["status"], "stopped");
    assert_eq!(output.result["removed_images"], 2);
    assert_eq!(output.result["reclaimed_image_bytes"], 5_000);
    assert_eq!(
        image_removals(&runner),
        [
            [format!("sakala/project-{project}:sha256:active-tag")],
            [format!("sakala/project-{project}:sha256:fresh-tag")],
        ]
    );
    let container_removed = position(&runner, |args| {
        args.first().is_some_and(|argument| argument == "rm")
    })
    .expect("container removal");
    let first_image_removed = position(&runner, |args| {
        args.first().is_some_and(|argument| argument == "image")
            && args.get(1).is_some_and(|argument| argument == "rm")
    })
    .expect("image removal");
    assert!(container_removed < first_image_removed);
    assert!(
        position(&runner, |args| {
            args.get(1).is_some_and(|argument| argument == "ls")
                && args.contains(&format!("label=dev.sakala.project-id={project}"))
        })
        .is_some(),
        "the listing is narrowed to the deleted project"
    );
}

#[tokio::test]
async fn a_repeated_delete_stop_still_removes_images_left_behind() {
    let temp = TempDir::new().expect("temp directory should be available");
    let mut request = lifecycle_request();
    request.remove_images = true;
    let project = request.project_id;
    let runner = Arc::new(FakeRunner::new(true).with_images(&[(
        "sha256:left",
        project,
        "2020-01-01T00:00:00Z",
        7,
    )]));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output =
        RuntimeExecutor::stop_project(&executor, request, Arc::new(RecordingReporter::default()))
            .await
            .expect("stop should be idempotent when the workload is absent");

    assert_eq!(output.result["status"], "already_stopped");
    assert_eq!(output.result["removed_images"], 1);
    assert_eq!(
        image_removals(&runner),
        [[format!("sakala/project-{project}:sha256:left-tag")]]
    );
}

#[tokio::test]
async fn image_gc_removes_every_image_of_a_project_without_containers_past_the_grace() {
    let temp = TempDir::new().expect("temp directory should be available");
    let orphan = Uuid::new_v4();
    let kept = Uuid::new_v4();
    let fresh = now_rfc3339();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_images(&[
                ("sha256:gone-old", orphan, "2020-01-01T00:00:00Z", 10),
                ("sha256:gone-newest", orphan, "2020-01-02T00:00:00Z", 20),
                ("sha256:too-fresh", orphan, &fresh, 30),
                ("sha256:kept-previous", kept, "2020-01-01T00:00:00Z", 40),
            ])
            .with_project_container(kept),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let cleanup = RuntimeExecutor::collect_stale_images(&executor)
        .await
        .expect("image GC should run");

    assert_eq!(cleanup.removed_images, 2);
    assert_eq!(cleanup.reclaimed_bytes, 30);
    assert_eq!(
        image_removals(&runner),
        [
            [format!("sakala/project-{orphan}:sha256:gone-newest-tag")],
            [format!("sakala/project-{orphan}:sha256:gone-old-tag")],
        ]
    );
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
    let soak_project = Uuid::new_v4();
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("previous-container\n")
            .with_previous_container_inspection("false\t/previous\n")
            .with_images(&[
                ("sha256:stale", soak_project, "2020-01-01T00:00:00Z", 1_024),
                ("sha256:newest", soak_project, "2020-01-02T00:00:00Z", 1_024),
            ])
            .with_image_users(format!("sha256:running\t{soak_project}\n")),
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

#[tokio::test]
async fn app_listening_on_the_wrong_port_fails_as_not_listening() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[(1, REFUSED_PROBE_STDERR)]));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let error = dispatch(executor, &deploy_with_start_timeout(1), reporter)
        .await
        .expect_err("an app that never answers must not become live");

    assert_eq!(error.code(), "runtime_app_not_listening");
    let message = error.to_string();
    assert!(message.contains("port 3000"), "{message}");
    assert!(message.contains("waiting 1 seconds"), "{message}");
    assert!(message.contains("0.0.0.0:$PORT"), "{message}");
    assert!(!route.exists(), "a route must not point at the failed app");
}

#[tokio::test]
async fn app_that_starts_listening_late_passes_within_the_deadline() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[
        (1, REFUSED_PROBE_STDERR),
        (1, REFUSED_PROBE_STDERR),
        (0, OK_PROBE_STDERR),
    ]));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    dispatch(executor, &deploy_with_start_timeout(5), reporter)
        .await
        .expect("a slow boot within the deadline is healthy");

    assert_eq!(runner.count_commands(is_probe), 3);
    assert!(route.exists());
}

#[tokio::test]
async fn root_returning_404_passes_because_the_app_answers() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[(1, NOT_FOUND_PROBE_STDERR)]));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    dispatch(executor, &deploy_with_start_timeout(1), reporter)
        .await
        .expect("any status below 500 means the app answers");

    assert_eq!(runner.count_commands(is_probe), 1);
    assert!(route.exists());
}

#[tokio::test]
async fn root_returning_500_fails_as_unhealthy_with_the_last_status() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_probe_results(&[(1, SERVER_ERROR_PROBE_STDERR)]));
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let error = dispatch(executor, &deploy_with_start_timeout(1), reporter)
        .await
        .expect_err("an app answering only 5xx must not become live");

    assert_eq!(error.code(), "runtime_app_unhealthy");
    let message = error.to_string();
    assert!(message.contains("port 3000"), "{message}");
    assert!(message.contains("HTTP 500"), "{message}");
    assert!(!route.exists());
    assert!(
        runner.count_commands(is_probe) > 1,
        "a 5xx during boot must be retried until the deadline"
    );
}

#[tokio::test]
async fn redirect_is_judged_by_the_first_response_and_stays_on_the_app() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[(1, REDIRECT_PROBE_STDERR)]));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(executor, &deploy_with_start_timeout(1), reporter)
        .await
        .expect("a redirect is an answer");

    let commands = runner.commands.lock().expect("command lock");
    let probe = commands
        .iter()
        .find(|command| is_probe(command))
        .expect("probe");
    let arguments = probe
        .args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let host = "sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993:3000";
    assert!(
        arguments.contains(&format!("http_proxy=http://{host}")),
        "proxy mode keeps every redirect on the app's connection: {arguments:?}"
    );
    assert!(arguments.windows(2).any(|pair| pair == ["-Y", "on"]));
    assert!(arguments.contains(&"--spider".to_owned()));
    assert_eq!(arguments.last(), Some(&format!("http://{host}/")));
    assert!(arguments.iter().any(|argument| argument == "sakala-caddy"));
}

#[tokio::test]
async fn exit_during_start_reports_logs_before_cleanup_and_names_the_exit_code() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["restarting\t2\t3\tfalse\tnone\t268435456\n"])
            .with_app_output(&["booting"], &["Error: cannot find module"]),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let started = std::time::Instant::now();
    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::clone(&reporter),
    )
    .await
    .expect_err("a crashing app must not become live");

    assert_eq!(error.code(), "runtime_app_crashed");
    assert!(error.to_string().contains("exit code 3"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "a crash must stop the wait early"
    );
    assert_eq!(runner.count_commands(is_probe), 0);
    assert!(!route.exists());

    let logs = reporter.logs.lock().expect("log lock");
    let line = |text: &str| {
        logs.iter()
            .find(|log| log.message == format!("[runtime] {text}"))
            .unwrap_or_else(|| panic!("{text} should be reported"))
    };
    assert_eq!(line("booting").stream, LogStream::Stdout);
    assert_eq!(line("Error: cannot find module").stream, LogStream::Stderr);
    let captured = runner.command_position(is_app_logs).expect("logs read");
    let removed = runner
        .command_position(is_candidate_removal)
        .expect("candidate removed");
    assert!(captured < removed, "logs must be read before cleanup");
}

/// What a Railpack Laravel app printed when FrankenPHP, a binary marked
/// `cap_net_bind_service=ep`, could not be executed. Its old message pointed at
/// the app, and the API's hint at missing environment variables.
#[tokio::test]
async fn a_program_the_kernel_refuses_to_run_is_named_as_such() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["restarting\t1\t126\tfalse\tnone\t268435456\n"])
            .with_app_output(
                &["Starting Laravel server ..."],
                &["/usr/local/bin/docker-php-entrypoint: 9: exec: frankenphp: Operation not permitted"],
            ),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("a program that cannot run must not become live");

    assert_eq!(error.code(), "runtime_app_exec_denied");
    let message = error.to_string();
    assert!(message.contains("exit code 126"), "{message}");
    assert!(message.contains("setcap -r"), "{message}");
}

#[tokio::test]
async fn crash_seen_between_restarts_reads_the_exit_code_from_history() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            // Docker resets the exit code as soon as the restart succeeds.
            .with_inspect_states(&["running\t1\t0\tfalse\tnone\t268435456\n"])
            .with_events("die\t7\ndie\t3\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("a restart during start is a crash");

    assert_eq!(error.code(), "runtime_app_crashed");
    assert!(error.to_string().contains("exit code 3"), "{error}");
}

#[tokio::test]
async fn crash_without_any_exit_history_still_says_it_crashed() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true).with_inspect_states(&["running\t1\t0\tfalse\tnone\t268435456\n"]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("a restart during start is a crash");

    assert_eq!(error.code(), "runtime_app_crashed");
    assert!(error.to_string().contains("no longer reports"), "{error}");
}

#[tokio::test]
async fn oom_flag_fails_with_the_applied_memory_limit() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["restarting\t1\t137\ttrue\tnone\t268435456\n"])
            .with_app_output(&["allocating"], &[]),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::clone(&reporter),
    )
    .await
    .expect_err("an OOM-killed app must not become live");

    assert_eq!(error.code(), "runtime_app_out_of_memory");
    assert!(error.to_string().contains("256 MB"), "{error}");
    assert!(
        reported_messages(&reporter)
            .iter()
            .any(|message| message == "[runtime] allocating")
    );
}

#[tokio::test]
async fn oom_between_restarts_is_read_from_history() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            // Docker was seen reporting exit 137 before the OOM flag was set.
            .with_inspect_states(&["restarting\t1\t137\tfalse\tnone\t268435456\n"])
            .with_events("oom\t<no value>\ndie\t137\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("an OOM-killed app must not become live");

    assert_eq!(error.code(), "runtime_app_out_of_memory");
}

#[tokio::test]
async fn app_surviving_a_worker_oom_stays_healthy_while_it_answers() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true).with_inspect_states(&["running\t0\t0\ttrue\tnone\t268435456\n"]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_with_start_timeout(1),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("the app answers, so it is healthy");
}

#[tokio::test]
async fn running_app_killed_by_oom_that_never_answers_reports_out_of_memory() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["running\t0\t0\ttrue\tnone\t268435456\n"])
            .with_probe_results(&[(1, REFUSED_PROBE_STDERR)]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(1),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("the app never answered");

    assert_eq!(error.code(), "runtime_app_out_of_memory");
}

#[tokio::test]
async fn declared_healthcheck_is_honoured_before_probing() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_inspect_states(&[
        "running\t0\t0\tfalse\tstarting\t268435456\n",
        "running\t0\t0\tfalse\thealthy\t268435456\n",
    ]));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("starting keeps waiting and healthy goes on to the probe");

    assert_eq!(runner.count_commands(is_probe), 1);

    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["running\t0\t0\tfalse\tunhealthy\t268435456\n"]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("an unhealthy HEALTHCHECK fails");

    assert_eq!(error.code(), "runtime_health_check_failed");
    assert!(error.to_string().contains("HEALTHCHECK"), "{error}");
    assert_eq!(runner.count_commands(is_probe), 0);
}

#[tokio::test]
async fn a_router_that_cannot_probe_is_not_blamed_on_the_app() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[(
        127,
        "OCI runtime exec failed: exec failed: unable to start container process: exec: \"wget\": executable file not found in $PATH\n",
    )]));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("readiness cannot be proven without the probe");

    assert_eq!(error.code(), "runtime_dependency_failed");
    assert!(error.to_string().contains("wget"), "{error}");
    assert_eq!(
        runner.count_commands(is_probe),
        1,
        "this cannot heal itself"
    );

    let temp = TempDir::new().expect("temp directory should be available");
    let runner =
        Arc::new(FakeRunner::new(true).with_probe_results(&[(1, BAD_ADDRESS_PROBE_STDERR)]));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(1),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("an unresolvable workload cannot be probed");

    assert_eq!(error.code(), "runtime_dependency_failed");
    assert!(error.to_string().contains("runtime network"), "{error}");
}

#[tokio::test]
async fn failed_routing_still_reports_the_candidate_output_before_cleanup() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_failed_caddy_validate()
            .with_app_output(&["listening on 3000"], &[]),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect_err("an invalid route must fail the deployment");

    assert_eq!(error.code(), "runtime_routing_failed");
    assert!(
        reported_messages(&reporter)
            .iter()
            .any(|message| message == "[runtime] listening on 3000")
    );
    let captured = runner.command_position(is_app_logs).expect("logs read");
    let removed = runner
        .command_position(is_candidate_removal)
        .expect("candidate removed");
    assert!(captured < removed);
}

#[tokio::test]
async fn failed_build_has_no_container_to_read_logs_from() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(BuildFailingRunner {
        inner: FakeRunner::new(true),
    });
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect_err("the build fails");

    assert_eq!(error.code(), "runtime_build_failed");
    assert_eq!(runner.inner.count_commands(is_app_logs), 0);
    assert!(
        !reported_messages(&reporter)
            .iter()
            .any(|message| message.contains("Last application output")),
    );
}

#[tokio::test]
async fn cancelled_start_skips_log_capture_and_still_cleans_up() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_inspect_delay(Duration::from_secs(60)));
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        runtime_config(&temp),
        runner.clone(),
    ));
    let cancellation = CancellationToken::new();
    let command = deploy_command("auto");
    let task = tokio::spawn({
        let cancellation = cancellation.clone();
        let executor = Arc::clone(&executor);
        async move {
            CommandDispatcher::new(executor)
                .dispatch(
                    &command,
                    Arc::new(RecordingReporter::default()),
                    cancellation,
                )
                .await
        }
    });
    for _ in 0..200 {
        let inspecting = runner.count_commands(|spec| {
            spec.args
                .first()
                .is_some_and(|argument| argument == "inspect")
        }) > 0;
        if inspecting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancellation.cancel();

    let error = task
        .await
        .expect("task should join")
        .expect_err("cancelled start fails");

    assert_eq!(error.code(), "runtime_cancelled");
    assert_eq!(runner.count_commands(is_app_logs), 0);
    assert!(runner.command_position(is_candidate_removal).is_some());
}

#[tokio::test]
async fn log_capture_failure_does_not_mask_the_original_error() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true).with_inspect_states(&["restarting\t2\t3\tfalse\tnone\t268435456\n"]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_with_start_timeout(5),
        Arc::new(RejectingLogReporter),
    )
    .await
    .expect_err("the deployment failed");

    assert_eq!(error.code(), "runtime_app_crashed");
    assert!(runner.command_position(is_candidate_removal).is_some());
}

#[tokio::test]
async fn log_capture_respects_the_batch_bound_and_says_when_there_is_no_output() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_inspect_states(&["restarting\t2\t3\tfalse\tnone\t268435456\n"])
            .with_app_output(&[], &[]),
    );
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_with_start_timeout(5);
    command.payload["log_bounds"] = json!({ "max_batch_lines": 7 });

    dispatch(executor, &command, Arc::clone(&reporter))
        .await
        .expect_err("the deployment failed");

    let commands = runner.commands.lock().expect("command lock");
    let logs = commands
        .iter()
        .find(|command| is_app_logs(command))
        .expect("logs read");
    assert!(logs.args.windows(2).any(|pair| pair == ["--tail", "7"]));
    assert!(
        reported_messages(&reporter)
            .iter()
            .any(|message| message.contains("no output"))
    );
}

#[tokio::test]
async fn explicit_health_check_shares_the_http_readiness_semantics() {
    let temp = TempDir::new().expect("temp directory should be available");
    let workload = "running\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n";
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(workload)
            .with_probe_results(&[(1, SERVER_ERROR_PROBE_STDERR)]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::health_check(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("health check reports without failing the command");

    assert_eq!(output.result["running"], true);
    assert_eq!(output.result["ready"], false);
    assert_eq!(output.result["error_code"], "runtime_app_unhealthy");
    assert!(
        output.result["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("HTTP 500"))
    );

    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(workload)
            .with_probe_results(&[(1, NOT_FOUND_PROBE_STDERR)]),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::health_check(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("health check completes");

    assert_eq!(output.result["ready"], true);
    assert!(output.result["reason"].is_null());
    assert!(output.result["error_code"].is_null());
    let commands = runner.commands.lock().expect("command lock");
    let probe = commands
        .iter()
        .find(|command| is_probe(command))
        .expect("probe");
    assert_eq!(
        probe.args.last().expect("url").to_string_lossy(),
        "http://sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993:3000/",
        "the probe must use the name the route uses, not the container id"
    );
}

const CRASHED_PROJECT: &str = "ff66ed4a-6303-4be6-8ef4-63c28b112680";
const CRASHED_DEPLOYMENT: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";

fn listed(id: &str, status: &str) -> String {
    format!("{id}\t{status}\t{CRASHED_PROJECT}\t{CRASHED_DEPLOYMENT}\n")
}

fn seconds_ago(seconds: i64) -> String {
    (OffsetDateTime::now_utc() - time::Duration::seconds(seconds))
        .format(&Rfc3339)
        .expect("timestamp formats")
}

/// One `docker inspect` line, in the order the crash report asks for.
fn crash_state(
    id: &str,
    status: &str,
    restarts: u32,
    exit_code: i32,
    oom: bool,
    up_seconds: i64,
) -> String {
    format!(
        "{id}aaaaaaaa\t{status}\t{restarts}\t{exit_code}\t{oom}\t{}\t{}\n",
        seconds_ago(up_seconds),
        seconds_ago(up_seconds + 1),
    )
}

async fn crashes_reported(runner: Arc<FakeRunner>, temp: &TempDir) -> Vec<CrashedWorkloadDetail> {
    crashes_with_config(runner, runtime_config(temp)).await
}

async fn crashes_with_config(
    runner: Arc<FakeRunner>,
    config: DockerRuntimeConfig,
) -> Vec<CrashedWorkloadDetail> {
    let executor = DockerRuntimeExecutor::with_runner(config, runner);
    let snapshots = RuntimeExecutor::health_snapshot(&executor)
        .await
        .expect("snapshot");
    RuntimeExecutor::crashed_workloads(&executor, &snapshots)
        .await
        .expect("crash details")
}

fn is_crash_inspect(spec: &CommandSpec) -> bool {
    spec.args
        .iter()
        .any(|argument| argument.to_string_lossy().contains("{{.Id}}"))
}

fn is_events(spec: &CommandSpec) -> bool {
    spec.args
        .first()
        .is_some_and(|argument| argument == "events")
}

#[tokio::test]
async fn restarting_workload_is_crashed_with_the_state_docker_still_holds() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("oomed123", "Restarting (137) 5 seconds ago"))
            .with_crash_inspect(
                "oomed123aaaaaaaa\trestarting\t3\t137\ttrue\t2026-11-05T14:28:30.5Z\t2026-11-05T14:28:41.612483825Z\n",
            ),
    );

    let crashed = crashes_reported(runner.clone(), &temp).await;

    assert_eq!(crashed.len(), 1, "{crashed:?}");
    assert_eq!(crashed[0].project_id.to_string(), CRASHED_PROJECT);
    assert_eq!(crashed[0].deployment_id.to_string(), CRASHED_DEPLOYMENT);
    assert_eq!(crashed[0].exit_code, Some(137));
    assert!(crashed[0].oom_killed);
    assert_eq!(
        serde_json::to_value(&crashed[0]).expect("detail")["finished_at"],
        "2026-11-05T14:28:41Z",
        "whole seconds in RFC 3339"
    );
    assert_eq!(runner.count_commands(is_events), 0, "nothing was missing");
}

#[tokio::test]
async fn slow_crash_loop_stays_crashed_between_crashes_with_its_last_exit() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("slowloop", "Up 3 seconds"))
            // Docker reports exit 0 for a running container whatever the last exit was.
            .with_crash_inspect(crash_state("slowloop", "running", 4, 0, false, 3))
            .with_events("slowloopaaaaaaaa\tdie\t7\nslowloopaaaaaaaa\tdie\t3\n"),
    );

    let crashed = crashes_reported(runner.clone(), &temp).await;

    assert_eq!(crashed.len(), 1, "{crashed:?}");
    assert_eq!(crashed[0].exit_code, Some(3), "the last exit wins");
    assert!(!crashed[0].oom_killed);
    assert!(crashed[0].finished_at.is_some(), "Docker keeps FinishedAt");
}

#[tokio::test]
async fn running_workload_without_exit_history_never_claims_exit_zero() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("slowloop", "Up 3 seconds"))
            .with_crash_inspect(crash_state("slowloop", "running", 3, 0, false, 3)),
    );

    let crashed = crashes_reported(runner, &temp).await;

    assert_eq!(crashed.len(), 1, "{crashed:?}");
    assert_eq!(crashed[0].exit_code, None);
    assert!(!crashed[0].oom_killed);
}

#[tokio::test]
async fn oom_describes_the_last_exit_and_not_the_current_run() {
    // Killed for memory on its last exit: Docker has already cleared the flag.
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("oomed123", "Up 3 seconds"))
            .with_crash_inspect(crash_state("oomed123", "running", 3, 0, false, 3))
            .with_events("oomed123aaaaaaaa\toom\t<no value>\noomed123aaaaaaaa\tdie\t137\n"),
    );
    let crashed = crashes_reported(runner, &temp).await;
    assert_eq!(crashed[0].exit_code, Some(137));
    assert!(crashed[0].oom_killed);

    // An earlier OOM, then a plain exit, then a worker killed in this run.
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("mixed123", "Up 3 seconds"))
            .with_crash_inspect(crash_state("mixed123", "running", 3, 0, true, 3))
            .with_events(concat!(
                "mixed123aaaaaaaa\toom\t<no value>\n",
                "mixed123aaaaaaaa\tdie\t137\n",
                "mixed123aaaaaaaa\tdie\t1\n",
                "mixed123aaaaaaaa\toom\t<no value>\n",
            )),
    );
    let crashed = crashes_reported(runner, &temp).await;
    assert_eq!(crashed[0].exit_code, Some(1));
    assert!(!crashed[0].oom_killed);

    // Docker was seen reporting exit 137 before setting the flag.
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("racing12", "Restarting (137) 1 second ago"))
            .with_crash_inspect(crash_state("racing12", "restarting", 1, 137, false, 3))
            .with_events("racing12aaaaaaaa\toom\t<no value>\nracing12aaaaaaaa\tdie\t137\n"),
    );
    let crashed = crashes_reported(runner, &temp).await;
    assert_eq!(crashed[0].exit_code, Some(137));
    assert!(crashed[0].oom_killed);
}

#[tokio::test]
async fn crash_loop_clears_after_the_stable_window() {
    let temp = TempDir::new().expect("temp directory should be available");
    let row = listed("oneoff12", "Up 20 seconds");
    let state = crash_state("oneoff12", "running", 3, 0, false, 20);

    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(row.clone())
            .with_crash_inspect(state.clone()),
    );
    let inside = crashes_reported(runner, &temp).await;
    assert_eq!(inside.len(), 1, "inside the default window");

    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(row)
            .with_crash_inspect(state),
    );
    let mut config = runtime_config(&temp);
    config.crash_stable_window = Duration::from_secs(10);
    let outside = crashes_with_config(runner.clone(), config).await;
    assert!(outside.is_empty(), "{outside:?}");
    assert_eq!(runner.count_commands(is_events), 0);
}

#[tokio::test]
async fn an_app_that_recovered_from_one_or_two_crashes_is_not_reported() {
    for restarts in [1, 2] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(
            FakeRunner::new(true)
                .with_docker_ps(listed("killed12", "Up 2 seconds"))
                .with_crash_inspect(crash_state("killed12", "running", restarts, 0, false, 2)),
        );

        let crashed = crashes_reported(runner.clone(), &temp).await;

        assert!(crashed.is_empty(), "{restarts} restarts: {crashed:?}");
        assert_eq!(runner.count_commands(is_events), 0);
    }
}

#[tokio::test]
async fn a_crash_loop_is_reported_from_its_third_restart_with_its_restart_count() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("looping1", "Up 2 seconds"))
            .with_crash_inspect(crash_state("looping1", "running", 3, 0, false, 2)),
    );

    let crashed = crashes_reported(runner, &temp).await;

    assert_eq!(crashed.len(), 1, "{crashed:?}");
    assert_eq!(crashed[0].restart_count, 3);
    assert!(crashed[0].running, "a process is up between two crashes");
}

#[tokio::test]
async fn a_first_crash_is_reported_while_docker_shows_it_is_not_running() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("first123", "Restarting (1) 1 second ago"))
            .with_crash_inspect(crash_state("first123", "restarting", 1, 1, false, 1)),
    );

    let crashed = crashes_reported(runner, &temp).await;

    assert_eq!(crashed.len(), 1, "{crashed:?}");
    assert_eq!(crashed[0].exit_code, Some(1));
    assert_eq!(crashed[0].restart_count, 1);
    assert!(!crashed[0].running);
}

#[tokio::test]
async fn a_docker_failure_reaches_the_user_as_an_exit_code() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(UnavailableDependencyRunner {
        program: "docker",
        required_argument: Some("ps"),
    });
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let error = dispatch(
        executor,
        &deploy_command("auto"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("Docker fails");

    let message = error.to_string();
    assert!(message.contains("exit code 127"), "{message}");
    assert!(!message.contains("Some("), "{message}");
}

#[tokio::test]
async fn default_stable_window_is_five_minutes() {
    assert_eq!(
        DockerRuntimeConfig::default().crash_stable_window,
        Duration::from_secs(300)
    );

    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("stable12", "Up 6 minutes"))
            .with_crash_inspect(crash_state("stable12", "running", 9, 0, false, 360)),
    );
    let crashed = crashes_reported(runner, &temp).await;
    assert!(crashed.is_empty(), "{crashed:?}");
}

#[tokio::test]
async fn restarted_or_woken_workload_is_not_a_crash() {
    // Real Docker resets RestartCount to 0 on `docker restart` and `docker start`,
    // and StartedAt is recent in both cases.
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("woken123", "Up 1 second"))
            .with_crash_inspect(crash_state("woken123", "running", 0, 0, false, 1)),
    );

    let crashed = crashes_reported(runner.clone(), &temp).await;

    assert!(crashed.is_empty(), "{crashed:?}");
    assert_eq!(runner.count_commands(is_events), 0);
}

#[tokio::test]
async fn stopped_workload_is_not_a_crash_even_after_earlier_restarts() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("slept789", "Up 1 second"))
            // The listing raced a sleep, so inspect already shows it stopped.
            .with_crash_inspect(crash_state("slept789", "exited", 3, 137, false, 1)),
    );

    let crashed = crashes_reported(runner, &temp).await;

    assert!(crashed.is_empty(), "{crashed:?}");
}

#[tokio::test]
async fn crash_report_uses_one_batched_inspect_and_one_batched_history_query() {
    let temp = TempDir::new().expect("temp directory should be available");
    let rows = format!(
        "{}{}{}",
        listed("first123", "Up 3 seconds"),
        listed("second45", "Up 2 minutes"),
        listed("third678", "Up 2 seconds"),
    );
    let states = format!(
        "{}{}{}",
        crash_state("first123", "running", 3, 0, false, 3),
        crash_state("second45", "running", 0, 0, false, 120),
        crash_state("third678", "running", 4, 0, false, 2),
    );
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(rows)
            .with_crash_inspect(states)
            .with_events("first123aaaaaaaa\tdie\t1\nthird678aaaaaaaa\tdie\t2\n"),
    );

    let crashed = crashes_reported(runner.clone(), &temp).await;

    assert_eq!(
        crashed
            .iter()
            .map(|detail| detail.exit_code)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    assert_eq!(runner.count_commands(is_crash_inspect), 1);
    assert_eq!(runner.count_commands(is_events), 1);
    let commands = runner.commands.lock().expect("command lock");
    let inspect = commands
        .iter()
        .find(|spec| is_crash_inspect(spec))
        .expect("inspect");
    for id in ["first123", "second45", "third678"] {
        assert!(inspect.args.iter().any(|argument| argument == id), "{id}");
    }
    let events = commands
        .iter()
        .find(|spec| is_events(spec))
        .expect("events");
    let filters = events
        .args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(filters.contains(&"container=first123aaaaaaaa".to_owned()));
    assert!(filters.contains(&"container=third678aaaaaaaa".to_owned()));
    assert!(
        !filters.contains(&"container=second45aaaaaaaa".to_owned()),
        "a stable workload needs no history: {filters:?}"
    );
}

#[tokio::test]
async fn a_healthy_node_runs_no_history_query_and_an_empty_node_runs_no_query() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(listed("fresh456", "Up 2 minutes"))
            .with_crash_inspect(crash_state("fresh456", "running", 0, 0, false, 120)),
    );
    let crashed = crashes_reported(runner.clone(), &temp).await;
    assert!(crashed.is_empty(), "{crashed:?}");
    assert_eq!(runner.count_commands(is_crash_inspect), 1);
    assert_eq!(runner.count_commands(is_events), 0);

    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let crashed = crashes_reported(runner.clone(), &temp).await;
    assert!(crashed.is_empty(), "{crashed:?}");
    assert_eq!(runner.count_commands(is_crash_inspect), 0);
}

fn event_types(reporter: &RecordingReporter) -> Vec<String> {
    reporter
        .events
        .lock()
        .expect("event lock")
        .iter()
        .map(|event| event.event_type.clone())
        .filter(|event_type| event_type.starts_with("deployment."))
        .filter(|event_type| !event_type.ends_with(".resolved"))
        .collect()
}

#[tokio::test]
async fn dockerfile_deployment_reports_its_stages_in_the_order_they_happen() {
    let temp = TempDir::new().expect("temp directory should be available");
    let reporter = Arc::new(RecordingReporter::default());
    let executor =
        DockerRuntimeExecutor::with_runner(runtime_config(&temp), Arc::new(FakeRunner::new(true)));

    dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("deployment completes");

    assert_eq!(
        event_types(&reporter),
        [
            "deployment.checkout.started",
            "deployment.build.started",
            "deployment.container.started",
            "deployment.health.started",
            "deployment.route.started",
            "deployment.runtime.ready",
        ],
        "a Dockerfile deployment has no analysis step"
    );
}

#[tokio::test]
async fn railpack_deployment_reports_analysis_before_the_build() {
    let temp = TempDir::new().expect("temp directory should be available");
    let reporter = Arc::new(RecordingReporter::default());
    let executor =
        DockerRuntimeExecutor::with_runner(runtime_config(&temp), Arc::new(FakeRunner::new(false)));

    dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect("deployment completes");

    assert_eq!(
        event_types(&reporter),
        [
            "deployment.checkout.started",
            "deployment.analysis.started",
            "deployment.build.started",
            "deployment.container.started",
            "deployment.health.started",
            "deployment.route.started",
            "deployment.runtime.ready",
        ]
    );
}

#[tokio::test]
async fn a_failed_readiness_never_reports_that_routing_started() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_probe_results(&[(1, REFUSED_PROBE_STDERR)]));
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    dispatch(
        executor,
        &deploy_with_start_timeout(1),
        Arc::clone(&reporter),
    )
    .await
    .expect_err("the app never answers");

    let stages = event_types(&reporter);
    assert_eq!(
        stages.last().map(String::as_str),
        Some("deployment.health.started")
    );
}

const BUILD_SECRET: &str = "kL9-unusual-build-value";

fn deploy_with_build_environment(builder: &str) -> AgentCommand {
    let mut command = deploy_command(builder);
    command.payload["environment"] = json!({
        "EMPTY_VALUE": "",
        "LD_PRELOAD": BUILD_SECRET,
        "PORT": "3000",
        "RAILPACK_NODE_VERSION": "22",
        "VITE_API_URL": BUILD_SECRET,
    });
    command
}

fn string_arguments(spec: &CommandSpec) -> Vec<String> {
    spec.args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn find_command(runner: &FakeRunner, matches: impl Fn(&CommandSpec) -> bool) -> CommandSpec {
    runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .find(|spec| matches(spec))
        .cloned()
        .expect("command should run")
}

fn is_buildx_build(spec: &CommandSpec) -> bool {
    spec.args.iter().any(|argument| argument == "buildx")
        && spec.args.iter().any(|argument| argument == "build")
}

#[tokio::test]
async fn dockerfile_build_receives_project_variables_only_as_buildkit_secrets() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_with_build_environment("dockerfile"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let build = find_command(&runner, is_buildx_build);
    let arguments = string_arguments(&build);
    for secret in [
        "id=LD_PRELOAD,env=SAKALA_BUILD_SECRET_0",
        "id=RAILPACK_NODE_VERSION,env=SAKALA_BUILD_SECRET_1",
        "id=VITE_API_URL,env=SAKALA_BUILD_SECRET_2",
    ] {
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--secret", secret]),
            "{arguments:?}"
        );
    }
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains(BUILD_SECRET))
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.starts_with("id=PORT")
                || argument.starts_with("id=EMPTY_VALUE")),
        "{arguments:?}"
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.starts_with("VITE_API_URL=")
                || argument.starts_with("LD_PRELOAD=")),
        "a build argument stays in the image history: {arguments:?}"
    );
    let names = build.secret_environment_names().collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "SAKALA_BUILD_SECRET_0",
            "SAKALA_BUILD_SECRET_1",
            "SAKALA_BUILD_SECRET_2"
        ],
        "a project variable name must never reach a host process's environment"
    );
    assert!(
        !format!("{build:?}").contains(BUILD_SECRET),
        "debug output hides values"
    );
}

#[tokio::test]
async fn railpack_learns_names_without_values_and_gets_values_through_buildkit() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_with_build_environment("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let prepare = find_command(&runner, is_sandbox);
    let arguments = string_arguments(&prepare);
    assert!(arguments.windows(2).any(|pair| {
        pair[0] == "--env" && pair[1] == "VITE_API_URL=provided-by-buildkit-secret"
    }));
    assert!(
        arguments.windows(2).any(|pair| {
            pair[0] == "--env" && pair[1] == "LD_PRELOAD=provided-by-buildkit-secret"
        })
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--env", "RAILPACK_NODE_VERSION"]),
        "Railpack's own settings need their real value: {arguments:?}"
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains(BUILD_SECRET))
    );
    assert_eq!(
        prepare.secret_environment_names().collect::<Vec<_>>(),
        ["RAILPACK_NODE_VERSION"],
        "only Railpack's own settings reach its environment under their own name"
    );

    let build = find_command(&runner, is_buildx_build);
    let arguments = string_arguments(&build);
    assert!(arguments.windows(2).any(|pair| {
        pair[0] == "--secret" && pair[1] == "id=VITE_API_URL,env=SAKALA_BUILD_SECRET_2"
    }));
    assert!(
        arguments
            .iter()
            .any(|argument| argument.starts_with("secrets-hash=")),
        "a changed value must invalidate cached steps: {arguments:?}"
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains(BUILD_SECRET))
    );
}

#[tokio::test]
async fn secrets_fingerprint_follows_the_values_and_hides_them() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        runtime_config(&temp),
        runner.clone(),
    ));
    let deploy = |value: &'static str| {
        let executor = Arc::clone(&executor);
        async move {
            let mut command = deploy_command("railpack");
            command.payload["environment"] = json!({ "VITE_API_URL": value });
            CommandDispatcher::new(executor)
                .dispatch(
                    &command,
                    Arc::new(RecordingReporter::default()),
                    CancellationToken::new(),
                )
                .await
                .expect("deployment completes");
        }
    };

    deploy("first-long-value").await;
    deploy("first-long-value").await;
    deploy("second-long-value").await;

    let fingerprints = runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .filter(|spec| is_buildx_build(spec))
        .flat_map(string_arguments)
        .filter(|argument| argument.starts_with("secrets-hash="))
        .collect::<Vec<_>>();
    assert_eq!(fingerprints.len(), 3);
    assert_eq!(
        fingerprints[0], fingerprints[1],
        "same values keep the cache"
    );
    assert_ne!(
        fingerprints[1], fingerprints[2],
        "changed values invalidate it"
    );
}

#[tokio::test]
async fn a_deployment_without_variables_adds_no_build_flags() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("railpack");
    command.payload["environment"] = json!({});

    dispatch(executor, &command, Arc::new(RecordingReporter::default()))
        .await
        .expect("deployment completes");

    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(!build.iter().any(|argument| argument == "--secret"));
    assert!(
        !build
            .iter()
            .any(|argument| argument.starts_with("secrets-hash="))
    );
    let prepare = find_command(&runner, is_sandbox);
    let arguments = string_arguments(&prepare);
    let railpack = arguments
        .iter()
        .position(|argument| argument == "railpack")
        .expect("railpack");
    assert!(
        !arguments[railpack..]
            .iter()
            .any(|argument| argument == "--env")
    );
    assert_eq!(prepare.secret_environment_names().count(), 0);
}

fn managed_workload_row() -> String {
    format!(
        "running\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\t{}\t\t\t\n",
        "b3c8cb55-3bc8-4725-a004-e69d9917d40b"
    )
}

#[tokio::test]
async fn a_recovered_log_follower_keeps_redacting_the_workload_environment() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "running\tUp 1 minute\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(managed_workload_row())
            .with_container_env(format!(
                "[\"PATH=/usr/local/sbin:/usr/bin\",\"PORT=3000\",\"VITE_API_URL={BUILD_SECRET}\"]\n"
            )),
    );
    let factory = Arc::new(RecordingReporterFactory::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    RuntimeExecutor::recover(&executor, Some(factory.clone()))
        .await
        .expect("recovery completes");

    let redactors = factory.redactors.lock().expect("factory lock");
    assert_eq!(redactors.len(), 1);
    assert_eq!(
        redactors[0].redact(&format!("using {BUILD_SECRET}")),
        "using [REDACTED]"
    );
    assert_eq!(
        redactors[0].redact("PATH=/usr/local/sbin:/usr/bin"),
        "PATH=/usr/local/sbin:/usr/bin",
        "the image's own PATH is not a secret"
    );
}

#[tokio::test]
async fn a_follower_still_starts_when_the_environment_cannot_be_read() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps(
                "running\tUp 1 minute\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\n",
            )
            .with_workload_lookup(managed_workload_row())
            .with_failed_env_inspect(),
    );
    let factory = Arc::new(RecordingReporterFactory::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = RuntimeExecutor::recover(&executor, Some(factory.clone()))
        .await
        .expect("recovery completes");

    assert_eq!(report.reattached_log_followers, 1);
    assert_eq!(
        factory.redactors.lock().expect("factory lock")[0].redact("TOKEN=abc"),
        "TOKEN=[REDACTED]",
        "key-name redaction still applies"
    );
}

fn deploy_with_config(builder: &str, config: serde_json::Value) -> AgentCommand {
    let mut command = deploy_command(builder);
    for (key, value) in config.as_object().expect("config object") {
        command.payload[key] = value.clone();
    }
    command
}

async fn deploy_outcome(
    runner: Arc<FakeRunner>,
    temp: &TempDir,
    command: &AgentCommand,
    reporter: Arc<RecordingReporter>,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(temp), runner);
    dispatch(executor, command, reporter).await
}

/// The positional context comes right after the labels. Secrets follow it.
fn build_context(arguments: &[String]) -> &str {
    let label = arguments
        .iter()
        .position(|argument| argument.starts_with("dev.sakala.deployment-id="))
        .expect("deployment label");
    &arguments[label + 1]
}

fn is_docker_run(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "run")
        && !is_sandbox(spec)
}

#[tokio::test]
async fn payload_without_build_config_builds_the_repository_root_as_before() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));

    deploy_outcome(
        runner.clone(),
        &temp,
        &deploy_command("auto"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("an old payload still deploys");

    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(build_context(&build).ends_with("/source"), "{build:?}");
    let built = runner.built_dockerfiles.lock().expect("dockerfile lock");
    assert!(built[0].contains("FROM scratch"), "{built:?}");
    assert!(!built[0].contains("apps-web"), "{built:?}");
    let run = string_arguments(&find_command(&runner, is_docker_run));
    assert!(
        !run.iter().any(|argument| argument == "--entrypoint"),
        "{run:?}"
    );
    assert!(
        run.last().expect("image").starts_with("sakala/project-"),
        "{run:?}"
    );
}

#[tokio::test]
async fn dockerfile_build_uses_the_root_directory_as_its_context() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let command = deploy_with_config("auto", json!({ "root_directory": "apps/web/" }));

    deploy_outcome(
        runner.clone(),
        &temp,
        &command,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(
        build_context(&build).ends_with("/source/apps/web"),
        "{build:?}"
    );
    let built = runner.built_dockerfiles.lock().expect("dockerfile lock");
    assert!(built[0].contains("# apps-web"), "{built:?}");
    assert!(
        built[0].starts_with("# Sakala fixed the build frontend"),
        "a repository must not choose its frontend: {built:?}"
    );
}

#[tokio::test]
async fn railpack_build_uses_the_root_directory_for_both_steps() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let command = deploy_with_config("auto", json!({ "root_directory": "apps/web" }));

    deploy_outcome(
        runner.clone(),
        &temp,
        &command,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let prepare = find_command(&runner, is_sandbox);
    assert!(sandbox_mount(&prepare, "/app").ends_with("/source/apps/web"));
    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(
        build_context(&build).ends_with("/source/apps/web"),
        "{build:?}"
    );
}

#[tokio::test]
async fn a_symlink_that_stays_in_the_repository_is_a_valid_root_directory() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let command = deploy_with_config("auto", json!({ "root_directory": "link-in" }));

    deploy_outcome(
        runner.clone(),
        &temp,
        &command,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(
        build_context(&build).ends_with("/source/apps/web"),
        "the resolved path is what gets used: {build:?}"
    );
}

#[tokio::test]
async fn malicious_root_directory_is_refused_before_any_process_runs() {
    for root_directory in [
        "../outside",
        "apps/../../outside",
        "..",
        "/etc",
        "//etc",
        "apps\\web",
        "apps\u{0}web",
        "apps\nweb",
    ] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(true));
        let command = deploy_with_config("auto", json!({ "root_directory": root_directory }));

        let error = deploy_outcome(
            runner.clone(),
            &temp,
            &command,
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("must be refused");

        assert_eq!(
            error.code(),
            "runtime_root_directory_invalid",
            "{root_directory:?}"
        );
        assert!(
            runner.commands.lock().expect("command lock").is_empty(),
            "{root_directory:?}"
        );
    }
    let too_long = "a/".repeat(200);
    let temp = TempDir::new().expect("temp directory should be available");
    let error = deploy_outcome(
        Arc::new(FakeRunner::new(true)),
        &temp,
        &deploy_with_config("auto", json!({ "root_directory": too_long })),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("must be refused");
    assert_eq!(error.code(), "runtime_root_directory_invalid");
}

#[tokio::test]
async fn root_directory_escaping_through_a_symlink_never_reaches_a_build() {
    for (root_directory, reason) in [
        ("link-out", "outside the repository"),
        ("link-out/ssl", "outside the repository"),
        ("does/not/exist", "does not exist"),
        ("a-file", "not a directory"),
    ] {
        let temp = TempDir::new().expect("temp directory should be available");
        let config = runtime_config(&temp);
        let workspace = config
            .workspace_root
            .join("b3c8cb55-3bc8-4725-a004-e69d9917d40b");
        let runner = Arc::new(FakeRunner::new(true));
        let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());
        let command = deploy_with_config("auto", json!({ "root_directory": root_directory }));

        let error = dispatch(executor, &command, Arc::new(RecordingReporter::default()))
            .await
            .expect_err("must be refused");

        assert_eq!(
            error.code(),
            "runtime_root_directory_invalid",
            "{root_directory}"
        );
        assert!(error.to_string().contains(reason), "{error}");
        assert!(!error.to_string().contains("/etc"), "{error}");
        assert_eq!(
            runner.count_commands(is_buildx_build),
            0,
            "{root_directory}"
        );
        assert!(!workspace.exists(), "the checkout must be cleaned up");
    }
}

#[tokio::test]
async fn a_dockerfile_symlink_to_a_host_file_is_not_built() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let command = deploy_with_config("auto", json!({ "root_directory": "apps/evil" }));

    let error = deploy_outcome(
        runner.clone(),
        &temp,
        &command,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("must be refused");

    assert_eq!(error.code(), "runtime_build_failed");
    assert!(
        error.to_string().contains("outside the repository"),
        "{error}"
    );
    assert_eq!(runner.count_commands(is_buildx_build), 0);
}

#[tokio::test]
async fn railpack_receives_commands_through_its_settings_and_never_in_arguments() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let command = deploy_with_config(
        "railpack",
        json!({ "build_command": "pnpm build:web", "start_command": "node server.js" }),
    );

    deploy_outcome(
        runner.clone(),
        &temp,
        &command,
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let prepare = find_command(&runner, is_sandbox);
    let arguments = string_arguments(&prepare);
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--env", "RAILPACK_BUILD_CMD"])
    );
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == ["--env", "RAILPACK_START_CMD"])
    );
    assert!(
        prepare
            .secret_environment_names()
            .any(|name| name == "RAILPACK_BUILD_CMD"),
        "the value reaches Railpack through its environment"
    );
    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(
        build
            .iter()
            .any(|argument| argument.starts_with("id=RAILPACK_BUILD_CMD,env="))
    );
    for argument in arguments.iter().chain(&build) {
        assert!(!argument.contains("pnpm build:web"), "{argument}");
        assert!(!argument.contains("node server.js"), "{argument}");
    }
    let run = string_arguments(&find_command(&runner, is_docker_run));
    assert!(
        !run.iter().any(|argument| argument == "--entrypoint"),
        "a Railpack image already starts with the command: {run:?}"
    );
}

#[tokio::test]
async fn dockerfile_start_command_runs_in_the_container_and_build_command_is_reported_unused() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let command = deploy_with_config(
        "auto",
        json!({ "build_command": "make web", "start_command": "node server.js --port $PORT" }),
    );

    deploy_outcome(runner.clone(), &temp, &command, Arc::clone(&reporter))
        .await
        .expect("deployment completes");

    let run = find_command(&runner, is_docker_run);
    assert_eq!(run.program, "docker", "never a shell on the host");
    let arguments = string_arguments(&run);
    let image = arguments
        .iter()
        .position(|argument| argument.starts_with("sakala/project-"))
        .expect("image");
    assert_eq!(
        &arguments[image + 1..],
        ["-c", "node server.js --port $PORT"]
    );
    assert!(
        arguments[..image]
            .windows(2)
            .any(|pair| pair == ["--entrypoint", "sh"]),
        "the image's entrypoint must not wrap the command: {arguments:?}"
    );
    let build = string_arguments(&find_command(&runner, is_buildx_build));
    assert!(
        !build.iter().any(|argument| argument.contains("make web")),
        "{build:?}"
    );
    assert!(
        reported_messages(&reporter)
            .iter()
            .any(|message| message.contains("build_command was not used")),
    );
}

#[tokio::test]
async fn an_empty_command_or_root_directory_means_not_set() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(RecordingReporter::default());
    let command = deploy_with_config(
        "auto",
        json!({ "build_command": "  ", "start_command": "", "root_directory": "." }),
    );

    deploy_outcome(runner.clone(), &temp, &command, Arc::clone(&reporter))
        .await
        .expect("deployment completes");

    let run = string_arguments(&find_command(&runner, is_docker_run));
    assert!(
        !run.iter().any(|argument| argument == "--entrypoint"),
        "{run:?}"
    );
    assert!(
        !reported_messages(&reporter)
            .iter()
            .any(|message| message.contains("build_command")),
    );
}

#[tokio::test]
async fn multi_line_commands_are_refused() {
    for field in ["build_command", "start_command"] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(true));
        let command = deploy_with_config("auto", json!({ field: "ok\nrm -rf /" }));

        let error = deploy_outcome(
            runner.clone(),
            &temp,
            &command,
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("must be refused");

        assert_eq!(error.code(), "invalid_runtime_command");
        assert!(error.to_string().contains(field), "{error}");
        assert!(runner.commands.lock().expect("command lock").is_empty());
    }
}

#[tokio::test]
async fn inspection_looks_at_the_requested_root_directory() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = inspect_command();
    command.payload["root_directory"] = json!("apps/web");

    let output = dispatch(executor, &command, Arc::new(RecordingReporter::default()))
        .await
        .expect("inspection completes");

    let info = find_command(&runner, is_sandbox);
    assert!(sandbox_mount(&info, "/app").ends_with("/source/apps/web"));
    let manifests = output.result["manifests"].as_array().expect("manifests");
    assert!(manifests.iter().any(|manifest| manifest == "package.json"));
    assert!(
        !manifests
            .iter()
            .any(|manifest| manifest == "pnpm-lock.yaml"),
        "the lockfile is at the repository root: {manifests:?}"
    );
    assert_eq!(output.result["dockerfile_found"], true);
}

#[tokio::test]
async fn inspection_reports_the_ports_the_dockerfile_exposes() {
    for (root_directory, expected) in [
        (None, json!([8080])),
        (Some("apps/web"), json!([80])),
        // The Dockerfile there is a symlink to a host file, which is never read.
        (Some("apps/evil"), json!([])),
    ] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(true));
        let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
        let mut command = inspect_command();
        if let Some(root_directory) = root_directory {
            command.payload["root_directory"] = json!(root_directory);
        }

        let output = dispatch(executor, &command, Arc::new(RecordingReporter::default()))
            .await
            .expect("inspection completes");

        assert_eq!(
            output.result["exposed_ports"], expected,
            "{root_directory:?}"
        );
    }
}

/// Railpack exits 1 for a repository with only a Dockerfile, but a Dockerfile build
/// never asks Railpack, so the preview still succeeds. Without a Dockerfile the
/// failure stands, because then Railpack is what would build the app.
#[tokio::test]
async fn a_railpack_failure_only_fails_the_inspection_without_a_dockerfile() {
    for dockerfile in [true, false] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(dockerfile).with_failed_sandbox_info());
        let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

        let result = dispatch(
            executor,
            &inspect_command(),
            Arc::new(RecordingReporter::default()),
        )
        .await;

        if dockerfile {
            let output = result.expect("inspection completes");
            assert_eq!(output.result["exposed_ports"], json!([8080]));
            assert_eq!(output.result["railpack"], serde_json::Value::Null);
        } else {
            let error = result.expect_err("inspection fails");
            assert_eq!(error.code(), "runtime_build_failed");
        }
    }
}

#[tokio::test]
async fn inspection_without_a_dockerfile_reports_no_ports() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = dispatch(
        executor,
        &inspect_command(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("inspection completes");

    assert_eq!(output.result["dockerfile_found"], false);
    assert_eq!(output.result["exposed_ports"], json!([]));
}

#[tokio::test]
async fn inspection_refuses_a_root_directory_outside_the_repository() {
    for (root_directory, processes_run) in [("../outside", false), ("link-out", true)] {
        let temp = TempDir::new().expect("temp directory should be available");
        let config = runtime_config(&temp);
        let workspace = config
            .workspace_root
            .join("b3c8cb55-3bc8-4725-a004-e69d9917d40b");
        let runner = Arc::new(FakeRunner::new(true));
        let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());
        let mut command = inspect_command();
        command.payload["root_directory"] = json!(root_directory);

        let error = dispatch(executor, &command, Arc::new(RecordingReporter::default()))
            .await
            .expect_err("must be refused");

        assert_eq!(error.code(), "runtime_root_directory_invalid");
        assert_eq!(
            !runner.commands.lock().expect("command lock").is_empty(),
            processes_run
        );
        assert_eq!(runner.count_commands(is_sandbox), 0);
        assert!(!workspace.exists());
    }
}

#[tokio::test]
async fn cancel_during_route_activation_lets_it_finish_instead_of_breaking_the_route() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_validate_delay(Duration::from_millis(300)));
    let config = runtime_config(&temp);
    let route = route_file(&config);
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(config, runner.clone()));
    let cancellation = CancellationToken::new();
    let task = tokio::spawn({
        let executor = Arc::clone(&executor);
        let cancellation = cancellation.clone();
        async move {
            let command = deploy_command("auto");
            CommandDispatcher::new(executor)
                .dispatch(
                    &command,
                    Arc::new(RecordingReporter::default()),
                    cancellation,
                )
                .await
        }
    });
    for _ in 0..400 {
        if runner.count_commands(|spec| spec.args.iter().any(|argument| argument == "validate")) > 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cancellation.cancel();

    task.await
        .expect("task should join")
        .expect("an activation in progress finishes");

    assert!(route.exists());
    assert_eq!(
        runner.count_commands(is_candidate_removal),
        0,
        "the candidate now serves the route"
    );
}

/// Runs `df` for real, because the failure is what `df` does with a missing path.
struct RealDfRunner {
    inner: FakeRunner,
}

#[async_trait]
impl ProcessRunner for RealDfRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        if spec.program == "df" {
            return sakala_agent_runtime::TokioProcessRunner::default()
                .run(spec, sink)
                .await;
        }
        self.inner.run(spec, sink).await
    }
}

#[tokio::test]
async fn preflight_passes_on_a_fresh_node_without_a_workspace_directory() {
    let temp = TempDir::new().expect("temp directory should be available");
    let config = DockerRuntimeConfig {
        workspace_root: temp.path().join("not/created/yet/builds"),
        ..runtime_config(&temp)
    };
    let workspace_root = config.workspace_root.clone();
    let executor = DockerRuntimeExecutor::with_runner(
        config,
        Arc::new(RealDfRunner {
            inner: FakeRunner::new(true),
        }),
    );

    let report = RuntimeExecutor::preflight(&executor)
        .await
        .expect("preflight should return a report");

    let disk = report
        .checks
        .iter()
        .find(|check| check.name == "workspace-disk")
        .expect("disk check");
    assert!(disk.ready, "{}", disk.detail);
    assert!(!report.has_fatal_failure(), "{:?}", report.checks);
    assert!(workspace_root.is_dir());
}

#[tokio::test]
async fn railpack_only_ever_runs_in_the_locked_down_sandbox() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());
    let mut command = deploy_command("railpack");
    command.payload["environment"] = json!({ "API_SECRET_VALUE": BUILD_SECRET });

    dispatch(executor, &command, Arc::new(RecordingReporter::default()))
        .await
        .expect("deployment completes");

    assert_eq!(
        runner.count_commands(|spec| spec.program == "railpack"),
        0,
        "Railpack must never run on the host"
    );
    let sandbox = find_command(&runner, is_sandbox);
    let arguments = string_arguments(&sandbox);
    for pair in [
        ["--cap-drop", "ALL"],
        ["--security-opt", "no-new-privileges:true"],
        ["--user", "65534:65534"],
        ["--memory", "512m"],
        ["--memory-swap", "512m"],
        ["--pids-limit", "256"],
        ["--network", "sakala-railpack"],
    ] {
        assert!(
            arguments.windows(2).any(|found| found == pair),
            "{pair:?}: {arguments:?}"
        );
    }
    assert!(arguments.iter().any(|argument| argument == "--read-only"));
    assert!(arguments.iter().any(|argument| argument == "--rm"));
    assert!(
        arguments
            .iter()
            .any(|argument| argument.ends_with(",dst=/app,readonly")),
        "{arguments:?}"
    );
    assert_eq!(
        arguments
            .iter()
            .filter(|argument| *argument == "--mount")
            .count(),
        1,
        "the read-only source is the only host path: {arguments:?}"
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.contains("docker.sock")
                || argument.contains("SAKALA_AGENT")
                || argument.contains(BUILD_SECRET)),
        "{arguments:?}"
    );
    assert_eq!(
        sandbox.secret_environment_names().count(),
        0,
        "no project value enters unless it is one of Railpack's own settings"
    );
}

#[tokio::test]
async fn inspection_runs_railpack_in_the_sandbox_too() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = dispatch(
        executor,
        &inspect_command(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("inspection completes");

    assert_eq!(runner.count_commands(|spec| spec.program == "railpack"), 0);
    assert_eq!(runner.count_commands(is_sandbox), 1);
    assert_eq!(output.result["railpack"]["provider"], "node");
}

/// Images that start as root and then drop to an unprivileged user need
/// CHOWN, SETUID, SETGID, and the file capabilities around them: the official
/// nginx image exited 1 with `chown("/var/cache/nginx/client_temp", 101) failed
/// (1: Operation not permitted)` when only NET_BIND_SERVICE was kept. That one
/// stays because the kernel refuses to exec FrankenPHP, a binary marked
/// `cap_net_bind_service=ep`, when it is outside the bounding set. Everything
/// else, NET_RAW above all, stays dropped.
#[tokio::test]
async fn a_workload_keeps_only_the_capabilities_of_the_allowlist() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let workload = string_arguments(&find_command(&runner, is_workload_run));
    let values_of = |flag: &str| {
        workload
            .windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(values_of("--cap-drop"), ["ALL"], "{workload:?}");
    let expected = [
        "--cap-drop",
        "ALL",
        "--cap-add",
        "CHOWN",
        "--cap-add",
        "DAC_OVERRIDE",
        "--cap-add",
        "FOWNER",
        "--cap-add",
        "FSETID",
        "--cap-add",
        "KILL",
        "--cap-add",
        "SETGID",
        "--cap-add",
        "SETUID",
        "--cap-add",
        "NET_BIND_SERVICE",
    ];
    assert!(
        workload
            .windows(expected.len())
            .any(|found| found == expected),
        "the drop comes first and the allowlist follows it: {workload:?}"
    );
    assert_eq!(
        values_of("--cap-add"),
        [
            "CHOWN",
            "DAC_OVERRIDE",
            "FOWNER",
            "FSETID",
            "KILL",
            "SETGID",
            "SETUID",
            "NET_BIND_SERVICE",
        ],
        "{workload:?}"
    );
    assert_eq!(
        values_of("--security-opt"),
        ["no-new-privileges:true"],
        "{workload:?}"
    );
    assert!(!workload.iter().any(|argument| argument == "--privileged"));
}

#[tokio::test]
async fn a_configured_runtime_reaches_only_the_workload_container() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let workload = string_arguments(&find_command(&runner, is_workload_run));
    assert!(
        workload
            .windows(2)
            .any(|pair| pair == ["--runtime", "runsc"]),
        "{workload:?}"
    );
    for pair in [
        ["--cap-drop", "ALL"],
        ["--security-opt", "no-new-privileges:true"],
    ] {
        assert!(
            workload.windows(2).any(|found| found == pair),
            "{workload:?}"
        );
    }
    assert!(workload.iter().any(|argument| argument == "--memory"));
    assert!(workload.iter().any(|argument| argument == "--pids-limit"));
    assert!(workload.iter().any(|argument| argument == "--cpus"));
    for other in [
        find_command(&runner, is_sandbox),
        find_command(&runner, is_buildx_build),
    ] {
        assert!(
            !string_arguments(&other)
                .iter()
                .any(|argument| argument == "--runtime"),
            "{other:?}"
        );
    }
}

#[tokio::test]
async fn without_a_configured_runtime_docker_chooses() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let workload = string_arguments(&find_command(&runner, is_workload_run));
    assert!(!workload.iter().any(|argument| argument == "--runtime"));
    let report = RuntimeExecutor::preflight(&DockerRuntimeExecutor::with_runner(
        runtime_config(&temp),
        runner,
    ))
    .await
    .expect("report");
    assert!(
        !report
            .checks
            .iter()
            .any(|check| check.name == "container-runtime")
    );
}

#[tokio::test]
async fn a_workload_the_runtime_refuses_fails_without_trying_another_runtime() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false).with_failed_workload_run());
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("the deployment must fail");

    let attempts = runner.count_commands(is_workload_run);
    assert_eq!(attempts, 1, "there is exactly one attempt");
    let workload = string_arguments(&find_command(&runner, is_workload_run));
    assert!(
        workload
            .windows(2)
            .any(|pair| pair == ["--runtime", "runsc"])
    );
}

#[tokio::test]
async fn preflight_is_fatal_when_the_configured_runtime_is_not_registered() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    let executor = DockerRuntimeExecutor::with_runner(config, runner);

    let report = RuntimeExecutor::preflight(&executor).await.expect("report");

    let check = report
        .checks
        .iter()
        .find(|check| check.name == "container-runtime")
        .expect("the check runs when a runtime is configured");
    assert!(check.fatal && !check.ready);
    assert!(check.detail.contains("runsc"), "{}", check.detail);
    assert!(check.detail.contains("runc"), "{}", check.detail);
    assert!(check.detail.contains("daemon.json"), "{}", check.detail);
    assert!(report.has_fatal_failure());
}

#[tokio::test]
async fn preflight_passes_and_telemetry_reports_a_registered_runtime() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_registered_runtime("runsc"));
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    fs::create_dir_all(&config.workspace_root).expect("workspace should be available");
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    let report = RuntimeExecutor::preflight(&executor).await.expect("report");
    assert!(!report.has_fatal_failure(), "{:?}", report.checks);

    let telemetry = RuntimeExecutor::node_telemetry(&executor)
        .await
        .expect("telemetry");
    assert_eq!(telemetry.runtime_dependencies["container_runtime"], "runsc");
    assert_eq!(telemetry.runtime_operational, Some(true));
}

#[tokio::test]
async fn a_runtime_that_ignores_cgroups_is_reported_but_does_not_stop_the_node() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_registered_runtime("runsc --ignore-cgroups"));
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    fs::create_dir_all(&config.workspace_root).expect("workspace should be available");
    let executor = DockerRuntimeExecutor::with_runner(config, runner);

    let report = RuntimeExecutor::preflight(&executor).await.expect("report");
    assert!(!report.has_fatal_failure(), "{:?}", report.checks);
    let limits = report
        .checks
        .iter()
        .find(|check| check.name == "container-runtime-limits")
        .expect("limits check");
    assert!(!limits.ready && !limits.fatal);
    assert!(limits.detail.contains("not enforced"), "{}", limits.detail);
    let dns = report
        .checks
        .iter()
        .find(|check| check.name == "container-dns")
        .expect("dns check");
    assert!(!dns.ready && !dns.fatal);

    let telemetry = RuntimeExecutor::node_telemetry(&executor)
        .await
        .expect("telemetry");
    assert_eq!(
        telemetry.runtime_dependencies["container_runtime_limits_enforced"],
        false
    );
}

#[tokio::test]
async fn configured_name_servers_reach_the_workload_through_a_read_only_file() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false));
    let config = DockerRuntimeConfig {
        container_dns: vec!["1.1.1.1".parse().expect("address")],
        ..runtime_config(&temp)
    };
    let root = config.workspace_root.clone();
    let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

    dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment completes");

    let file = root.join(".sakala-resolv.conf");
    assert_eq!(
        fs::read_to_string(&file).expect("resolv.conf"),
        "nameserver 1.1.1.1\n"
    );
    let workload = string_arguments(&find_command(&runner, is_workload_run));
    let mount = format!(
        "type=bind,src={},dst=/etc/resolv.conf,readonly",
        file.display()
    );
    assert!(workload.contains(&mount), "{workload:?}");
    for other in [
        find_command(&runner, is_sandbox),
        find_command(&runner, is_buildx_build),
    ] {
        assert!(
            !string_arguments(&other)
                .iter()
                .any(|argument| argument.contains("resolv.conf")),
            "{other:?}"
        );
    }
}

#[tokio::test]
async fn heartbeat_turns_not_operational_when_the_runtime_disappears() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let config = DockerRuntimeConfig {
        container_runtime: Some("runsc".to_owned()),
        ..runtime_config(&temp)
    };
    fs::create_dir_all(&config.workspace_root).expect("workspace should be available");
    let executor = DockerRuntimeExecutor::with_runner(config, runner);

    let telemetry = RuntimeExecutor::node_telemetry(&executor)
        .await
        .expect("telemetry");

    assert_eq!(telemetry.runtime_operational, Some(false));
}

#[tokio::test]
async fn an_unreadable_result_from_the_sandbox_is_refused() {
    for stdout in ["root:x:0:0:root\n", "bm90IGpzb24=\n"] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(false).with_sandbox_stdout(stdout));
        let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

        let error = dispatch(
            executor,
            &inspect_command(),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("an unreadable report must be refused");

        assert!(error.to_string().contains("unreadable"), "{error}");
        assert!(!error.to_string().contains("root:x"), "{error}");
    }
}

#[tokio::test]
async fn an_unpinned_or_missing_sandbox_image_is_refused() {
    for image in ["", "sakala-railpack:latest"] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(false));
        let config = DockerRuntimeConfig {
            railpack_image: image.to_owned(),
            ..runtime_config(&temp)
        };
        let executor = DockerRuntimeExecutor::with_runner(config, runner.clone());

        let report = RuntimeExecutor::preflight(&executor).await.expect("report");
        assert!(report.has_fatal_failure(), "{image:?}");

        let error = dispatch(
            executor,
            &inspect_command(),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("must be refused");
        assert_eq!(error.code(), "runtime_dependency_failed", "{image:?}");
        assert_eq!(
            runner.count_commands(|spec| {
                spec.args.first().is_some_and(|argument| argument == "run")
            }),
            0
        );
    }
}

#[tokio::test]
async fn a_plan_naming_another_projects_image_is_refused() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(false).with_sandbox_plan(
        r#"{"steps":[{"inputs":[{"image":"\u0073akala/project-11111111:abc"}]}]}"#,
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_command("railpack"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("must be refused");

    assert_eq!(error.code(), "runtime_build_failed");
    assert_eq!(runner.count_commands(is_buildx_build), 0);
}

#[tokio::test]
async fn builds_never_reuse_anything_from_another_project() {
    for (builder, dockerfile) in [("dockerfile", true), ("railpack", false)] {
        let temp = TempDir::new().expect("temp directory should be available");
        let runner = Arc::new(FakeRunner::new(dockerfile));
        let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

        dispatch(
            executor,
            &deploy_command(builder),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect("deployment completes");

        let build = string_arguments(&find_command(&runner, is_buildx_build));
        assert!(
            build.iter().any(|argument| argument == "--no-cache"),
            "{build:?}"
        );
        assert!(
            build.iter().any(|argument| argument == "--pull"),
            "{build:?}"
        );
        let namespace = if dockerfile {
            "BUILDKIT_CACHE_MOUNT_NS=ff66ed4a-6303-4be6-8ef4-63c28b112680"
        } else {
            "cache-key=ff66ed4a-6303-4be6-8ef4-63c28b112680"
        };
        assert!(
            build.iter().any(|argument| argument == namespace),
            "{build:?}"
        );
    }
}

#[test]
fn failure_fixture_documents_the_exact_messages_the_runtime_sends() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../examples/failures/deploy-failures.json"
    ))
    .expect("failure fixture should be JSON");
    let errors = [
        RuntimeError::AppNotListening {
            port: 3000,
            waited_seconds: 120,
        },
        RuntimeError::AppUnhealthy {
            port: 3000,
            status: 500,
            waited_seconds: 120,
        },
        RuntimeError::AppCrashed { exit_code: Some(3) },
        RuntimeError::AppCrashed { exit_code: None },
        RuntimeError::AppExecDenied { exit_code: 126 },
        RuntimeError::AppOutOfMemory {
            memory_limit_mb: Some(256),
        },
        RuntimeError::PortConflict {
            environment_port: Some(8080),
            container_port: 3000,
        },
        RuntimeError::PortConflict {
            environment_port: None,
            container_port: 3000,
        },
    ];
    let expected = fixture.as_array().expect("fixture is a list");
    assert_eq!(expected.len(), errors.len());
    for (error, expected) in errors.iter().zip(expected) {
        assert_eq!(expected["error_code"], error.code());
        assert_eq!(expected["error_message"], error.to_string());
        assert!(
            error.to_string().chars().count() <= 1_000,
            "fits the fail body"
        );
    }
}

const PROJECT_NETWORK: &str = "sakala-project-ff66ed4a-6303-4be6-8ef4-63c28b112680";

fn command_index(runner: &FakeRunner, matches: impl Fn(&[String]) -> bool) -> Option<usize> {
    runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .position(|command| command.program == "docker" && matches(&string_arguments(command)))
}

fn is_network(args: &[String], action: &str) -> bool {
    args.first().is_some_and(|arg| arg == "network") && args.get(1).is_some_and(|a| a == action)
}

fn network_removed(runner: &FakeRunner) -> bool {
    command_index(runner, |args| args == ["network", "rm", PROJECT_NETWORK]).is_some()
}

#[tokio::test]
async fn a_workload_joins_only_its_own_project_network_after_the_router_does() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_command("auto"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("deployment should complete");

    let run = string_arguments(&find_command(&runner, is_workload_run));
    let network = run
        .iter()
        .position(|arg| arg == "--network")
        .map(|index| run[index + 1].clone());
    assert_eq!(network.as_deref(), Some(PROJECT_NETWORK));
    assert!(!run.iter().any(|arg| arg == "sakala-runtime"), "{run:?}");

    let create = command_index(&runner, |args| is_network(args, "create")).expect("create");
    let created = string_arguments(&runner.commands.lock().expect("command lock")[create]);
    for label in [
        "dev.sakala.managed=true",
        "dev.sakala.network-kind=project",
        "dev.sakala.project-id=ff66ed4a-6303-4be6-8ef4-63c28b112680",
        "dev.sakala.agent-id=local-agent-01",
    ] {
        assert!(created.iter().any(|arg| arg == label), "{label}");
    }
    assert_eq!(created.last().map(String::as_str), Some(PROJECT_NETWORK));
    let connect = command_index(&runner, |args| {
        args == ["network", "connect", PROJECT_NETWORK, "sakala-caddy"]
    })
    .expect("the router joins the project network");
    let started = command_index(&runner, |args| args.first().is_some_and(|a| a == "run"))
        .expect("workload run");
    let probed = runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .position(is_probe)
        .expect("probe");
    assert!(create < connect && connect < started && started < probed);
    assert!(
        !network_removed(&runner),
        "a live deployment keeps its network"
    );
}

#[tokio::test]
async fn stop_removes_the_project_network_once_only_the_router_uses_it() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("stopping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n")
            .with_network_users(PROJECT_NETWORK, "sakala-caddy\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    RuntimeExecutor::stop_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("stop should succeed");

    let removed_container =
        command_index(&runner, |args| args == ["rm", "stopping"]).expect("container removal");
    let disconnect = command_index(&runner, |args| {
        args == ["network", "disconnect", PROJECT_NETWORK, "sakala-caddy"]
    })
    .expect("the router leaves the network");
    let removed_network = command_index(&runner, |args| args == ["network", "rm", PROJECT_NETWORK])
        .expect("network removal");
    assert!(removed_container < disconnect && disconnect < removed_network);
}

#[tokio::test]
async fn stop_keeps_the_network_while_another_container_still_uses_it() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("stopping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n")
            .with_network_users(
                PROJECT_NETWORK,
                "sakala-caddy\nsakala-app-ff66ed4a-sleeping\n",
            ),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    RuntimeExecutor::stop_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("stop should succeed");

    assert!(!network_removed(&runner));
    assert!(command_index(&runner, |args| is_network(args, "disconnect")).is_none());
}

#[tokio::test]
async fn stopping_an_absent_workload_still_removes_its_leftover_network() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let output = RuntimeExecutor::stop_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("stop is idempotent");

    assert_eq!(output.result["status"], "already_stopped");
    assert!(network_removed(&runner));
}

#[tokio::test]
async fn sleep_keeps_the_network_the_stopped_container_needs_to_wake() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(
        FakeRunner::new(true)
            .with_docker_ps("sleeping\tUp 1 minute\tportfolio.run.sakala.localhost\t3000\n"),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    RuntimeExecutor::sleep_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("sleep should succeed");

    assert!(command_index(&runner, |args| args.first().is_some_and(|a| a == "network")).is_none());
}

#[tokio::test]
async fn a_failed_first_deployment_leaves_no_project_network() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_failed_workload_run());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    dispatch(
        executor,
        &deploy_command("auto"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("the deployment must fail");

    let candidate_removed = runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .position(is_candidate_removal)
        .expect("candidate removal");
    let removed = command_index(&runner, |args| args == ["network", "rm", PROJECT_NETWORK])
        .expect("network removal");
    assert!(candidate_removed < removed);
}

#[tokio::test]
async fn waking_a_workload_reconnects_the_router_before_probing_it() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "sleeping\tExited (0) 1 minute ago\tportfolio.run.sakala.localhost\t3000\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    RuntimeExecutor::wake_project(
        &executor,
        lifecycle_request(),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect("wake should succeed");

    let connect = command_index(&runner, |args| {
        args == ["network", "connect", PROJECT_NETWORK, "sakala-caddy"]
    })
    .expect("router reconnect");
    let probed = runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .position(is_probe)
        .expect("probe");
    assert!(connect < probed);
    assert!(command_index(&runner, |args| is_network(args, "create")).is_none());
}

#[tokio::test]
async fn startup_recovery_removes_unused_project_networks_and_reconnects_the_router() {
    let temp = TempDir::new().expect("temp directory should be available");
    let used = "sakala-project-11111111-1111-4111-8111-111111111111";
    let attached = "sakala-project-22222222-2222-4222-8222-222222222222";
    let empty = "sakala-project-33333333-3333-4333-8333-333333333333";
    let mut runner = FakeRunner::new(true)
        .with_network_users(used, "sakala-app-11111111-a\n")
        .with_network_users(attached, "sakala-caddy\nsakala-app-22222222-b\n");
    runner.project_networks_stdout = format!("{used}\n{attached}\n{empty}\nsakala-project-x\n");
    runner.router_networks_stdout = format!("sakala-runtime\n{attached}\n{empty}\n");
    // The router itself does not keep a network in use.
    runner.container_networks_stdout = format!(
        "sakala-app-11111111-a\t{used}\nsakala-app-22222222-b\t{attached}\nsakala-caddy\tsakala-runtime,{attached},{empty}\n"
    );
    let runner = Arc::new(runner);
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    RuntimeExecutor::recover(&executor, None)
        .await
        .expect("recovery should succeed");

    let listing = find_command(&runner, |spec| {
        string_arguments(spec).starts_with(&["network".to_owned(), "ls".to_owned()])
    });
    assert!(
        string_arguments(&listing)
            .iter()
            .any(|arg| arg == "label=dev.sakala.agent-id=local-agent-01"),
        "only this Agent's networks are touched"
    );
    let removed = |name: &str| command_index(&runner, |args| args == ["network", "rm", name]);
    let connected = |name: &str| {
        command_index(&runner, |args| {
            args == ["network", "connect", name, "sakala-caddy"]
        })
    };
    assert!(removed(empty).is_some());
    assert!(removed(used).is_none() && removed(attached).is_none());
    assert!(connected(used).is_some());
    assert!(connected(attached).is_none() && connected(empty).is_none());
    assert!(removed("sakala-project-x").is_none());
}

fn docker_calls(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .commands
        .lock()
        .expect("command lock")
        .iter()
        .filter(|command| command.program == "docker")
        .map(string_arguments)
        .collect()
}

#[tokio::test]
async fn periodic_reconcile_reattaches_a_recreated_router_without_removing_while_busy() {
    let temp = TempDir::new().expect("temp directory should be available");
    let a = "sakala-project-11111111-1111-4111-8111-111111111111";
    let b = "sakala-project-22222222-2222-4222-8222-222222222222";
    let empty = "sakala-project-33333333-3333-4333-8333-333333333333";
    let mut runner = FakeRunner::new(true);
    runner.project_networks_stdout = format!("{a}\n{b}\n{empty}\n");
    // Compose recreated the router: it is back on the shared network only.
    runner.router_networks_stdout = "sakala-runtime\n".to_owned();
    let runner = Arc::new(runner);
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let result = RuntimeExecutor::reconcile_networks(&executor, false)
        .await
        .expect("reconcile");

    assert_eq!(result.router_attached, 3);
    assert_eq!(result.removed, 0);
    for name in [a, b, empty] {
        assert!(
            command_index(&runner, |args| args
                == ["network", "connect", name, "sakala-caddy"])
            .is_some(),
            "{name}"
        );
    }
    let calls = docker_calls(&runner);
    assert!(
        !calls
            .iter()
            .any(|args| args.first().is_some_and(|arg| arg == "ps")
                || is_network(args, "rm")
                || is_network(args, "disconnect")),
        "nothing is removed while a command runs: {calls:?}"
    );
}

#[tokio::test]
async fn periodic_reconcile_is_cheap_when_nothing_changed() {
    let temp = TempDir::new().expect("temp directory should be available");
    let a = "sakala-project-11111111-1111-4111-8111-111111111111";
    let mut runner = FakeRunner::new(true);
    runner.project_networks_stdout = format!("{a}\n");
    runner.router_networks_stdout = format!("sakala-runtime\n{a}\n");
    runner.container_networks_stdout = format!("sakala-app-11111111-a\t{a}\n");
    let runner = Arc::new(runner);
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let result = RuntimeExecutor::reconcile_networks(&executor, true)
        .await
        .expect("reconcile");

    assert_eq!(result.removed + result.router_attached, 0);
    let calls = docker_calls(&runner);
    assert_eq!(
        calls.len(),
        3,
        "network ls, docker ps, router inspect: {calls:?}"
    );
}

#[tokio::test]
async fn idle_periodic_reconcile_removes_a_network_no_container_uses() {
    let temp = TempDir::new().expect("temp directory should be available");
    let a = "sakala-project-11111111-1111-4111-8111-111111111111";
    let empty = "sakala-project-33333333-3333-4333-8333-333333333333";
    let mut runner = FakeRunner::new(true);
    runner.project_networks_stdout = format!("{a}\n{empty}\n");
    runner.router_networks_stdout = format!("sakala-runtime\n{a}\n{empty}\n");
    runner.container_networks_stdout =
        format!("sakala-app-11111111-a\t{a}\nsakala-caddy\tsakala-runtime,{a},{empty}\n");
    let runner = Arc::new(runner);
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let result = RuntimeExecutor::reconcile_networks(&executor, true)
        .await
        .expect("reconcile");

    assert_eq!(result.removed, 1);
    assert!(command_index(&runner, |args| args == ["network", "rm", empty]).is_some());
    assert!(command_index(&runner, |args| args == ["network", "rm", a]).is_none());
}

#[tokio::test]
async fn preflight_warns_without_stopping_the_node_when_address_pools_run_short() {
    let temp = TempDir::new().expect("temp directory should be available");
    let mut runner = FakeRunner::new(true);
    runner.address_pools_stdout = "null\n".to_owned();
    let config = DockerRuntimeConfig {
        max_active_containers: 40,
        ..runtime_config(&temp)
    };
    let executor = DockerRuntimeExecutor::with_runner(config, Arc::new(runner));

    let report = RuntimeExecutor::preflight(&executor)
        .await
        .expect("preflight");

    assert!(!report.has_fatal_failure());
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "project-networks")
        .expect("check");
    assert!(!check.ready && !check.fatal);
    assert!(check.detail.contains("about 31 more"), "{}", check.detail);
    assert!(
        check.detail.contains("default-address-pools"),
        "{}",
        check.detail
    );
}

#[tokio::test]
async fn preflight_warns_when_docker_cannot_create_a_network() {
    let temp = TempDir::new().expect("temp directory should be available");
    let mut runner = FakeRunner::new(true);
    runner.network_create_stderr = Some(
        "Error response from daemon: all predefined address pools have been fully subnetted"
            .to_owned(),
    );
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), Arc::new(runner));

    let report = RuntimeExecutor::preflight(&executor)
        .await
        .expect("preflight");

    assert!(!report.has_fatal_failure());
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "project-networks")
        .expect("check");
    assert!(!check.ready);
    assert!(check.detail.contains("fully subnetted"), "{}", check.detail);
}

#[tokio::test]
async fn a_deployment_without_a_free_address_range_fails_as_node_capacity() {
    let temp = TempDir::new().expect("temp directory should be available");
    let mut runner = FakeRunner::new(true);
    runner.network_create_stderr = Some(
        "Error response from daemon: could not find an available, non-overlapping IPv4 address pool among the defaults to assign to the network".to_owned(),
    );
    let runner = Arc::new(runner);
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(
        executor,
        &deploy_command("auto"),
        Arc::new(RecordingReporter::default()),
    )
    .await
    .expect_err("the deployment must fail");

    assert_eq!(error.code(), "runtime_capacity_exceeded");
    assert!(!error.to_string().contains("daemon"), "{error}");
    assert_eq!(runner.count_commands(is_workload_run), 0);
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

const SANDBOX_IMAGE: &str = "sakala-railpack@sha256:0123456789abcdef";

fn runtime_config(temp: &TempDir) -> DockerRuntimeConfig {
    DockerRuntimeConfig {
        railpack_image: SANDBOX_IMAGE.to_owned(),
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
        remove_images: false,
        cancellation: CancellationToken::new(),
    }
}

// `docker inspect` output: status, restarts, exit code, OOM flag, HEALTHCHECK, memory bytes.
const RUNNING_STATE: &str = "running\t0\t0\tfalse\tnone\t268435456\n";
// Captured from `wget` in caddy:2.11.3-alpine (BusyBox v1.37.0) against real servers.
const OK_PROBE_STDERR: &str = "Connecting to app:3000 (172.25.0.3:3000)\n  HTTP/1.1 200 OK\n  Content-Length: 4\n  \nremote file exists\n";
const NOT_FOUND_PROBE_STDERR: &str = "Connecting to app:3000 (172.25.0.4:3000)\n  HTTP/1.1 404 Not Found\nwget: server returned error: HTTP/1.1 404 Not Found\n";
const SERVER_ERROR_PROBE_STDERR: &str = "Connecting to app:3000 (172.25.0.5:3000)\n  HTTP/1.1 500 Internal Server Error\nwget: server returned error: HTTP/1.1 500 Internal Server Error\n";
const REFUSED_PROBE_STDERR: &str = "Connecting to app:3000 (172.25.0.3:3000)\nwget: can't connect to remote host (172.25.0.3): Connection refused\n";
const REDIRECT_PROBE_STDERR: &str = "Connecting to app:3000 (172.25.0.9:3000)\n  HTTP/1.1 302 Found\n  Location: http://outside.invalid/x\n  HTTP/1.1 302 Found\n  Location: http://outside.invalid/x\nwget: too many redirections\n";
const BAD_ADDRESS_PROBE_STDERR: &str = "wget: bad address 'app:3000'\n";

fn is_sandbox(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "run")
        && spec.args.iter().any(|argument| argument == SANDBOX_IMAGE)
}

/// Host path mounted at `target` in the sandbox.
fn sandbox_mount(spec: &CommandSpec, target: &str) -> String {
    spec.args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .find_map(|argument| {
            let rest = argument.strip_prefix("type=bind,src=")?;
            let (source, destination) = rest.split_once(",dst=")?;
            destination
                .split(',')
                .next()
                .is_some_and(|value| value == target)
                .then(|| source.to_owned())
        })
        .expect("sandbox mount")
}

/// The `docker run` of a workload, as opposed to the Railpack sandbox.
fn is_workload_run(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "run")
        && !is_sandbox(spec)
}

fn is_probe(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "exec")
        && spec.args.iter().any(|argument| argument == "wget")
        && !spec.args.iter().any(|argument| argument == "--help")
}

fn is_app_logs(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "logs")
        && !spec.args.iter().any(|argument| argument == "--follow")
}

fn is_candidate_removal(spec: &CommandSpec) -> bool {
    spec.program == "docker"
        && spec.args.first().is_some_and(|argument| argument == "rm")
        && spec.args.iter().any(|argument| argument == "--force")
}

fn deploy_with_start_timeout(seconds: u64) -> AgentCommand {
    let mut command = deploy_command("auto");
    command.payload["timeouts"] = json!({
        "build_timeout_seconds": 5,
        "start_timeout_seconds": seconds,
        "command_timeout_seconds": 10
    });
    command
}

fn route_file(config: &DockerRuntimeConfig) -> std::path::PathBuf {
    config
        .caddy_sites_dir
        .join("ff66ed4a-6303-4be6-8ef4-63c28b112680.Caddyfile")
}

fn reported_messages(reporter: &RecordingReporter) -> Vec<String> {
    reporter
        .logs
        .lock()
        .expect("log lock")
        .iter()
        .map(|log| log.message.clone())
        .collect()
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
    fail_previous_stop: bool,
    image_list_stdout: String,
    image_inspect_stdout: String,
    image_users_stdout: String,
    follow_delay: Option<Duration>,
    follow_lines: Vec<String>,
    live_caddy: AtomicBool,
    active_builds: AtomicUsize,
    max_concurrent_builds: AtomicUsize,
    inspect_states: Mutex<VecDeque<String>>,
    probe_results: Mutex<VecDeque<(i32, String)>>,
    app_stdout: Vec<String>,
    app_stderr: Vec<String>,
    events_stdout: String,
    crash_inspect_stdout: String,
    fail_caddy_validate: bool,
    container_env_stdout: String,
    env_inspect_fails: bool,
    validate_delay: Option<Duration>,
    sandbox_plan: String,
    sandbox_stdout: Option<String>,
    registered_runtimes: String,
    fail_workload_run: bool,
    fail_sandbox_info: bool,
    built_dockerfiles: Mutex<Vec<String>>,
    /// `docker ps --filter network=<name>` output, by network. Missing means unused.
    network_users: Mutex<HashMap<String, String>>,
    project_networks_stdout: String,
    /// `docker ps --all --format '{{.Names}}\t{{.Networks}}'` output.
    container_networks_stdout: String,
    router_networks_stdout: String,
    address_pools_stdout: String,
    network_create_stderr: Option<String>,
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

struct BuildFailingRunner {
    inner: FakeRunner,
}

#[async_trait]
impl ProcessRunner for BuildFailingRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        if spec.args.iter().any(|argument| argument == "buildx") {
            self.inner
                .commands
                .lock()
                .expect("command lock")
                .push(spec.clone());
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "simulated build failure".to_owned(),
            });
        }
        self.inner.run(spec, sink).await
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
            fail_previous_stop: false,
            image_list_stdout: String::new(),
            image_inspect_stdout: String::new(),
            image_users_stdout: String::new(),
            follow_delay: None,
            follow_lines: Vec::new(),
            live_caddy: AtomicBool::new(true),
            active_builds: AtomicUsize::new(0),
            max_concurrent_builds: AtomicUsize::new(0),
            inspect_states: Mutex::new(VecDeque::new()),
            probe_results: Mutex::new(VecDeque::new()),
            app_stdout: vec!["application listening".to_owned()],
            app_stderr: Vec::new(),
            events_stdout: String::new(),
            crash_inspect_stdout: String::new(),
            fail_caddy_validate: false,
            container_env_stdout: "[]\n".to_owned(),
            env_inspect_fails: false,
            validate_delay: None,
            sandbox_plan: r#"{"steps":[]}"#.to_owned(),
            sandbox_stdout: None,
            registered_runtimes: "io.containerd.runc.v2\nrunc\n".to_owned(),
            fail_workload_run: false,
            fail_sandbox_info: false,
            built_dockerfiles: Mutex::new(Vec::new()),
            network_users: Mutex::new(HashMap::new()),
            project_networks_stdout: String::new(),
            container_networks_stdout: String::new(),
            router_networks_stdout: String::new(),
            address_pools_stdout: r#"[{"Base":"172.16.0.0/12","Size":24}]"#.to_owned(),
            network_create_stderr: None,
        }
    }

    fn with_network_users(self, network: &str, users: &str) -> Self {
        self.network_users
            .lock()
            .expect("network lock")
            .insert(network.to_owned(), users.to_owned());
        self
    }

    /// Container states `docker inspect` reports, in order. The last one repeats.
    fn with_inspect_states(self, states: &[&str]) -> Self {
        *self.inspect_states.lock().expect("state lock") =
            states.iter().map(|state| (*state).to_owned()).collect();
        self
    }

    /// `wget` results, in order, as exit code and stderr. The last one repeats.
    fn with_probe_results(self, results: &[(i32, &str)]) -> Self {
        *self.probe_results.lock().expect("probe lock") = results
            .iter()
            .map(|(code, stderr)| (*code, (*stderr).to_owned()))
            .collect();
        self
    }

    fn with_app_output(mut self, stdout: &[&str], stderr: &[&str]) -> Self {
        self.app_stdout = stdout.iter().map(|line| (*line).to_owned()).collect();
        self.app_stderr = stderr.iter().map(|line| (*line).to_owned()).collect();
        self
    }

    fn with_events(mut self, stdout: impl Into<String>) -> Self {
        self.events_stdout = stdout.into();
        self
    }

    fn with_crash_inspect(mut self, stdout: impl Into<String>) -> Self {
        self.crash_inspect_stdout = stdout.into();
        self
    }

    fn with_sandbox_plan(mut self, plan: &str) -> Self {
        self.sandbox_plan = plan.to_owned();
        self
    }

    fn with_registered_runtime(mut self, name: &str) -> Self {
        self.registered_runtimes.push_str(name);
        self.registered_runtimes.push('\n');
        self
    }

    fn with_failed_workload_run(mut self) -> Self {
        self.fail_workload_run = true;
        self
    }

    /// `railpack info` exits 1, as it does when no provider recognises the repository.
    fn with_failed_sandbox_info(mut self) -> Self {
        self.fail_sandbox_info = true;
        self
    }

    fn with_sandbox_stdout(mut self, stdout: &str) -> Self {
        self.sandbox_stdout = Some(stdout.to_owned());
        self
    }

    fn with_validate_delay(mut self, delay: Duration) -> Self {
        self.validate_delay = Some(delay);
        self
    }

    fn with_container_env(mut self, stdout: impl Into<String>) -> Self {
        self.container_env_stdout = stdout.into();
        self
    }

    fn with_failed_env_inspect(mut self) -> Self {
        self.env_inspect_fails = true;
        self
    }

    fn with_failed_caddy_validate(mut self) -> Self {
        self.fail_caddy_validate = true;
        self
    }

    fn command_position(&self, matches: impl Fn(&CommandSpec) -> bool) -> Option<usize> {
        self.commands
            .lock()
            .expect("command lock")
            .iter()
            .position(matches)
    }

    fn count_commands(&self, matches: impl Fn(&CommandSpec) -> bool) -> usize {
        self.commands
            .lock()
            .expect("command lock")
            .iter()
            .filter(|command| matches(command))
            .count()
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

    fn with_follow_lines(mut self, lines: &[&str]) -> Self {
        self.follow_lines = lines.iter().map(|line| (*line).to_owned()).collect();
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

    fn with_failed_previous_stop(mut self) -> Self {
        self.fail_previous_stop = true;
        self
    }

    /// `images` holds `(id, project, created, size)` and is printed the way
    /// `docker image ls` and `docker image inspect` print it.
    fn with_images(mut self, images: &[(&str, Uuid, &str, u64)]) -> Self {
        self.image_list_stdout = images
            .iter()
            .map(|(id, project, _, _)| format!("{id}\tsakala/project-{project}\t{id}-tag\n"))
            .collect();
        self.image_inspect_stdout = images
            .iter()
            .map(|(id, project, created, size)| {
                format!("{id}\t{project}\t<no value>\t{created}\t{size}\n")
            })
            .collect();
        self
    }

    /// What `docker container inspect` prints for the host's containers: the
    /// image ID each runs and, after a tab, its project label.
    fn with_image_users(mut self, stdout: impl Into<String>) -> Self {
        self.image_users_stdout = stdout.into();
        self
    }

    /// One container of `project` on an image outside the listing, so the
    /// project is not orphaned and keeps the active-and-previous rule.
    fn with_project_container(self, project: Uuid) -> Self {
        self.with_docker_ps("project-container\n")
            .with_image_users(format!("sha256:elsewhere\t{project}\n"))
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
        if let Some(output) = self.network_output(spec) {
            return Ok(output);
        }
        let previous_stop = spec.program == "docker"
            && spec.args.first().is_some_and(|argument| argument == "stop")
            && spec
                .args
                .iter()
                .any(|argument| argument == "previous-container");
        if previous_stop && self.fail_previous_stop {
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "simulated previous container stop failure".to_owned(),
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
        if spec.program == "docker" && spec.args.iter().any(|argument| argument == "--follow") {
            for line in &self.follow_lines {
                sink.line(ProcessStream::Stdout, line).await?;
            }
            if let Some(delay) = self.follow_delay {
                tokio::time::sleep(delay).await;
            }
        }

        if spec.program == "git" && spec.args.iter().any(|argument| argument == "init") {
            let source = spec.args.last().expect("git init target");
            fs::create_dir_all(source).expect("fake repository should be created");
            if self.dockerfile {
                fs::write(
                    std::path::Path::new(source).join("Dockerfile"),
                    "FROM scratch\nEXPOSE 8080\n",
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

            // A monorepo, with the ways a root directory can go wrong.
            let source = std::path::Path::new(source);
            let app = source.join("apps/web");
            fs::create_dir_all(&app).expect("fake app directory");
            fs::write(app.join("package.json"), "{}\n").expect("fake app manifest");
            if self.dockerfile {
                fs::write(
                    app.join("Dockerfile"),
                    "# syntax=evil/frontend\nFROM scratch\n# apps-web\nEXPOSE 80/tcp\n",
                )
                .expect("fake app Dockerfile");
            }
            fs::write(source.join("a-file"), "not a directory\n").expect("fake file");
            std::os::unix::fs::symlink("/etc", source.join("link-out")).expect("escaping symlink");
            std::os::unix::fs::symlink("apps/web", source.join("link-in")).expect("inner symlink");
            let evil = source.join("apps/evil");
            fs::create_dir_all(&evil).expect("fake evil directory");
            std::os::unix::fs::symlink("/etc/hostname", evil.join("Dockerfile"))
                .expect("escaping Dockerfile");
        }

        if is_buildx_build(spec)
            && let Some(position) = spec.args.iter().position(|argument| argument == "--file")
            && let Ok(content) = fs::read_to_string(&spec.args[position + 1])
        {
            self.built_dockerfiles
                .lock()
                .expect("dockerfile lock")
                .push(content);
        }
        if self.fail_sandbox_info
            && is_sandbox(spec)
            && spec.args.iter().any(|argument| argument == "info")
        {
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "Railpack could not determine how to build the app.".to_owned(),
            });
        }
        if is_sandbox(spec) {
            let result = if spec.args.iter().any(|argument| argument == "prepare") {
                self.sandbox_plan.as_str()
            } else {
                r#"{"provider":"node","version":"22"}"#
            };
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                stdout: self
                    .sandbox_stdout
                    .clone()
                    .unwrap_or_else(|| BASE64_STANDARD.encode(result)),
                stderr: String::new(),
            });
        }

        if self.fail_workload_run && is_workload_run(spec) {
            return Ok(ProcessOutput {
                success: false,
                code: Some(125),
                stdout: String::new(),
                stderr: "docker: unknown or invalid runtime name: runsc".to_owned(),
            });
        }
        if is_probe(spec) {
            let (code, stderr) = {
                let mut results = self.probe_results.lock().expect("probe lock");
                if results.len() > 1 {
                    results.pop_front().expect("probe result")
                } else {
                    results
                        .front()
                        .cloned()
                        .unwrap_or((0, OK_PROBE_STDERR.to_owned()))
                }
            };
            return Ok(ProcessOutput {
                success: code == 0,
                code: Some(code),
                stdout: String::new(),
                stderr,
            });
        }
        if let Some(delay) = self.validate_delay
            && spec.args.iter().any(|argument| argument == "validate")
        {
            tokio::time::sleep(delay).await;
        }
        if self.fail_caddy_validate && spec.args.iter().any(|argument| argument == "validate") {
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "simulated invalid Caddyfile".to_owned(),
            });
        }
        if is_app_logs(spec) {
            for line in &self.app_stdout {
                sink.line(ProcessStream::Stdout, line).await?;
            }
            for line in &self.app_stderr {
                sink.line(ProcessStream::Stderr, line).await?;
            }
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                stdout: self.app_stdout.join("\n"),
                stderr: self.app_stderr.join("\n"),
            });
        }

        let reads_container_env = spec
            .args
            .iter()
            .any(|argument| argument.to_string_lossy().contains(".Config.Env"));
        if reads_container_env && self.env_inspect_fails {
            return Ok(ProcessOutput {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "simulated inspect failure".to_owned(),
            });
        }

        let scripted_state;
        let stdout = if spec.program == "docker"
            && spec.args.first().is_some_and(|arg| arg == "info")
            && spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().contains(".Runtimes"))
        {
            self.registered_runtimes.as_str()
        } else if spec.program == "docker" && spec.args.first().is_some_and(|arg| arg == "ps") {
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
            &self.image_inspect_stdout
        } else if spec.program == "docker"
            && spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().starts_with("{{.Image}}"))
        {
            &self.image_users_stdout
        } else if spec.program == "docker" && spec.args.iter().any(|argument| argument == "inspect")
        {
            if reads_container_env {
                &self.container_env_stdout
            } else if spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().contains("{{.Id}}"))
            {
                &self.crash_inspect_stdout
            } else {
                let mut states = self.inspect_states.lock().expect("state lock");
                scripted_state = if states.len() > 1 {
                    states.pop_front().expect("scripted state")
                } else {
                    states
                        .front()
                        .cloned()
                        .unwrap_or_else(|| RUNNING_STATE.to_owned())
                };
                &scripted_state
            }
        } else if spec.program == "docker" && spec.args.first().is_some_and(|arg| arg == "events") {
            &self.events_stdout
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

impl FakeRunner {
    /// Answers the project network commands, which the generic branches would
    /// otherwise mistake for container commands.
    fn network_output(&self, spec: &CommandSpec) -> Option<ProcessOutput> {
        if spec.program != "docker" {
            return None;
        }
        let args = string_arguments(spec);
        let ok = |stdout: &str| ProcessOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        };
        if args.first().is_some_and(|arg| arg == "ps")
            && let Some(network) = args.iter().find_map(|arg| arg.strip_prefix("network="))
        {
            let users = self.network_users.lock().expect("network lock");
            return Some(ok(users.get(network).map_or("", String::as_str)));
        }
        if args.first().is_some_and(|arg| arg == "ps")
            && args.iter().any(|arg| arg.contains("{{.Networks}}"))
        {
            return Some(ok(&self.container_networks_stdout));
        }
        if args
            .iter()
            .any(|arg| arg.contains(".NetworkSettings.Networks"))
        {
            return Some(ok(&self.router_networks_stdout));
        }
        if args.iter().any(|arg| arg.contains(".DefaultAddressPools")) {
            return Some(ok(&self.address_pools_stdout));
        }
        if args.first().is_some_and(|arg| arg == "network") {
            match args.get(1).map(String::as_str) {
                Some("create") => {
                    if let Some(stderr) = &self.network_create_stderr {
                        return Some(ProcessOutput {
                            success: false,
                            code: Some(1),
                            stdout: String::new(),
                            stderr: stderr.clone(),
                        });
                    }
                }
                Some("ls")
                    if args
                        .iter()
                        .any(|arg| arg == "label=dev.sakala.network-kind=project") =>
                {
                    return Some(ok(&self.project_networks_stdout));
                }
                _ => {}
            }
            return Some(ok(""));
        }
        None
    }
}

#[derive(Default)]
struct RecordingReporter {
    events: Mutex<Vec<DeploymentEvent>>,
    logs: Mutex<Vec<DeploymentLog>>,
    committed_output: Mutex<Option<CommandOutput>>,
}

#[derive(Default)]
struct RecordingReporterFactory {
    created: AtomicUsize,
    reporters: Mutex<Vec<(Uuid, LogBounds, Arc<RecordingReporter>)>>,
    redactors: Mutex<Vec<ValueRedactor>>,
}

impl RuntimeReporterFactory for RecordingReporterFactory {
    fn reporter(
        &self,
        command_id: Uuid,
        log_bounds: LogBounds,
        redactor: ValueRedactor,
    ) -> Arc<dyn RuntimeReporter> {
        self.redactors.lock().expect("factory lock").push(redactor);
        self.created.fetch_add(1, Ordering::SeqCst);
        let reporter = Arc::new(RecordingReporter::default());
        self.reporters.lock().expect("factory lock").push((
            command_id,
            log_bounds,
            Arc::clone(&reporter),
        ));
        reporter
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

    fn mark_deployment_committed(&self, output: CommandOutput) {
        *self.committed_output.lock().expect("commit lock") = Some(output);
    }

    fn deployment_committed(&self) -> bool {
        self.committed_output.lock().expect("commit lock").is_some()
    }

    fn committed_output(&self) -> Option<CommandOutput> {
        self.committed_output.lock().expect("commit lock").clone()
    }
}

struct RejectingLogReporter;

#[async_trait]
impl RuntimeReporter for RejectingLogReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Err(RuntimeExecutionError::reporting(
            "log delivery stopped: control-plane rejected report with HTTP 422",
        ))
    }

    async fn flush(&self) -> Result<(), RuntimeExecutionError> {
        Err(RuntimeExecutionError::reporting(
            "log delivery stopped: control-plane rejected report with HTTP 422",
        ))
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
