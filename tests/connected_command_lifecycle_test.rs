use std::sync::Arc;

use async_trait::async_trait;
use sakala_agent_core::{api::ApiClient, commands::CommandHandler};
use sakala_agent_protocol::{AgentCommand, CommandStatus, CommandType};
use sakala_agent_runtime::{ExecutionOutcome, NoopRuntimeExecutor, RuntimeError, RuntimeExecutor};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, body_partial_json, header, method, path},
};

const COMMAND_ID: &str = "b3c8cb55-3bc8-4725-a004-e69d9917d40b";

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
    CommandHandler::new(client, runtime)
        .handle(&commands[0])
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
            "error_message": "runtime executor failed: simulated runtime failure"
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

    let error = CommandHandler::new(client, runtime)
        .handle(&command)
        .await
        .expect_err("runtime failure should propagate after being reported");

    assert!(error.to_string().contains("simulated runtime failure"));
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
        .and(body_json(json!({})))
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
            "repository_url": "https://example.invalid/student/demo-app.git",
            "runtime_network": "sakala-runtime"
        }
    })
}

struct FailingRuntimeExecutor;

#[async_trait]
impl RuntimeExecutor for FailingRuntimeExecutor {
    async fn execute(&self, _command: &AgentCommand) -> Result<ExecutionOutcome, RuntimeError> {
        Err(RuntimeError::Execution(
            "simulated runtime failure".to_owned(),
        ))
    }
}
