//! A failed deployment seen from the control plane: the real executor and the
//! real reporter, with only host processes and the API replaced.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::{
    AgentConfig, NodeLifecycle,
    api::ApiClient,
    commands::CommandProcessor,
    logs::redactor::ValueRedactor,
    ports::{
        CommandOutput, DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter,
    },
    reporting::ApiRuntimeReporter,
    scheduler,
};
use sakala_agent_protocol::{AgentCommand, DeploymentEvent, DeploymentEventLevel, LogBounds};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, ProcessStream, RuntimeError,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const COMMAND_ID: &str = "b3c8cb55-3bc8-4725-a004-e69d9917d40b";
const API_KEY: &str = "sk-very-secret-value";
const GITHUB_TOKEN: &str = "ghs_ephemeraltokenvalue";
const DATABASE_PASSWORD: &str = "hunter2-database";
const PLAIN_VALUE: &str = "kL9-unusual-value-under-a-plain-name";
const COMMAND_ID_NOT_RUNNING: &str = "0198a2f0-6a51-7c3e-9d6e-1b2c3d4e5f60";

#[tokio::test]
async fn crash_during_start_delivers_redacted_logs_before_the_failure_report() {
    let (server, requests) = api_recording_requests().await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner::default());
    let processor = processor(&server, &temp, runner.clone());

    processor
        .process(&deploy_command(), CancellationToken::new())
        .await
        .expect_err("the crash must fail the command");

    let requests = requests.lock().expect("request lock").clone();
    let index_of = |suffix: &str| {
        requests
            .iter()
            .position(|(path, _)| path.ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} should be requested: {requests:?}"))
    };
    assert!(
        index_of("/logs") < index_of("/fail"),
        "the app output must arrive before the terminal report"
    );

    let (_, failure) = &requests[index_of("/fail")];
    assert_eq!(failure["error_code"], "runtime_app_crashed");
    assert!(
        failure["error_message"]
            .as_str()
            .expect("message")
            .contains("exit code 3")
    );

    let delivered = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/logs"))
        .flat_map(|(_, body)| body["logs"].as_array().expect("log batch").clone())
        .collect::<Vec<_>>();
    let messages = delivered
        .iter()
        .map(|log| log["message"].as_str().expect("message").to_owned())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message == "[runtime] listening failed"),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("API_KEY=[REDACTED]")),
        "{messages:?}"
    );
    assert!(
        delivered
            .iter()
            .any(|log| log["stream"] == "stderr" && log["message"].as_str().is_some()),
        "stderr output keeps its stream"
    );

    let everything_sent = serde_json::to_string(&requests).expect("requests serialize");
    for secret in [API_KEY, GITHUB_TOKEN, DATABASE_PASSWORD] {
        assert!(
            !everything_sent.contains(secret),
            "{secret} must not reach the control plane"
        );
    }
    assert!(
        runner.logs_read_before_removal(),
        "the output must be read while the container still exists"
    );
}

#[tokio::test]
async fn port_conflict_fails_without_echoing_a_value_that_is_not_a_port() {
    let (server, requests) = api_recording_requests().await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner::default());
    let processor = processor(&server, &temp, runner.clone());
    let mut command = deploy_command();
    command.payload["environment"] = json!({ "PORT": DATABASE_PASSWORD });

    processor
        .process(&command, CancellationToken::new())
        .await
        .expect_err("the conflict must fail the command");

    let requests = requests.lock().expect("request lock").clone();
    let (_, failure) = requests
        .iter()
        .find(|(path, _)| path.ends_with("/fail"))
        .expect("failure reported");
    assert_eq!(failure["error_code"], "runtime_port_conflict");
    assert!(
        failure["error_message"]
            .as_str()
            .expect("message")
            .contains("container_port 3000")
    );
    assert!(
        !serde_json::to_string(&requests)
            .expect("requests serialize")
            .contains(DATABASE_PASSWORD)
    );
}

#[tokio::test]
async fn a_command_environment_value_never_reaches_the_control_plane() {
    let (server, requests) = api_recording_requests().await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner {
        app_lines: vec![
            format!("connecting to {PLAIN_VALUE}"),
            "still alive at 3000 in production".to_owned(),
        ],
        ..CrashingAppRunner::default()
    });
    let processor = processor(&server, &temp, runner);
    let mut command = deploy_command();
    command.payload["environment"] = json!({
        "APP_ENV": "production",
        "NOT_A_SECRET_LOOKING_NAME": PLAIN_VALUE,
        "SHORT": "3000",
    });

    processor
        .process(&command, CancellationToken::new())
        .await
        .expect_err("the crash must fail the command");

    let requests = requests.lock().expect("request lock").clone();
    let everything_sent = serde_json::to_string(&requests).expect("requests serialize");
    assert!(!everything_sent.contains(PLAIN_VALUE), "{everything_sent}");
    assert!(everything_sent.contains("connecting to [REDACTED]"));
    assert!(
        everything_sent.contains("still alive at 3000 in production"),
        "short and ordinary values must not mangle logs: {everything_sent}"
    );
}

