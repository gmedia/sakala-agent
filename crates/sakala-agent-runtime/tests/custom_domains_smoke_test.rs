//! Proves custom domains (ADR-024) against a real Docker daemon, a real
//! BuildKit, and a real router Caddy: a deployment with a custom domain is
//! served under it with the visitor's `Host`, its visits count as the
//! project's traffic, sleep and wake keep the domain, `UpdateProjectDomains`
//! moves it without a redeploy, another project cannot take it, and stopping
//! the project removes it.
//!
//! Only the Git checkout is replaced: the repository is a Dockerfile written
//! into the workspace. Opt-in, because it needs a daemon, `caddy:2.11.3-alpine`
//! locally, and a BuildKit that can pull `busybox:1.37`:
//!
//! ```bash
//! BUILDX_BUILDER=<builder> cargo test -p sakala-agent-runtime \
//!     --test custom_domains_smoke_test -- --ignored --nocapture
//! ```
//!
//! The projects get fresh UUIDs, so every container, network, and image the
//! agent creates is named after them, and only those are removed, plus the
//! router and runtime network starting with `sakala-cdsm-`.

use std::{process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    DeployProjectRequest, RuntimeExecutionError, RuntimeExecutor, RuntimeReporter,
    UpdateProjectDomainsRequest, WorkloadLifecycleRequest,
};
use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEvent, DeploymentLog, RequestedCustomDomains, SleepingPage,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, RuntimeError, TokioProcessRunner, containers::project_network_name,
};
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const ROUTER: &str = "sakala-cdsm-caddy";
const RUNTIME_NETWORK: &str = "sakala-cdsm-runtime";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPOSITORY: &str = "https://github.com/gmedia/example-app.git";
const PORT: u16 = 3000;
const CUSTOM: &str = "www.contoh-sakala.com";
const MOVED: &str = "contoh-sakala.net";
/// The app answers `/cgi-bin/host` with the `Host` it received, and exits on
/// `SIGTERM` so a stop does not wait out the grace period.
const DOCKERFILE: &str = r#"FROM busybox:1.37
RUN mkdir -p /www/cgi-bin \
 && echo ok > /www/index.html \
 && echo '#!/bin/sh' > /www/cgi-bin/host \
 && echo 'printf "Content-Type: text/plain\r\n\r\nhost=%s" "$HTTP_HOST"' >> /www/cgi-bin/host \
 && chmod +x /www/cgi-bin/host
CMD ["sh", "-c", "trap 'exit 0' TERM; httpd -f -p \"$PORT\" -h /www & wait"]
"#;

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

/// A raw HTTP/1.0 request from inside the router, so every status comes back
/// with its body.
fn request(host: &str, path: &str) -> String {
    must(&[
        "exec",
        ROUTER,
        "sh",
        "-c",
        &format!("printf 'GET {path} HTTP/1.0\\r\\nHost: {host}\\r\\n\\r\\n' | nc 127.0.0.1 80"),
    ])
}

/// The body the router serves for `host` with a 2xx status. BusyBox `wget`,
/// unlike the raw request above, gets the CGI output of BusyBox `httpd`
/// through the router.
fn body(host: &str, path: &str) -> String {
    must(&[
        "exec",
        ROUTER,
        "wget",
        "-q",
        "-O-",
        "-T",
        "10",
        "--header",
        &format!("Host: {host}"),
        &format!("http://127.0.0.1{path}"),
    ])
}

