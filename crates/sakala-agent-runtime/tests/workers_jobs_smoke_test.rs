//! Proves worker processes and job runs (ADR-022) against a real Docker and a
//! real Caddy, through the executor's own deploy, lifecycle, and job code: a
//! worker starts beside the web replica without a route, a worker that dies
//! before the route switches fails the deployment and the previous version
//! keeps serving, a worker that ends with code 0 is not a crash, restart,
//! sleep, wake, and stop handle web and workers together, and a job runs once
//! in a container of the live image, also while the project sleeps, without
//! waking it, and is removed after success, failure, timeout, or cancellation,
//! also when it is cancelled while Docker creates its container. A one-off
//! container left behind is removed by the periodic reconcile, and never
//! keeps its project from stopping.
//!
//! No checkout and no build: the image is a labelled copy of `busybox:1.37`
//! that every deployment reuses. Opt-in, because it needs a daemon plus
//! `caddy:2.11.3-alpine` and `busybox:1.37` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test workers_jobs_smoke_test -- --ignored --nocapture
//! ```
//!
//! It runs on Docker's default runtime. The gVisor runtime of the hosted
//! nodes (`SAKALA_CONTAINER_RUNTIME=runsc`) is not exercised here.
//!
//! Every container it creates starts with `sakala-smwj-` or carries the fresh
//! project UUID, and it removes only those, the project's image, and the
//! networks it creates.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    DeployProjectRequest, RunJobRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
    WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, SleepingPage};
use sakala_agent_runtime::{
    DockerRuntimeConfig, DockerRuntimeExecutor, TokioProcessRunner,
    containers::{
        ReplicaSlot, container_name, image_name, job_container_name, project_network_name,
        worker_container_name,
    },
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PREFIX: &str = "sakala-smwj-";
const ROUTER: &str = "sakala-smwj-caddy";
const RUNTIME_NETWORK: &str = "sakala-smwj-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const APP_IMAGE: &str = "busybox:1.37";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const DOMAIN: &str = "portfolio.run.sakala.localhost";
const PORT: u16 = 3000;
/// Serves `web` and exits on `SIGTERM`, which `httpd` as PID 1 would ignore.
const WEB_COMMAND: &str = "mkdir -p /www; echo web > /www/index.html; trap 'exit 0' TERM; httpd -f -p \"$PORT\" -h /www & wait";
/// A worker that keeps running and stops on `SIGTERM`.
const QUEUE_COMMAND: &str = "trap 'exit 0' TERM; while true; do echo tick; sleep 1; done";
/// Shorter than the defaults, so the test spends seconds, not minutes.
const WORKER_STARTUP: Duration = Duration::from_secs(2);
const WORKER_STOP_GRACE: Duration = Duration::from_secs(3);
/// How long after a cancelled job ended a container Docker was still
/// creating would show up. Staging saw it about a second after the cancel.
const LATE_CONTAINER_WINDOW: Duration = Duration::from_secs(2);
/// After the job is requested, when it is cancelled: from before its
/// container exists to while Docker creates or starts it.
const CANCEL_DELAYS_MS: [u64; 6] = [0, 25, 50, 100, 200, 400];
/// The agent ID of `DockerRuntimeConfig::default()`.
const AGENT_ID: &str = "local-agent-01";

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

fn status_of(name: &str) -> Option<String> {
    let (exists, text) = docker(&["inspect", "--format", "{{.State.Status}}", name]);
    exists.then(|| text.trim().to_owned())
}

fn label_of(name: &str, label: &str) -> String {
    must(&[
        "inspect",
        "--format",
        &format!("{{{{index .Config.Labels \"{label}\"}}}}"),
        name,
    ])
    .trim()
    .to_owned()
}

/// Status line and body of one request through the router.
fn request() -> (String, String) {
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

fn containers_of(project_id: Uuid) -> Vec<String> {
    must(&[
        "ps",
        "-a",
        "--filter",
        &format!("label=dev.sakala.project-id={project_id}"),
        "--format",
        "{{.Names}}",
    ])
    .lines()
    .filter(|name| !name.is_empty())
    .map(str::to_owned)
    .collect()
}

fn remove_everything(project_id: Uuid, image: &str) {
    let (_, containers) = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{PREFIX}"),
        "--format",
        "{{.Names}}",
    ]);
    let mut names = containers
        .lines()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    names.extend(containers_of(project_id));
    for name in names {
        docker(&["rm", "-f", "-v", &name]);
    }
    docker(&["image", "rm", "-f", image]);
    docker(&["network", "rm", &project_network_name(project_id)]);
    docker(&["network", "rm", RUNTIME_NETWORK]);
}

