use std::{
    collections::HashMap,
    io::Write,
    net::{SocketAddr, TcpListener},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use sakala_agent_core::{
    CoreError,
    api::ApiClient,
    ports::RuntimeReporter,
    reporting::{ApiRuntimeReporter, EventDeliveryPolicy, LogDeliveryPolicy},
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
    reporter.log(log("two")).await.expect("second line queues");
    let error = reporter
        .flush()
        .await
        .expect_err("the full batch must fail on a partial acknowledgement");
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
    reporter.log(log("first")).await.expect("line queues");
    let error = reporter
        .flush()
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
    reporter.log(log("over budget")).await.expect("line queues");
    let error = reporter
        .flush()
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

/// Fast retries so an outage of a few hundred milliseconds behaves like the
/// 80-second one seen on staging on 2026-11-19.
fn fast_policy() -> LogDeliveryPolicy {
    LogDeliveryPolicy {
        flush_interval: Duration::from_millis(20),
        retry_min_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(40),
        flush_grace: Duration::from_secs(10),
        ..LogDeliveryPolicy::default()
    }
}

fn reporter_with(
    base_url: &str,
    bounds: LogBounds,
    policy: LogDeliveryPolicy,
) -> ApiRuntimeReporter {
    let client = ApiClient::new(base_url, "runtime-01", "test-agent-token")
        .expect("test client should be valid");
    ApiRuntimeReporter::with_policy(client, command_id(), bounds, policy)
}

fn batch_of(lines: u64) -> LogBounds {
    LogBounds {
        max_batch_lines: Some(lines),
        ..LogBounds::default()
    }
}

/// Answers `503` while `down` is set, then acknowledges whole batches.
struct Outage {
    down: Arc<AtomicBool>,
}

impl Respond for Outage {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if self.down.load(Ordering::SeqCst) {
            ResponseTemplate::new(503)
        } else {
            AcceptWholeBatch.respond(request)
        }
    }
}

/// Batches the control plane acknowledged, in arrival order, keyed by their
/// `Idempotency-Key` so a resent batch can be checked against the original.
async fn acknowledged_batches(server: &MockServer, failed: usize) -> Vec<(String, Vec<String>)> {
    log_requests(server)
        .await
        .iter()
        .skip(failed)
        .map(|request| (idempotency_key(request), log_messages(request)))
        .collect()
}

/// Waits until the fake server has answered `count` log requests. Bounded so a
/// broken reporter fails the test instead of hanging it.
async fn wait_for_log_requests(server: &MockServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while log_requests(server).await.len() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the reporter sends its first batch");
}

fn numbered(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("line {index:03}")).collect()
}

#[tokio::test]
async fn lines_logged_during_an_unreachable_api_arrive_in_order_once_it_returns() {
    // Staging, 2026-11-19 17.53 WIB: `error sending request` for 80 seconds,
    // three attempts per batch, and the deployment lost every later line.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve a port");
    let address: SocketAddr = reserved.local_addr().expect("reserved address");
    drop(reserved);
    let reporter = reporter_with(&format!("http://{address}"), batch_of(10), fast_policy());

    let lines = numbered(35);
    for line in &lines {
        reporter
            .log(log(line))
            .await
            .expect("logging never waits for the API");
    }
    // Several retries hit a closed port before the API comes back.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let listener = TcpListener::bind(address).expect("the API returns on the same address");
    let server = MockServer::builder().listener(listener).start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(AcceptWholeBatch)
        .mount(&server)
        .await;

    reporter
        .flush()
        .await
        .expect("everything is delivered after the outage");

    let delivered = acknowledged_batches(&server, 0)
        .await
        .into_iter()
        .flat_map(|(_, messages)| messages)
        .collect::<Vec<_>>();
    assert_eq!(delivered, lines, "every line, once, in the order logged");
}

#[tokio::test]
async fn a_batch_resent_after_server_errors_keeps_its_key_content_and_order() {
    let server = MockServer::start().await;
    let down = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(Outage {
            down: Arc::clone(&down),
        })
        .mount(&server)
        .await;
    let reporter = reporter_with(&server.uri(), batch_of(4), fast_policy());

    let lines = numbered(10);
    for line in &lines {
        reporter.log(log(line)).await.expect("line queues");
    }
    while log_requests(&server).await.len() < 3 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    down.store(false, Ordering::SeqCst);
    reporter
        .flush()
        .await
        .expect("delivery resumes when the API recovers");

    let requests = log_requests(&server).await;
    let failed = requests
        .iter()
        .take_while(|request| idempotency_key(request) == idempotency_key(&requests[0]))
        .count();
    assert!(
        failed >= 3,
        "the first batch was retried while the API failed"
    );
    // Every retry of the first batch is byte-for-byte the same request, so
    // the control plane can answer it from its idempotency record.
    for request in &requests[..failed] {
        assert_eq!(request.body, requests[0].body);
    }
    let batches = acknowledged_batches(&server, failed - 1).await;
    assert_eq!(batches[0].0, idempotency_key(&requests[0]));
    let keys = batches
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut distinct = keys.clone();
    distinct.dedup();
    assert_eq!(keys, distinct, "each later batch has its own key");
    let delivered = batches
        .into_iter()
        .flat_map(|(_, messages)| messages)
        .collect::<Vec<_>>();
    assert_eq!(delivered, lines);
}

