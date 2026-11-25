//! Proves against a real Caddy that a project route hands the app exactly one
//! client address, and that only the edge can choose it.
//!
//! The runtime Caddyfile of a node trusts the addresses host Caddy or Sakala
//! Edge connect from (`servers { trusted_proxies ... }` in sakala-deployment).
//! From such an address the app must see the visitor in `X-Forwarded-For` and
//! `X-Real-IP` and the edge's `X-Forwarded-Proto`; from any other peer, such as
//! a workload on a project network calling the router directly, it must see
//! that peer and whatever the router observed itself, never the headers the
//! peer wrote. Which address Caddy resolves is decided by Caddy, so only Caddy
//! itself can prove it.
//!
//! Opt-in, because it needs a daemon plus `caddy:2.11.3-alpine` and
//! `busybox:1.37` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test client_ip_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container it creates starts with `sakala-smclientip-`, and it removes
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

const PREFIX: &str = "sakala-smclientip-";
const ROUTER: &str = "sakala-smclientip-caddy";
const APP: &str = "sakala-smclientip-app";
/// Stands in for both `sakala-runtime` and a project network: the router, the
/// app, the edge, and a workload all share it, which is the hard case.
const NETWORK: &str = "sakala-smclientip-net";
/// A fixed subnet, so the edge and the workload have known addresses.
const SUBNET: &str = "10.213.77.0/24";
const ROUTER_IP: &str = "10.213.77.2";
const EDGE_IP: &str = "10.213.77.10";
const WORKLOAD_IP: &str = "10.213.77.20";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const APP_IMAGE: &str = "busybox:1.37";
const DOMAIN: &str = "portfolio.sakala.app";
const PORT: u16 = 3000;
const CGI_PATH: &str = "/cgi-bin/headers";
/// The visitor as host Caddy reports it.
const VISITOR: &str = "203.0.113.7";
/// An address the visitor or a workload makes up.
const FORGED: &str = "198.51.100.66";
/// How long a fresh BusyBox `httpd` may take to accept connections.
const APP_READY_BOUND: Duration = Duration::from_secs(10);

/// The global options of the runtime Caddyfile in sakala-deployment, with the
/// edge's address as the only trusted one.
const ROUTER_CADDYFILE: &str = "{
\tadmin localhost:2019
\tauto_https off
\tservers {
\t\ttrusted_proxies static {$SAKALA_RUNTIME_TRUSTED_PROXIES}
\t\ttrusted_proxies_strict
\t\tclient_ip_headers X-Forwarded-For
\t}
}