#[tokio::test]
async fn event_message_and_metadata_hide_a_command_value() {
    let (server, requests) = api_recording_requests().await;
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let reporter = ApiRuntimeReporter::new(
        client,
        COMMAND_ID.parse().expect("command UUID"),
        LogBounds::default(),
    )
    .with_redactor(ValueRedactor::new([PLAIN_VALUE.to_owned()]));

    reporter
        .event(DeploymentEvent {
            event_type: "deployment.health.started".to_owned(),
            level: DeploymentEventLevel::Info,
            message: format!("waiting for {PLAIN_VALUE}"),
            metadata: json!({ "nested": { "value": PLAIN_VALUE }, "port": 3000 }),
            occurred_at: OffsetDateTime::now_utc(),
        })
        .await
        .expect("event delivered");

    let requests = requests.lock().expect("request lock").clone();
    let event = &requests[0].1["events"][0];
    assert_eq!(event["message"], "waiting for [REDACTED]");
    assert_eq!(event["metadata"]["nested"]["value"], "[REDACTED]");
    assert_eq!(event["metadata"]["port"], 3000);
}

#[tokio::test]
async fn failure_message_hides_a_command_value() {
    let (server, requests) = api_recording_requests().await;
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let executor: Arc<dyn RuntimeExecutor> = Arc::new(LeakyFailureRuntime);
    let processor = CommandProcessor::new(client, executor, Duration::from_secs(900));
    let mut command = deploy_command();
    command.payload["environment"] = json!({ "SOME_NAME": PLAIN_VALUE });

    processor
        .process(&command, CancellationToken::new())
        .await
        .expect_err("the runtime fails");

    let requests = requests.lock().expect("request lock").clone();
    let (_, failure) = requests
        .iter()
        .find(|(path, _)| path.ends_with("/fail"))
        .expect("failure reported");
    assert_eq!(failure["error_message"], "could not use [REDACTED]");
}

/// Runs the real poller against an API that asks to cancel the running deployment.
#[tokio::test]
async fn cancel_requested_while_building_kills_the_build_and_fails_as_cancelled() {
    let server = MockServer::start().await;
    let (requests, polls) = mount_cancelling_api(&server, deploy_command()).await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner {
        build: BuildBehaviour::Hangs(vec!["step 000".to_owned()]),
        ..CrashingAppRunner::default()
    });

    run_poller_until_terminal(&server, &temp, runner.clone(), &requests).await;

    let requests = requests.lock().expect("request lock").clone();
    let (_, failure) = requests
        .iter()
        .find(|(path, _)| path.ends_with("/fail"))
        .expect("failure reported");
    assert_eq!(failure["error_code"], "runtime_cancelled");
    assert!(
        !requests.iter().any(|(path, _)| path.ends_with("/complete")),
        "a cancelled build must not complete"
    );
    assert!(
        polls.load(Ordering::SeqCst) >= 2,
        "the poller must keep polling while its slot is busy"
    );
    assert!(
        runner.build_process_dropped.load(Ordering::SeqCst),
        "dropping the process future is what kills its process group"
    );
    let calls = runner.calls.lock().expect("call lock").clone();
    assert!(calls.iter().any(|call| call == "rm"), "{calls:?}");
    assert!(calls.iter().any(|call| call == "image"), "{calls:?}");
    assert!(!calls.iter().any(|call| call == "run"), "{calls:?}");
    assert!(
        !calls.iter().any(|call| call == "logs"),
        "there is no app output to read: {calls:?}"
    );
    assert!(
        !temp.path().join("builds").join(COMMAND_ID).exists(),
        "the workspace must be removed"
    );
}

