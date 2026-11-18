//! Proves replicas against a real Docker and a real Caddy: the balanced route
//! the agent writes is accepted, requests reach every replica, a killed replica
//! does not cost a single failed request and is started again by replica
//! restore, and sleep, wake, and stop handle the whole set through the
//! executor's own lifecycle code.
//!
//! Opt-in, because it needs a daemon plus `caddy:2.11.3-alpine` and
//! `busybox:1.37` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test replicas_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container it creates starts with `sakala-smrep-`, and it removes only
//! those and the one project network it creates.

use std::{
    collections::BTreeSet,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    RuntimeExecutionError, RuntimeExecutor, RuntimeReporter, WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, SleepingPage};
use sakala_agent_runtime::{
    DockerRuntimeConfig, DockerRuntimeExecutor, TokioProcessRunner,
    containers::project_network_name,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PREFIX: &str = "sakala-smrep-";
const ROUTER: &str = "sakala-smrep-caddy";
/// Stands in for `sakala-runtime`. The router must start on a user-defined
/// network, as on a node: on Docker's default bridge its resolver keeps the
/// host's name servers for a few seconds after it joins a project network, so
/// the first requests fail to resolve every replica.
const RUNTIME_NETWORK: &str = "sakala-smrep-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const APP_IMAGE: &str = "busybox:1.37";
const DOMAIN: &str = "portfolio.run.sakala.localhost";
const PORT: u16 = 3000;
const REPLICAS: u32 = 3;
const REQUESTS: usize = 30;
/// How long a restored replica may take to get requests again. Caddy skips an
/// upstream for `fail_duration` (10 s) after it failed, then tries it again.
const BACK_IN_ROTATION_BOUND: Duration = Duration::from_secs(30);

/// What the runtime Caddyfile of a node looks like.
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

/// Status line and body of one request through the router. `wget`, because
/// BusyBox `nc` half-closes the connection after the request, which makes
/// Caddy cancel a proxied request and answer an empty 200 itself.
fn request(host: &str) -> (String, String) {
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
            &format!("Host: {host}"),
            "http://127.0.0.1/",
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
    (
        status,
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

/// The replicas that answered `REQUESTS` requests in a row, and the slowest
/// of those requests. Panics on any answer that is not a 200.
fn answering_replicas() -> (BTreeSet<String>, Duration) {
    let mut slowest = Duration::ZERO;
    let replicas = (0..REQUESTS)
        .map(|attempt| {
            let started = Instant::now();
            let (status, body) = request(DOMAIN);
            slowest = slowest.max(started.elapsed());
            assert!(
                status.ends_with(" 200 OK"),
                "request {attempt}: {status} {body}"
            );
            body
        })
        .collect();
    (replicas, slowest)
}

fn replica_name(index: u32) -> String {
    format!("{PREFIX}app-r{index}")
}

fn status_of(name: &str) -> String {
    must(&["inspect", "--format", "{{.State.Status}}", name])
        .trim()
        .to_owned()
}

fn remove_everything(network: &str) {
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
    docker(&["network", "rm", network]);
    docker(&["network", "rm", RUNTIME_NETWORK]);
}

struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        remove_everything(&self.0);
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
async fn real_caddy_balances_replicas_and_survives_a_killed_one() {
    let project_id = Uuid::new_v4();
    let deployment_id = Uuid::new_v4();
    let network = project_network_name(project_id);
    remove_everything(&network);
    let _cleanup = Cleanup(network.clone());
    let temp = TempDir::new().expect("temp directory");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites directory");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");

    must(&["network", "create", &network]);
    must(&["network", "create", RUNTIME_NETWORK]);
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--pull=never",
        "--network",
        RUNTIME_NETWORK,
        "-v",
        &format!("{}:/etc/caddy/Caddyfile:ro", caddyfile.display()),
        "-v",
        &format!("{}:/etc/caddy/sites:ro", sites.display()),
        ROUTER_IMAGE,
    ]);
    // What a deployment of three replicas leaves on the node: one container per
    // replica with the agent's labels, each answering with its own index.
    for index in 0..REPLICAS {
        let name = replica_name(index);
        must(&[
            "run",
            "-d",
            "--name",
            &name,
            "--pull=never",
            "--network",
            &network,
            "--label",
            "dev.sakala.managed=true",
            "--label",
            &format!("dev.sakala.project-id={project_id}"),
            "--label",
            &format!("dev.sakala.deployment-id={deployment_id}"),
            "--label",
            &format!("dev.sakala.domain={DOMAIN}"),
            "--label",
            &format!("dev.sakala.port={PORT}"),
            "--label",
            &format!("dev.sakala.replica={index}"),
            "--label",
            &format!("dev.sakala.replicas={REPLICAS}"),
            APP_IMAGE,
            "sh",
            "-c",
            &format!(
                "mkdir -p /www && echo r{index} > /www/index.html && exec httpd -f -p {PORT} -h /www"
            ),
        ]);
    }

    let runner = Arc::new(TokioProcessRunner::new(Duration::from_secs(30)));
    let executor = DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            caddy_container: ROUTER.to_owned(),
            caddy_sites_dir: sites.clone(),
            api_url: "https://api.staging.sakala.dev".to_owned(),
            ..DockerRuntimeConfig::default()
        },
        runner,
    );
    let lifecycle = || WorkloadLifecycleRequest {
        command_id: Uuid::new_v4(),
        project_id,
        deployment_id,
        remove_images: false,
        sleeping_page: SleepingPage::Waking,
        cancellation: CancellationToken::new(),
    };
    let route_path = sites.join(format!("{project_id}.Caddyfile"));

    // The executor finds the set through its labels, waits for a replica, and
    // writes one balanced route that Caddy validates and loads.
    executor
        .refresh_route(lifecycle(), Arc::new(NoopReporter))
        .await
        .expect("Caddy should accept the balanced route");
    let route = std::fs::read_to_string(&route_path).expect("route file");
    println!("\n--- balanced route\n{route}");
    assert!(route.contains(&format!(
        "reverse_proxy {}:{PORT} {}:{PORT} {}:{PORT} {{",
        replica_name(0),
        replica_name(1),
        replica_name(2)
    )));

    let all = (0..REPLICAS)
        .map(|index| format!("r{index}"))
        .collect::<BTreeSet<_>>();
    let (spread, slowest) = answering_replicas();
    println!("--- {REQUESTS} requests reached {spread:?}, slowest {slowest:?}");
    assert_eq!(spread, all, "every replica gets requests");

    // A replica dies without notice. Not one request may fail.
    must(&["kill", &replica_name(1)]);
    let (survivors, slowest) = answering_replicas();
    println!(
        "--- after killing r1, {REQUESTS} requests reached {survivors:?}, slowest {slowest:?}"
    );
    assert!(!survivors.contains("r1"), "{survivors:?}");
    assert!(!survivors.is_empty());
    assert_eq!(status_of(&replica_name(1)), "exited");

    // Docker leaves a killed container stopped. One restore pass starts it
    // again, without touching the route, and a second pass has nothing to do.
    let restore_started = Instant::now();
    let restored = executor.restore_replicas().await.expect("restore pass");
    assert_eq!((restored.restored, restored.failed), (1, 0));
    assert_eq!(status_of(&replica_name(1)), "running");
    let again = executor
        .restore_replicas()
        .await
        .expect("second restore pass");
    assert_eq!((again.restored, again.failed), (0, 0));
    assert_eq!(
        std::fs::read_to_string(&route_path).expect("route file"),
        route,
        "the route already lists r1"
    );
    let back = loop {
        let (reached, _) = answering_replicas();
        if reached == all {
            break restore_started.elapsed();
        }
        assert!(
            restore_started.elapsed() < BACK_IN_ROTATION_BOUND,
            "r1 is not back in rotation after {:?}: {reached:?}",
            restore_started.elapsed()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    println!("--- r1 restored and back in rotation after {back:?}, no request failed");

    // Sleep stops every replica and puts up the sleeping page. BusyBox httpd
    // ignores SIGTERM, so this waits out one shared grace period.
    let sleep_started = Instant::now();
    let slept = executor
        .sleep_project(lifecycle(), Arc::new(NoopReporter))
        .await
        .expect("sleep");
    assert_eq!(slept.result["status"], "sleeping");
    println!("--- sleep took {:?}", sleep_started.elapsed());
    for index in 0..REPLICAS {
        assert_eq!(status_of(&replica_name(index)), "exited", "r{index}");
    }
    let (status, _) = request(DOMAIN);
    assert!(status.ends_with(" 503 Service Unavailable"), "{status}");

    // Wake starts all three again, the killed one included, and restores the
    // balanced route.
    let woken = executor
        .wake_project(lifecycle(), Arc::new(NoopReporter))
        .await
        .expect("wake");
    assert_eq!(woken.result["status"], "ready");
    for index in 0..REPLICAS {
        assert_eq!(status_of(&replica_name(index)), "running", "r{index}");
    }
    // The route went live once one replica answered; give the rest a moment.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (after_wake, slowest) = answering_replicas();
    println!("--- after wake, {REQUESTS} requests reached {after_wake:?}, slowest {slowest:?}");
    assert_eq!(after_wake, all);

    // StopProject removes every replica and the route.
    let stopped = executor
        .stop_project(lifecycle(), Arc::new(NoopReporter))
        .await
        .expect("stop");
    assert_eq!(stopped.result["status"], "stopped");
    for index in 0..REPLICAS {
        let (exists, _) = docker(&["inspect", &replica_name(index)]);
        assert!(!exists, "r{index} is removed");
    }
    assert!(!route_path.exists());
    let (status, _) = request(DOMAIN);
    assert!(status.ends_with(" 404 Not Found"), "{status}");
}
