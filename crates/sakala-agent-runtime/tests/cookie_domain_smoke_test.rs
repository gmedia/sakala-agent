//! Proves against a real Caddy that every project route makes upstream cookies
//! host-only: each `Set-Cookie` value an app sends reaches the client without
//! its `Domain` attribute, with every other attribute and the cookie value
//! intact. Projects share the registrable domain `sakala.app` until it is on
//! the Public Suffix List, so a cookie set for `Domain=sakala.app` by one app
//! would otherwise be sent to every other app. The rewrite is a Go regular
//! expression evaluated by Caddy, so only Caddy itself can prove it.
//!
//! Opt-in, because it needs a daemon plus `caddy:2.11.3-alpine` and
//! `busybox:1.37` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test cookie_domain_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container it creates starts with `sakala-smcookie-`, and it removes
//! only those and the one network it creates.

use std::{os::unix::fs::PermissionsExt, process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::ports::{RuntimeExecutionError, RuntimeReporter};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};
use sakala_agent_runtime::{
    TokioProcessRunner,
    routing::{CaddyFileRouteManager, DockerExecCaddyReloader, RouteManager, RouteSpec},
};
use tempfile::TempDir;
use uuid::Uuid;

const PREFIX: &str = "sakala-smcookie-";
const ROUTER: &str = "sakala-smcookie-caddy";
/// Stands in for `sakala-runtime`.
const NETWORK: &str = "sakala-smcookie-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const APP_IMAGE: &str = "busybox:1.37";
const DOMAIN: &str = "portfolio.sakala.app";
const PORT: u16 = 3000;
/// Where the router listens, seen from inside its own container.
const ROUTER_ADDRESS: &str = "127.0.0.1:80";
const CGI_PATH: &str = "/cgi-bin/cookies";
const BODY: &str = "cookies sent";
/// How long a fresh BusyBox `httpd` may take to accept connections.
const APP_READY_BOUND: Duration = Duration::from_secs(10);

/// What the app sends, and what the client must receive for it.
const COOKIES: &[(&str, &str)] = &[
    (
        "a=1; Domain=sakala.app; Path=/; HttpOnly",
        "a=1; Path=/; HttpOnly",
    ),
    ("b=2; domain=.Sakala.App; Secure", "b=2; Secure"),
    ("c=3; Path=/; SameSite=Lax", "c=3; Path=/; SameSite=Lax"),
    ("pref=domain%3Dx; Path=/", "pref=domain%3Dx; Path=/"),
    ("domain=5; Path=/", "domain=5; Path=/"),
    (
        "d=4; Path=/; Domain = sakala.app ; Max-Age=60",
        "d=4; Path=/; Max-Age=60",
    ),
    ("e=6;Domain=sakala.app", "e=6"),
    (
        "f=7; DOMAIN=.sakala.app; Path=/; Domain=x.sakala.app; Secure",
        "f=7; Path=/; Secure",
    ),
    (
        "g=8; Expires=Wed, 21 Oct 2026 07:28:00 GMT; Domain=sakala.app",
        "g=8; Expires=Wed, 21 Oct 2026 07:28:00 GMT",
    ),
];

/// What the runtime Caddyfile of a node looks like.
const ROUTER_CADDYFILE: &str = "{
\tadmin localhost:2019
\tauto_https off
}