#[tokio::test]
async fn retry_after_is_honoured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(AcceptWholeBatch)
        .mount(&server)
        .await;
    let reporter = reporter_with(&server.uri(), LogBounds::default(), fast_policy());

    let started = Instant::now();
    reporter.log(log("throttled")).await.expect("line queues");
    reporter
        .flush()
        .await
        .expect("delivered after the requested wait");

    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the retry waited {:?}, less than Retry-After",
        started.elapsed()
    );
    assert_eq!(log_requests(&server).await.len(), 2);
}

#[tokio::test]
async fn a_full_buffer_drops_the_oldest_queued_lines_and_marks_the_gap() {
    let server = MockServer::start().await;
    let down = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(Outage {
            down: Arc::clone(&down),
        })
        .mount(&server)
        .await;
    let reporter = reporter_with(
        &server.uri(),
        batch_of(2),
        LogDeliveryPolicy {
            max_buffered_lines: 5,
            // Only a full batch or a flush sends, so a slow runner cannot
            // split the first batch.
            flush_interval: Duration::from_secs(60),
            ..fast_policy()
        },
    );

    let lines = numbered(12);
    reporter.log(log(&lines[0])).await.expect("line queues");
    reporter.log(log(&lines[1])).await.expect("line queues");
    // The first batch is now in flight and outside the buffer.
    wait_for_log_requests(&server, 1).await;
    for line in &lines[2..] {
        reporter
            .log(log(line))
            .await
            .expect("a full buffer still accepts the newest line");
    }
    down.store(false, Ordering::SeqCst);

    let error = reporter
        .flush()
        .await
        .expect_err("dropped lines are reported as not delivered");
    assert!(
        error.to_string().contains("5 log lines were not delivered"),
        "{error}"
    );

    let requests = log_requests(&server).await;
    let failed = requests
        .iter()
        .filter(|request| log_messages(request) == lines[..2])
        .count()
        - 1;
    let delivered = acknowledged_batches(&server, failed)
        .await
        .into_iter()
        .flat_map(|(_, messages)| messages)
        .collect::<Vec<_>>();
    assert_eq!(delivered[..2], lines[..2]);
    assert_eq!(
        delivered[2],
        "5 log lines are missing here: the Sakala Agent could not reach the control plane in time and dropped them."
    );
    assert_eq!(delivered[3..], lines[7..], "the newest lines are kept");
    let notice = requests
        .iter()
        .flat_map(|request| {
            request.body_json::<serde_json::Value>().expect("json")["logs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|line| {
            line["message"]
                .as_str()
                .is_some_and(|message| message.contains("missing here"))
        })
        .expect("the gap notice is sent");
    assert_eq!(notice["stream"], "system");
}

#[tokio::test]
async fn a_rejection_other_than_conflict_or_budget_stops_without_retry() {
    for status in [400, 403, 404] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(logs_path()))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let reporter = reporter_with(&server.uri(), LogBounds::default(), fast_policy());

        reporter.log(log("refused")).await.expect("line queues");
        reporter.log(log("refused too")).await.expect("line queues");
        let error = reporter.flush().await.expect_err("a 4xx stops delivery");
        assert!(
            error.to_string().contains(&status.to_string()),
            "{status}: {error}"
        );
        reporter
            .log(log("after"))
            .await
            .expect_err("delivery stays stopped");
    }
}

