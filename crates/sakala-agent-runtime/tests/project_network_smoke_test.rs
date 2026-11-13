//! Proves tenant network isolation against a real Docker daemon: the router reaches
//! every project by name, a project cannot reach another, and removing a project
//! takes its network with it. A recreated router is reconnected by the periodic
//! network worker.
//!
//! Opt-in, because it needs a daemon and `caddy:2.11.3-alpine` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test project_network_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container and network it creates starts with `sakala-netiso-`, and it
//! removes only those. No image is built.

use std::{collections::BTreeMap, process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::{
    ports::{RuntimeExecutionError, RuntimeExecutor, WorkloadLifecycleRequest},
    scheduler::metrics::SchedulerMetrics,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, DeploymentEvent, DeploymentLog, LogBounds, SleepingPage,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, ResourceSafetyConfig, RuntimeError, TokioProcessRunner,
    containers::{
        ContainerEngine, DockerContainerEngine, RunContainerRequest, WorkloadIsolation,
        container_name, project_network_name,
    },
    health::{DockerHealthChecker, HealthChecker, ReadinessRequest},
};
use tempfile::TempDir;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PREFIX: &str = "sakala-netiso-";
const ROUTER: &str = "sakala-netiso-caddy";
const SHARED: &str = "sakala-netiso-runtime";
const LEGACY: &str = "sakala-netiso-legacy";
const IMAGE: &str = "caddy:2.11.3-alpine";
const AGENT: &str = "sakala-netiso-agent";
const PORT: u16 = 3000;

/// Gives every agent-made name this test's prefix, and takes it off again in
/// output, so the agent sees its own names.
struct RenamingRunner(TokioProcessRunner);

fn rename(value: &str) -> String {
    value
        .replace("sakala-project-", "sakala-netiso-project-")
        .replace("sakala-app-", "sakala-netiso-app-")
}

#[async_trait]
impl ProcessRunner for RenamingRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let mut renamed = spec.clone();
        renamed.args = spec
            .args
            .iter()
            .map(|arg| rename(&arg.to_string_lossy()).into())
            .collect();
        let mut output = self.0.run(&renamed, sink).await?;
        output.stdout = output
            .stdout
            .replace("sakala-netiso-project-", "sakala-project-")
            .replace("sakala-netiso-app-", "sakala-app-");
        Ok(output)
    }
}

struct NullReporter;

#[async_trait]
impl sakala_agent_core::ports::RuntimeReporter for NullReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

struct Project {
    id: Uuid,
    deployment: Uuid,
    body: &'static str,
}

impl Project {
    fn new(body: &'static str) -> Self {
        Self {
            id: Uuid::new_v4(),
            deployment: Uuid::new_v4(),
            body,
        }
    }

    /// The name on the daemon.
    fn container(&self) -> String {
        rename(&container_name(self.id, self.deployment))
    }

    fn network(&self) -> String {
        rename(&project_network_name(self.id))
    }
}

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
    (output.status.success(), text.trim().to_owned())
}

fn must(args: &[&str]) -> String {
    let (ok, text) = docker(args);
    assert!(ok, "docker {args:?} failed: {text}");
    text
}

/// `wget` from inside `from`, with a short deadline.
fn fetch(from: &str, url: &str) -> (bool, String) {
    docker(&["exec", from, "wget", "-q", "-T", "3", "-O", "-", url])
}

fn router_networks() -> String {
    must(&[
        "container",
        "inspect",
        "--format",
        "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}} {{end}}",
        ROUTER,
    ])
}

fn network_exists(name: &str) -> bool {
    docker(&["network", "inspect", name]).0
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
    let (_, networks) = docker(&[
        "network",
        "ls",
        "--filter",
        &format!("name=^{PREFIX}"),
        "--format",
        "{{.Name}}",
    ]);
    for name in networks.lines().filter(|name| !name.is_empty()) {
        assert!(name.starts_with(PREFIX), "refusing to remove {name}");
        docker(&["network", "rm", name]);
    }
}

struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        remove_everything();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon and caddy:2.11.3-alpine"]
async fn projects_reach_only_the_router_and_leave_no_network_behind() {
    remove_everything();
    let _cleanup = Cleanup;
    let temp = TempDir::new().expect("temp directory");
    let runtime = std::env::var("SAKALA_SMOKE_CONTAINER_RUNTIME").ok();

    // What sakala-deployment's compose.runtime.yaml starts: the router on the
    // shared network, plus a workload an older Agent left there.
    must(&["network", "create", SHARED]);
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--network",
        SHARED,
        "--pull=never",
        IMAGE,
    ]);
    must(&[
        "run",
        "-d",
        "--name",
        LEGACY,
        "--network",
        SHARED,
        "--pull=never",
        IMAGE,
        "caddy",
        "respond",
        "--listen",
        ":3000",
        "legacy",
    ]);

    let runner: Arc<dyn ProcessRunner> = Arc::new(RenamingRunner(TokioProcessRunner::new(
        Duration::from_secs(60),
    )));
    let engine = DockerContainerEngine::new(
        Arc::clone(&runner),
        ROUTER.to_owned(),
        ResourceSafetyConfig::default(),
        20,
        AGENT.to_owned(),
        Duration::from_secs(300),
        WorkloadIsolation {
            container_runtime: runtime.clone(),
            container_dns: None,
        },
    );
    let health = DockerHealthChecker::new(
        Arc::clone(&runner),
        ROUTER.to_owned(),
        Duration::from_millis(500),
    );

    let a = Project::new("project-a");
    let b = Project::new("project-b");
    for project in [&a, &b] {
        let workspace = temp.path().join(project.id.to_string());
        std::fs::create_dir_all(&workspace).expect("workspace");
        engine
            .start(
                &RunContainerRequest {
                    command_id: Uuid::new_v4(),
                    project_id: project.id,
                    deployment_id: project.deployment,
                    name: container_name(project.id, project.deployment),
                    image: IMAGE.to_owned(),
                    workspace,
                    environment: BTreeMap::new(),
                    resources: AppliedRuntimeResources {
                        memory_mb: 256,
                        cpu_millis: 500,
                        pids_limit: 128,
                    },
                    domain: "smoke.run.sakala.localhost".to_owned(),
                    port: PORT,
                    log_bounds: LogBounds::default(),
                    start_command: Some(format!("caddy respond --listen :{PORT} {}", project.body)),
                },
                &NullReporter,
            )
            .await
            .expect("workload starts");
        // The agent's own readiness probe runs from the router, by name.
        health
            .wait_until_ready(&ReadinessRequest {
                container: container_name(project.id, project.deployment),
                port: PORT,
                started_at: Instant::now(),
                timeout: Duration::from_secs(30),
                expected_restarts: Some(0),
                operation: "smoke",
            })
            .await
            .expect("the router reaches the workload by name");
    }

    println!("router networks: {}", router_networks());
    for project in [&a, &b] {
        let (ok, body) = fetch(ROUTER, &format!("http://{}:{PORT}/", project.container()));
        println!("router -> {}: ok={ok} body={body}", project.body);
        assert!(ok && body == project.body, "{body}");
        let networks = must(&[
            "container",
            "inspect",
            "--format",
            "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}} {{end}}",
            &project.container(),
        ]);
        assert_eq!(networks.trim(), project.network(), "only its own network");
    }
    let (ok, body) = fetch(ROUTER, &format!("http://{LEGACY}:{PORT}/"));
    println!("router -> legacy workload on the shared network: ok={ok} body={body}");
    assert!(ok && body == "legacy");

    // Project A against B, by name and by address, and against the legacy workload.
    let b_ip = must(&[
        "container",
        "inspect",
        "--format",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
        &b.container(),
    ]);
    let legacy_ip = must(&[
        "container",
        "inspect",
        "--format",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
        LEGACY,
    ]);
    for (what, url) in [
        ("B by name", format!("http://{}:{PORT}/", b.container())),
        ("B by IP", format!("http://{b_ip}:{PORT}/")),
        ("legacy by name", format!("http://{LEGACY}:{PORT}/")),
        ("legacy by IP", format!("http://{legacy_ip}:{PORT}/")),
    ] {
        let (ok, output) = fetch(&a.container(), &url);
        println!("A -> {what} ({url}): ok={ok} {output}");
        assert!(!ok, "project A reached {what}");
    }
    let (ok, body) = fetch(&a.container(), &format!("http://{}:{PORT}/", a.container()));
    println!("A -> itself: ok={ok} body={body}");
    assert!(
        ok,
        "the workload itself works, so the failures above are isolation"
    );

    // StopProject through the real executor.
    std::fs::create_dir_all(temp.path().join("builds")).expect("workspace");
    std::fs::create_dir_all(temp.path().join("sites")).expect("route directory");
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            agent_id: AGENT.to_owned(),
            workspace_root: temp.path().join("builds"),
            caddy_sites_dir: temp.path().join("sites"),
            caddy_container: ROUTER.to_owned(),
            runtime_network: SHARED.to_owned(),
            railpack_network: "sakala-netiso-railpack".to_owned(),
            ..DockerRuntimeConfig::default()
        },
        Arc::clone(&runner),
    ));
    let preflight = executor.preflight().await.expect("preflight");
    let check = preflight
        .checks
        .iter()
        .find(|check| check.name == "project-networks")
        .expect("project network check");
    println!(
        "preflight project-networks: ready={} fatal={} {}",
        check.ready, check.fatal, check.detail
    );
    assert!(!check.fatal);
    assert!(!network_exists("sakala-netiso-project-preflight"));

    // Compose recreates the router while the Agent keeps running: it comes back
    // on the shared network only, and the periodic worker reconnects it.
    let url = |project: &Project| format!("http://{}:{PORT}/", project.container());
    must(&["rm", "-f", "-v", ROUTER]);
    must(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--network",
        SHARED,
        "--pull=never",
        IMAGE,
    ]);
    println!("router networks after recreation: {}", router_networks());
    assert!(!fetch(ROUTER, &url(&a)).0 && !fetch(ROUTER, &url(&b)).0);
    let (stop_worker, worker_shutdown) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(sakala_agent_core::networks::worker::run(
        Arc::clone(&executor) as Arc<dyn RuntimeExecutor>,
        Duration::from_secs(1),
        Arc::new(SchedulerMetrics::default()),
        worker_shutdown,
    ));
    let recreated = Instant::now();
    while !(fetch(ROUTER, &url(&a)).0 && fetch(ROUTER, &url(&b)).0) {
        assert!(
            recreated.elapsed() < Duration::from_secs(20),
            "the worker did not reconnect the router"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!(
        "router -> A and B again {:.1}s after recreation (worker interval 1s); networks: {}",
        recreated.elapsed().as_secs_f64(),
        router_networks()
    );
    assert_eq!(fetch(ROUTER, &url(&a)).1, "project-a");
    assert_eq!(fetch(ROUTER, &url(&b)).1, "project-b");
    assert!(fetch(ROUTER, &format!("http://{LEGACY}:{PORT}/")).0);
    stop_worker.send(true).expect("worker shutdown");
    worker.await.expect("worker");
    let lifecycle = |project: &Project| WorkloadLifecycleRequest {
        command_id: Uuid::new_v4(),
        project_id: project.id,
        deployment_id: project.deployment,
        remove_images: false,
        sleeping_page: SleepingPage::Waking,
        cancellation: CancellationToken::new(),
    };
    let output = executor
        .stop_project(lifecycle(&a), Arc::new(NullReporter))
        .await
        .expect("stop project A");
    println!("stop A: {}", output.result);
    println!("router networks after removing A: {}", router_networks());
    assert!(!network_exists(&a.network()), "A's network is gone");
    assert!(!router_networks().contains(&a.network()));
    assert!(fetch(ROUTER, &format!("http://{}:{PORT}/", b.container())).0);

    // A recreated router has lost B's network; startup recovery reconnects it.
    must(&["network", "disconnect", &b.network(), ROUTER]);
    assert!(!fetch(ROUTER, &format!("http://{}:{PORT}/", b.container())).0);
    executor.recover(None).await.expect("recover");
    let (ok, body) = fetch(ROUTER, &format!("http://{}:{PORT}/", b.container()));
    println!("router -> B after recovery reattached it: ok={ok} body={body}");
    assert!(ok && body == "project-b");

    // A network whose containers vanished while the Agent was down.
    must(&["rm", "-f", "-v", &b.container()]);
    executor.recover(None).await.expect("recover");
    println!("router networks after recovery GC: {}", router_networks());
    assert!(!network_exists(&b.network()), "B's empty network is gone");
    assert_eq!(router_networks().trim(), SHARED);
}