fn status_line(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
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

fn domain(project_id: Uuid) -> String {
    format!(
        "p{}.run.sakala.localhost",
        &project_id.simple().to_string()[..12]
    )
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

struct Cleanup(Vec<Uuid>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        // First, because Docker keeps a network the router is still connected to.
        docker(&["rm", "-f", "-v", ROUTER]);
        for project in &self.0 {
            remove_project(*project);
        }
        docker(&["network", "rm", RUNTIME_NETWORK]);
    }
}

fn deploy_request(project_id: Uuid, custom_domains: &[&str]) -> DeployProjectRequest {
    let payload: DeployProjectPayload = serde_json::from_value(json!({
        "repository_url": REPOSITORY,
        "commit_sha": COMMIT,
        "domain": domain(project_id),
        "container_port": PORT,
        "custom_domains": custom_domains,
        "timeouts": {
            "build_timeout_seconds": 300,
            "start_timeout_seconds": 30,
            "command_timeout_seconds": 600
        }
    }))
    .expect("payload");
    DeployProjectRequest {
        command_id: Uuid::new_v4(),
        project_id,
        deployment_id: Uuid::new_v4(),
        payload,
        repository_credential: None,
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and a BuildKit that can pull busybox:1.37"]
async fn a_custom_domain_is_served_counted_kept_through_sleep_and_moved_without_a_redeploy() {
    let project_id = Uuid::new_v4();
    let other_project = Uuid::new_v4();
    docker(&["rm", "-f", "-v", ROUTER]);
    docker(&["network", "rm", RUNTIME_NETWORK]);
    let _cleanup = Cleanup(vec![project_id, other_project]);
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
    let executor = DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            agent_id: "sakala-cdsm-agent".to_owned(),
            workspace_root: temp.path().join("builds"),
            caddy_container: ROUTER.to_owned(),
            caddy_sites_dir: sites.clone(),
            runtime_network: RUNTIME_NETWORK.to_owned(),
            min_workspace_free_bytes: 0,
            public_api_url: "https://api.staging.sakala.dev".to_owned(),
            ..DockerRuntimeConfig::default()
        },
        Arc::new(SmokeRunner {
            real: TokioProcessRunner::new(Duration::from_secs(600)),
            builder,
        }),
    );
    let project_domain = domain(project_id);
    let route_path = sites.join(format!("{project_id}.Caddyfile"));

    // Deploy with a custom domain: the router serves the project under it, and
    // the app sees the visitor's Host, not the project domain.
    let deployment = deploy_request(project_id, &[CUSTOM]);
    let deployment_id = deployment.deployment_id;
    executor
        .deploy_project(deployment, Arc::new(NoopReporter))
        .await
        .expect("deployment with a custom domain");
    let route = std::fs::read_to_string(&route_path).expect("route file");
    println!("\n--- route\n{route}");
    assert!(route.contains(&format!("{project_domain}:80, {CUSTOM}:80 {{")));
    assert!(status_line(&request(CUSTOM, "/")).contains(" 200 "));
    assert_eq!(body(CUSTOM, "/cgi-bin/host"), format!("host={CUSTOM}"));
    assert_eq!(
        body(&project_domain, "/cgi-bin/host"),
        format!("host={project_domain}")
    );

    // Visits through the custom domain alone keep the project awake.
    let traffic = || async {
        executor
            .workload_traffic()
            .await
            .expect("per-host counters")
            .into_iter()
            .find(|item| item.project_id == project_id)
            .map(|item| item.last_request_at)
            .expect("the project is listed")
    };
    let first = traffic().await;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(traffic().await, first, "no visit, no change");
    request(CUSTOM, "/");
    let visited = traffic().await;
    assert!(
        visited > first,
        "a visit through {CUSTOM} counts: {visited} after {first}"
    );

    // Sleep without a list keeps the custom domain on the sleeping page, which
    // wakes the project by its own slug.
    let lifecycle = |deployment_id| WorkloadLifecycleRequest {
        command_id: Uuid::new_v4(),
        project_id,
        deployment_id,
        remove_images: false,
        sleeping_page: SleepingPage::Waking,
        custom_domains: RequestedCustomDomains::Absent,
        cancellation: CancellationToken::new(),
    };
    executor
        .sleep_project(lifecycle(deployment_id), Arc::new(NoopReporter))
        .await
        .expect("sleep");
    let response = request(CUSTOM, "/");
    assert!(status_line(&response).contains(" 503 "), "{response}");
    let slug = project_domain.split('.').next().expect("slug");
    assert!(
        response.contains(&format!(
            "https://api.staging.sakala.dev/api/v1/public/projects/{slug}/wake"
        )),
        "the sleeping page wakes the project by its slug"
    );

    // Wake brings the custom domain back with the app.
    executor
        .wake_project(lifecycle(deployment_id), Arc::new(NoopReporter))
        .await
        .expect("wake");
    assert_eq!(body(CUSTOM, "/cgi-bin/host"), format!("host={CUSTOM}"));

    // UpdateProjectDomains moves the domain without touching the containers.
    let containers_before = must(&[
        "ps",
        "-q",
        "--filter",
        &format!("label=dev.sakala.project-id={project_id}"),
    ]);
    let output = executor
        .update_project_domains(
            UpdateProjectDomainsRequest {
                command_id: Uuid::new_v4(),
                project_id,
                custom_domains: vec![MOVED.to_owned()],
                cancellation: CancellationToken::new(),
            },
            Arc::new(NoopReporter),
        )
        .await
        .expect("update");
    assert_eq!(
        output.result,
        json!({ "status": "updated", "custom_domains": [MOVED] })
    );
    assert!(status_line(&request(CUSTOM, "/")).contains(" 404 "));
    assert_eq!(body(MOVED, "/cgi-bin/host"), format!("host={MOVED}"));
    assert_eq!(
        must(&[
            "ps",
            "-q",
            "--filter",
            &format!("label=dev.sakala.project-id={project_id}"),
        ]),
        containers_before
    );

    // Another project cannot take the domain; the first keeps serving it.
    let error = executor
        .deploy_project(
            deploy_request(other_project, &[MOVED]),
            Arc::new(NoopReporter),
        )
        .await
        .expect_err("the domain is taken");
    println!("--- other project: {error}");
    assert_eq!(error.code(), "runtime_custom_domain_taken");
    assert_eq!(body(MOVED, "/cgi-bin/host"), format!("host={MOVED}"));

    // Stopping the project removes the domain with its route.
    executor
        .stop_project(lifecycle(deployment_id), Arc::new(NoopReporter))
        .await
        .expect("stop");
    assert!(!route_path.exists());
    assert!(status_line(&request(MOVED, "/")).contains(" 404 "));
}