#[tokio::test]
async fn flush_gives_up_after_its_grace_and_delivery_resumes_for_later_lines() {
    let server = MockServer::start().await;
    let down = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path(logs_path()))
        .respond_with(Outage {
            down: Arc::clone(&down),
        })
        .mount(&server)
        .await;
    // After its first failure the worker waits at least a minute before it
    // tries again, so once that failure has reached the server nothing is in
    // flight and no retry can happen during the test. That makes "not retried
    // after the grace" a fact about the requests, not about a sleep.
    let reporter = reporter_with(
        &server.uri(),
        batch_of(2),
        LogDeliveryPolicy {
            flush_interval: Duration::from_secs(60),
            retry_min_delay: Duration::from_secs(60),
            retry_max_delay: Duration::from_secs(60),
            ..fast_policy()
        },
    );

    for line in ["a", "b", "c"] {
        reporter.log(log(line)).await.expect("line queues");
    }
    wait_for_log_requests(&server, 1).await;
    let error = reporter
        .flush_within(Duration::from_millis(200))
        .await
        .expect_err("the API never answered within the grace");
    assert!(
        error.to_string().contains("3 log lines were not delivered"),
        "{error}"
    );

    // Container output keeps flowing under the same command after cutover.
    down.store(false, Ordering::SeqCst);
    reporter
        .log(log("later"))
        .await
        .expect("new lines are still accepted");
    let error = reporter
        .flush()
        .await
        .expect_err("the earlier loss is still reported");
    assert!(error.to_string().contains("3 log lines"), "{error}");

    let requests = log_requests(&server).await;
    assert_eq!(
        requests.iter().map(log_messages).collect::<Vec<_>>(),
        [
            vec!["a".to_owned(), "b".to_owned()],
            vec!["later".to_owned()]
        ],
        "the abandoned batch is not retried any more"
    );
}

fn events_path() -> String {
    format!("/api/agent/v1/commands/{COMMAND_ID}/events")
}

/// Short enough that a test outlasts the budget in well under a second.
fn fast_event_policy() -> EventDeliveryPolicy {
    EventDeliveryPolicy {
        retry_min_delay: Duration::from_millis(10),
        retry_max_delay: Duration::from_millis(40),
        budget: Duration::from_millis(400),
    }
}

fn event_reporter(server: &MockServer) -> ApiRuntimeReporter {
    reporter_with(&server.uri(), LogBounds::default(), fast_policy())
        .with_event_policy(fast_event_policy())
}

fn event(event_type: &str) -> DeploymentEvent {
    DeploymentEvent {
        event_type: event_type.to_owned(),
        level: DeploymentEventLevel::Info,
        message: format!("{event_type} message"),
        metadata: json!({ "port": 3000 }),
        occurred_at: OffsetDateTime::now_utc(),
    }
}

async fn event_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("requests should be recorded")
        .into_iter()
        .filter(|request| request.url.path() == events_path())
        .collect()
}

fn event_type_of(request: &Request) -> String {
    request.body_json::<serde_json::Value>().expect("json body")["events"][0]["type"]
        .as_str()
        .expect("event type")
        .to_owned()
}

/// Answers `503` while `down` is set, then acknowledges the one event.
struct EventOutage {
    down: Arc<AtomicBool>,
}

impl Respond for EventOutage {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.down.load(Ordering::SeqCst) {
            ResponseTemplate::new(503)
        } else {
            accepted(1)
        }
    }
}

/// Answers `503` to the first `failures` requests, then acknowledges.
struct FailTimes {
    failures: usize,
    seen: AtomicUsize,
}

impl FailTimes {
    fn new(failures: usize) -> Self {
        Self {
            failures,
            seen: AtomicUsize::new(0),
        }
    }
}

impl Respond for FailTimes {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.seen.fetch_add(1, Ordering::SeqCst) < self.failures {
            ResponseTemplate::new(503)
        } else {
            accepted(1)
        }
    }
}

/// Collects what `tracing` writes in this test binary.
///
/// It is the global subscriber rather than a thread-local one: with a single
/// dispatcher, `tracing` caches a callsite's interest from whichever thread
/// hits it first, so a thread-local subscriber misses the Agent's warnings
/// when a parallel test reached the same `warn!` before it. Tests that read it
/// look for a value only they use.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    fn global() -> &'static Self {
        static LOGS: OnceLock<CapturedLogs> = OnceLock::new();
        LOGS.get_or_init(|| {
            let logs = Self::default();
            let sink = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || sink.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other test in this binary installs a subscriber");
            logs
        })
    }

    fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("log lock"))
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Before X-14 an event gave up after three attempts within about 1.5 s and
/// the error failed the deployment step that sent it.
#[tokio::test]
async fn an_event_is_retried_with_the_same_key_and_body_until_the_api_recovers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(events_path()))
        .respond_with(FailTimes::new(4))
        .mount(&server)
        .await;
    let reporter = event_reporter(&server);

    reporter
        .event(event("deployment.build.started"))
        .await
        .expect("the event is delivered once the API answers");

    let requests = event_requests(&server).await;
    assert_eq!(requests.len(), 5, "four failures, then the delivery");
    let key = idempotency_key(&requests[0]);
    for request in &requests {
        assert_eq!(idempotency_key(request), key, "every retry reuses the key");
        assert_eq!(
            request.body, requests[0].body,
            "every retry resends the same body"
        );
    }
    assert_eq!(reporter.undelivered_events(), 0);

    reporter
        .event(event("deployment.container.started"))
        .await
        .expect("the next event is delivered");
    let requests = event_requests(&server).await;
    assert_ne!(
        idempotency_key(&requests[5]),
        key,
        "each event is its own idempotent request"
    );
}

