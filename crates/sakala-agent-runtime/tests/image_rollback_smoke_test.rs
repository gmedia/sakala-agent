//! Proves image retention for rollback and the restart of a crashed project
//! against a real Docker daemon, a real BuildKit, and a real Caddy: after
//! deployments A, B, and C only C (live) and B (previous) stay; a rollback to B
//! starts from B's image without a build and leaves B live and C previous; a
//! rollback to A, whose image is gone, builds again; a project whose
//! containers all exited restarts, and a sleeping one is refused.
//!
//! Only the Git checkout is replaced: the repository is a Dockerfile written
//! into the workspace. Opt-in, because it needs a daemon, `caddy:2.11.3-alpine`
//! locally, and a BuildKit that can pull `busybox:1.37`:
//!
//! ```bash
//! BUILDX_BUILDER=<builder> cargo test -p sakala-agent-runtime \
//!     --test image_rollback_smoke_test -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `BUILDX_BUILDER` is optional. Where BuildKit on Docker's default bridge
//! stalls, use a builder on the host network:
//! `docker buildx create --name sakala-host --driver docker-container --driver-opt network=host`.
//!
//! The projects get fresh UUIDs, so every container, network, and image the
//! agent creates is named after them, and only those are removed, plus the
//! router and runtime network starting with `sakala-rollbacksm-`.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
    WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEvent, DeploymentLog, IMAGE_REUSE_SKIPPED_EVENT,
    IMAGE_REUSED_EVENT, SleepingPage,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, RuntimeError, TokioProcessRunner,
    containers::{image_name, previous_image_reference, project_network_name},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ROUTER: &str = "sakala-rollbacksm-caddy";
const RUNTIME_NETWORK: &str = "sakala-rollbacksm-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPOSITORY: &str = "https://github.com/gmedia/example-app.git";
const PORT: u16 = 3000;
/// The app serves the value of `GREETING` it was started with, and exits on
/// `SIGTERM`: as PID 1, `httpd` ignores it, and every `docker stop` of the
/// previous deployment would wait out the whole grace period.
const DOCKERFILE: &str = "FROM busybox:1.37
CMD [\"sh\", \"-c\", \"mkdir -p /www; printf '%s' \\\"$GREETING\\\" > /www/index.html; trap 'exit 0' TERM; httpd -f -p \\\"$PORT\\\" -h /www & wait\"]
";
const BUILD_STARTED_EVENT: &str = "deployment.build.started";
const CHECKOUT_STARTED_EVENT: &str = "deployment.checkout.started";
const ROUTE_STARTED_EVENT: &str = "deployment.route.started";

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

/// The tests share one router, so they must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    (output.status.success(), text.trim().to_owned())
}

fn must(args: &[&str]) -> String {
    let (ok, text) = docker(args);
    assert!(ok, "docker {args:?} failed: {text}");
    text
}

fn image_exists(reference: &str) -> bool {
    docker(&["image", "inspect", "--format", "{{.Id}}", reference]).0
}

fn image_id(reference: &str) -> String {
    must(&["image", "inspect", "--format", "{{.Id}}", reference])
}

/// The body the router serves for `domain`.
fn serve(domain: &str) -> String {
    must(&[
        "exec",
        ROUTER,
        "wget",
        "-q",
        "-O-",
        "-T",
        "10",
        "--header",
        &format!("Host: {domain}"),
        "http://127.0.0.1/",
    ])
}

/// Real processes, except Git: `git init` writes the repository, and the other
/// Git commands succeed without doing anything. Builds use `BUILDX_BUILDER`.
struct SmokeRunner {
    real: TokioProcessRunner,
    builder: Option<String>,
}

#[async_trait]
impl ProcessRunner for SmokeRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        if spec.program == "git" {
            if spec.args.iter().any(|argument| argument == "init") {
                let source = std::path::Path::new(spec.args.last().expect("git init target"));
                std::fs::create_dir_all(source).expect("source");
                std::fs::write(source.join("Dockerfile"), DOCKERFILE).expect("Dockerfile");
            }
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                ..ProcessOutput::default()
            });
        }
        let mut spec = spec.clone();
        if let Some(builder) = &self.builder
            && spec.args.iter().any(|argument| argument == "buildx")
        {
            spec = spec.env("BUILDX_BUILDER", builder.clone());
        }
        self.real.run(&spec, sink).await
    }
}

#[derive(Default)]
struct RecordingReporter {
    events: Mutex<Vec<DeploymentEvent>>,
}

#[async_trait]
impl RuntimeReporter for RecordingReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        self.events.lock().expect("event lock").push(event);
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

