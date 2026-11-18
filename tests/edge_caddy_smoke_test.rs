//! Runs `sakala-edge` against a real Caddy: the generated routes file is
//! imported by a static Caddyfile, validated and reloaded with the real
//! `caddy` binary, and requests are proxied to two upstream containers.
//!
//! Opt-in because it needs a Docker daemon and `caddy:2.11.3-alpine`:
//!
//! ```bash
//! cargo test -p sakala-edge --test edge_caddy_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container and network it creates starts with `sakala-edge-sm`, and it
//! removes only those.

use std::{
    collections::HashSet,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::{StatusCode, Url, header::HOST};
use sakala_agent_core::ports::SecretString;
use sakala_agent_runtime::{ProjectDomainSuffixes, TokioProcessRunner};
use sakala_edge::{
    caddy::{COMMAND_TIMEOUT, CaddyControl},
    files::EdgeFiles,
    source::{ROUTES_PATH, RouteSource},
    sync::{EdgeSync, SyncOutcome, SyncTiming},
    table::{RoutePolicy, UpstreamAllowlist},
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const CADDY_IMAGE: &str = "caddy:2.11.3-alpine";
const PREFIX: &str = "sakala-edge-sm";
const UPSTREAM_PORT: u16 = 8180;
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Static host configuration as an operator writes it: TLS and the answer for
/// unknown hosts stay here, the routes come from the imported file.
const STATIC_CADDYFILE: &str = "{
\tadmin localhost:2019
\tskip_install_trust
}

http://*.sakala.app:80 {
\timport /var/lib/sakala-edge/routes.caddy
\thandle {
\t\trespond \"unknown host\" 404
\t}
}

*.staging.sakala.app {
\ttls internal
\timport /var/lib/sakala-edge/routes.caddy
\thandle {
\t\trespond \"unknown host\" 404
\t}
}
";

struct Docker {
    names: Vec<String>,
    network: String,
}

impl Docker {
    fn new() -> Self {
        let network = format!("{PREFIX}-{}", std::process::id());
        docker(&["network", "create", &network]);
        Self {
            names: Vec::new(),
            network,
        }
    }

    fn run(&mut self, suffix: &str, args: &[&str]) -> String {
        let name = format!("{}-{suffix}", self.network);
        let mut command = vec!["run", "-d", "--name", &name, "--network", &self.network];
        command.extend_from_slice(args);
        docker(&command);
        self.names.push(name.clone());
        name
    }

    fn address(name: &str) -> String {
        docker(&[
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            name,
        ])
    }
}

impl Drop for Docker {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
    }
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker CLI");
    assert!(
        output.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn get(client: &reqwest::Client, base: &str, host: &str) -> (StatusCode, String) {
    let response = client
        .get(base)
        .header(HOST, host)
        .send()
        .await
        .expect("request through Edge");
    let status = response.status();
    (status, response.text().await.unwrap_or_default())
}

fn write(path: &Path, content: &str) {
    std::fs::write(path, content).expect("write file");
}

#[tokio::test]
#[ignore = "needs a Docker daemon and caddy:2.11.3-alpine"]
async fn real_caddy_serves_balances_and_fails_over_with_the_generated_routes() {
    let mut containers = Docker::new();
    let upstream = |body: &'static str| {
        move |containers: &mut Docker| {
            let listen = format!(":{UPSTREAM_PORT}");
            containers.run(
                body,
                &[
                    CADDY_IMAGE,
                    "caddy",
                    "respond",
                    "--listen",
                    &listen,
                    "--body",
                    body,
                ],
            )
        }
    };
    let upstream_a = upstream("upstream-a")(&mut containers);
    let upstream_b = upstream("upstream-b")(&mut containers);
    let address_a = format!("{}:{UPSTREAM_PORT}", Docker::address(&upstream_a));
    let address_b = format!("{}:{UPSTREAM_PORT}", Docker::address(&upstream_b));

    let etc = tempfile::tempdir().expect("tempdir");
    let state = tempfile::tempdir().expect("tempdir");
    write(&etc.path().join("Caddyfile"), STATIC_CADDYFILE);
    // The installer creates an empty routes file so Caddy starts before Edge.
    write(&state.path().join("routes.caddy"), "");
    let etc_mount = format!("{}:/etc/caddy:ro", etc.path().display());
    // The directory is mounted, not the file, so a rename is seen inside.
    let state_mount = format!("{}:/var/lib/sakala-edge:ro", state.path().display());
    let edge = containers.run(
        "edge",
        &[
            "-p",
            "127.0.0.1::80",
            "-v",
            &etc_mount,
            "-v",
            &state_mount,
            CADDY_IMAGE,
        ],
    );
    let port = docker(&["port", &edge, "80/tcp"]);
    let base = format!("http://{}/", port.lines().next().expect("published port"));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    let started = Instant::now();
    loop {
        match client.get(&base).header(HOST, "x.sakala.app").send().await {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => break,
            _ if started.elapsed() < READY_TIMEOUT => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            other => panic!("Edge Caddy never became ready: {other:?}"),
        }
    }

    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ROUTES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "generated_at": "2026-11-18T03:00:00Z",
            "revision": "smoke-1",
            "expected_minimum_routes": 3,
            "routes": [
                { "host": "blog.sakala.app", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a",
                  "upstreams": [address_a, address_b] },
                { "host": "solo.sakala.app", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b",
                  "upstreams": [address_a] },
                { "host": "blog.staging.sakala.app", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1c",
                  "upstreams": [address_b] },
            ],
        })))
        .mount(&api)
        .await;
    let validate = format!(
        "docker exec {edge} caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile"
    );
    let reload = format!(
        "docker exec {edge} caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile"
    );
    let mut sync = EdgeSync::new(
        RouteSource::new(
            &Url::parse(&api.uri()).expect("url"),
            SecretString::new("smoke"),
        )
        .expect("client"),
        RoutePolicy {
            domain_suffixes: ProjectDomainSuffixes::default(),
            upstreams: UpstreamAllowlist::default(),
        },
        CaddyControl::new(
            Arc::new(TokioProcessRunner::new(COMMAND_TIMEOUT)),
            validate.parse().expect("command"),
            reload.parse().expect("command"),
        ),
        EdgeFiles::new(state.path().to_path_buf()),
        SyncTiming {
            poll_interval: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        },
    );

    assert_eq!(
        sync.sync_once()
            .await
            .expect("real Caddy accepts the routes"),
        SyncOutcome::Applied
    );
    println!(
        "{}",
        std::fs::read_to_string(state.path().join("routes.caddy")).expect("routes")
    );

    let mut bodies = HashSet::new();
    for _ in 0..30 {
        let (status, body) = get(&client, &base, "blog.sakala.app").await;
        assert_eq!(status, StatusCode::OK);
        bodies.insert(body);
    }
    assert_eq!(
        bodies,
        HashSet::from(["upstream-a".to_owned(), "upstream-b".to_owned()])
    );
    assert_eq!(
        get(&client, &base, "solo.sakala.app").await,
        (StatusCode::OK, "upstream-a".to_owned())
    );
    assert_eq!(
        get(&client, &base, "other.sakala.app").await.0,
        StatusCode::NOT_FOUND
    );

    docker(&["kill", &upstream_a]);
    for _ in 0..20 {
        assert_eq!(
            get(&client, &base, "blog.sakala.app").await,
            (StatusCode::OK, "upstream-b".to_owned()),
            "a request failed after one node went away"
        );
    }
}