#[tokio::test]
async fn an_event_the_api_never_takes_is_dropped_with_a_warning_and_the_command_continues() {
    // Unique to this test, so its warning can be told apart from others.
    const EVENT_TYPE: &str = "deployment.undeliverable.test";
    let logs = CapturedLogs::global();
    let server = MockServer::start().await;
    let down = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path(events_path()))
        .respond_with(EventOutage {
            down: Arc::clone(&down),
        })
        .mount(&server)
        .await;
    let reporter = event_reporter(&server);

    let started = Instant::now();
    reporter
        .event(event(EVENT_TYPE))
        .await
        .expect("an undeliverable event must not fail the step that sent it");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "delivery stays within its budget: {:?}",
        started.elapsed()
    );
    let first_attempts = event_requests(&server).await.len();
    assert!(
        first_attempts >= 2,
        "the first event is retried: {first_attempts}"
    );
    assert_eq!(reporter.undelivered_events(), 1);
    let warnings = logs.lines_with(EVENT_TYPE);
    assert!(
        warnings.iter().any(|line| line.contains("WARN")
            && line.contains("deployment event was not delivered")
            && line.contains("undelivered_events=1")),
        "{warnings:#?}"
    );

    // While the API stays down, later events cost one attempt each.
    reporter
        .event(event("deployment.container.started"))
        .await
        .expect("still not an error");
    assert_eq!(event_requests(&server).await.len(), first_attempts + 1);
    assert_eq!(reporter.undelivered_events(), 2);

    // Once it is back, events are delivered again and the count stays.
    down.store(false, Ordering::SeqCst);
    reporter
        .event(event("deployment.health.started"))
        .await
        .expect("delivered after recovery");
    let requests = event_requests(&server).await;
    assert_eq!(requests.len(), first_attempts + 2);
    assert_eq!(
        event_type_of(requests.last().expect("a request")),
        "deployment.health.started"
    );
    assert_eq!(reporter.undelivered_events(), 2);
}

/// A fake control plane that stores events by `Idempotency-Key` the way
/// `ReportDeploymentEventAction` does, and loses the response to the first
/// request after storing it.
struct DedupingApi {
    stored: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    lose_next_response: AtomicBool,
}

impl Respond for DedupingApi {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let key = idempotency_key(request);
        let mut stored = self.stored.lock().expect("store lock");
        if let Some(body) = stored.get(&key) {
            if *body != request.body {
                return ResponseTemplate::new(409);
            }
            return ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "accepted_count": 1,
                    "duplicate_count": 1,
                    "first_sequence": 1,
                    "last_sequence": 1
                }
            }));
        }
        stored.insert(key, request.body.clone());
        if self.lose_next_response.swap(false, Ordering::SeqCst) {
            return ResponseTemplate::new(502);
        }
        accepted(1)
    }
}

#[tokio::test]
async fn an_event_stored_before_its_response_was_lost_is_not_stored_twice() {
    let server = MockServer::start().await;
    let stored = Arc::new(Mutex::new(HashMap::new()));
    Mock::given(method("POST"))
        .and(path(events_path()))
        .respond_with(DedupingApi {
            stored: Arc::clone(&stored),
            lose_next_response: AtomicBool::new(true),
        })
        .mount(&server)
        .await;
    let reporter = event_reporter(&server);

    reporter
        .event(event("deployment.runtime.ready"))
        .await
        .expect("the retry is acknowledged as a duplicate");

    assert_eq!(event_requests(&server).await.len(), 2);
    assert_eq!(stored.lock().expect("store lock").len(), 1);
    assert_eq!(reporter.undelivered_events(), 0);
}

#[tokio::test]
async fn a_conflict_still_fails_the_event_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(events_path()))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "status": "Cancelled",
            "terminal_at": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    let reporter = event_reporter(&server);

    let error = reporter
        .event(event("deployment.route.started"))
        .await
        .expect_err("a command the API no longer accepts reports for must stop");
    assert!(error.to_string().contains("Cancelled"), "{error}");
    assert_eq!(reporter.undelivered_events(), 0);
}

#[tokio::test]
async fn an_event_rejected_for_its_content_is_dropped_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(events_path()))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "The message exceeds the command limit."
        })))
        .expect(1)
        .mount(&server)
        .await;
    let reporter = event_reporter(&server);

    reporter
        .event(event("deployment.build.started"))
        .await
        .expect("a rejected event does not fail the deployment");
    assert_eq!(reporter.undelivered_events(), 1);
}