struct Deployed {
    id: Uuid,
    /// The whole command, cleanup of the previous deployment included.
    elapsed: Duration,
    /// Until the route moved to the new version.
    serving_after: Duration,
    events: Vec<DeploymentEvent>,
}

impl Deployed {
    fn types(&self) -> Vec<&str> {
        self.events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect()
    }

    fn metadata(&self, event_type: &str) -> Value {
        self.events
            .iter()
            .find(|event| event.event_type == event_type)
            .map(|event| event.metadata.clone())
            .unwrap_or_else(|| panic!("{event_type} missing from {:?}", self.types()))
    }

    fn reported(&self, event_type: &str) -> bool {
        self.types().contains(&event_type)
    }
}

struct Harness {
    executor: DockerRuntimeExecutor,
    projects: Mutex<Vec<Uuid>>,
    _temp: TempDir,
}

impl Harness {
    fn start() -> Self {
        remove_router();
        let temp = TempDir::new().expect("temp directory");
        let sites = temp.path().join("sites");
        std::fs::create_dir_all(&sites).expect("sites directory");
        std::fs::create_dir_all(temp.path().join("builds")).expect("workspace");
        let caddyfile = temp.path().join("Caddyfile");
        std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("router Caddyfile");
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
        let builder = std::env::var("BUILDX_BUILDER")
            .ok()
            .filter(|builder| !builder.is_empty());
        println!(
            "buildx builder: {}",
            builder.as_deref().unwrap_or("default")
        );
        let executor = DockerRuntimeExecutor::with_runner(
            DockerRuntimeConfig {
                agent_id: "sakala-rollbacksm-agent".to_owned(),
                workspace_root: temp.path().join("builds"),
                caddy_container: ROUTER.to_owned(),
                caddy_sites_dir: sites,
                runtime_network: RUNTIME_NETWORK.to_owned(),
                min_workspace_free_bytes: 0,
                ..DockerRuntimeConfig::default()
            },
            Arc::new(SmokeRunner {
                real: TokioProcessRunner::new(Duration::from_secs(600)),
                builder,
            }),
        );
        Self {
            executor,
            projects: Mutex::new(Vec::new()),
            _temp: temp,
        }
    }

    fn project(&self) -> Uuid {
        let project = Uuid::new_v4();
        self.projects.lock().expect("project lock").push(project);
        project
    }

    async fn deploy(&self, project_id: Uuid, greeting: &str, reuse: Option<Uuid>) -> Deployed {
        let deployment_id = Uuid::new_v4();
        let mut payload = json!({
            "repository_url": REPOSITORY,
            "commit_sha": COMMIT,
            "domain": domain(project_id),
            "container_port": PORT,
            "environment": { "GREETING": greeting },
            "timeouts": {
                "build_timeout_seconds": 300,
                "start_timeout_seconds": 30,
                "command_timeout_seconds": 600
            }
        });
        if let Some(source) = reuse {
            payload["reuse_image_from_deployment_id"] = json!(source);
        }
        let payload: DeployProjectPayload = serde_json::from_value(payload).expect("payload");
        let reporter = Arc::new(RecordingReporter::default());
        let started = Instant::now();
        let started_at = OffsetDateTime::now_utc();
        let result = self
            .executor
            .deploy_project(
                DeployProjectRequest {
                    command_id: Uuid::new_v4(),
                    project_id,
                    deployment_id,
                    payload,
                    repository_credential: None,
                    cancellation: CancellationToken::new(),
                },
                Arc::clone(&reporter) as Arc<dyn RuntimeReporter>,
            )
            .await;
        let elapsed = started.elapsed();
        let events = reporter.events.lock().expect("event lock").clone();
        if let Err(error) = result {
            panic!(
                "deployment failed: {error}; events {:?}",
                events
                    .iter()
                    .map(|event| event.event_type.as_str())
                    .collect::<Vec<_>>()
            );
        }
        let serving_after = events
            .iter()
            .find(|event| event.event_type == ROUTE_STARTED_EVENT)
            .and_then(|event| Duration::try_from(event.occurred_at - started_at).ok())
            .expect("the route moved");
        Deployed {
            id: deployment_id,
            elapsed,
            serving_after,
            events,
        }
    }

    /// The image IDs the project's containers run.
    fn running_images(&self, project_id: Uuid) -> Vec<String> {
        let containers = must(&[
            "ps",
            "-q",
            "--filter",
            &format!("label=dev.sakala.project-id={project_id}"),
        ]);
        containers
            .lines()
            .filter(|id| !id.is_empty())
            .map(|id| must(&["inspect", "--format", "{{.Image}}", id]))
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // First, because Docker keeps a network the router is still connected to.
        docker(&["rm", "-f", "-v", ROUTER]);
        for project in self.projects.lock().expect("project lock").iter() {
            remove_project(*project);
        }
        remove_router();
    }
}

