use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::{
    commands::CommandDispatcher,
    ports::{
        CommandOutput, DeployProjectRequest, RepositoryCredential, RuntimeExecutionError,
        RuntimeExecutor, RuntimeReporter, SecretString,
    },
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, DeploymentEvent, DeploymentLog,
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
    assert_eq!(report.checks.len(), 7);
    assert!(report.checks.iter().any(|check| check.name == "git"));
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "caddy-routing")
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
async fn cancelled_deployment_cleans_workspace_and_candidate_artifacts() {
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
async fn git_checkout_failure_uses_a_repository_error_code() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FailingGitRunner);
    let reporter = Arc::new(RecordingReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let error = dispatch(executor, &deploy_command("auto"), reporter)
        .await
        .expect_err("failed fetch should stop deployment");

    assert_eq!(error.code(), "runtime_repository_failed");
}

#[tokio::test]
async fn activated_route_is_not_cleaned_up_when_ready_reporting_fails() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true));
    let reporter = Arc::new(FailingReadyReporter::default());
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner.clone());

    let error = dispatch(executor, &deploy_command("auto"), Arc::clone(&reporter))
        .await
        .expect_err("ready report should fail");

    assert!(error.to_string().contains("ready event rejected"));
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
async fn reconciliation_detects_stopped_or_incompletely_labeled_containers() {
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(FakeRunner::new(true).with_docker_ps(
        "deadbeef\tExited (1) 2 minutes ago\tff66ed4a-6303-4be6-8ef4-63c28b112680\t4f1f21ef-730d-42d5-a46d-d965353cb993\nmissing\tUp 10 minutes\t\t\n",
    ));
    let executor = DockerRuntimeExecutor::with_runner(runtime_config(&temp), runner);

    let report = sakala_agent_core::ports::RuntimeExecutor::reconcile(&executor)
        .await
        .expect("reconciliation scan should complete");

    assert_eq!(report.inspected_containers, 2);
    assert_eq!(report.orphans.len(), 2);
    assert!(
        report
            .orphans
            .iter()
            .any(|orphan| orphan.reason.contains("not running"))
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

struct FakeRunner {
    dockerfile: bool,
    commands: Mutex<Vec<CommandSpec>>,
    docker_ps_stdout: String,
    build_delay: Option<Duration>,
    inspect_delay: Option<Duration>,
    active_builds: AtomicUsize,
    max_concurrent_builds: AtomicUsize,
}

struct FailingGitRunner;

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
            stdout: String::new(),
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
            build_delay: None,
            inspect_delay: None,
            active_builds: AtomicUsize::new(0),
            max_concurrent_builds: AtomicUsize::new(0),
        }
    }

    fn with_docker_ps(mut self, stdout: impl Into<String>) -> Self {
        self.docker_ps_stdout = stdout.into();
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
            self.docker_ps_stdout.as_str()
        } else if spec.program == "docker" && spec.args.iter().any(|argument| argument == "inspect")
        {
            "running\n"
        } else if spec.program == "docker" && spec.args.iter().any(|argument| argument == "logs") {
            "application listening\n"
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