struct Cleanup {
    project_id: Uuid,
    image: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        remove_everything(self.project_id, &self.image);
    }
}

#[derive(Default)]
struct RecordingReporter {
    events: Mutex<Vec<DeploymentEvent>>,
    logs: Mutex<Vec<DeploymentLog>>,
}

#[async_trait]
impl RuntimeReporter for RecordingReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        self.events.lock().expect("lock").push(event);
        Ok(())
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        self.logs.lock().expect("lock").push(log);
        Ok(())
    }
}

struct Node {
    executor: Arc<DockerRuntimeExecutor>,
    project_id: Uuid,
    image_deployment: Uuid,
    sites: std::path::PathBuf,
    _temp: TempDir,
}

impl Node {
    async fn deploy(
        &self,
        deployment_id: Uuid,
        processes: Value,
    ) -> Result<Value, RuntimeExecutionError> {
        let payload = serde_json::from_value(json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": COMMIT,
            "domain": DOMAIN,
            "container_port": PORT,
            "builder": "dockerfile",
            "start_command": WEB_COMMAND,
            "reuse_image_from_deployment_id": self.image_deployment,
            "processes": processes,
        }))
        .expect("deploy payload");
        self.executor
            .deploy_project(
                DeployProjectRequest {
                    command_id: Uuid::new_v4(),
                    project_id: self.project_id,
                    deployment_id,
                    payload,
                    repository_credential: None,
                    cancellation: CancellationToken::new(),
                },
                Arc::new(RecordingReporter::default()),
            )
            .await
            .map(|output| output.result)
    }

    fn lifecycle(&self, deployment_id: Uuid) -> WorkloadLifecycleRequest {
        WorkloadLifecycleRequest {
            command_id: Uuid::new_v4(),
            project_id: self.project_id,
            deployment_id,
            remove_images: false,
            sleeping_page: SleepingPage::Waking,
            cancellation: CancellationToken::new(),
        }
    }

    fn job(&self, deployment_id: Uuid, command: &str, timeout_seconds: u64) -> RunJobRequest {
        RunJobRequest {
            command_id: Uuid::new_v4(),
            project_id: self.project_id,
            deployment_id,
            payload: serde_json::from_value(json!({
                "command": command,
                "timeout_seconds": timeout_seconds,
                "commit_sha": COMMIT,
                "reuse_image_from_deployment_id": self.image_deployment,
                "container_port": PORT,
                "environment": { "JOB_GREETING": "hello" },
            }))
            .expect("job payload"),
            cancellation: CancellationToken::new(),
        }
    }

    fn web(&self, deployment_id: Uuid) -> String {
        container_name(self.project_id, deployment_id)
    }

    fn worker(&self, deployment_id: Uuid) -> String {
        worker_container_name(self.project_id, deployment_id, "queue", ReplicaSlot::SINGLE)
    }

    fn route(&self) -> String {
        std::fs::read_to_string(self.sites.join(format!("{}.Caddyfile", self.project_id)))
            .expect("route file")
    }

    fn jobs_left(&self) -> Vec<String> {
        containers_of(self.project_id)
            .into_iter()
            .filter(|name| name.starts_with("sakala-job-"))
            .collect()
    }

    /// A job container whose command is gone, in `created` state on the
    /// project's network, as staging found one on 2026-11-24: it inherits
    /// `dev.sakala.managed=true` from the image and has no domain label.
    fn leftover_job(&self, deployment_id: Uuid, agent_id: &str) -> String {
        let name = job_container_name(self.project_id, Uuid::new_v4());
        must(&[
            "create",
            "--name",
            &name,
            "--pull=never",
            "--network",
            &project_network_name(self.project_id),
            "--label",
            "dev.sakala.role=job",
            "--label",
            &format!("dev.sakala.project-id={}", self.project_id),
            "--label",
            &format!("dev.sakala.deployment-id={deployment_id}"),
            "--label",
            &format!("dev.sakala.command-id={}", Uuid::new_v4()),
            "--label",
            &format!("dev.sakala.agent-id={agent_id}"),
            "--entrypoint",
            "sh",
            &image_name(self.project_id, self.image_deployment, COMMIT),
            "-c",
            "sleep 60",
        ]);
        assert_eq!(status_of(&name).as_deref(), Some("created"));
        assert_eq!(label_of(&name, "dev.sakala.managed"), "true");
        name
    }
}