fn domain(project_id: Uuid) -> String {
    format!(
        "p{}.run.sakala.localhost",
        &project_id.simple().to_string()[..12]
    )
}

fn remove_router() {
    docker(&["rm", "-f", "-v", ROUTER]);
    docker(&["network", "rm", RUNTIME_NETWORK]);
}

/// Every container, network, and image named after `project_id`.
fn remove_project(project_id: Uuid) {
    let (_, containers) = docker(&[
        "ps",
        "-aq",
        "--filter",
        &format!("label=dev.sakala.project-id={project_id}"),
    ]);
    for id in containers.lines().filter(|id| !id.is_empty()) {
        docker(&["rm", "-f", "-v", id]);
    }
    docker(&["network", "rm", &project_network_name(project_id)]);
    let repository = format!("sakala/project-{project_id}");
    let (_, images) = docker(&[
        "image",
        "ls",
        "--format",
        "{{.Repository}}:{{.Tag}}",
        &repository,
    ]);
    for reference in images.lines().filter(|line| line.starts_with(&repository)) {
        docker(&["image", "rm", "-f", reference]);
    }
}

/// The image IDs of the project, each with its tags, sorted by ID.
fn project_images(project_id: Uuid) -> Vec<(String, Vec<String>)> {
    let repository = format!("sakala/project-{project_id}");
    let listing = must(&[
        "image",
        "ls",
        "--no-trunc",
        "--format",
        "{{.ID}}\t{{.Tag}}",
        &repository,
    ]);
    let mut images: Vec<(String, Vec<String>)> = Vec::new();
    for line in listing.lines().filter(|line| !line.is_empty()) {
        let (id, tag) = line.split_once('\t').expect("id and tag");
        match images.iter_mut().find(|(known, _)| known == id) {
            Some((_, tags)) => tags.push(tag.to_owned()),
            None => images.push((id.to_owned(), vec![tag.to_owned()])),
        }
    }
    for (_, tags) in &mut images {
        tags.sort();
    }
    images.sort();
    images
}

/// The image ID the project's `previous` tag points at, if any.
fn previous_image(project_id: Uuid) -> Option<String> {
    let (found, id) = docker(&[
        "image",
        "inspect",
        "--format",
        "{{.Id}}",
        &previous_image_reference(project_id),
    ]);
    found.then_some(id)
}

/// One entry of [`project_images`]: `id` with `tags`, sorted the same way.
fn listed(id: &str, mut tags: Vec<String>) -> (String, Vec<String>) {
    tags.sort();
    (id.to_owned(), tags)
}

