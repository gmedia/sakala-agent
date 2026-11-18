//! Drives `sakala-edge` against a fake control plane (wiremock) and a fake
//! Caddy (a process runner that records commands), with real files in a
//! temporary state directory.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use reqwest::Url;
use sakala_agent_core::ports::SecretString;
use sakala_agent_runtime::{
    CommandSpec, ProcessOutput, ProcessOutputSink, ProcessRunner, ProjectDomainSuffixes,
    RuntimeError,
};
use sakala_edge::{
    caddy::CaddyControl,
    files::EdgeFiles,
    source::{ROUTES_PATH, RouteSource},
    sync::{EdgeSync, SyncOutcome, SyncTiming},
    table::{RoutePolicy, UpstreamAllowlist},
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const TOKEN: &str = "edge-test-token";
const PROJECT_A: &str = "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a";
const PROJECT_B: &str = "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b";

/// Records each Caddy command and answers with success unless told to fail
/// one action.
#[derive(Default)]
struct FakeCaddy {
    calls: Mutex<Vec<String>>,
    failing_action: Mutex<Option<&'static str>>,
}

impl FakeCaddy {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn fail(&self, action: Option<&'static str>) {
        *self.failing_action.lock().expect("failing action") = action;
    }
}

#[async_trait]
impl ProcessRunner for FakeCaddy {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let action = spec.args[0].to_string_lossy().into_owned();
        self.calls.lock().expect("calls").push(action.clone());
        let failing = *self.failing_action.lock().expect("failing action") == Some(action.as_str());
        Ok(ProcessOutput {
            success: !failing,
            code: Some(i32::from(failing)),
            stdout: String::new(),
            stderr: if failing {
                "Error: adapting config".to_owned()
            } else {
                String::new()
            },
        })
    }
}

fn table(revision: &str, expected_minimum_routes: u64, routes: Value) -> Value {
    json!({
        "generated_at": "2026-11-18T03:00:00Z",
        "revision": revision,
        "expected_minimum_routes": expected_minimum_routes,
        "routes": routes,
    })
}

fn two_routes(revision: &str) -> Value {
    table(
        revision,
        2,
        json!([
            { "host": "blog.sakala.app", "project_id": PROJECT_A,
              "upstreams": ["10.0.47.201:8180", "10.0.47.202:8180"] },
            { "host": "shop.sakala.app", "project_id": PROJECT_B, "upstreams": ["10.0.47.201:8180"] },
        ]),
    )
}

fn edge(api_url: &str, dir: &Path, caddy: &Arc<FakeCaddy>) -> EdgeSync {
    let source = RouteSource::new(
        &Url::parse(api_url).expect("api url"),
        SecretString::new(TOKEN),
    )
    .expect("client");
    let runner: Arc<dyn ProcessRunner> = caddy.clone();
    EdgeSync::new(
        source,
        RoutePolicy {
            domain_suffixes: ProjectDomainSuffixes::default(),
            upstreams: UpstreamAllowlist::default(),
        },
        CaddyControl::new(
            runner,
            "caddy validate --config /etc/caddy/Caddyfile"
                .parse()
                .expect("command"),
            "caddy reload --config /etc/caddy/Caddyfile"
                .parse()
                .expect("command"),
        ),
        EdgeFiles::new(dir.to_path_buf()),
        SyncTiming {
            poll_interval: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        },
    )
}

async fn serve(server: &MockServer, response: ResponseTemplate) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path(ROUTES_PATH))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

/// A URL nothing listens on. A dropped `MockServer` does not do: wiremock
/// returns it to a pool, and another test may be using it already.
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    drop(listener);
    format!("http://{address}")
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).expect("readable file")
}

fn status(dir: &Path) -> Value {
    serde_json::from_str(&read(dir.join("status.json"))).expect("status json")
}

/// A state directory that already served `two_routes("rev-1")`.
async fn synced(server: &MockServer, dir: &Path, caddy: &Arc<FakeCaddy>) -> EdgeSync {
    serve(
        server,
        ResponseTemplate::new(200).set_body_json(two_routes("rev-1")),
    )
    .await;
    let mut sync = edge(&server.uri(), dir, caddy);
    assert_eq!(
        sync.sync_once().await.expect("first sync"),
        SyncOutcome::Applied
    );
    sync
}