/// The cancel arrives after the route went live, so the command still completes.
#[tokio::test]
async fn cancel_requested_after_cutover_completes_the_deployment() {
    let server = MockServer::start().await;
    let (requests, _polls) = mount_cancelling_api(&server, deploy_command()).await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner {
        healthy_app: true,
        startup_log_delay: Some(Duration::from_secs(3)),
        ..CrashingAppRunner::default()
    });

    run_poller_until_terminal(&server, &temp, runner.clone(), &requests).await;

    let requests = requests.lock().expect("request lock").clone();
    assert!(
        requests.iter().any(|(path, _)| path.ends_with("/complete")),
        "{requests:?}"
    );
    assert!(!requests.iter().any(|(path, _)| path.ends_with("/fail")));
    let (_, completion) = requests
        .iter()
        .find(|(path, _)| path.ends_with("/complete"))
        .expect("completion reported");
    assert!(
        completion["result"].get("finalization_deferred").is_none(),
        "the deployment must really finish, not ask for a repair: {completion}"
    );
    let calls = runner.calls.lock().expect("call lock").clone();
    assert!(
        !calls.iter().any(|call| call == "rm"),
        "the live candidate must not be removed: {calls:?}"
    );
}

/// Offers the command once, then lists it in `cancel_requested` on every later
/// poll, the way the API repeats it until the command is terminal.
async fn mount_cancelling_api(
    server: &MockServer,
    command: AgentCommand,
) -> (RecordedRequests, Arc<AtomicUsize>) {
    let polls = Arc::new(AtomicUsize::new(0));
    let requests: RecordedRequests = Arc::default();
    let (recorded, poll_count) = (Arc::clone(&requests), Arc::clone(&polls));
    Mock::given(method("GET"))
        .and(path("/api/agent/v1/commands"))
        .respond_with(move |_: &Request| {
            let body = if poll_count.fetch_add(1, Ordering::SeqCst) == 0 {
                json!({ "data": [command] })
            } else {
                json!({ "data": [], "cancel_requested": [COMMAND_ID, COMMAND_ID_NOT_RUNNING] })
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let item_count = ["events", "logs"]
                .iter()
                .find_map(|key| body[*key].as_array().map(Vec::len));
            recorded
                .lock()
                .expect("request lock")
                .push((request.url.path().to_owned(), body));
            match item_count {
                Some(count) => ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "accepted_count": count,
                        "duplicate_count": 0,
                        "first_sequence": 1,
                        "last_sequence": count,
                    }
                })),
                None => ResponseTemplate::new(204),
            }
        })
        .mount(server)
        .await;
    (requests, polls)
}

async fn run_poller_until_terminal(
    server: &MockServer,
    temp: &TempDir,
    runner: Arc<CrashingAppRunner>,
    requests: &RecordedRequests,
) {
    let mut values = HashMap::new();
    values.insert("SAKALA_AGENT_MODE".to_owned(), "connected".to_owned());
    values.insert(
        "SAKALA_AGENT_TOKEN".to_owned(),
        "test-agent-token".to_owned(),
    );
    values.insert("SAKALA_API_URL".to_owned(), server.uri());
    values.insert("SAKALA_POLL_INTERVAL_SECONDS".to_owned(), "1".to_owned());
    values.insert("SAKALA_SHUTDOWN_GRACE_SECONDS".to_owned(), "1".to_owned());
    let config = AgentConfig::from_values(&values).expect("connected config should be valid");
    let client = ApiClient::from_config(&config).expect("test client should be valid");
    let runtime_config = DockerRuntimeConfig {
        railpack_image: "sakala-railpack@sha256:0123456789abcdef".to_owned(),
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        health_attempts: 1,
        health_interval: Duration::ZERO,
        ..DockerRuntimeConfig::default()
    };
    let executor: Arc<dyn RuntimeExecutor> =
        Arc::new(DockerRuntimeExecutor::with_runner(runtime_config, runner));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(scheduler::poller::run(
        config,
        Some(client),
        Arc::clone(&executor),
        Arc::new(NodeLifecycle::new()),
        Arc::new(scheduler::metrics::SchedulerMetrics::default()),
        shutdown_rx,
    ));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let terminal = requests
            .lock()
            .expect("request lock")
            .iter()
            .any(|(path, _)| path.ends_with("/fail") || path.ends_with("/complete"));
        if terminal {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the command never reached a terminal report"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    shutdown_tx.send(true).expect("poller should be running");
    task.await.expect("poller stops cleanly");
    executor.shutdown().await.expect("runtime shuts down");
}

struct LeakyFailureRuntime;

#[async_trait]
impl RuntimeExecutor for LeakyFailureRuntime {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::new(
            "runtime_execution_failed",
            format!("could not use {PLAIN_VALUE}"),
        ))
    }
}