fn tag_of(project_id: Uuid, deployment: &Deployed) -> String {
    image_name(project_id, deployment.id, COMMIT)
        .rsplit_once(':')
        .expect("tag")
        .1
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and a BuildKit that can pull busybox:1.37"]
async fn the_node_keeps_the_live_and_previous_image_and_a_rollback_needs_no_build() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::start();
    let project = harness.project();

    let a = harness.deploy(project, "a", None).await;
    let b = harness.deploy(project, "b", None).await;
    let c = harness.deploy(project, "c", None).await;
    for deployed in [&a, &b, &c] {
        assert!(
            deployed.reported(BUILD_STARTED_EVENT),
            "{:?}",
            deployed.types()
        );
    }
    let image_a = image_name(project, a.id, COMMIT);
    let image_b = image_name(project, b.id, COMMIT);
    let image_c = image_name(project, c.id, COMMIT);
    let (id_b, id_c) = (image_id(&image_b), image_id(&image_c));
    println!("after A, B, C: {:?}", project_images(project));
    assert!(!image_exists(&image_a), "A was pruned after C went live");
    assert_eq!(harness.running_images(project), std::slice::from_ref(&id_c));
    assert_eq!(previous_image(project), Some(id_b.clone()));
    let mut expected = vec![
        listed(&id_b, vec!["previous".to_owned(), tag_of(project, &b)]),
        listed(&id_c, vec![tag_of(project, &c)]),
    ];
    expected.sort();
    assert_eq!(project_images(project), expected);
    assert_eq!(serve(&domain(project)), "c");

    // Roll back to B: its commit and the deployment that built its image.
    let rollback = harness.deploy(project, "rolled back", Some(b.id)).await;
    println!("event order of the rollback: {:?}", rollback.types());
    assert!(
        rollback.reported(IMAGE_REUSED_EVENT),
        "{:?}",
        rollback.types()
    );
    for skipped in [CHECKOUT_STARTED_EVENT, BUILD_STARTED_EVENT] {
        assert!(!rollback.reported(skipped), "{:?}", rollback.types());
    }
    assert_eq!(
        rollback.metadata(IMAGE_REUSED_EVENT)["image"],
        json!(image_b)
    );
    assert_eq!(serve(&domain(project)), "rolled back");
    assert_eq!(harness.running_images(project), std::slice::from_ref(&id_b));
    assert_eq!(previous_image(project), Some(id_c.clone()));
    let mut expected = vec![
        listed(&id_b, vec![tag_of(project, &b)]),
        listed(&id_c, vec!["previous".to_owned(), tag_of(project, &c)]),
    ];
    expected.sort();
    assert_eq!(project_images(project), expected);

    // Periodic image GC follows the same rule and finds nothing more.
    let cleanup = harness
        .executor
        .collect_stale_images()
        .await
        .expect("image GC");
    assert_eq!(cleanup.removed_images, 0);
    assert_eq!(project_images(project), expected);

    // Roll back to A, whose image is gone: the deployment builds it again.
    let rebuilt = harness.deploy(project, "a again", Some(a.id)).await;
    assert_eq!(
        rebuilt.metadata(IMAGE_REUSE_SKIPPED_EVENT),
        json!({ "source_deployment_id": a.id, "reason": "image_missing" })
    );
    assert!(rebuilt.reported(BUILD_STARTED_EVENT));
    assert_eq!(serve(&domain(project)), "a again");
    let id_rebuilt = image_id(&image_name(project, rebuilt.id, COMMIT));
    assert_eq!(
        harness.running_images(project),
        std::slice::from_ref(&id_rebuilt)
    );
    // B was live, so B is previous now, and C goes.
    assert_eq!(previous_image(project), Some(id_b.clone()));
    assert!(!image_exists(&image_c));
    assert_eq!(project_images(project).len(), 2);
    println!(
        "rollback (reuse): {:.1} s, route after {:.1} s; rollback (rebuild): {:.1} s, route after {:.1} s",
        rollback.elapsed.as_secs_f64(),
        rollback.serving_after.as_secs_f64(),
        rebuilt.elapsed.as_secs_f64(),
        rebuilt.serving_after.as_secs_f64()
    );
}

fn lifecycle(
    project_id: Uuid,
    deployment_id: Uuid,
    sleeping_page: SleepingPage,
) -> WorkloadLifecycleRequest {
    WorkloadLifecycleRequest {
        custom_domains: Default::default(),
        command_id: Uuid::new_v4(),
        project_id,
        deployment_id,
        remove_images: false,
        sleeping_page,
        cancellation: CancellationToken::new(),
    }
}

fn container_states(project_id: Uuid) -> Vec<String> {
    must(&[
        "ps",
        "-a",
        "--filter",
        &format!("label=dev.sakala.project-id={project_id}"),
        "--format",
        "{{.State}}",
    ])
    .lines()
    .map(str::to_owned)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and a BuildKit that can pull busybox:1.37"]
async fn restart_brings_back_an_exited_project_but_leaves_a_sleeping_one_asleep() {
    let _serial = SERIAL.lock().await;
    let harness = Harness::start();
    let project = harness.project();
    let deployed = harness.deploy(project, "up again", None).await;

    // Every container exited, as after a crash Docker no longer restarts.
    let containers = must(&[
        "ps",
        "-q",
        "--filter",
        &format!("label=dev.sakala.project-id={project}"),
    ]);
    for id in containers.lines() {
        must(&["stop", "--time", "1", id]);
    }
    assert_eq!(container_states(project), ["exited"]);

    let restarted = harness
        .executor
        .restart_project(
            lifecycle(project, deployed.id, SleepingPage::Waking),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect("an exited project restarts");
    assert_eq!(restarted.result["status"], "ready");
    assert_eq!(container_states(project), ["running"]);
    assert_eq!(serve(&domain(project)), "up again");

    harness
        .executor
        .sleep_project(
            lifecycle(project, deployed.id, SleepingPage::Unavailable),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect("sleep");
    assert_eq!(container_states(project), ["exited"]);
    let refused = harness
        .executor
        .restart_project(
            lifecycle(project, deployed.id, SleepingPage::Waking),
            Arc::new(RecordingReporter::default()),
        )
        .await
        .expect_err("a sleeping project is not restarted");
    assert_eq!(refused.code(), "runtime_workload_not_running");
    assert_eq!(container_states(project), ["exited"]);
}
