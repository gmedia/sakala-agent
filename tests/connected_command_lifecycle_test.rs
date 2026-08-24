use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use sakala_agent_core::{
    AgentConfig, NodeLifecycle,
    api::ApiClient,
    commands::CommandProcessor,
    ports::{
        CommandOutput, DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter,
    },
    scheduler,
};
use sakala_agent_protocol::{AgentCommand, CommandStatus, CommandType, CompleteCommandPayload};
use sakala_agent_runtime::NoopRuntimeExecutor;
use serde_json::json;
use tokio::{sync::watch, time::sleep};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, body_partial_json, header, method, path},
};

const COMMAND_ID: &str = "b3c8cb55-3bc8-4725-a004-e69d9917d40b";

#[tokio::test]
async fn failed_api_polling_respects_interval_without_retry_storm() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agent/v1/commands"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
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
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(scheduler::poller::run(
        config,
        Some(client),
        Arc::new(NoopRuntimeExecutor),
        Arc::new(NodeLifecycle::new()),
        Arc::new(scheduler::metrics::SchedulerMetrics::default()),
        shutdown_rx,
    ));

    sleep(std::time::Duration::from_millis(2_200)).await;
    shutdown_tx
        .send(true)
        .expect("poller should still be running");
    task.await.expect("poller task should stop cleanly");

    let polls = server
        .received_requests()
        .await
        .expect("requests should be recorded")
        .into_iter()
        .filter(|request| request.url.path() == "/api/agent/v1/commands")
        .count();
    assert!((2..=3).contains(&polls), "unexpected poll count: {polls}");
}

#[tokio::test]
async fn claim_conflict_skips_runtime_execution_and_terminal_reporting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/claim")))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;

    let command: AgentCommand = serde_json::from_value(command_fixture())
        .expect("command fixture should match the protocol");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let runtime: Arc<dyn RuntimeExecutor> = Arc::new(FailingRuntimeExecutor);

    CommandProcessor::new(client, runtime, std::time::Duration::from_secs(900))
        .process(&command, CancellationToken::new())
        .await
        .expect("claim conflict must safely skip the command");
}

#[tokio::test]
async fn private_repository_credential_fixture_matches_agent_contract() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/repository-credential"
        )))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "x-access-token",
            "token": "ghs_ephemeral_fixture_token"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let command_id: AgentCommand = serde_json::from_value(command_fixture())
        .expect("command fixture should match the protocol");
    let credential = client
        .repository_credential(command_id.id)
        .await
        .expect("credential fixture should deserialize");

    assert_eq!(credential.username, "x-access-token");
    assert!(!format!("{credential:?}").contains("ghs_ephemeral_fixture_token"));
}

#[tokio::test]
async fn terminal_retry_is_idempotent_only_for_the_same_terminal_state() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/complete"
        )))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "status": "Succeeded",
            "terminal_at": "2026-08-23T10:00:00Z"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/fail")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "status": "Succeeded",
            "terminal_at": "2026-08-23T10:00:00Z"
        })))
        .mount(&server)
        .await;

    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let command: AgentCommand = serde_json::from_value(command_fixture())
        .expect("command fixture should match the protocol");
    client
        .complete(command.id, &CompleteCommandPayload { result: json!({}) })
        .await
        .expect("same terminal result must be idempotent");
    let error = client
        .fail(
            command.id,
            "runtime_execution_failed",
            "must not overwrite success",
        )
        .await
        .expect_err("failure must not overwrite a succeeded command");
    assert!(error.to_string().contains("Succeeded"));
}

#[tokio::test]
async fn connected_agent_polls_and_reports_a_complete_noop_lifecycle() {
    let server = MockServer::start().await;
    mount_lifecycle_mocks(&server).await;

    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let commands = client
        .poll_commands()
        .await
        .expect("Laravel resource envelope should deserialize");

    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command_type, CommandType::DeployProject);
    assert_eq!(commands[0].status, CommandStatus::Pending);

    let runtime: Arc<dyn RuntimeExecutor> = Arc::new(NoopRuntimeExecutor);
    CommandProcessor::new(client, runtime, std::time::Duration::from_secs(900))
        .process(&commands[0], CancellationToken::new())
        .await
        .expect("noop command lifecycle should complete");
}