#[tokio::test]
async fn a_failed_build_delivers_all_its_output_in_order_before_the_failure() {
    let (server, requests) = api_recording_requests().await;
    let temp = TempDir::new().expect("temp directory should be available");
    let lines = (0..250).map(|index| format!("step {index:03}")).collect();
    let runner = Arc::new(CrashingAppRunner {
        build: BuildBehaviour::Fails(lines),
        ..CrashingAppRunner::default()
    });
    let processor = processor(&server, &temp, runner);

    processor
        .process(&deploy_command(), CancellationToken::new())
        .await
        .expect_err("the build fails");

    let requests = requests.lock().expect("request lock").clone();
    let (logs, failure) = build_report(&requests);
    let expected = (0..250)
        .map(|index| format!("[docker-build] step {index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(logs, expected, "every line, in order");
    assert_eq!(failure["error_code"], "runtime_build_failed");
    assert!(
        failure["error_message"]
            .as_str()
            .expect("message")
            .contains("docker-build"),
        "the failure names the stage: {failure}"
    );
}

#[tokio::test]
async fn a_build_timeout_keeps_earlier_output_and_names_the_stage_and_limit() {
    let (server, requests) = api_recording_requests().await;
    let temp = TempDir::new().expect("temp directory should be available");
    let runner = Arc::new(CrashingAppRunner {
        build: BuildBehaviour::Hangs(vec!["step 000".to_owned(), "step 001".to_owned()]),
        ..CrashingAppRunner::default()
    });
    let processor = processor(&server, &temp, runner);
    let mut command = deploy_command();
    command.payload["timeouts"]["build_timeout_seconds"] = json!(1);

    processor
        .process(&command, CancellationToken::new())
        .await
        .expect_err("the build times out");

    let requests = requests.lock().expect("request lock").clone();
    let (logs, failure) = build_report(&requests);
    assert_eq!(logs, ["[docker-build] step 000", "[docker-build] step 001"]);
    assert_eq!(failure["error_code"], "runtime_timeout");
    let message = failure["error_message"].as_str().expect("message");
    assert!(message.contains("deployment-build"), "{message}");
    assert!(message.contains("1s"), "{message}");
}

fn build_report(requests: &[(String, Value)]) -> (Vec<String>, Value) {
    let logs = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/logs"))
        .flat_map(|(_, body)| body["logs"].as_array().cloned().unwrap_or_default())
        .filter_map(|log| log["message"].as_str().map(str::to_owned))
        .filter(|message| message.starts_with("[docker-build]"))
        .collect();
    let last_log = requests
        .iter()
        .rposition(|(path, _)| path.ends_with("/logs"));
    let fail = requests
        .iter()
        .position(|(path, _)| path.ends_with("/fail"))
        .expect("failure reported");
    assert!(
        last_log < Some(fail),
        "logs come before the terminal report"
    );
    (logs, requests[fail].1.clone())
}

type RecordedRequests = Arc<Mutex<Vec<(String, Value)>>>;

async fn api_recording_requests() -> (MockServer, RecordedRequests) {
    let server = MockServer::start().await;
    let requests: RecordedRequests = Arc::default();
    let recorded = Arc::clone(&requests);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let item_count = ["events", "logs"]
                .iter()
                .find_map(|key| body[*key].as_array().map(Vec::len));
            recorded
                .lock()
                .expect("request lock")
                .push((request.url.path().to_owned(), body));
            match item_count {
                Some(count) => ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "accepted_count": count,
                        "duplicate_count": 0,
                        "first_sequence": 1,
                        "last_sequence": count,
                    }
                })),
                None => ResponseTemplate::new(204),
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/agent/v1/unused"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    (server, requests)
}

fn processor(
    server: &MockServer,
    temp: &TempDir,
    runner: Arc<CrashingAppRunner>,
) -> CommandProcessor {
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let config = DockerRuntimeConfig {
        railpack_image: "sakala-railpack@sha256:0123456789abcdef".to_owned(),
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        health_attempts: 1,
        health_interval: Duration::ZERO,
        ..DockerRuntimeConfig::default()
    };
    let executor: Arc<dyn RuntimeExecutor> =
        Arc::new(DockerRuntimeExecutor::with_runner(config, runner));
    CommandProcessor::new(client, executor, Duration::from_secs(900))
}

fn deploy_command() -> AgentCommand {
    serde_json::from_value(json!({
        "id": COMMAND_ID,
        "type": "DeployProject",
        "status": "Pending",
        "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
        "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
        "payload": {
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.run.sakala.localhost",
            "container_port": 3000,
            "environment": {},
            "timeouts": {
                "build_timeout_seconds": 5,
                "start_timeout_seconds": 5,
                "command_timeout_seconds": 10
            },
            "log_bounds": {
                "max_line_length": 4096,
                "max_batch_lines": 500,
                "max_total_bytes": 10485760
            }
        }
    }))
    .expect("command fixture should match the protocol")
}