#[tokio::test]
async fn the_first_fetch_writes_the_routes_and_reloads_caddy() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());

    let sync = synced(&server, dir.path(), &caddy).await;

    assert_eq!(caddy.calls(), ["validate", "reload"]);
    let routes = read(dir.path().join("routes.caddy"));
    assert!(routes.contains("@sakala_edge_blog_sakala_app host blog.sakala.app\n"));
    assert!(routes.contains("reverse_proxy 10.0.47.201:8180 10.0.47.202:8180 {"));
    assert!(
        routes
            .contains("handle @sakala_edge_shop_sakala_app {\n\treverse_proxy 10.0.47.201:8180\n}")
    );
    let snapshot: Value =
        serde_json::from_str(&read(dir.path().join("routes.json"))).expect("json");
    assert_eq!(snapshot["revision"], "rev-1");
    assert_eq!(snapshot["routes"].as_array().map(Vec::len), Some(2));
    let status = status(dir.path());
    assert_eq!(status["revision"], "rev-1");
    assert_eq!(status["route_count"], 2);
    assert_eq!(status["source"], "api");
    assert_eq!(status["consecutive_failures"], 0);
    assert_eq!(status["last_error"], Value::Null);
    assert!(status["last_success_at"].is_string());
    assert_eq!(sync.current().map(|table| table.revision()), Some("rev-1"));
    let requests = server.received_requests().await.expect("recorded");
    assert!(requests[0].headers.get("if-none-match").is_none());
}

#[tokio::test]
async fn an_api_error_keeps_the_routes_and_records_the_failure() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    let mut sync = synced(&server, dir.path(), &caddy).await;
    let routes = read(dir.path().join("routes.caddy"));
    let snapshot = read(dir.path().join("routes.json"));

    serve(&server, ResponseTemplate::new(500)).await;
    let error = sync.sync_once().await.expect_err("500 fails");

    assert_eq!(error.code(), "api_status");
    assert_eq!(read(dir.path().join("routes.caddy")), routes);
    assert_eq!(read(dir.path().join("routes.json")), snapshot);
    assert_eq!(caddy.calls(), ["validate", "reload"]);
    let status = status(dir.path());
    assert_eq!(status["revision"], "rev-1");
    assert_eq!(status["consecutive_failures"], 1);
    assert_eq!(status["last_error"]["code"], "api_status");

    serve(
        &server,
        ResponseTemplate::new(200).set_body_string("{\"routes\":"),
    )
    .await;
    assert_eq!(
        sync.sync_once().await.expect_err("malformed").code(),
        "malformed"
    );
    assert_eq!(status_of(dir.path(), "consecutive_failures"), json!(2));
}

fn status_of(dir: &Path, key: &str) -> Value {
    status(dir)[key].clone()
}

#[tokio::test]
async fn a_table_smaller_than_the_expected_minimum_is_rejected() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    let mut sync = synced(&server, dir.path(), &caddy).await;
    let routes = read(dir.path().join("routes.caddy"));

    let shrunk = table(
        "rev-2",
        2,
        json!([{ "host": "blog.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.47.201:8180"] }]),
    );
    serve(&server, ResponseTemplate::new(200).set_body_json(shrunk)).await;
    let error = sync.sync_once().await.expect_err("rejected");

    assert_eq!(error.code(), "below_expected_minimum");
    assert_eq!(read(dir.path().join("routes.caddy")), routes);
    assert_eq!(caddy.calls(), ["validate", "reload"]);
    assert_eq!(status_of(dir.path(), "revision"), json!("rev-1"));

    serve(
        &server,
        ResponseTemplate::new(200).set_body_json(table("rev-3", 0, json!([]))),
    )
    .await;
    assert_eq!(
        sync.sync_once().await.expect_err("empty").code(),
        "empty_table"
    );
}

