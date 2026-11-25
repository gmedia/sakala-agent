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
use sakala_agent_runtime::{ForbiddenZones, ProjectDomainSuffixes, TokioProcessRunner};
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
        // One network per test, so the tests can run side by side.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let test = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let network = format!("{PREFIX}-{}-{test}", std::process::id());
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
            forbidden_zones: ForbiddenZones::default(),
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

/// Host configuration for custom domains as `sakala-deployment` installs it,
/// except that the snippet asks Caddy's internal CA instead of Let's Encrypt,
/// so the test needs no public name. The generated file is imported at the top
/// level, beside the wildcard sites.
const CUSTOM_DOMAINS_CADDYFILE: &str = "{
\tadmin localhost:2019
\tskip_install_trust
}

(sakala_custom_domain_tls) {
\ttls internal
}

import /var/lib/sakala-edge/custom-domains.caddy

http://*.sakala.app:80 {
\timport /var/lib/sakala-edge/routes.caddy
\thandle {
\t\trespond \"unknown host\" 404
\t}
}
";

#[tokio::test]
#[ignore = "needs a Docker daemon and caddy:2.11.3-alpine"]
async fn real_caddy_serves_custom_domain_sites_from_the_generated_file() {
    let mut containers = Docker::new();
    let listen = format!(":{UPSTREAM_PORT}");
    // Answers with what reached it, so the test sees the Host and the
    // forwarded headers Edge sent.
    let upstream = containers.run(
        "custom-upstream",
        &[
            CADDY_IMAGE,
            "caddy",
            "respond",
            "--listen",
            &listen,
            "--body",
            "host={http.request.host} proto={http.request.header.X-Forwarded-Proto} for={http.request.header.X-Forwarded-For}",
        ],
    );
    let address = format!("{}:{UPSTREAM_PORT}", Docker::address(&upstream));

    let etc = tempfile::tempdir().expect("tempdir");
    let state = tempfile::tempdir().expect("tempdir");
    write(&etc.path().join("Caddyfile"), CUSTOM_DOMAINS_CADDYFILE);
    write(&state.path().join("routes.caddy"), "");
    write(&state.path().join("custom-domains.caddy"), "");
    let etc_mount = format!("{}:/etc/caddy:ro", etc.path().display());
    let state_mount = format!("{}:/var/lib/sakala-edge:ro", state.path().display());
    let edge = containers.run(
        "custom-edge",
        &[
            "-p",
            "127.0.0.1::80",
            "-p",
            "127.0.0.1::443",
            "-v",
            &etc_mount,
            "-v",
            &state_mount,
            CADDY_IMAGE,
        ],
    );
    let published = |port: &str| {
        docker(&["port", &edge, port])
            .lines()
            .next()
            .expect("published port")
            .parse::<std::net::SocketAddr>()
            .expect("socket address")
    };
    let http = published("80/tcp");
    let https = published("443/tcp");

    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(ROUTES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "generated_at": "2026-11-25T03:00:00Z",
            "revision": "smoke-custom-1",
            "expected_minimum_routes": 1,
            "routes": [
                { "host": "blog.sakala.app", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a",
                  "upstreams": [address] },
            ],
            "expected_minimum_custom_routes": 2,
            "custom_routes": [
                { "host": "www.contoh-sakala.test", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a",
                  "upstreams": [address] },
                { "host": "kosong.contoh-sakala.test", "project_id": "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b",
                  "upstreams": [] },
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
    let started = Instant::now();
    while docker(&["exec", &edge, "caddy", "version"]).is_empty() {
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "Edge Caddy never started"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut sync = EdgeSync::new(
        RouteSource::new(
            &Url::parse(&api.uri()).expect("url"),
            SecretString::new("smoke"),
        )
        .expect("client"),
        RoutePolicy {
            domain_suffixes: ProjectDomainSuffixes::default(),
            upstreams: UpstreamAllowlist::default(),
            forbidden_zones: ForbiddenZones::default(),
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
            .expect("real Caddy accepts the custom domain sites"),
        SyncOutcome::Applied
    );
    assert_eq!(sync.status().custom_routes_error, None);
    let generated =
        std::fs::read_to_string(state.path().join("custom-domains.caddy")).expect("custom file");
    println!("{generated}");
    let adapted = docker(&[
        "exec",
        &edge,
        "caddy",
        "adapt",
        "--config",
        "/etc/caddy/Caddyfile",
        "--adapter",
        "caddyfile",
    ]);
    for host in ["www.contoh-sakala.test", "kosong.contoh-sakala.test"] {
        assert!(
            adapted.contains(&format!("\"{host}\"")),
            "{host} in {adapted}"
        );
    }

    // Each custom domain is its own TLS site, chosen by SNI. The client
    // trusts the internal CA of this Caddy, which `tls internal` creates when
    // it loads the sites, and verifies every certificate against it.
    let started = Instant::now();
    let root_pem = loop {
        let (found, pem) = {
            let output = Command::new("docker")
                .args([
                    "exec",
                    &edge,
                    "cat",
                    "/data/caddy/pki/authorities/local/root.crt",
                ])
                .output()
                .expect("docker CLI");
            (output.status.success(), output.stdout)
        };
        if found {
            break pem;
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "Caddy never created its internal CA"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let root = reqwest::Certificate::from_pem(&root_pem).expect("Caddy root certificate");
    let client = |host: &'static str| {
        reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(root.clone())
            .resolve(host, https)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("client")
    };
    let www = "www.contoh-sakala.test";
    let started = Instant::now();
    let body = loop {
        let response = client(www)
            .get(format!("https://{www}:{}/", https.port()))
            .header("X-Forwarded-For", "203.0.113.9")
            .header("X-Forwarded-Proto", "http")
            .send()
            .await;
        match response {
            Ok(response) if response.status() == StatusCode::OK => {
                break response.text().await.expect("body");
            }
            _ if started.elapsed() < READY_TIMEOUT => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            other => panic!("the custom domain never answered: {other:?}"),
        }
    };
    println!("{www}: {body}");
    assert!(
        body.starts_with(&format!("host={www} proto=https for=")),
        "{body}"
    );
    assert!(
        !body.contains("203.0.113.9"),
        "a forwarded header from the visitor reached the app: {body}"
    );

    let kosong = "kosong.contoh-sakala.test";
    let response = client(kosong)
        .get(format!("https://{kosong}:{}/", https.port()))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .map(|v| v.as_bytes()),
        Some(&b"no-store"[..])
    );
    assert!(
        response
            .text()
            .await
            .expect("body")
            .starts_with("Belum ada aplikasi di alamat ini.")
    );

    // Plain HTTP on a custom domain is sent to HTTPS, which is also where
    // Caddy answers HTTP-01 challenges.
    let response = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
        .get(format!("http://{http}/"))
        .header(HOST, www)
        .send()
        .await
        .expect("request");
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);

    // The project route keeps working next to the custom sites.
    let (status, body) = get(
        &reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
        &format!("http://{http}/"),
        "blog.sakala.app",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("host=blog.sakala.app"), "{body}");
}