fn start_node() -> (Node, Cleanup) {
    let project_id = Uuid::new_v4();
    let image_deployment = Uuid::new_v4();
    let image = image_name(project_id, image_deployment, COMMIT);
    remove_everything(project_id, &image);
    let cleanup = Cleanup {
        project_id,
        image: image.clone(),
    };
    let temp = TempDir::new().expect("temp directory");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites directory");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");
    let workspace = temp.path().join("builds");
    std::fs::create_dir_all(&workspace).expect("workspace directory");

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
    // What a build of `image_deployment` leaves: the app image with the labels
    // the Agent writes, so every deployment reuses it without a build.
    let builder = format!("{PREFIX}image");
    must(&["create", "--name", &builder, "--pull=never", APP_IMAGE]);
    must(&[
        "commit",
        "--change",
        "LABEL dev.sakala.managed=true",
        "--change",
        &format!("LABEL dev.sakala.project-id={project_id}"),
        "--change",
        &format!("LABEL dev.sakala.deployment-id={image_deployment}"),
        "--change",
        "LABEL dev.sakala.builder=dockerfile",
        &builder,
        &image,
    ]);
    must(&["rm", &builder]);

    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            caddy_container: ROUTER.to_owned(),
            caddy_sites_dir: sites.clone(),
            workspace_root: workspace,
            public_api_url: "https://api.staging.sakala.dev".to_owned(),
            worker_startup_window: WORKER_STARTUP,
            worker_stop_grace: WORKER_STOP_GRACE,
            ..DockerRuntimeConfig::default()
        },
        Arc::new(TokioProcessRunner::new(Duration::from_secs(60))),
    ));
    (
        Node {
            executor,
            project_id,
            image_deployment,
            sites,
            _temp: temp,
        },
        cleanup,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and busybox:1.37"]
async fn real_docker_runs_workers_beside_the_web_process_and_jobs_from_the_live_image() {
    let (node, _cleanup) = start_node();
    let queue = json!([{ "name": "queue", "command": QUEUE_COMMAND }]);

    // A worker starts beside the web replica, with its labels and restart
    // policy, and only the web replica is routed.
    let first = Uuid::new_v4();
    node.deploy(first, queue.clone())
        .await
        .expect("deploy with a worker");
    assert_eq!(status_of(&node.web(first)).as_deref(), Some("running"));
    assert_eq!(status_of(&node.worker(first)).as_deref(), Some("running"));
    assert_eq!(
        label_of(&node.worker(first), "dev.sakala.workload-kind"),
        "worker"
    );
    assert_eq!(label_of(&node.worker(first), "dev.sakala.process"), "queue");
    assert_eq!(
        label_of(&node.web(first), "dev.sakala.workload-kind"),
        "web"
    );
    assert_eq!(
        must(&[
            "inspect",
            "--format",
            "{{.HostConfig.RestartPolicy.Name}}",
            &node.worker(first)
        ])
        .trim(),
        "unless-stopped"
    );
    assert!(!node.route().contains("sakala-worker"), "{}", node.route());
    let (status, body) = request();
    assert!(status.ends_with(" 200 OK"), "{status}");
    assert_eq!(body, "web");
    let metrics = node.executor.workload_metrics().await.expect("metrics");
    let ours = metrics
        .iter()
        .filter(|item| item.project_id == node.project_id)
        .collect::<Vec<_>>();
    assert_eq!(ours.len(), 2, "{ours:?}");
    assert!(
        ours.iter()
            .any(|item| item.process.as_deref() == Some("queue"))
    );
    assert!(ours.iter().any(|item| item.process.is_none()));
    println!("--- web and worker run, metrics name the worker");

    // A worker that dies before the route switches fails the deployment; the
    // previous version and its worker keep running and serving.
    let failing = Uuid::new_v4();
    let error = node
        .deploy(
            failing,
            json!([{ "name": "queue", "command": "echo boom; exit 3" }]),
        )
        .await
        .expect_err("the worker exits");
    assert_eq!(error.code(), "worker_start_failed");
    assert!(error.to_string().contains("exit code 3"), "{error}");
    assert_eq!(status_of(&node.web(failing)), None, "candidate removed");
    assert_eq!(status_of(&node.worker(failing)), None, "candidate removed");
    assert_eq!(status_of(&node.worker(first)).as_deref(), Some("running"));
    let (status, _) = request();
    assert!(status.ends_with(" 200 OK"), "{status}");
    println!("--- a failing worker kept the previous version live: {error}");

    // A worker that ends with code 0 on purpose, after the startup window, is
    // restarted by Docker and never reported as crashed. The redeploy stops the
    // previous worker with its own grace.
    let looping = Uuid::new_v4();
    let redeploy_started = Instant::now();
    node.deploy(
        looping,
        json!([{ "name": "queue", "command": "trap 'exit 0' TERM; sleep 3; exit 0" }]),
    )
    .await
    .expect("deploy a worker that ends on purpose");
    println!(
        "--- redeploy with a worker took {:?}",
        redeploy_started.elapsed()
    );
    assert_eq!(
        status_of(&node.worker(first)),
        None,
        "previous worker removed"
    );
    tokio::time::sleep(Duration::from_secs(12)).await;
    let restarts = must(&[
        "inspect",
        "--format",
        "{{.RestartCount}}",
        &node.worker(looping),
    ]);
    let restarts = restarts.trim().parse::<u32>().expect("restart count");
    assert!(restarts >= 2, "Docker restarted it {restarts} times");
    let snapshots = node.executor.health_snapshot().await.expect("snapshot");
    let crashed = node
        .executor
        .crashed_workloads(&snapshots)
        .await
        .expect("crash report");
    assert!(
        !crashed
            .iter()
            .any(|detail| detail.project_id == node.project_id),
        "{crashed:?}"
    );
    println!("--- a worker that ended with code 0 {restarts} times is not a crash");

    // Back to a steady worker for the lifecycle.
    let live = Uuid::new_v4();
    node.deploy(live, queue).await.expect("deploy");

    // Restart restarts both and routes only the web replica.
    node.executor
        .restart_project(node.lifecycle(live), Arc::new(RecordingReporter::default()))
        .await
        .expect("restart");
    assert_eq!(status_of(&node.worker(live)).as_deref(), Some("running"));
    assert!(!node.route().contains("sakala-worker"));

    // Sleep stops both and shows the sleeping page.
    node.executor
        .sleep_project(node.lifecycle(live), Arc::new(RecordingReporter::default()))
        .await
        .expect("sleep");
    assert_eq!(status_of(&node.web(live)).as_deref(), Some("exited"));
    assert_eq!(status_of(&node.worker(live)).as_deref(), Some("exited"));
    let (status, _) = request();
    assert!(status.ends_with(" 503 Service Unavailable"), "{status}");

    // A job runs while the project sleeps, from the live image, without waking it.
    let reporter = Arc::new(RecordingReporter::default());
    let output = node
        .executor
        .run_job(
            node.job(live, "echo \"job says $JOB_GREETING\"", 60),
            Arc::clone(&reporter) as Arc<dyn RuntimeReporter>,
        )
        .await
        .expect("job while asleep");
    assert_eq!(output.result["exit_code"], 0);
    assert!(
        reporter.logs.lock().expect("lock").iter().any(|log| {
            log.message == "[job] job says [REDACTED]" || log.message == "[job] job says hello"
        }),
        "{:?}",
        reporter.logs.lock().expect("lock")
    );
    assert!(node.jobs_left().is_empty(), "{:?}", node.jobs_left());
    assert_eq!(
        status_of(&node.web(live)).as_deref(),
        Some("exited"),
        "still asleep"
    );
    let (status, _) = request();
    assert!(status.ends_with(" 503 Service Unavailable"), "{status}");
    println!("--- a job ran while the project slept: {}", output.result);

    // Wake starts both again.
    node.executor
        .wake_project(node.lifecycle(live), Arc::new(RecordingReporter::default()))
        .await
        .expect("wake");
    assert_eq!(status_of(&node.web(live)).as_deref(), Some("running"));
    assert_eq!(status_of(&node.worker(live)).as_deref(), Some("running"));

    // A failing job, one past its timeout, and a cancelled one each end with
    // their own code and leave no container.
    let error = node
        .executor
        .run_job(
            node.job(live, "exit 4", 60),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("non-zero");
    assert_eq!(error.code(), "job_failed");
    assert!(error.to_string().contains("exit code 4"), "{error}");
    let timed = Instant::now();
    let error = node
        .executor
        .run_job(
            node.job(live, "sleep 60", 2),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("timeout");
    assert_eq!(error.code(), "job_timeout");
    assert!(
        timed.elapsed() < Duration::from_secs(20),
        "{:?}",
        timed.elapsed()
    );
    let cancelled = node.job(live, "sleep 60", 60);
    let token = cancelled.cancellation.clone();
    let executor = Arc::clone(&node.executor);
    let running = tokio::spawn(async move {
        executor
            .run_job(cancelled, Arc::new(RecordingReporter::default()))
            .await
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(node.jobs_left().len(), 1, "the job is running");
    token.cancel();
    let error = running.await.expect("task").expect_err("cancelled");
    assert_eq!(error.code(), "runtime_cancelled");
    assert!(node.jobs_left().is_empty(), "{:?}", node.jobs_left());
    println!("--- failed, timed out, and cancelled jobs left nothing behind");

    // A job cancelled right after it was requested, while Docker may still
    // be creating its container, leaves nothing behind, also once Docker
    // would have finished creating it.
    for delay_ms in CANCEL_DELAYS_MS {
        let job = node.job(live, "sleep 60", 60);
        let token = job.cancellation.clone();
        let executor = Arc::clone(&node.executor);
        let running = tokio::spawn(async move {
            executor
                .run_job(job, Arc::new(RecordingReporter::default()))
                .await
        });
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        token.cancel();
        let error = running.await.expect("task").expect_err("cancelled");
        assert_eq!(error.code(), "runtime_cancelled", "{delay_ms} ms: {error}");
        tokio::time::sleep(LATE_CONTAINER_WINDOW).await;
        assert!(
            node.jobs_left().is_empty(),
            "cancelled after {delay_ms} ms: {:?}",
            node.jobs_left()
        );
    }
    // And cancelled the moment its container first shows up.
    let job = node.job(live, "sleep 60", 60);
    let name = job_container_name(node.project_id, job.command_id);
    let token = job.cancellation.clone();
    let executor = Arc::clone(&node.executor);
    let running = tokio::spawn(async move {
        executor
            .run_job(job, Arc::new(RecordingReporter::default()))
            .await
    });
    let appeared = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = status_of(&name) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the job container appears");
    token.cancel();
    let error = running.await.expect("task").expect_err("cancelled");
    assert_eq!(error.code(), "runtime_cancelled");
    tokio::time::sleep(LATE_CONTAINER_WINDOW).await;
    assert!(node.jobs_left().is_empty(), "{:?}", node.jobs_left());
    println!(
        "--- jobs cancelled after {CANCEL_DELAYS_MS:?} ms and once its container was {appeared} left nothing behind"
    );

    // The periodic reconcile removes a job container of this Agent whose
    // command is gone, and lifecycle commands never mistake it for a replica.
    let leftover = node.leftover_job(live, AGENT_ID);
    node.executor
        .health_check(node.lifecycle(live), Arc::new(RecordingReporter::default()))
        .await
        .expect("health check beside a leftover job container");
    node.executor
        .reconcile_networks(false)
        .await
        .expect("periodic reconcile");
    assert_eq!(status_of(&leftover), None, "the sweep removed it");
    println!("--- the periodic reconcile removed a leftover job container");

    // Stop removes web and workers, and a leftover job container of any
    // Agent, so the project network and the route go too.
    let leftover = node.leftover_job(live, "another-agent");
    node.executor
        .stop_project(node.lifecycle(live), Arc::new(RecordingReporter::default()))
        .await
        .expect("stop with a leftover job container");
    assert!(
        containers_of(node.project_id).is_empty(),
        "{:?}",
        containers_of(node.project_id)
    );
    assert_eq!(status_of(&leftover), None);
    let (network_left, _) = docker(&["network", "inspect", &project_network_name(node.project_id)]);
    assert!(!network_left, "the project network is removed");
    assert!(
        !node
            .sites
            .join(format!("{}.Caddyfile", node.project_id))
            .exists(),
        "the route is removed"
    );
    println!("--- stop removed a leftover job container, the network, and the route");
}
