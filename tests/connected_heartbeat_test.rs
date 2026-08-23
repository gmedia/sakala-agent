use sakala_agent_core::api::ApiClient;
use sakala_agent_protocol::{DesiredNodeLifecycleState, HeartbeatPayload};
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
        "metadata": {
            "version": "0.1.0",
            "protocol_version": 4,
            "runtime_driver": "docker",
            "lifecycle_state": "active",
            "uptime_seconds": 86400,
            "resources": {
                "cpu_total": 4,
                "cpu_load_1m": 0.42,
                "memory_total_bytes": 8589934592u64,
                "memory_available_bytes": 4294967296u64,
                "disk_total_bytes": 107374182400u64,
                "disk_available_bytes": 53687091200u64
            }
        },
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
            "metadata": {
                "version": "0.1.0",
                "protocol_version": 4,
                "runtime_driver": "docker",
                "lifecycle_state": "active",
                "uptime_seconds": 86400,
                "resources": {
                    "cpu_total": 4,
                    "cpu_load_1m": 0.42,
                    "memory_total_bytes": 8589934592u64,
                    "memory_available_bytes": 4294967296u64,
                    "disk_total_bytes": 107374182400u64,
                    "disk_available_bytes": 53687091200u64
                }
            },
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

#[tokio::test]
async fn connected_bootstrap_reads_authoritative_node_lifecycle() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agent/v1/node-state"))
        .and(header("authorization", "Bearer test-agent-token"))
        .and(header("x-agent-id", "runtime-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "desired_state": "drained" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid");

    let state = client
        .node_lifecycle()
        .await
        .expect("desired lifecycle should be readable");

    assert_eq!(state.desired_state, DesiredNodeLifecycleState::Drained);
}