#[tokio::test]
async fn a_restart_while_the_api_is_down_serves_the_snapshot() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    drop(synced(&server, dir.path(), &caddy).await);
    let routes = read(dir.path().join("routes.caddy"));
    // The routes file is lost, for example on a rebuilt host that kept only
    // the state directory's snapshot.
    std::fs::remove_file(dir.path().join("routes.caddy")).expect("remove routes");

    let caddy = Arc::new(FakeCaddy::default());
    let mut restarted = edge(&closed_port_url(), dir.path(), &caddy);
    assert!(restarted.restore_snapshot().await.expect("snapshot served"));

    assert_eq!(read(dir.path().join("routes.caddy")), routes);
    assert_eq!(caddy.calls(), ["validate", "reload"]);
    assert_eq!(status_of(dir.path(), "source"), json!("snapshot"));
    assert_eq!(status_of(dir.path(), "revision"), json!("rev-1"));

    let error = restarted.sync_once().await.expect_err("API is down");
    assert_eq!(error.code(), "api_unreachable");
    assert_eq!(read(dir.path().join("routes.caddy")), routes);
    assert_eq!(status_of(dir.path(), "source"), json!("snapshot"));
}

#[tokio::test]
async fn a_snapshot_that_matches_the_routes_file_does_not_reload_caddy() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    drop(synced(&server, dir.path(), &caddy).await);

    let caddy = Arc::new(FakeCaddy::default());
    let mut restarted = edge(&server.uri(), dir.path(), &caddy);
    assert!(restarted.restore_snapshot().await.expect("snapshot served"));

    assert!(caddy.calls().is_empty());
    assert_eq!(
        restarted.current().map(|table| table.revision()),
        Some("rev-1")
    );
    assert_eq!(status_of(dir.path(), "source"), json!("snapshot"));

    // The API confirms the revision with 304; the routes now count as current.
    serve(&server, ResponseTemplate::new(304)).await;
    assert_eq!(
        restarted.sync_once().await.expect("sync"),
        SyncOutcome::Unchanged
    );
    assert_eq!(status_of(dir.path(), "source"), json!("api"));
    assert!(caddy.calls().is_empty());
}

#[tokio::test]
async fn a_start_without_snapshot_waits_for_the_api() {
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    let mut sync = edge(&closed_port_url(), &dir.path().join("state"), &caddy);

    assert!(!sync.restore_snapshot().await.expect("no snapshot"));
    assert!(!dir.path().join("state/routes.caddy").exists());
    assert!(caddy.calls().is_empty());
}

#[tokio::test]
async fn a_snapshot_the_policy_now_refuses_leaves_the_routes_file_alone() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    drop(synced(&server, dir.path(), &caddy).await);
    let mut snapshot: Value =
        serde_json::from_str(&read(dir.path().join("routes.json"))).expect("json");
    snapshot["routes"][0]["upstreams"] = json!(["8.8.8.8:80"]);
    std::fs::write(dir.path().join("routes.json"), snapshot.to_string()).expect("write");
    let routes = read(dir.path().join("routes.caddy"));

    let caddy = Arc::new(FakeCaddy::default());
    let mut restarted = edge(&server.uri(), dir.path(), &caddy);
    let error = restarted.restore_snapshot().await.expect_err("refused");

    assert_eq!(error.code(), "upstream_not_allowed");
    assert_eq!(read(dir.path().join("routes.caddy")), routes);
    assert!(caddy.calls().is_empty());
    assert!(restarted.current().is_none());
}

#[tokio::test]
async fn an_unchanged_revision_writes_nothing_and_is_sent_as_if_none_match() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    let mut sync = synced(&server, dir.path(), &caddy).await;
    let routes_modified = std::fs::metadata(dir.path().join("routes.caddy"))
        .and_then(|metadata| metadata.modified())
        .expect("mtime");
    let snapshot_modified = std::fs::metadata(dir.path().join("routes.json"))
        .and_then(|metadata| metadata.modified())
        .expect("mtime");

    // An API without conditional requests sends the same table again.
    serve(
        &server,
        ResponseTemplate::new(200).set_body_json(two_routes("rev-1")),
    )
    .await;
    assert_eq!(
        sync.sync_once().await.expect("sync"),
        SyncOutcome::Unchanged
    );
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(
        requests[0]
            .headers
            .get("if-none-match")
            .and_then(|value| value.to_str().ok()),
        Some("\"rev-1\"")
    );

    // An API with conditional requests answers 304.
    serve(&server, ResponseTemplate::new(304)).await;
    assert_eq!(
        sync.sync_once().await.expect("sync"),
        SyncOutcome::Unchanged
    );

    assert_eq!(caddy.calls(), ["validate", "reload"]);
    let after = |name: &str| {
        std::fs::metadata(dir.path().join(name))
            .and_then(|metadata| metadata.modified())
            .expect("mtime")
    };
    assert_eq!(after("routes.caddy"), routes_modified);
    assert_eq!(after("routes.json"), snapshot_modified);
    assert_eq!(status_of(dir.path(), "consecutive_failures"), json!(0));
}