import /etc/caddy/sites/*.Caddyfile

:80 {
\trespond \"Not found\" 404
}
";

/// Prints the forwarding headers the app received, one `name=value` per line.
const CGI_SCRIPT: &str = "#!/bin/sh
printf 'Content-Type: text/plain\\r\\n\\r\\n'
printf 'xff=%s\\n' \"$HTTP_X_FORWARDED_FOR\"
printf 'xri=%s\\n' \"$HTTP_X_REAL_IP\"
printf 'xfp=%s\\n' \"$HTTP_X_FORWARDED_PROTO\"
printf 'xfh=%s\\n' \"$HTTP_X_FORWARDED_HOST\"
";

fn docker(args: &[&str]) -> (bool, String) {
    let output = Command::new("timeout")
        .arg("60")
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

/// What the app reports for one request to the router from a throwaway
/// container at `source`, sending `headers` besides the project's `Host`.
fn request_from(source: &str, headers: &[(&str, &str)]) -> String {
    let name = format!("{PREFIX}client");
    let mut args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--name".to_owned(),
        name,
        "--pull=never".to_owned(),
        "--network".to_owned(),
        NETWORK.to_owned(),
        "--ip".to_owned(),
        source.to_owned(),
        APP_IMAGE.to_owned(),
        "wget".to_owned(),
        "-q".to_owned(),
        "-O-".to_owned(),
        "-T".to_owned(),
        "10".to_owned(),
        "--header".to_owned(),
        format!("Host: {DOMAIN}"),
    ];
    for (header, value) in headers {
        args.push("--header".to_owned());
        args.push(format!("{header}: {value}"));
    }
    args.push(format!("http://{ROUTER_IP}{CGI_PATH}"));
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    must(&args).trim().to_owned()
}

fn expected(client: &str, proto: &str) -> String {
    format!("xff={client}\nxri={client}\nxfp={proto}\nxfh={DOMAIN}")
}

async fn wait_until_app_answers() {
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
            &format!("http://{APP}:{PORT}{CGI_PATH}"),
        ]);
        if ok {
            return;
        }
        assert!(
            started.elapsed() < APP_READY_BOUND,
            "{APP} does not answer after {:?}",
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
async fn real_caddy_takes_the_client_address_only_from_the_edge() {
    remove_everything();
    let _cleanup = Cleanup;
    let temp = TempDir::new().expect("temp directory");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites directory");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");
    let cgi_bin = temp.path().join("cgi-bin");
    std::fs::create_dir_all(&cgi_bin).expect("cgi-bin directory");
    let script = cgi_bin.join("headers");
    std::fs::write(&script, CGI_SCRIPT).expect("CGI script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("CGI script is executable");

    must(&["network", "create", "--subnet", SUBNET, NETWORK]);
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--pull=never",
        "--network",
        NETWORK,
        "--ip",
        ROUTER_IP,
        "-e",
        &format!("SAKALA_RUNTIME_TRUSTED_PROXIES={EDGE_IP}/32"),
        "-v",
        &format!("{}:/etc/caddy/Caddyfile:ro", caddyfile.display()),
        "-v",
        &format!("{}:/etc/caddy/sites:ro", sites.display()),
        ROUTER_IMAGE,
    ]);
    must(&[
        "run",
        "-d",
        "--name",
        APP,
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
    wait_until_app_answers().await;

    let runner = Arc::new(TokioProcessRunner::new(Duration::from_secs(30)));
    let routes = CaddyFileRouteManager::new(
        sites.clone(),
        Arc::new(DockerExecCaddyReloader::new(runner, ROUTER.to_owned())),
    );
    let project_id = Uuid::new_v4();
    routes
        .activate(
            &RouteSpec {
                custom_domains: Vec::new(),
                project_id,
                deployment_id: Uuid::new_v4(),
                domain: DOMAIN.to_owned(),
                upstreams: vec![APP.to_owned()],
                port: PORT,
            },
            &NoopReporter,
        )
        .await
        .expect("Caddy should accept the route");
    let route =
        std::fs::read_to_string(sites.join(format!("{project_id}.Caddyfile"))).expect("route file");
    println!("\n--- route\n{route}");

    // What host Caddy sends: the visitor it saw and the scheme it terminated.
    // A visitor's own X-Real-IP passes host Caddy untouched and must not
    // reach the app.
    let from_edge = request_from(
        EDGE_IP,
        &[
            ("X-Forwarded-For", VISITOR),
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-Host", DOMAIN),
            ("X-Real-IP", FORGED),
        ],
    );
    println!("--- from the edge\n{from_edge}");
    assert_eq!(from_edge, expected(VISITOR, "https"));

    // A chain whose left part the visitor wrote: only the address the edge
    // appended counts.
    let chained = request_from(
        EDGE_IP,
        &[
            ("X-Forwarded-For", &format!("{FORGED}, {VISITOR}")),
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-Host", DOMAIN),
        ],
    );
    println!("--- from the edge with a forged chain\n{chained}");
    assert_eq!(chained, expected(VISITOR, "https"));

    // A workload calling the router directly with the same headers gets
    // nothing it wrote: the app sees the workload and plain HTTP.
    let from_workload = request_from(
        WORKLOAD_IP,
        &[
            ("X-Forwarded-For", VISITOR),
            ("X-Forwarded-Proto", "https"),
            ("X-Forwarded-Host", "evil.example"),
            ("X-Real-IP", FORGED),
        ],
    );
    println!("--- from a workload\n{from_workload}");
    assert_eq!(from_workload, expected(WORKLOAD_IP, "http"));
}