import /etc/caddy/sites/*.Caddyfile

:80 {
\trespond \"Not found\" 404
}
";

fn docker(args: &[&str]) -> (bool, String) {
    let output = Command::new("timeout")
        .arg("30")
        .arg("docker")
        .args(args)
        .output()
        .expect("docker should be installed");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn must(args: &[&str]) -> String {
    let (ok, text) = docker(args);
    assert!(ok, "docker {args:?} failed: {text}");
    text
}

/// A CGI script that answers with every cookie of [`COOKIES`] as its own
/// `Set-Cookie` header.
fn cgi_script() -> String {
    let mut script = String::from("#!/bin/sh\nprintf 'Content-Type: text/plain\\r\\n'\n");
    for (sent, _) in COOKIES {
        script.push_str(&format!("printf '%s\\r\\n' 'Set-Cookie: {sent}'\n"));
    }
    script.push_str(&format!("printf '\\r\\n{BODY}\\n'\n"));
    script
}

/// Status line, `Set-Cookie` values in order, and body of one request made
/// from inside the router to `authority`, with the project's domain as `Host`.
fn request(authority: &str) -> (String, Vec<String>, String) {
    let output = Command::new("timeout")
        .arg("30")
        .arg("docker")
        .args([
            "exec",
            ROUTER,
            "wget",
            "-S",
            "-O-",
            "-T",
            "10",
            "--header",
            &format!("Host: {DOMAIN}"),
            &format!("http://{authority}{CGI_PATH}"),
        ])
        .output()
        .expect("docker should be installed");
    let headers = String::from_utf8_lossy(&output.stderr);
    let status = headers
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("HTTP/"))
        .unwrap_or_default()
        .to_owned();
    let cookies = headers
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie")
                .then(|| value.trim().to_owned())
        })
        .collect();
    (
        status,
        cookies,
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

async fn wait_until_app_answers(name: &str) {
    let started = std::time::Instant::now();
    loop {
        let (ok, _) = docker(&[
            "exec",
            ROUTER,
            "wget",
            "-q",
            "-O-",
            "-T",
            "2",
            &format!("http://{name}:{PORT}{CGI_PATH}"),
        ]);
        if ok {
            return;
        }
        assert!(
            started.elapsed() < APP_READY_BOUND,
            "{name} does not answer after {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn remove_everything() {
    let (_, containers) = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{PREFIX}"),
        "--format",
        "{{.Names}}",
    ]);
    for name in containers.lines().filter(|name| !name.is_empty()) {
        assert!(name.starts_with(PREFIX), "refusing to remove {name}");
        docker(&["rm", "-f", "-v", name]);
    }
    docker(&["network", "rm", NETWORK]);
}

struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        remove_everything();
    }
}

struct NoopReporter;

#[async_trait]
impl RuntimeReporter for NoopReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and busybox:1.37"]
async fn real_caddy_makes_every_upstream_cookie_host_only() {
    remove_everything();
    let _cleanup = Cleanup;
    let temp = TempDir::new().expect("temp directory");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites directory");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");
    let cgi_bin = temp.path().join("cgi-bin");
    std::fs::create_dir_all(&cgi_bin).expect("cgi-bin directory");
    let script = cgi_bin.join("cookies");
    std::fs::write(&script, cgi_script()).expect("CGI script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("CGI script is executable");

    must(&["network", "create", NETWORK]);
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--pull=never",
        "--network",
        NETWORK,
        "-v",
        &format!("{}:/etc/caddy/Caddyfile:ro", caddyfile.display()),
        "-v",
        &format!("{}:/etc/caddy/sites:ro", sites.display()),
        ROUTER_IMAGE,
    ]);
    let apps = [format!("{PREFIX}app-r0"), format!("{PREFIX}app-r1")];
    for name in &apps {
        must(&[
            "run",
            "-d",
            "--name",
            name,
            "--pull=never",
            "--network",
            NETWORK,
            "-v",
            &format!("{}:/www/cgi-bin:ro", cgi_bin.display()),
            APP_IMAGE,
            "httpd",
            "-f",
            "-p",
            &PORT.to_string(),
            "-h",
            "/www",
        ]);
    }
    // The app itself sends every cookie unchanged, so whatever the client gets
    // without a `Domain` attribute was rewritten by the router.
    let sent = COOKIES
        .iter()
        .map(|(sent, _)| (*sent).to_owned())
        .collect::<Vec<_>>();
    for name in &apps {
        wait_until_app_answers(name).await;
        let (status, cookies, _) = request(&format!("{name}:{PORT}"));
        assert!(status.ends_with(" 200 OK"), "{name}: {status}");
        assert_eq!(cookies, sent, "{name} sends the cookies unchanged");
    }

    let runner = Arc::new(TokioProcessRunner::new(Duration::from_secs(30)));
    let routes = CaddyFileRouteManager::new(
        sites.clone(),
        Arc::new(DockerExecCaddyReloader::new(runner, ROUTER.to_owned())),
    );
    let project_id = Uuid::new_v4();
    let expected = COOKIES
        .iter()
        .map(|(_, received)| (*received).to_owned())
        .collect::<Vec<_>>();

    // One replica, then the balanced route of two: both must rewrite cookies.
    for upstreams in [&apps[..1], &apps[..]] {
        routes
            .activate(
                &RouteSpec {
                    project_id,
                    deployment_id: Uuid::new_v4(),
                    domain: DOMAIN.to_owned(),
                    upstreams: upstreams.to_vec(),
                    port: PORT,
                },
                &NoopReporter,
            )
            .await
            .expect("Caddy should accept the route");
        let route = std::fs::read_to_string(sites.join(format!("{project_id}.Caddyfile")))
            .expect("route file");
        println!("\n--- route with {} upstream(s)\n{route}", upstreams.len());

        // Two requests per upstream, so a balanced route reaches both.
        for attempt in 0..upstreams.len() * 2 {
            let (status, cookies, body) = request(ROUTER_ADDRESS);
            println!("--- request {attempt}: {status}");
            for cookie in &cookies {
                println!("    Set-Cookie: {cookie}");
            }
            assert!(status.ends_with(" 200 OK"), "{status} {body}");
            assert_eq!(body, BODY);
            assert_eq!(cookies, expected);
        }
    }
}
