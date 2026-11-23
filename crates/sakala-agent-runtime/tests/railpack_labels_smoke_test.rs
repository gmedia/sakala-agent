//! Proves that a Railpack image carries the Sakala labels, against a real
//! Docker daemon, a real Railpack sandbox and frontend, and a real Caddy:
//! Railpack's frontend ignores `--label`, so the Agent writes them with a
//! commit after the build. The labelled image keeps the config Railpack wrote,
//! the unlabelled name is gone, a second deployment marks the first image as
//! the previous one, and a rollback to it starts without a build. Image GC
//! collects an unlabelled image named only in a deleted project's repository,
//! as earlier Agents left Railpack images, and keeps one that has another name.
//!
//! Only the Git checkout is replaced: the repository is a static site written
//! into the workspace, which Railpack serves with Caddy. Opt-in, because it
//! needs a daemon, `caddy:2.11.3-alpine` locally, a Railpack sandbox image
//! (`docker/railpack-sandbox`) pinned by ID, and a BuildKit that can pull
//! Railpack's images and download Caddy:
//!
//! ```bash
//! SAKALA_RAILPACK_IMAGE=sha256:<sandbox image ID> BUILDX_BUILDER=<builder> \
//!     cargo test -p sakala-agent-runtime --test railpack_labels_smoke_test \
//!     -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `BUILDX_BUILDER` is optional, as in `image_reuse_smoke_test`. The project
//! gets a fresh UUID, so every container, network, and image the agent creates
//! is named after it, and only those are removed, plus the router and runtime
//! network starting with `sakala-rplabelsm-`. Image GC runs node-wide, as in
//! `image_rollback_smoke_test`, so it also removes other Sakala images older
//! than the default 24-hour grace whose project has no container.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
};
use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEvent, DeploymentLog, IMAGE_REUSED_EVENT,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, RuntimeError, TokioProcessRunner,
    containers::{image_name, previous_image_reference, project_network_name},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ROUTER: &str = "sakala-rplabelsm-caddy";
const RUNTIME_NETWORK: &str = "sakala-rplabelsm-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPOSITORY: &str = "https://github.com/gmedia/example-app.git";
const PORT: u16 = 3000;
const PAGE: &str = "railpack labels smoke";
const BUILD_STARTED_EVENT: &str = "deployment.build.started";

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

fn image_json(reference: &str, field: &str) -> Value {
    serde_json::from_str(&must(&[
        "image",
        "inspect",
        "--format",
        &format!("{{{{json {field}}}}}"),
        reference,
    ]))
    .expect("JSON")
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

/// Real processes, except Git: `git init` writes the static site, and the
/// other Git commands succeed without doing anything. Records the config of
/// every image a label container is created from, before the commit.
struct SmokeRunner {
    real: TokioProcessRunner,
    builder: Option<String>,
    unlabelled_configs: Mutex<Vec<Value>>,
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
                std::fs::write(
                    source.join("index.html"),
                    format!("<!doctype html><title>smoke</title><p>{PAGE}</p>\n"),
                )
                .expect("index.html");
                // The sandbox reads the checkout as an unprivileged user, as it
                // reads a clone made with the usual umask.
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o755))
                    .expect("source permissions");
                std::fs::set_permissions(
                    source.join("index.html"),
                    std::fs::Permissions::from_mode(0o644),
                )
                .expect("index.html permissions");
            }
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                ..ProcessOutput::default()
            });
        }
        if spec
            .args
            .first()
            .is_some_and(|argument| argument == "container")
            && spec
                .args
                .get(1)
                .is_some_and(|argument| argument == "create")
        {
            let image = spec.args.last().expect("image").to_string_lossy();
            self.unlabelled_configs
                .lock()
                .expect("config lock")
                .push(image_json(&image, ".Config"));
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

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        let message = &log.message;
        if message.contains("[railpack-label]") || message.to_lowercase().contains("error") {
            println!("log: {message}");
        }
        Ok(())
    }
}

struct Deployed {
    id: Uuid,
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
}

struct Harness {
    executor: DockerRuntimeExecutor,
    runner: Arc<SmokeRunner>,
    project: Uuid,
    _temp: TempDir,
}

impl Harness {
    fn start(sandbox: String) -> Self {
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
        let runner = Arc::new(SmokeRunner {
            real: TokioProcessRunner::new(Duration::from_secs(900)),
            builder,
            unlabelled_configs: Mutex::new(Vec::new()),
        });
        let executor = DockerRuntimeExecutor::with_runner(
            DockerRuntimeConfig {
                agent_id: "sakala-rplabelsm-agent".to_owned(),
                workspace_root: temp.path().join("builds"),
                caddy_container: ROUTER.to_owned(),
                caddy_sites_dir: sites,
                runtime_network: RUNTIME_NETWORK.to_owned(),
                railpack_image: sandbox,
                min_workspace_free_bytes: 0,
                ..DockerRuntimeConfig::default()
            },
            runner.clone(),
        );
        Self {
            executor,
            runner,
            project: Uuid::new_v4(),
            _temp: temp,
        }
    }

