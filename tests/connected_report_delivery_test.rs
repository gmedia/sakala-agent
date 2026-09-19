use std::time::Duration;

use sakala_agent_core::{
    CoreError, api::ApiClient, ports::RuntimeReporter, reporting::ApiRuntimeReporter,
    support::retry::RetryPolicy,
};
use sakala_agent_protocol::{
    CompleteCommandPayload, DeploymentEvent, DeploymentEventLevel, DeploymentLog, LogBounds,
    LogStream,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{header_exists, method, path},
};

const COMMAND_ID: &str = "b3c8cb55-3bc8-4725-a004-e69d9917d40b";

fn command_id() -> Uuid {
    Uuid::parse_str(COMMAND_ID).expect("fixture command id")
}

fn logs_path() -> String {
    format!("/api/agent/v1/commands/{COMMAND_ID}/logs")
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(server.uri(), "runtime-01", "test-agent-token")
        .expect("test client should be valid")
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(20),
        })
}

fn accepted(count: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "data": {
            "accepted_count": count,
            "duplicate_count": 0,
            "first_sequence": 1,
            "last_sequence": count
        }
    }))
}

/// Acknowledges every item of the received batch, as the control plane does
/// (`accepted_count = count(items)`).
struct AcceptWholeBatch;

impl Respond for AcceptWholeBatch {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let count = request
            .body_json::<serde_json::Value>()
            .ok()
            .and_then(|body| body["logs"].as_array().map(Vec::len))
            .unwrap_or(0) as u64;
        accepted(count)
    }
}

fn log(message: &str) -> DeploymentLog {
    DeploymentLog {
        stream: LogStream::Stdout,
        message: message.to_owned(),
        recorded_at: OffsetDateTime::now_utc(),
    }
}

fn reporter(server: &MockServer, bounds: LogBounds) -> ApiRuntimeReporter {
    ApiRuntimeReporter::with_flush_interval(
        client(server),
        command_id(),
        bounds,
        Duration::from_millis(50),
    )
}

async fn log_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("requests should be recorded")
        .into_iter()
        .filter(|request| request.url.path() == logs_path())
        .collect()
}

fn idempotency_key(request: &Request) -> String {
    request
        .headers
        .get("idempotency-key")
        .expect("report must carry an Idempotency-Key")
        .to_str()
        .expect("key should be ASCII")
        .to_owned()
}

fn log_messages(request: &Request) -> Vec<String> {
    request.body_json::<serde_json::Value>().expect("json body")["logs"]
        .as_array()
        .expect("batch logs array")
        .iter()
        .map(|log| log["message"].as_str().expect("message").to_owned())
        .collect()
}

#[tokio::test]
async fn reporter_batches_log_lines_under_one_idempotency_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .and(header_exists("idempotency-key"))
        .respond_with(accepted(3))
        .mount(&server)
        .await;

    let reporter = reporter(&server, LogBounds::default());
    for line in ["one", "two", "three"] {
        reporter.log(log(line)).await.expect("line should queue");
    }
    reporter
        .flush()
        .await
        .expect("flush should deliver the batch");

    let requests = log_requests(&server).await;
    assert_eq!(requests.len(), 1, "three lines must travel in one batch");
    assert_eq!(log_messages(&requests[0]), ["one", "two", "three"]);
    assert!(Uuid::parse_str(&idempotency_key(&requests[0])).is_ok());
}

#[tokio::test]
async fn reporter_flushes_a_full_batch_and_the_remainder_by_timer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(AcceptWholeBatch)
        .mount(&server)
        .await;

    let reporter = reporter(
        &server,
        LogBounds {
            max_batch_lines: Some(2),
            ..LogBounds::default()
        },
    );
    for line in ["a", "b", "c"] {
        reporter.log(log(line)).await.expect("line should queue");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let requests = log_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(log_messages(&requests[0]), ["a", "b"]);
    assert_eq!(log_messages(&requests[1]), ["c"]);
    assert_ne!(
        idempotency_key(&requests[0]),
        idempotency_key(&requests[1]),
        "each batch is its own idempotent request"
    );
}

