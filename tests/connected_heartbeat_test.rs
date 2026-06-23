use sakala_agent_core::api::ApiClient;
use sakala_agent_protocol::HeartbeatPayload;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

#[tokio::test]
async fn connected_heartbeat_uses_the_agent_contract() {
    let server = MockServer::start().await;
    let payload: HeartbeatPayload = serde_json::from_value(json!({
        "status": "ready",
        "hostname": "runtime-01",
        "runtime_network": "sakala-runtime",
        "capabilities": ["noop-runtime"],
        "metadata": { "version": "0.1.0" },
        "sent_at": "2026-06-23T08:00:00Z"
    }))
    .expect("heartbeat fixture should match the protocol");

    Mock::given(method("POST"))
        .and(path("/api/agent/v1/heartbeat"))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .and(header("accept", "application/json"))
        .and(body_json(json!({
            "status": "ready",
            "hostname": "runtime-01",
            "runtime_network": "sakala-runtime",
            "capabilities": ["noop-runtime"],
            "metadata": { "version": "0.1.0" },
            "sent_at": "2026-06-23T08:00:00Z"
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");

    client
        .heartbeat(&payload)
        .await
        .expect("heartbeat request should succeed");
}