#[tokio::test]
async fn connected_agent_reports_runtime_failures_with_stable_error_fields() {
    let server = MockServer::start().await;
    mount_claim_mock(&server).await;
    mount_event_mock(&server, "command.claimed", "Agent claimed command.").await;

    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/fail")))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(body_json(json!({
            "error_code": "runtime_execution_failed",
            "error_message": "simulated runtime failure"
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let command: AgentCommand = serde_json::from_value(command_fixture())
        .expect("command fixture should match the protocol");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let runtime: Arc<dyn RuntimeExecutor> = Arc::new(FailingRuntimeExecutor);

    let error = CommandProcessor::new(client, runtime, std::time::Duration::from_secs(900))
        .process(&command, CancellationToken::new())
        .await
        .expect_err("runtime failure should propagate after being reported");

    assert!(error.to_string().contains("simulated runtime failure"));
}

#[tokio::test]
async fn connected_agent_reports_command_timeout_with_stable_failure_status() {
    let server = MockServer::start().await;
    mount_claim_mock(&server).await;
    mount_event_mock(&server, "command.claimed", "Agent claimed command.").await;

    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/fail")))
        .and(body_json(json!({
            "error_code": "runtime_timeout",
            "error_message": "command execution exceeded its 1s timeout"
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let mut fixture = command_fixture();
    fixture["payload"]["timeouts"]["command_timeout_seconds"] = json!(1);
    let command: AgentCommand = serde_json::from_value(fixture).expect("valid command");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");

    let error = CommandProcessor::new(
        client,
        Arc::new(SlowRuntimeExecutor),
        std::time::Duration::from_secs(1),
    )
    .process(&command, CancellationToken::new())
    .await
    .expect_err("slow runtime must time out");

    assert!(error.to_string().contains("exceeded its 1s timeout"));
}

#[tokio::test]
async fn command_deadline_after_cutover_completes_committed_deployment() {
    let server = MockServer::start().await;
    mount_claim_mock(&server).await;
    mount_event_mock(&server, "command.claimed", "Agent claimed command.").await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/complete"
        )))
        .and(body_json(json!({ "result": null })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut fixture = command_fixture();
    fixture["payload"]["timeouts"]["command_timeout_seconds"] = json!(1);
    let command: AgentCommand = serde_json::from_value(fixture).expect("valid command");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");

    CommandProcessor::new(
        client,
        Arc::new(CommittedSlowRuntimeExecutor),
        std::time::Duration::from_secs(1),
    )
    .process(&command, CancellationToken::new())
    .await
    .expect("committed deployment must complete after its deadline");
}

#[tokio::test]
async fn post_commit_finalization_is_bounded_and_uses_cutover_result() {
    let server = MockServer::start().await;
    mount_claim_mock(&server).await;
    mount_event_mock(&server, "command.claimed", "Agent claimed command.").await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/complete"
        )))
        .and(body_json(json!({
            "result": {
                "status": "committed-at-cutover",
                "finalization_deferred": true,
                "finalization_deferred_reason": "grace_elapsed"
            }
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let mut fixture = command_fixture();
    fixture["payload"]["timeouts"]["command_timeout_seconds"] = json!(1);
    let command: AgentCommand = serde_json::from_value(fixture).expect("valid command");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    let started = std::time::Instant::now();

    CommandProcessor::new(
        client,
        Arc::new(CommittedHangingRuntimeExecutor),
        std::time::Duration::from_secs(1),
    )
    .with_post_commit_finalization_grace(std::time::Duration::from_millis(50))
    .process(&command, CancellationToken::new())
    .await
    .expect("committed deployment must complete from its cutover snapshot");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "post-commit finalization must not wait for a hanging runtime"
    );
}

#[tokio::test]
async fn post_commit_finalization_error_uses_cutover_result() {
    let server = MockServer::start().await;
    mount_claim_mock(&server).await;
    mount_event_mock(&server, "command.claimed", "Agent claimed command.").await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/complete"
        )))
        .and(body_json(json!({
            "result": {
                "status": "committed-at-cutover",
                "finalization_deferred": true,
                "finalization_deferred_reason": "runtime_error"
            }
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let command: AgentCommand = serde_json::from_value(command_fixture()).expect("valid command");
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");

    CommandProcessor::new(
        client,
        Arc::new(CommittedFailingRuntimeExecutor),
        std::time::Duration::from_secs(900),
    )
    .process(&command, CancellationToken::new())
    .await
    .expect("post-commit finalization error must preserve committed deployment");
}

async fn mount_lifecycle_mocks(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/agent/v1/commands"))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [command_fixture()]
        })))
        .expect(1)
        .mount(server)
        .await;

    mount_claim_mock(server).await;

    mount_event_mock(server, "command.claimed", "Agent claimed command.").await;
    mount_event_mock(
        server,
        "runtime.noop.completed",
        "Noop runtime completed command without host changes.",
    )
    .await;

    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/logs")))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(body_partial_json(json!({
            "stream": "system",
            "message": "Foundation mode: no Docker, Caddy, or Railpack operation executed."
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!(
            "/api/agent/v1/commands/{COMMAND_ID}/complete"
        )))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(body_json(json!({ "result": null })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_event_mock(server: &MockServer, event_type: &str, message: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/events")))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(body_partial_json(json!({
            "type": event_type,
            "level": "info",
            "message": message
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_claim_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/claim")))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

fn command_fixture() -> serde_json::Value {
    json!({
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
            "builder": "auto",
            "environment": {},
            "resources": {
                "memory_mb": 256,
                "cpu_millis": 500,
                "pids_limit": 128
            },
            "timeouts": {
                "build_timeout_seconds": 600,
                "start_timeout_seconds": 120,
                "command_timeout_seconds": 900
            },
            "log_bounds": {
                "max_line_length": 4096,
                "max_batch_lines": 500,
                "max_total_bytes": 10485760
            }
        }
    })
}

struct FailingRuntimeExecutor;

struct SlowRuntimeExecutor;

struct CommittedSlowRuntimeExecutor;

struct CommittedHangingRuntimeExecutor;

struct CommittedFailingRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for CommittedFailingRuntimeExecutor {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        reporter.mark_deployment_committed(CommandOutput::with_result(json!({
            "status": "committed-at-cutover"
        })));
        Err(RuntimeExecutionError::new(
            "runtime_finalization_failed",
            "simulated post-commit finalization failure",
        ))
    }
}

#[async_trait]
impl RuntimeExecutor for CommittedHangingRuntimeExecutor {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        reporter.mark_deployment_committed(CommandOutput::with_result(json!({
            "status": "committed-at-cutover"
        })));
        sleep(std::time::Duration::from_secs(60)).await;
        unreachable!("bounded finalization must drop the hanging execution")
    }
}

#[async_trait]
impl RuntimeExecutor for CommittedSlowRuntimeExecutor {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        reporter.mark_deployment_committed(CommandOutput::empty());
        sleep(std::time::Duration::from_millis(1_100)).await;
        Ok(CommandOutput::empty())
    }
}

#[async_trait]
impl RuntimeExecutor for SlowRuntimeExecutor {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        sleep(std::time::Duration::from_secs(60)).await;
        Ok(CommandOutput::empty())
    }
}

#[async_trait]
impl RuntimeExecutor for FailingRuntimeExecutor {
    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::new(
            "runtime_execution_failed",
            "simulated runtime failure",
        ))
    }
}