    async fn deploy(&self, reuse: Option<Uuid>) -> Deployed {
        let deployment_id = Uuid::new_v4();
        let mut payload = json!({
            "repository_url": REPOSITORY,
            "commit_sha": COMMIT,
            "builder": "railpack",
            "domain": domain(self.project),
            "container_port": PORT,
            "timeouts": {
                "build_timeout_seconds": 600,
                "start_timeout_seconds": 60,
                "command_timeout_seconds": 900
            }
        });
        if let Some(source) = reuse {
            payload["reuse_image_from_deployment_id"] = json!(source);
        }
        let payload: DeployProjectPayload = serde_json::from_value(payload).expect("payload");
        let reporter = Arc::new(RecordingReporter::default());
        let result = self
            .executor
            .deploy_project(
                DeployProjectRequest {
                    command_id: Uuid::new_v4(),
                    project_id: self.project,
                    deployment_id,
                    payload,
                    repository_credential: None,
                    cancellation: CancellationToken::new(),
                },
                Arc::clone(&reporter) as Arc<dyn RuntimeReporter>,
            )
            .await;
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
        Deployed {
            id: deployment_id,
            events,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // First, because Docker keeps a network the router is still connected to.
        docker(&["rm", "-f", "-v", ROUTER]);
        remove_project(self.project);
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

fn without_labels(mut config: Value) -> Value {
    config
        .as_object_mut()
        .expect("config object")
        .remove("Labels");
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, SAKALA_RAILPACK_IMAGE, and a BuildKit that can pull Railpack's images"]
async fn a_railpack_image_carries_its_labels_and_can_be_rolled_back_to_without_a_build() {
    let sandbox = std::env::var("SAKALA_RAILPACK_IMAGE")
        .expect("SAKALA_RAILPACK_IMAGE names the sandbox image by ID or digest");
    let harness = Harness::start(sandbox);
    let project = harness.project;

    let first = harness.deploy(None).await;
    assert!(
        first.types().contains(&BUILD_STARTED_EVENT),
        "{:?}",
        first.types()
    );
    assert!(serve(&domain(project)).contains(PAGE));
    let first_image = image_name(project, first.id, COMMIT);
    let labels = image_json(&first_image, ".Config.Labels");
    println!("labels of the Railpack image: {labels}");
    assert_eq!(
        labels,
        json!({
            "dev.sakala.managed": "true",
            "dev.sakala.project-id": project,
            "dev.sakala.deployment-id": first.id,
            "dev.sakala.builder": "railpack",
        })
    );
    assert!(
        !image_exists(&format!("{first_image}-unlabelled")),
        "the unlabelled name is removed"
    );
    let before = harness
        .runner
        .unlabelled_configs
        .lock()
        .expect("config lock")[0]
        .clone();
    println!("config Railpack wrote: {before}");
    assert_eq!(
        before.get("Labels").cloned().unwrap_or(Value::Null),
        Value::Null,
        "Railpack's own image has no labels"
    );
    assert_eq!(
        without_labels(image_json(&first_image, ".Config")),
        without_labels(before),
        "the commit changes nothing but the labels"
    );

    let second = harness.deploy(None).await;
    assert!(serve(&domain(project)).contains(PAGE));
    let second_image = image_name(project, second.id, COMMIT);
    assert_eq!(
        image_json(&second_image, ".Config.Labels")["dev.sakala.deployment-id"],
        json!(second.id)
    );
    assert_eq!(
        image_id(&previous_image_reference(project)),
        image_id(&first_image),
        "the replaced Railpack image becomes the previous one"
    );

    let rollback = harness.deploy(Some(first.id)).await;
    println!("event order of the rollback: {:?}", rollback.types());
    assert!(
        !rollback.types().contains(&BUILD_STARTED_EVENT),
        "{:?}",
        rollback.types()
    );
    assert_eq!(
        rollback.metadata(IMAGE_REUSED_EVENT),
        json!({
            "image": first_image,
            "source_deployment_id": first.id,
            "builder": "railpack",
        })
    );
    assert!(serve(&domain(project)).contains(PAGE));

    // Images an earlier Agent left: no labels, named in the repository of a
    // project that is gone. Image GC collects the one whose every name is in
    // that repository, and leaves the one that also has a name elsewhere.
    let deleted = Uuid::new_v4();
    let legacy = format!("sakala/project-{deleted}:legacy");
    let mixed = format!("sakala/project-{deleted}:mixed");
    build_old_unlabelled_image("LEGACY", &[&legacy]);
    build_old_unlabelled_image("MIXED", &[&mixed, FOREIGN_IMAGE]);
    let cleanup = harness
        .executor
        .collect_stale_images()
        .await
        .expect("image GC");
    println!("image GC removed {} image(s)", cleanup.removed_images);
    let survived = (
        image_exists(&legacy),
        image_exists(&mixed),
        image_exists(FOREIGN_IMAGE),
    );
    docker(&["image", "rm", &mixed, FOREIGN_IMAGE]);
    docker(&["image", "rm", &legacy]);
    assert_eq!(
        survived,
        (false, true, true),
        "(legacy, mixed, foreign) after image GC"
    );
    assert!(cleanup.removed_images >= 1);
    assert!(image_exists(&first_image), "the live image stays");
    assert!(image_exists(&second_image), "the previous image stays");
}

/// A second name outside every project repository.
const FOREIGN_IMAGE: &str = "sakala-rplabelsm/foreign:keep";

/// An image without any label whose creation date is long past the orphan
/// grace period, as Railpack images of an earlier Agent are by now.
fn build_old_unlabelled_image(marker: &str, names: &[&str]) {
    let context = TempDir::new().expect("context");
    std::fs::write(
        context.path().join("Dockerfile"),
        format!("FROM busybox:1.37\nENV SAKALA_RPLABELSM_{marker}=1\n"),
    )
    .expect("Dockerfile");
    let mut args = vec![
        "buildx".to_owned(),
        "build".to_owned(),
        "--load".to_owned(),
        "--quiet".to_owned(),
        "--build-arg".to_owned(),
        "SOURCE_DATE_EPOCH=1700000000".to_owned(),
    ];
    for name in names {
        args.extend(["--tag".to_owned(), (*name).to_owned()]);
    }
    args.push(context.path().display().to_string());
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    must(&args);
    assert_eq!(image_json(names[0], ".Config.Labels"), Value::Null);
}
