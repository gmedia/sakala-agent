//! Proves a sleeping route against a real Caddy: Caddy accepts the generated
//! file, answers the project's domain with the sleeping page, and counts the
//! visit per host where the agent reads it. A route Caddy refused would break
//! the reload of every project on the node, so a unit test is not enough.
//!
//! Opt-in, because it needs a daemon and `caddy:2.11.3-alpine` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test sleeping_route_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container it creates starts with `sakala-smsleep-`, and it removes only
//! those.

use std::{process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::ports::{RuntimeExecutionError, RuntimeExecutor, RuntimeReporter};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};
use sakala_agent_runtime::{
    DockerRuntimeConfig, DockerRuntimeExecutor, TokioProcessRunner,
    routing::{
        CaddyFileRouteManager, DockerExecCaddyMetrics, DockerExecCaddyReloader, RouteIdentity,
        RouteManager, RouteSpec, SleepingRouteSpec, wake_url,
    },
};
use tempfile::TempDir;
use uuid::Uuid;

const PREFIX: &str = "sakala-smsleep-";
const ROUTER: &str = "sakala-smsleep-caddy";
const APP: &str = "sakala-smsleep-app";
const IMAGE: &str = "caddy:2.11.3-alpine";
const DOMAIN: &str = "portfolio.run.sakala.localhost";

/// What the runtime Caddyfile of a node looks like once idle sleep is enabled.
const ROUTER_CADDYFILE: &str = "{
\tadmin localhost:2019
\tauto_https off
\tmetrics {
\t\tper_host
\t}
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

/// A raw HTTP/1.0 request from inside the router, so the whole response comes
/// back, a 503 included, which BusyBox `wget` would not print.
fn request(host: &str) -> String {
    must(&[
        "exec",
        ROUTER,
        "sh",
        "-c",
        &format!("printf 'GET / HTTP/1.0\\r\\nHost: {host}\\r\\n\\r\\n' | nc 127.0.0.1 80"),
    ])
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
#[ignore = "needs a local Docker daemon and caddy:2.11.3-alpine"]
async fn real_caddy_serves_the_sleeping_page_and_counts_the_visit() {
    remove_everything();
    let _cleanup = Cleanup;
    let temp = TempDir::new().expect("temp directory");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites directory");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--pull=never",
        "-v",
        &format!("{}:/etc/caddy/Caddyfile:ro", caddyfile.display()),
        "-v",
        &format!("{}:/etc/caddy/sites:ro", sites.display()),
        IMAGE,
    ]);
    let project_id = Uuid::new_v4();
    let deployment_id = Uuid::new_v4();
    // Stands in for the project's container, so the executor can map the
    // domain to it. Only its labels matter.
    must(&[
        "run",
        "-d",
        "--name",
        APP,
        "--pull=never",
        "--label",
        "dev.sakala.managed=true",
        "--label",
        &format!("dev.sakala.project-id={project_id}"),
        "--label",
        &format!("dev.sakala.deployment-id={deployment_id}"),
        "--label",
        &format!("dev.sakala.domain={DOMAIN}"),
        "--entrypoint",
        "sleep",
        IMAGE,
        "300",
    ]);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let runner = Arc::new(TokioProcessRunner::new(Duration::from_secs(30)));
    let routes = CaddyFileRouteManager::new(
        sites.clone(),
        Arc::new(DockerExecCaddyReloader::new(
            runner.clone(),
            ROUTER.to_owned(),
        )),
    );
    let executor = DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            caddy_container: ROUTER.to_owned(),
            caddy_sites_dir: sites.clone(),
            ..DockerRuntimeConfig::default()
        },
        runner.clone(),
    );
    let traffic_of_project = || async {
        executor.workload_traffic().await.map(|traffic| {
            traffic
                .into_iter()
                .find(|item| item.project_id == project_id)
                .map(|item| (item.deployment_id, item.last_request_at))
        })
    };

    // Nothing counted per host yet: the router has said nothing about activity.
    assert_eq!(traffic_of_project().await, None);

    let route = SleepingRouteSpec {
        project_id,
        deployment_id,
        domain: DOMAIN.to_owned(),
        wake_url: wake_url("https://api.staging.sakala.dev", "portfolio").expect("wake URL"),
    };
    routes
        .activate_sleeping(&route, &NoopReporter)
        .await
        .expect("Caddy should validate and load the sleeping route");
    let file =
        std::fs::read_to_string(sites.join(format!("{project_id}.Caddyfile"))).expect("route file");
    println!("\n--- sleeping route\n{file}");

    let response = request(DOMAIN);
    println!("--- response\n{response}");
    let (head, body) = response.split_once("\r\n\r\n").expect("headers and body");
    assert!(head.starts_with("HTTP/1.0 503"), "{head}");
    for header in [
        "Retry-After: 5",
        "Cache-Control: no-store",
        "Content-Type: text/html; charset=utf-8",
        "X-Robots-Tag: noindex",
    ] {
        assert!(
            head.lines().any(|line| line == header),
            "{header} in {head}"
        );
    }
    assert!(
        body.starts_with("<!doctype html>\n<html lang=\"id\">"),
        "{body}"
    );
    assert!(body.ends_with("</html>"), "{body}");
    assert!(body.contains("<title>Proyek sedang dinyalakan</title>"));
    assert!(body.contains(
        "fetch(\"https://api.staging.sakala.dev/api/v1/public/projects/portfolio/wake\",{method:\"POST\",mode:\"no-cors\",keepalive:true}).catch(()=>0);"
    ));
    assert!(body.contains(
        "body{display:flex;align-items:center;justify-content:center;background:#08413D;"
    ));
    assert!(!body.contains('\t'), "the heredoc indentation is stripped");
    // Another domain still falls through to the node's 404.
    assert!(request("other.run.sakala.localhost").starts_with("HTTP/1.0 404"));

    // The visit is counted under the project's host, where the agent reads it.
    let counters = DockerExecCaddyMetrics::new(runner.clone(), ROUTER.to_owned())
        .read()
        .await
        .expect("request counters");
    println!("--- request counters\n{counters}");
    assert!(
        counters.contains(&format!("host=\"{DOMAIN}\"")),
        "{counters}"
    );
    let (traffic_deployment, first_read) = traffic_of_project()
        .await
        .expect("per-host counters")
        .expect("the project is listed");
    assert_eq!(traffic_deployment, deployment_id);

    // A new visit moves the time forward; no visit keeps it.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, unchanged) = traffic_of_project().await.flatten().expect("listed");
    assert_eq!(unchanged, first_read);
    request(DOMAIN);
    let (_, visited) = traffic_of_project().await.flatten().expect("listed");
    assert!(visited > first_read, "{visited} after {first_read}");

    // Wake writes the serving route over the sleeping one; Caddy accepts it.
    routes
        .activate(
            &RouteSpec {
                project_id,
                deployment_id,
                domain: DOMAIN.to_owned(),
                upstream: APP.to_owned(),
                port: 3000,
            },
            &NoopReporter,
        )
        .await
        .expect("Caddy should accept the serving route over the sleeping one");
    assert!(!request(DOMAIN).starts_with("HTTP/1.0 503"));

    // Back to sleep, then StopProject removes the sleeping route.
    routes
        .activate_sleeping(&route, &NoopReporter)
        .await
        .expect("sleeping route again");
    assert!(
        routes
            .deactivate(
                RouteIdentity {
                    project_id,
                    deployment_id: Some(deployment_id),
                },
                &NoopReporter,
            )
            .await
            .expect("deactivation")
    );
    assert!(request(DOMAIN).starts_with("HTTP/1.0 404"));
}