#[tokio::test]
async fn transient_failure_is_retried_with_the_same_idempotency_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        // The API counts every item of the request in `accepted_count` and
        // reports the already-persisted subset in `duplicate_count`.
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "accepted_count": 1,
                "duplicate_count": 1,
                "first_sequence": 1,
                "last_sequence": 1
            }
        })))
        .mount(&server)
        .await;

    let acknowledgement = client(&server)
        .logs(command_id(), &[log("retry me")])
        .await
        .expect("retry must succeed after a transient failure");
    assert_eq!(acknowledgement.accepted_count, 1);
    assert_eq!(acknowledgement.duplicate_count, 1);

    let requests = log_requests(&server).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(idempotency_key(&requests[0]), idempotency_key(&requests[1]));
}

#[tokio::test]
async fn malformed_acknowledgement_is_not_treated_as_delivered() {
    for body in [
        json!({ "ok": true }),
        json!({ "data": { "accepted_count": 1 } }),
        json!({ "data": { "accepted_count": 1, "duplicate_count": 2, "first_sequence": 1, "last_sequence": 1 } }),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(logs_path()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .logs(command_id(), &[log("unacknowledged")])
            .await
            .expect_err("a 200 without a valid acknowledgement is not delivery");
        assert!(
            matches!(error, CoreError::InvalidReportAcknowledgement(_)),
            "{body}: {error}"
        );
        assert!(error.stops_report_delivery());
    }

    // A 200 whose body is not JSON at all is equally undelivered.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>proxy error</html>"))
        .expect(1)
        .mount(&server)
        .await;
    let error = client(&server)
        .logs(command_id(), &[log("unacknowledged")])
        .await
        .expect_err("non-JSON 200 is not delivery");
    assert!(matches!(error, CoreError::InvalidReportAcknowledgement(_)));
}

#[tokio::test]
async fn partial_acknowledgement_is_not_treated_as_delivered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "accepted_count": 1,
                "duplicate_count": 0,
                "first_sequence": 10,
                "last_sequence": 10
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let error = client(&server)
        .logs(command_id(), &[log("one"), log("two"), log("three")])
        .await
        .expect_err("an acknowledgement for fewer items than sent is not delivery");
    assert!(
        matches!(error, CoreError::InvalidReportAcknowledgement(_)),
        "{error}"
    );
    assert!(error.to_string().contains("accepted 1 of 3"), "{error}");
    assert!(error.stops_report_delivery());
}

#[tokio::test]
async fn reporter_does_not_drop_a_partially_acknowledged_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "accepted_count": 1,
                "duplicate_count": 0,
                "first_sequence": 1,
                "last_sequence": 1
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let reporter = reporter(
        &server,
        LogBounds {
            max_batch_lines: Some(2),
            ..LogBounds::default()
        },
    );
    reporter.log(log("one")).await.expect("first line queues");
    let error = reporter
        .log(log("two"))
        .await
        .expect_err("the full batch flush must fail on a partial acknowledgement");
    assert!(error.to_string().contains("accepted 1 of 2"), "{error}");
    reporter
        .log(log("three"))
        .await
        .expect_err("delivery stays stopped after the partial acknowledgement");
}

#[tokio::test]
async fn legacy_no_content_acknowledgement_is_still_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .logs(command_id(), &[log("legacy")])
        .await
        .expect("204 from an older control plane remains a delivery");
}

#[tokio::test]
async fn reporter_does_not_drop_a_batch_on_malformed_acknowledgement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .expect(1)
        .mount(&server)
        .await;

    let reporter = reporter(
        &server,
        LogBounds {
            max_batch_lines: Some(1),
            ..LogBounds::default()
        },
    );
    let error = reporter
        .log(log("first"))
        .await
        .expect_err("undelivered batch must surface as a delivery failure");
    assert!(error.to_string().contains("acknowledgement"), "{error}");
    reporter
        .log(log("second"))
        .await
        .expect_err("delivery stays stopped so the follower terminates");
}