#[derive(Default)]
enum BuildBehaviour {
    #[default]
    Succeeds,
    Fails(Vec<String>),
    Hangs(Vec<String>),
}

#[derive(Default)]
struct CrashingAppRunner {
    /// `docker` subcommands in the order they ran.
    calls: Mutex<Vec<String>>,
    /// Replaces the default app output when set.
    app_lines: Vec<String>,
    build: BuildBehaviour,
    /// Makes the app answer its probe instead of crashing.
    healthy_app: bool,
    /// Holds the step just after the route went live.
    startup_log_delay: Option<Duration>,
    build_process_dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl CrashingAppRunner {
    fn logs_read_before_removal(&self) -> bool {
        let calls = self.calls.lock().expect("call lock");
        let position = |name: &str| calls.iter().position(|call| call == name);
        matches!(
            (position("logs"), position("rm")),
            (Some(logs), Some(rm)) if logs < rm
        )
    }
}

#[async_trait]
impl ProcessRunner for CrashingAppRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let first = spec
            .args
            .first()
            .map(|argument| argument.to_string_lossy().into_owned())
            .unwrap_or_default();
        if spec.program == "docker" {
            self.calls.lock().expect("call lock").push(first.clone());
        }
        let mut stdout = String::new();
        if spec.program == "docker" && first == "buildx" {
            let lines = match &self.build {
                BuildBehaviour::Succeeds => &Vec::new(),
                BuildBehaviour::Fails(lines) | BuildBehaviour::Hangs(lines) => lines,
            };
            for line in lines {
                // BuildKit's plain progress goes to stderr.
                sink.line(ProcessStream::Stderr, line).await?;
            }
            match &self.build {
                BuildBehaviour::Succeeds => {}
                BuildBehaviour::Fails(_) => {
                    return Ok(ProcessOutput {
                        success: false,
                        code: Some(1),
                        stdout: String::new(),
                        stderr: lines.join("\n"),
                    });
                }
                BuildBehaviour::Hangs(_) => {
                    let _dropped = SetOnDrop(Arc::clone(&self.build_process_dropped));
                    std::future::pending::<()>().await;
                }
            }
        }
        if spec.program == "git" && first == "init" {
            let source = spec.args.last().expect("git init target");
            std::fs::create_dir_all(source).expect("source directory");
            std::fs::write(Path::new(source).join("Dockerfile"), "FROM scratch\n")
                .expect("Dockerfile");
        } else if spec.program == "df" {
            stdout = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000000 1000 9999000 1% /\n".to_owned();
        } else if spec.program == "docker" && first == "inspect" && self.healthy_app {
            stdout = "running\t0\t0\tfalse\tnone\t268435456\n".to_owned();
        } else if spec.program == "docker" && first == "inspect" {
            stdout = "restarting\t2\t3\tfalse\tnone\t268435456\n".to_owned();
        } else if spec.program == "docker" && first == "exec" && self.healthy_app {
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                stdout: String::new(),
                stderr: "  HTTP/1.1 200 OK\n".to_owned(),
            });
        } else if spec.program == "docker"
            && first == "logs"
            && spec.args.iter().any(|argument| argument == "--follow")
        {
        } else if let Some(delay) = self
            .startup_log_delay
            .filter(|_| spec.program == "docker" && first == "logs")
        {
            tokio::time::sleep(delay).await;
        } else if spec.program == "docker" && first == "logs" && !self.app_lines.is_empty() {
            for line in &self.app_lines {
                sink.line(ProcessStream::Stdout, line).await?;
            }
        } else if spec.program == "docker" && first == "logs" {
            sink.line(ProcessStream::Stdout, "listening failed").await?;
            sink.line(ProcessStream::Stdout, &format!("API_KEY={API_KEY}"))
                .await?;
            sink.line(
                ProcessStream::Stderr,
                &format!("fatal: DATABASE_URL=postgres://app:{DATABASE_PASSWORD}@db/app"),
            )
            .await?;
            sink.line(ProcessStream::Stderr, &format!("clone used {GITHUB_TOKEN}"))
                .await?;
        }
        Ok(ProcessOutput {
            success: true,
            code: Some(0),
            stdout,
            stderr: String::new(),
        })
    }
}