#[tokio::test]
async fn a_new_revision_with_the_same_routes_moves_the_snapshot_without_a_reload() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    let mut sync = synced(&server, dir.path(), &caddy).await;

    serve(
        &server,
        ResponseTemplate::new(200).set_body_json(two_routes("rev-2")),
    )
    .await;
    assert_eq!(
        sync.sync_once().await.expect("sync"),
        SyncOutcome::SameRoutes
    );

    assert_eq!(caddy.calls(), ["validate", "reload"]);
    let snapshot: Value =
        serde_json::from_str(&read(dir.path().join("routes.json"))).expect("json");
    assert_eq!(snapshot["revision"], "rev-2");
    assert_eq!(status_of(dir.path(), "revision"), json!("rev-2"));
}

#[tokio::test]
async fn a_table_caddy_refuses_is_rolled_back() {
    for failing_action in ["validate", "reload"] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let caddy = Arc::new(FakeCaddy::default());
        let mut sync = synced(&server, dir.path(), &caddy).await;
        let routes = read(dir.path().join("routes.caddy"));
        let snapshot = read(dir.path().join("routes.json"));

        let moved = table(
            "rev-2",
            1,
            json!([{ "host": "blog.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.48.9:8180"] },
                   { "host": "new.sakala.app", "project_id": PROJECT_B, "upstreams": ["10.0.48.9:8180"] }]),
        );
        serve(&server, ResponseTemplate::new(200).set_body_json(moved)).await;
        caddy.fail(Some(failing_action));
        let error = sync.sync_once().await.expect_err("Caddy refuses");

        assert_eq!(error.code(), "caddy_refused", "{failing_action}");
        assert_eq!(
            read(dir.path().join("routes.caddy")),
            routes,
            "{failing_action}"
        );
        assert_eq!(
            read(dir.path().join("routes.json")),
            snapshot,
            "{failing_action}"
        );
        assert!(!dir.path().join("routes.caddy.tmp").exists());
        let expected: &[&str] = if failing_action == "validate" {
            &["validate", "reload", "validate", "reload"]
        } else {
            &["validate", "reload", "validate", "reload", "reload"]
        };
        assert_eq!(caddy.calls(), expected, "{failing_action}");
        assert_eq!(sync.current().map(|table| table.revision()), Some("rev-1"));

        // The next poll tries the same revision again once Caddy accepts it.
        caddy.fail(None);
        assert_eq!(sync.sync_once().await.expect("retry"), SyncOutcome::Applied);
        assert!(read(dir.path().join("routes.caddy")).contains("new.sakala.app"));
    }
}

#[tokio::test]
async fn the_api_path_is_appended_to_a_base_url_with_a_path() {
    let source = RouteSource::new(
        &Url::parse("https://example.test/sakala/").expect("url"),
        SecretString::new(TOKEN),
    )
    .expect("client");
    assert_eq!(
        source.url().as_str(),
        "https://example.test/sakala/api/edge/v1/routes"
    );
}

#[tokio::test]
async fn shutdown_abandons_a_fetch_in_flight() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let caddy = Arc::new(FakeCaddy::default());
    serve(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(two_routes("rev-1"))
            .set_delay(Duration::from_secs(30)),
    )
    .await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker = tokio::spawn(edge(&server.uri(), dir.path(), &caddy).run(shutdown.clone()));

    // Wait until the request reached the API, then stop.
    let started = std::time::Instant::now();
    while server
        .received_requests()
        .await
        .is_none_or(|requests| requests.is_empty())
    {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no request reached the API"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("the loop stops without waiting for the response")
        .expect("worker");
    assert!(caddy.calls().is_empty());
    assert!(!dir.path().join("routes.caddy").exists());
}