#[tokio::test]
async fn exhausted_log_budget_stops_delivery_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "The cumulative log budget for this command has been exceeded.",
            "errors": { "logs": ["The cumulative log budget for this command has been exceeded."] }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let reporter = reporter(
        &server,
        LogBounds {
            max_batch_lines: Some(1),
            ..LogBounds::default()
        },
    );
    let error = reporter
        .log(log("over budget"))
        .await
        .expect_err("422 must surface as a delivery stop");
    assert!(error.to_string().contains("422"), "{error}");
    reporter
        .log(log("never sent"))
        .await
        .expect_err("delivery stays stopped after a rejection");
    reporter
        .flush()
        .await
        .expect_err("flush also reports the stopped delivery");
}

#[tokio::test]
async fn terminal_command_rejects_new_events_and_logs_with_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/events")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "status": "Expired",
            "terminal_at": null
        })))
        .expect(1)
        .mount(&server)
        .await;

    let error = client(&server)
        .event(
            command_id(),
            &DeploymentEvent {
                event_type: "deployment.build.started".to_owned(),
                level: DeploymentEventLevel::Info,
                message: "late".to_owned(),
                metadata: json!({}),
                occurred_at: OffsetDateTime::now_utc(),
            },
        )
        .await
        .expect_err("409 must not be retried");
    assert!(error.stops_report_delivery());
    assert!(error.to_string().contains("Expired"), "{error}");
}

#[tokio::test]
async fn expired_lease_is_a_terminal_conflict_for_complete_and_fail() {
    let server = MockServer::start().await;
    for action in ["complete", "fail"] {
        Mock::given(method("POST"))
            .and(path(format!(
                "/api/agent/v1/commands/{COMMAND_ID}/{action}"
            )))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "status": "Expired",
                "terminal_at": null
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = client(&server);

    let complete = client
        .complete(command_id(), &CompleteCommandPayload::default())
        .await
        .expect_err("expired command cannot be completed");
    assert!(matches!(complete, CoreError::CommandTerminalConflict(_)));
    let fail = client
        .fail(command_id(), "runtime_timeout", "too late")
        .await
        .expect_err("expired command cannot be failed");
    assert!(fail.to_string().contains("Expired"));
}

#[tokio::test]
async fn terminal_conflict_carries_terminal_at_when_present() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/fail")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "status": "Succeeded",
            "terminal_at": "2026-09-14T10:00:00+00:00"
        })))
        .mount(&server)
        .await;

    let error = client(&server)
        .fail(command_id(), "runtime_timeout", "late")
        .await
        .expect_err("failure cannot overwrite success");
    assert!(
        error.to_string().contains("2026-09-14T10:00:00+00:00"),
        "{error}"
    );
}

#[tokio::test]
async fn fail_body_is_sanitized_to_control_plane_limits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/api/agent/v1/commands/{COMMAND_ID}/fail")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .fail(
            command_id(),
            "runtime build/failed",
            &format!("boom\u{202E}\n{}", "x".repeat(2_000)),
        )
        .await
        .expect("sanitized failure should be accepted");

    let request = server
        .received_requests()
        .await
        .expect("requests should be recorded")
        .into_iter()
        .find(|request| request.url.path().ends_with("/fail"))
        .expect("fail request");
    let body: serde_json::Value = request.body_json().expect("json body");
    assert_eq!(body["error_code"], "runtime_build_failed");
    let message = body["error_message"].as_str().expect("message");
    assert!(message.starts_with("boomxxx"));
    assert_eq!(message.chars().count(), 1_000);
    assert!(!message.contains('\u{202E}'));
}
