//! Runs the real executor and reporter against a real Docker daemon. Only the
//! Git checkout is replaced, and most builds just tag a prepared image.
//!
//! It is opt-in because it needs a daemon plus `caddy:2.11.3-alpine` and
//! `node:24-alpine` locally:
//!
//! ```bash
//! cargo test -p sakala-agent --test docker_smoke_test -- --ignored --nocapture
//! ```
//!
//! Every container, network, and image it creates starts with `sakala-agent-sm`,
//! and it removes only those. The prefix is deliberately narrower than
//! `sakala-agent-`, which other work on the same daemon may share.

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_core::{
    api::ApiClient,
    commands::CommandProcessor,
    logs::redactor::ValueRedactor,
    ports::{
        CleanupRuntimeRequest, RuntimeExecutor, RuntimeReporter, SecretString,
        WorkloadLifecycleRequest,
    },
    reporting::ApiRuntimeReporter,
};
use sakala_agent_protocol::{
    AgentCommand, CrashedWorkloadDetail, DeploymentBuilder, LogBounds, RuntimeCleanupTarget,
};
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, RuntimeError, TokioProcessRunner,
    builders::{BuildRequest, ImageBuildService, ImageBuilder},
    railpack::RailpackSandbox,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
/// The apps run on Node, under the same options a real app gets.
const APP_IMAGE: &str = "node:24-alpine";
const NETWORK: &str = "sakala-agent-sm-net";
const ROUTER: &str = "sakala-agent-sm-caddy";
const OUTSIDE: &str = "sakala-agent-sm-outside";
const SANDBOX_NETWORK: &str = "sakala-agent-sm-railpack";
const HOST_NETWORK_BUILDER: &str = "sakala-agent-sm-builder";
const PREFIX: &str = "sakala-agent-sm";
/// Stands in for `sakala-app-` and keeps the name inside one 63-octet DNS label.
const APP_NAME_PREFIX: &str = "sakala-agent-sm-";
/// Stands in for `sakala-project-`, the network of each project.
const PROJECT_NETWORK_PREFIX: &str = "sakala-agent-sm-project-";
const SECRET: &str = "sk-smoke-secret-value";
/// Shortened from five minutes, and longer than the crash loop's run time.
const STABLE_WINDOW: Duration = Duration::from_secs(10);
/// The tests share one cleanup, so they must not overlap.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An app is one `sh -c` line starting a small Node server.
fn node_app(handler: &str, listen: &str, extra: &str) -> String {
    format!(
        "exec node -e \"require('http').createServer((q,s)=>{{{handler}}}).listen({listen});{extra}\""
    )
}

fn serve(status: u16) -> String {
    node_app(
        &format!("s.statusCode={status};s.end('body')"),
        "process.env.PORT,'0.0.0.0'",
        "",
    )
}

#[tokio::test]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and node:24-alpine"]
async fn real_docker_deployments_report_what_actually_happened() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    docker(&["network", "create", NETWORK]);
    docker(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--network",
        NETWORK,
        "--pull=never",
        ROUTER_IMAGE,
    ]);
    docker(&[
        "run",
        "-d",
        "--name",
        OUTSIDE,
        "--network",
        NETWORK,
        "--pull=never",
        "--entrypoint",
        "sh",
        ROUTER_IMAGE,
        "-c",
        "touch /tmp/requests; while true; do printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n' | nc -l -p 3000 >> /tmp/requests; done",
    ]);

    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start(&temp).await;

    // Wrong port: the app runs fine, just not where the route points.
    let run = harness
        .deploy(
            "wrongport",
            &format!(
                "echo listening on 9000; {}",
                node_app("s.end('body')", "9000,'0.0.0.0'", "")
            ),
            &[],
        )
        .await;
    run.print("app listening on the wrong port");
    run.assert_failed("runtime_app_not_listening", &["port 3000", "0.0.0.0:$PORT"]);
    run.assert_logged("[runtime] listening on 9000");

    // The right port on loopback is the same failure from the route's side.
    let run = harness
        .deploy(
            "loopback",
            &node_app("s.end('body')", "process.env.PORT,'127.0.0.1'", ""),
            &[],
        )
        .await;
    run.print("app listening on 127.0.0.1 only");
    run.assert_failed("runtime_app_not_listening", &["port 3000"]);

    // A page that does not exist is still an app that answers.
    let run = harness.deploy("notfound", &serve(404), &[]).await;
    run.print("root returns 404");
    run.assert_completed();

    let run = harness.deploy("good", &serve(200), &[]).await;
    run.print("root returns 200");
    run.assert_completed();
    println!("stages: {:?}", run.stages);
    assert_eq!(
        run.stages,
        [
            "deployment.checkout.started",
            "deployment.build.started",
            "deployment.container.started",
            "deployment.health.started",
            "deployment.route.started",
            "deployment.runtime.ready",
        ],
    );
    let good = run.clone();

    let run = harness
        .deploy(
            "err500",
            &format!("echo database is unreachable >&2; {}", serve(500)),
            &[],
        )
        .await;
    run.print("root returns 500");
    run.assert_failed("runtime_app_unhealthy", &["port 3000", "HTTP 500"]);
    run.assert_logged("[runtime] database is unreachable");

    // Exit during start: logs survive, a secret in them does not.
    let run = harness
        .deploy(
            "exits",
            &format!("echo booting; echo API_KEY={SECRET} >&2; echo module not found >&2; exit 3"),
            &[],
        )
        .await;
    run.print("app exits during start");
    run.assert_failed("runtime_app_crashed", &["exit code 3"]);
    run.assert_logged("[runtime] booting");
    run.assert_logged("[runtime] module not found");
    assert!(
        run.elapsed < Duration::from_secs(4),
        "a crash must stop the wait early: {:?}",
        run.elapsed
    );
    assert!(
        !run.sent.contains(SECRET),
        "the secret must not reach the control plane"
    );
    assert!(run.sent.contains("API_KEY=[REDACTED]"), "{}", run.sent);
    assert_no_containers_named(&run.project_prefix());

    let run = harness
        .deploy_with_memory("oom", "echo allocating; exec tail /dev/zero", 16, &[])
        .await;
    run.print("app is killed for memory during start");
    run.assert_failed("runtime_app_out_of_memory", &["16 MB"]);
    run.assert_logged("[runtime] allocating");

    let run = harness
        .deploy(
            "redirect",
            &node_app(
                &format!("s.writeHead(302,{{Location:'http://{OUTSIDE}:3000/'}});s.end()"),
                "process.env.PORT,'0.0.0.0'",
                "",
            ),
            &[],
        )
        .await;
    run.print("root redirects outside the app");
    run.assert_completed();
    let outside_requests = docker(&["exec", OUTSIDE, "cat", "/tmp/requests"]);
    println!(
        "requests seen by the redirect target: {:?}",
        outside_requests.trim()
    );
    assert!(
        outside_requests.trim().is_empty(),
        "the probe must not follow a redirect out of the app: {outside_requests}"
    );

    let run = harness
        .deploy(
            "unhealthycheck",
            &serve(200),
            &["HEALTHCHECK --interval=1s --retries=1 --timeout=1s CMD false"],
        )
        .await;
    run.print("image HEALTHCHECK reports unhealthy");
    run.assert_failed("runtime_health_check_failed", &["HEALTHCHECK"]);

    let run = harness
        .deploy("conflict", &serve(200), &[])
        .await
        .with_environment(&harness, json!({ "PORT": "8080" }))
        .await;
    run.print("user PORT disagrees with container_port");
    run.assert_failed(
        "runtime_port_conflict",
        &["PORT=8080", "container_port 3000"],
    );

    // HealthCheck on a live workload uses the same probe.
    let health = harness
        .executor
        .health_check(good.lifecycle(), harness.reporter())
        .await
        .expect("health check");
    println!("\n--- HealthCheck on the 200 app\n{health:?}");
    assert_eq!(health.result["ready"], true);

    // A workload that crashes after going live, next to one the agent put to sleep.
    let crashing = harness
        .deploy(
            "crashlater",
            &node_app(
                "s.end('body')",
                "process.env.PORT,'0.0.0.0'",
                "setTimeout(()=>{console.log('gone');process.exit(7)},3000)",
            ),
            &[],
        )
        .await;
    crashing.print("app that goes live and then crashes");
    crashing.assert_completed();
    harness
        .executor
        .sleep_project(good.lifecycle(), harness.reporter())
        .await
        .expect("sleep");
    println!("\n--- docker status after the crash window");
    let crashed = harness.wait_for_crash(&crashing).await;
    println!(
        "{}",
        docker(&[
            "ps",
            "-a",
            "--filter",
            "label=dev.sakala.managed=true",
            "--format",
            "{{.Names}}\t{{.Status}}"
        ])
    );
    println!(
        "crashed_details: {}",
        serde_json::to_string(&crashed).expect("json")
    );
    assert!(
        crashed
            .iter()
            .any(|detail| detail.project_id == crashing.project_id
                && detail.exit_code == Some(7)
                && !detail.oom_killed
                && detail.finished_at.is_some()),
        "{crashed:?}"
    );
    assert!(
        !crashed
            .iter()
            .any(|detail| detail.project_id == good.project_id),
        "a workload the agent put to sleep is not a crash: {crashed:?}"
    );

    // The crash loop stays reported across checks, including while it is `Up`
    // between crashes, which is when it used to flap.
    harness.wait_for_running_crash_loop(&crashing).await;
    println!("\n--- slow crash loop sampled across interval checks");
    let mut statuses_while_crashed = Vec::new();
    for _ in 0..12 {
        let (status, detail) = harness.crash_state_of(&crashing).await;
        let detail = detail.unwrap_or_else(|| panic!("not reported while {status}"));
        println!(
            "status: {status}; exit_code: {:?}, restart_count: {}, running: {}",
            detail.exit_code, detail.restart_count, detail.running
        );
        assert_eq!(detail.exit_code, Some(7), "while {status}");
        assert!(detail.restart_count >= 3, "while {status}");
        statuses_while_crashed.push(status);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        statuses_while_crashed
            .iter()
            .any(|status| status.starts_with("Up")),
        "the sample must include the window that used to flap: {statuses_while_crashed:?}"
    );

    // One crash, then healthy: never reported while it serves.
    let once = crashes_then_serves("killedonce", 1);
    let killed = harness.deploy("killedonce", &once, &[]).await;
    killed.print("app that dies once and comes back");
    killed.assert_completed();
    let started = Instant::now();
    let (mut checks, mut up_after_restart, mut reported_while_restarting) = (0, 0, 0);
    while started.elapsed() < STABLE_WINDOW {
        let (status, detail) = harness.crash_state_of(&killed).await;
        checks += 1;
        if status.starts_with("Up") {
            // The report inspects a moment after this status was listed, so it
            // can catch the restart. It must never call a running app crashed.
            assert!(
                detail.as_ref().is_none_or(|detail| !detail.running),
                "reported while {status}: {detail:?}"
            );
            if restart_count_of(&killed) == 1 {
                up_after_restart += 1;
            }
        } else if detail.is_some() {
            reported_while_restarting += 1;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!(
        "single crash: {checks} checks, {up_after_restart} up after the restart and not reported, {reported_while_restarting} caught in Docker's brief restarting state"
    );
    assert!(up_after_restart > 0, "the app never restarted");

    // Crashes three times, then stays up: reported, then clears after the window.
    let thrice = crashes_then_serves("thrice", 3);
    let cleared = harness.deploy("thrice", &thrice, &[]).await;
    cleared.print("app that crash-loops and then recovers");
    cleared.assert_completed();
    let detail = harness.wait_for_running_crash_loop(&cleared).await;
    println!("crash loop reported: {}", json!(detail));
    assert_eq!(detail.exit_code, Some(5));
    let reported_at = Instant::now();
    let status = loop {
        let (status, detail) = harness.crash_state_of(&cleared).await;
        if detail.is_none() {
            break status;
        }
        assert!(
            reported_at.elapsed() < STABLE_WINDOW + Duration::from_secs(10),
            "the crash loop never cleared"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    println!(
        "crash loop cleared after {:.1} s, status: {status}",
        reported_at.elapsed().as_secs_f64()
    );
    assert!(status.starts_with("Up"), "{status}");

    // An intentional restart or wake right after a crash is not another crash.
    for (name, title) in [
        ("thenrestart", "RestartProject"),
        ("thenwake", "SleepProject and WakeProject"),
    ] {
        let run = harness
            .deploy(name, &crashes_then_serves(name, 3), &[])
            .await;
        run.assert_completed();
        harness.wait_for_running_crash_loop(&run).await;
        if name == "thenrestart" {
            harness
                .executor
                .restart_project(run.lifecycle(), harness.reporter())
                .await
                .expect("restart");
        } else {
            harness
                .executor
                .sleep_project(run.lifecycle(), harness.reporter())
                .await
                .expect("sleep");
            harness
                .executor
                .wake_project(run.lifecycle(), harness.reporter())
                .await
                .expect("wake");
        }
        let snapshots = harness.executor.health_snapshot().await.expect("snapshot");
        let crashed = harness
            .executor
            .crashed_workloads(&snapshots)
            .await
            .expect("crash details");
        println!(
            "\n--- {title} right after a crash: reported as crashed: {}",
            crashed
                .iter()
                .any(|detail| detail.project_id == run.project_id)
        );
        assert!(
            !crashed
                .iter()
                .any(|detail| detail.project_id == run.project_id),
            "{title} is not a crash: {crashed:?}"
        );
    }

    harness.executor.shutdown().await.expect("shutdown");
}

/// A real BuildKit build through the real builder and reporter: only the Git
/// checkout is replaced. Proves where a project variable can and cannot go.
#[tokio::test]
#[ignore = "needs a local Docker daemon with BuildKit and alpine:3.22"]
async fn dockerfile_build_uses_project_variables_without_keeping_them_in_the_image() {
    const VALUE: &str = "kL9-build-time-value-for-smoke";
    const IMAGE: &str = "sakala-agent-sm/build-secret:latest";
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();

    let temp = TempDir::new().expect("temp directory");
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).expect("source");
    // Unique per run: a cached step would run nothing and print nothing.
    std::fs::write(
        source.join("Dockerfile"),
        format!(
            concat!(
                "FROM alpine:3.22\n",
                "RUN echo {} > /run-id\n",
                "RUN echo layer-search-control-marker > /control\n",
                "ARG VITE_API_URL\n",
                "RUN [ -z \"$VITE_API_URL\" ] && echo build arg stays empty\n",
                "RUN --mount=type=secret,id=VITE_API_URL,env=VITE_API_URL ",
                "sh -c 'echo seen as env: $VITE_API_URL; printf \"%s\" \"$VITE_API_URL\" | wc -c > /env-length'\n",
                "RUN --mount=type=secret,id=VITE_API_URL ",
                "sh -c 'wc -c < /run/secrets/VITE_API_URL > /file-length'\n",
                "RUN printf 'caf\\351 latin1 output\\n'\n",
                "RUN [ ! -e /run/secrets/VITE_API_URL ] && echo not mounted without asking\n",
            ),
            Uuid::new_v4()
        ),
    )
    .expect("Dockerfile");

    let (server, requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = ApiRuntimeReporter::new(client, Uuid::new_v4(), LogBounds::default())
        .with_redactor(ValueRedactor::new([VALUE.to_owned()]));
    let builder = real_builder();

    let output = builder
        .build(
            &BuildRequest {
                project_id: Uuid::new_v4(),
                deployment_id: Uuid::new_v4(),
                workspace: temp.path().to_path_buf(),
                source,
                image: IMAGE.to_owned(),
                requested: DeploymentBuilder::Dockerfile,
                environment: vec![("VITE_API_URL".to_owned(), SecretString::new(VALUE))],
                build_command: None,
                start_command: None,
            },
            &reporter,
        )
        .await;
    reporter.flush().await.expect("flush");

    let requests = requests.lock().expect("request lock").clone();
    let logs = requests
        .iter()
        .filter(|(path, _)| path.ends_with("/logs"))
        .flat_map(|(_, body)| body["logs"].as_array().cloned().unwrap_or_default())
        .filter_map(|log| log["message"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    for line in logs.iter().filter(|line| {
        line.contains("seen as env")
            || line.contains("build arg stays")
            || line.contains("latin1")
            || line.contains("not mounted")
    }) {
        println!("build log: {line}");
    }
    output.unwrap_or_else(|error| panic!("build failed: {error}\n{}", logs.join("\n")));

    let sent = serde_json::to_string(&requests).expect("requests serialize");
    assert!(
        !sent.contains(VALUE),
        "the value must not reach the control plane"
    );
    assert!(sent.contains("seen as env: [REDACTED]"), "{sent}");

    // The step really had the value, under both documented forms.
    let length = VALUE.len().to_string();
    for file in ["/env-length", "/file-length"] {
        let read = docker(&["run", "--rm", "--pull=never", IMAGE, "cat", file]);
        println!("{file} inside the image: {}", read.trim());
        assert_eq!(read.trim(), length, "{file}");
    }

    // And the image keeps none of it, in history, config, or any layer.
    let history = docker(&["history", "--no-trunc", IMAGE]);
    assert!(!history.contains(VALUE), "history: {history}");
    let inspect = docker(&["image", "inspect", IMAGE]);
    assert!(!inspect.contains(VALUE), "config: {inspect}");
    // The marker proves this search would find a value a step had written.
    let search = |needle: &str| {
        docker(&[
            "run",
            "--rm",
            "--pull=never",
            IMAGE,
            "sh",
            "-c",
            &format!(
                "grep -rlsF '{needle}' $(ls / | grep -vE '^(proc|sys|dev)$' | sed 's|^|/|'); true"
            ),
        ])
    };
    let control = search("layer-search-control-marker");
    println!("control marker found in: {}", control.trim());
    assert!(control.contains("/control"), "{control}");
    let found = search(VALUE);
    println!(
        "value in history: false, in image config: false, in image filesystem: {:?}",
        found.trim()
    );
    assert!(found.trim().is_empty(), "{found}");
}

/// A real build, cancelled the way the poller cancels it, must stop for real.
#[tokio::test]
#[ignore = "needs a local Docker daemon with BuildKit and alpine:3.22"]
async fn cancelling_a_real_build_stops_it_and_leaves_nothing_behind() {
    // Unique, so it identifies this build's process on the host.
    const MARKER: &str = "86417";
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start(&temp).await;
    harness.runner.use_real_build(vec![(
        "Dockerfile".to_owned(),
        format!(
            "FROM alpine:3.22\nRUN echo {} > /run-id\nRUN echo building; sleep {MARKER}\n",
            Uuid::new_v4()
        ),
    )]);
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    let canceller = tokio::spawn(async move {
        for _ in 0..240 {
            if sleeping_build_processes(MARKER) > 0 {
                println!("build step is running on the host; cancelling");
                trigger.cancel();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        trigger.cancel();
        false
    });

    let run = harness
        .run_custom(
            "cancelbuild",
            json!({ "builder": "dockerfile" }),
            cancellation,
        )
        .await;

    run.print("real build cancelled while its step runs");
    assert!(
        canceller.await.expect("join"),
        "the build step never started"
    );
    run.assert_failed("runtime_cancelled", &[]);
    let mut remaining = 1;
    for _ in 0..60 {
        remaining = sleeping_build_processes(MARKER);
        if remaining == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!("build step processes left on the host: {remaining}");
    assert_eq!(remaining, 0, "the build step must stop");
    let buildx = docker_host_processes("docker buildx.*sakala-agent-sm/project-");
    assert_eq!(buildx, 0, "the buildx client must stop");
    let images = docker(&["images", "--format", "{{.Repository}}"]);
    assert!(
        !images
            .lines()
            .any(|image| image.starts_with("sakala-agent-sm/project-")),
        "{images}"
    );
    assert_no_containers_named(&run.project_prefix());
    harness.executor.shutdown().await.expect("shutdown");
}

/// A monorepo through real BuildKit: a Dockerfile that cannot start on its own,
/// made to work only by the root directory and start command.
#[tokio::test]
#[ignore = "needs a local Docker daemon with BuildKit and node:24-alpine"]
async fn root_directory_and_start_command_work_with_a_real_dockerfile_build() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start(&temp).await;
    harness.runner.use_real_build(vec![
        (
            "Dockerfile".to_owned(),
            "FROM scratch\nthis is not the Dockerfile to build\n".to_owned(),
        ),
        (
            "apps/web/Dockerfile".to_owned(),
            format!(
                "FROM node:24-alpine\nENV RUN_ID={}\nCOPY server.js /server.js\nENTRYPOINT [\"false\"]\n",
                Uuid::new_v4()
            ),
        ),
        (
            "apps/web/server.js".to_owned(),
            "require('http').createServer((q,s)=>s.end('monorepo')).listen(process.env.PORT,'0.0.0.0');console.log('started on', process.env.PORT)\n".to_owned(),
        ),
    ]);

    let run = harness
        .run_custom(
            "monorepo",
            json!({
                "root_directory": "apps/web",
                "start_command": "exec node /server.js",
                "container_port": 8123,
                "build_command": "not used by a Dockerfile",
            }),
            CancellationToken::new(),
        )
        .await;

    run.print("real Dockerfile build from a root directory with a start command");
    run.assert_completed();
    run.assert_logged("[runtime] started on 8123");
    assert!(
        run.logs
            .iter()
            .any(|line| line.contains("build_command was not used")),
        "{:?}",
        run.logs
    );
    harness.executor.shutdown().await.expect("shutdown");
}

/// Exits with code 5 on its first `crashes` runs, each after serving for two
/// seconds so that the deployment goes live, then serves for good.
fn crashes_then_serves(name: &str, crashes: u32) -> String {
    format!(
        "n=$(cat /tmp/{name} 2>/dev/null || echo 0); echo $((n+1)) > /tmp/{name}; if [ \"$n\" -ge {crashes} ]; then {}; else {}; fi",
        serve(200),
        node_app(
            "s.end('body')",
            "process.env.PORT,'0.0.0.0'",
            "setTimeout(()=>process.exit(5),2000)",
        ),
    )
}

fn restart_count_of(run: &Deployment) -> u32 {
    let name = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{}", run.project_prefix()),
        "--format",
        "{{.Names}}",
    ]);
    docker(&["inspect", "--format", "{{.RestartCount}}", name.trim()])
        .trim()
        .parse()
        .expect("restart count")
}

fn start_router() {
    docker(&["network", "create", NETWORK]);
    docker(&[
        "run",
        "-d",
        "--name",
        ROUTER,
        "--network",
        NETWORK,
        "--pull=never",
        ROUTER_IMAGE,
    ]);
}

fn sleeping_build_processes(marker: &str) -> usize {
    docker_host_processes(&format!("sleep {marker}"))
}

fn docker_host_processes(pattern: &str) -> usize {
    Command::new("pgrep")
        .args(["-fc", pattern])
        .output()
        .ok()
        .map_or(0, |output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        })
}

fn real_builder() -> ImageBuildService {
    let runner: Arc<dyn ProcessRunner> = Arc::new(TokioProcessRunner::default());
    let config = DockerRuntimeConfig::default();
    ImageBuildService::new(
        Arc::clone(&runner),
        config.railpack_frontend,
        RailpackSandbox::new(runner, String::new(), SANDBOX_NETWORK.to_owned()),
    )
}

/// Builds the sandbox from the repository's own Dockerfile and returns its image ID.
/// Honours `BUILDX_BUILDER`, because downloads inside BuildKit stall on some machines.
fn build_sandbox_image() -> String {
    const TAG: &str = "sakala-agent-sm/railpack-sandbox:smoke";
    let context = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/railpack-sandbox");
    docker(&[
        "buildx",
        "build",
        "--network",
        "host",
        "--load",
        "--tag",
        TAG,
        context,
    ]);
    docker(&["image", "inspect", "--format", "{{.Id}}", TAG])
        .trim()
        .to_owned()
}

fn write_repository(root: &std::path::Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("repository");
    for (path, content) in files {
        std::fs::write(root.join(path), content).expect("file");
    }
}

/// Lets a test read what a repository's code saw inside the sandbox. The sandbox has
/// no writable host path of its own, so this adds one mount to it and changes nothing
/// else about the command the Agent runs.
struct EvidenceRunner {
    evidence: std::path::PathBuf,
}

impl EvidenceRunner {
    fn new(temp: &TempDir) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let evidence = temp.path().join("evidence");
        std::fs::create_dir(&evidence).expect("evidence");
        std::fs::set_permissions(&evidence, std::fs::Permissions::from_mode(0o777))
            .expect("evidence permissions");
        Self { evidence }
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.evidence.join(name)).unwrap_or_else(|error| {
            panic!("{name}: {error}: the code never ran, so this proves nothing")
        })
    }
}

#[async_trait]
impl ProcessRunner for EvidenceRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let mut real = spec.clone();
        if real.args.first().is_some_and(|argument| argument == "run") {
            real.args.insert(1, "--mount".into());
            real.args.insert(
                2,
                format!("type=bind,src={},dst=/evidence", self.evidence.display()).into(),
            );
        }
        TokioProcessRunner::default().run(&real, sink).await
    }
}

/// The reviewer's proof of concept, run only inside the sandbox: a repository whose
/// `mise.toml` executes code during analysis, and one whose `.bun-version` is a
/// symlink to a host file. The control proves the symlink channel really carries
/// content, so its absence for the host file means something.
#[tokio::test]
#[ignore = "needs a local Docker daemon and network access to build the sandbox image"]
async fn a_malicious_repository_stays_inside_the_railpack_sandbox() {
    let token = std::env::var("SAKALA_AGENT_TOKEN")
        .expect("run this with SAKALA_AGENT_TOKEN set, to prove it does not get in");
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    let image = build_sandbox_image();
    let temp = TempDir::new().expect("temp directory");
    let evidence = Arc::new(EvidenceRunner::new(&temp));
    let sandbox = RailpackSandbox::new(evidence.clone(), image, SANDBOX_NETWORK.to_owned());
    let (server, requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = ApiRuntimeReporter::new(client, Uuid::new_v4(), LogBounds::default());

    let host_marker = temp.path().join("host-marker");
    let host_secret = temp.path().join("host-secret.txt");
    const HOST_CONTENT: &str = "HOST-FILE-CONTENT-kL9";
    const CONTROL_CONTENT: &str = "CONTROL-CONTENT-zQ7";
    std::fs::write(&host_secret, HOST_CONTENT).expect("host file");
    let (marker, secret) = (host_marker.display(), host_secret.display());
    let package = r#"{"name":"evil","version":"1.0.0","scripts":{"start":"node index.js"}}"#;

    // Code execution through a tool version.
    let exec_repo = temp.path().join("exec-repo");
    write_repository(
        &exec_repo,
        &[
            ("package.json", package),
            (
                "mise.toml",
                &format!(
                    "[tools]\nnode = \"{{{{ exec(command='id > /evidence/id; env > /evidence/env; cat /proc/1/environ > /evidence/pid1 2>&1 || true; touch {marker} 2>/evidence/marker || true; ls -la /var/run/docker.sock > /evidence/socket 2>&1 || true; cat {secret} > /evidence/file 2>&1 || true; touch /app/written 2>/evidence/app || true; touch /written 2>/evidence/root || true; touch /out/written 2>/evidence/out || true; dd if=/dev/zero of=/tmp/big bs=1M count=300 2>/evidence/tmpfs || true; rm -f /tmp/big; echo 22') }}}}\"\n"
                ),
            ),
        ],
    );
    sandbox
        .info(&exec_repo, &reporter)
        .await
        .expect("analysis of the repository finishes");
    let id = evidence.read("id");
    println!("the repository's code ran as: {}", id.trim());
    assert!(id.contains("uid=65534"), "{id}");
    let environment = evidence.read("env");
    println!(
        "variables inside: {}",
        environment
            .lines()
            .filter_map(|line| line.split('=').next())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for seen in [environment, evidence.read("pid1")] {
        assert!(!seen.contains("SAKALA_"), "{seen}");
        assert!(!seen.contains(&token), "{seen}");
        assert!(!seen.contains("test-agent-token"), "{seen}");
    }
    let socket = evidence.read("socket");
    println!("docker socket: {}", socket.trim());
    assert!(socket.contains("No such file"), "{socket}");
    let file = evidence.read("file");
    println!("host file: {}", file.trim());
    assert!(!file.contains(HOST_CONTENT), "{file}");
    assert!(!host_marker.exists(), "the marker reached the host");
    println!("host marker: {}", evidence.read("marker").trim());
    assert!(
        !exec_repo.join("written").exists(),
        "the source must be read-only"
    );
    for (name, expected) in [
        ("app", "Read-only file system"),
        ("root", "Read-only file system"),
        ("out", "No such file or directory"),
        ("tmpfs", "No space left on device"),
    ] {
        let seen = evidence.read(name);
        println!("write to {name}: {}", seen.trim().replace('\n', " | "));
        assert!(seen.contains(expected), "{name}: {seen}");
    }

    // The control: a symlink to a file the sandbox can see reaches the deployment log.
    let control_repo = temp.path().join("control-repo");
    write_repository(
        &control_repo,
        &[
            ("package.json", package),
            ("bun.lockb", ""),
            ("inside.txt", CONTROL_CONTENT),
        ],
    );
    std::os::unix::fs::symlink("/app/inside.txt", control_repo.join(".bun-version")).expect("link");
    let control = sandbox
        .prepare(&control_repo, temp.path(), &[], &reporter)
        .await;
    assert!(control.is_err(), "an impossible version cannot be planned");
    reporter.flush().await.expect("flush");
    let sent = serde_json::to_string(&*requests.lock().expect("request lock")).expect("json");
    assert!(
        sent.contains(CONTROL_CONTENT),
        "the channel carries nothing, so this test proves nothing: {sent}"
    );
    println!("control: the content of a symlinked .bun-version reached the log");

    // The same channel, aimed at a host file and at the process environment.
    let link_repo = temp.path().join("link-repo");
    write_repository(&link_repo, &[("package.json", package), ("bun.lockb", "")]);
    std::os::unix::fs::symlink(&host_secret, link_repo.join(".bun-version")).expect("link");
    std::os::unix::fs::symlink(&host_secret, link_repo.join(".nvmrc")).expect("link");
    std::os::unix::fs::symlink("/proc/self/environ", link_repo.join(".node-version"))
        .expect("link");
    let plan = sandbox
        .prepare(&link_repo, temp.path(), &[], &reporter)
        .await
        .ok();
    let info = sandbox
        .info(&link_repo, &reporter)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    reporter.flush().await.expect("flush");
    let sent = serde_json::to_string(&*requests.lock().expect("request lock")).expect("json");
    let plan = plan.map_or_else(String::new, |path| {
        std::fs::read_to_string(path).expect("plan")
    });
    for (name, seen) in [("report", &info), ("plan", &plan), ("logs", &sent)] {
        assert!(!seen.contains(HOST_CONTENT), "{name}: {seen}");
        assert!(!seen.contains("SAKALA_AGENT"), "{name}: {seen}");
        assert!(!seen.contains(&token), "{name}: {seen}");
    }
    println!("host file content or token in report, plan, or logs: false");
}

/// Sends the sandbox's real command at everything nearby and records what answers.
/// Internet and the host are expected to answer: `docs/RAILPACK_STRATEGY.md` says so.
#[tokio::test]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, and network access"]
async fn the_sandbox_cannot_reach_other_containers_but_can_reach_out() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    let image = build_sandbox_image();
    let temp = TempDir::new().expect("temp directory");
    let evidence = Arc::new(EvidenceRunner::new(&temp));
    let sandbox = RailpackSandbox::new(evidence.clone(), image, SANDBOX_NETWORK.to_owned());
    sandbox.ensure_network().await.expect("sandbox network");
    let (server, _requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = ApiRuntimeReporter::new(client, Uuid::new_v4(), LogBounds::default());

    // A workload on the runtime network, and another sandbox on the sandbox network.
    start_router();
    const PEER: &str = "sakala-agent-sm-peer";
    docker(&[
        "run",
        "-d",
        "--name",
        PEER,
        "--network",
        SANDBOX_NETWORK,
        "--pull=never",
        ROUTER_IMAGE,
    ]);
    let address_of = |name: &str| {
        docker(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            name,
        ])
        .trim()
        .to_owned()
    };
    let (workload, peer) = (address_of(ROUTER), address_of(PEER));
    // Both really listen, so "closed" below is the network and not an idle port.
    for name in [ROUTER, PEER] {
        docker(&[
            "exec",
            name,
            "wget",
            "-q",
            "-O",
            "/dev/null",
            "http://127.0.0.1:80/",
        ]);
    }
    let gateway = docker(&[
        "network",
        "inspect",
        "--format",
        "{{(index .IPAM.Config 0).Gateway}}",
        SANDBOX_NETWORK,
    ])
    .trim()
    .to_owned();
    // Listens on every address of the host, like a service bound to 0.0.0.0.
    let host = std::net::TcpListener::bind("0.0.0.0:0").expect("host listener");
    let host_port = host.local_addr().expect("address").port();

    let probe = |name: &str, target: &str| {
        format!("(nc -z -w 5 {target} && echo open || echo closed) > /evidence/{name};")
    };
    let script = [
        probe("workload", &format!("{workload} 80")),
        probe("peer", &format!("{peer} 80")),
        probe("host", &format!("{gateway} {host_port}")),
        probe("metadata", "169.254.169.254 80"),
        probe("internet", "1.1.1.1 443"),
    ]
    .concat();
    let repo = temp.path().join("repo");
    write_repository(
        &repo,
        &[
            ("package.json", r#"{"name":"probe","version":"1.0.0"}"#),
            (
                "mise.toml",
                &format!("[tools]\nnode = \"{{{{ exec(command='{script} echo 22') }}}}\"\n"),
            ),
        ],
    );
    sandbox.info(&repo, &reporter).await.expect("analysis");

    let answer = |name: &str| evidence.read(name).trim().to_owned();
    for name in ["workload", "peer", "host", "metadata", "internet"] {
        println!("{name}: {}", answer(name));
    }
    assert_eq!(answer("workload"), "closed", "the runtime network");
    assert_eq!(
        answer("peer"),
        "closed",
        "containers on the sandbox network"
    );
    assert_eq!(answer("internet"), "open", "Railpack needs this");
    docker(&["rm", "-f", "-v", PEER]);
}

/// Two projects, the same Dockerfile, different secrets, through real BuildKit.
#[tokio::test]
#[ignore = "needs a local Docker daemon with BuildKit and alpine:3.22"]
async fn projects_share_nothing_through_the_build_cache() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    let temp = TempDir::new().expect("temp directory");
    let builder = real_builder();
    let (server, requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = ApiRuntimeReporter::new(client, Uuid::new_v4(), LogBounds::default());
    let cache_id = Uuid::new_v4();
    let dockerfile = format!(
        concat!(
            "# syntax=sakala-agent-sm/this-frontend-does-not-exist:never\n",
            "FROM alpine:3.22\n",
            "RUN --mount=type=secret,id=S,env=S sh -c 'echo \"$S\" > /baked'\n",
            "RUN --mount=type=cache,id={},target=/c sh -c 'cat /c/left-behind > /seen 2>/dev/null || echo NOTHING > /seen; cat /baked > /c/left-behind'\n",
        ),
        cache_id
    );
    let source = temp.path().join("source");
    write_repository(&source, &[("Dockerfile", &dockerfile)]);

    let build = |name: &'static str, secret: &'static str| {
        let (builder, reporter, source, workspace) = (
            &builder,
            &reporter,
            source.clone(),
            temp.path().to_path_buf(),
        );
        async move {
            let image = format!("sakala-agent-sm/cache-{name}:latest");
            builder
                .build(
                    &BuildRequest {
                        project_id: Uuid::new_v4(),
                        deployment_id: Uuid::new_v4(),
                        workspace,
                        source,
                        image: image.clone(),
                        requested: DeploymentBuilder::Dockerfile,
                        environment: vec![("S".to_owned(), SecretString::new(secret))],
                        build_command: None,
                        start_command: None,
                    },
                    reporter,
                )
                .await
                .expect("build");
            let read = |file: &str| {
                docker(&["run", "--rm", "--pull=never", &image, "cat", file])
                    .trim()
                    .to_owned()
            };
            (read("/baked"), read("/seen"))
        }
    };

    let (first_secret, first_seen) = build("one", "secret-of-project-one").await;
    let (second_secret, second_seen) = build("two", "secret-of-project-two").await;
    println!("project one holds {first_secret}, its cache mount held {first_seen}");
    println!("project two holds {second_secret}, its cache mount held {second_seen}");
    assert_eq!(first_secret, "secret-of-project-one");
    assert_eq!(second_secret, "secret-of-project-two");
    assert_eq!(first_seen, "NOTHING");
    assert_eq!(
        second_seen, "NOTHING",
        "a cache mount must not cross projects"
    );

    // `--no-cache` hides a cache mount for that build, so the namespace is checked on
    // its own here: a build with the same namespace sees it, another one does not.
    let bare = temp.path().join("namespace");
    let body = format!(
        "FROM alpine:3.22\nARG BUST\nRUN --mount=type=cache,id={},target=/c sh -c 'cat /c/left-behind > /seen 2>/dev/null || echo NOTHING > /seen; echo FROM-EARLIER-BUILD > /c/left-behind'\n",
        Uuid::new_v4()
    );
    write_repository(&bare, &[("Dockerfile", &body)]);
    let mut seen_by_namespace = Vec::new();
    for (bust, namespace) in [("1", "project-a"), ("2", "project-a"), ("3", "project-b")] {
        let image = format!("sakala-agent-sm/cache-ns-{bust}:latest");
        docker(&[
            "buildx",
            "build",
            "--load",
            "--quiet",
            "--build-arg",
            &format!("BUST={bust}"),
            "--build-arg",
            &format!("BUILDKIT_CACHE_MOUNT_NS={namespace}"),
            "--tag",
            &image,
            bare.to_str().expect("path"),
        ]);
        seen_by_namespace.push(
            docker(&["run", "--rm", "--pull=never", &image, "cat", "/seen"])
                .trim()
                .to_owned(),
        );
    }
    println!("cache mount by namespace: {seen_by_namespace:?}");
    assert_eq!(
        seen_by_namespace,
        ["NOTHING", "FROM-EARLIER-BUILD", "NOTHING"]
    );

    // Declaring the namespace in the Dockerfile must not let a project pick another's.
    let shared = temp.path().join("shared");
    write_repository(
        &shared,
        &[(
            "Dockerfile",
            "FROM alpine:3.22\nARG BUILDKIT_CACHE_MOUNT_NS=chosen\nRUN --mount=type=cache,id=x,target=/c sh -c 'cat /c/left-behind > /seen 2>/dev/null || echo NOTHING > /seen; echo FROM-EARLIER-BUILD > /c/left-behind'\n",
        )],
    );
    let mut seen_by_shared = Vec::new();
    for name in ["shared-one", "shared-two"] {
        let image = format!("sakala-agent-sm/cache-{name}:latest");
        builder
            .build(
                &BuildRequest {
                    project_id: Uuid::new_v4(),
                    deployment_id: Uuid::new_v4(),
                    workspace: temp.path().to_path_buf(),
                    source: shared.clone(),
                    image: image.clone(),
                    requested: DeploymentBuilder::Dockerfile,
                    environment: Vec::new(),
                    build_command: None,
                    start_command: None,
                },
                &reporter,
            )
            .await
            .expect("build");
        seen_by_shared.push(
            docker(&["run", "--rm", "--pull=never", &image, "cat", "/seen"])
                .trim()
                .to_owned(),
        );
    }
    println!("a cache namespace set by ARG: {seen_by_shared:?}");
    assert_eq!(seen_by_shared, ["NOTHING", "NOTHING"]);

    // A project cannot start from another project's image on this node.
    let stolen = temp.path().join("stolen");
    write_repository(
        &stolen,
        &[(
            "Dockerfile",
            "FROM sakala-agent-sm/cache-one:latest\nRUN cat /baked\n",
        )],
    );
    let error = builder
        .build(
            &BuildRequest {
                project_id: Uuid::new_v4(),
                deployment_id: Uuid::new_v4(),
                workspace: temp.path().to_path_buf(),
                source: stolen,
                image: "sakala-agent-sm/cache-stolen:latest".to_owned(),
                requested: DeploymentBuilder::Dockerfile,
                environment: Vec::new(),
                build_command: None,
                start_command: None,
            },
            &reporter,
        )
        .await
        .expect_err("another project's local image must not resolve");
    println!("FROM another project's image: {error}");
    let stolen = temp.path().join("stolen-copy");
    write_repository(
        &stolen,
        &[(
            "Dockerfile",
            "FROM alpine:3.22\nCOPY --from=sakala-agent-sm/cache-one:latest /baked /baked\n",
        )],
    );
    let error = builder
        .build(
            &BuildRequest {
                project_id: Uuid::new_v4(),
                deployment_id: Uuid::new_v4(),
                workspace: temp.path().to_path_buf(),
                source: stolen,
                image: "sakala-agent-sm/cache-stolen:latest".to_owned(),
                requested: DeploymentBuilder::Dockerfile,
                environment: Vec::new(),
                build_command: None,
                start_command: None,
            },
            &reporter,
        )
        .await
        .expect_err("COPY --from another project's local image must not resolve");
    println!("COPY --from another project's image: {error}");

    // A repository cannot choose a frontend that ignores all of the above. This
    // frontend does not exist, so the build would fail if the line were honoured.
    for directive in ["# syntax=", "#syntax=", "// syntax="] {
        let chosen = temp.path().join("chosen");
        write_repository(
            &chosen,
            &[(
                "Dockerfile",
                &format!(
                    "{directive}sakala-agent-sm/this-frontend-does-not-exist:never\nFROM alpine:3.22\nRUN true\n"
                ),
            )],
        );
        let result = builder
            .build(
                &BuildRequest {
                    project_id: Uuid::new_v4(),
                    deployment_id: Uuid::new_v4(),
                    workspace: temp.path().to_path_buf(),
                    source: chosen,
                    image: "sakala-agent-sm/cache-chosen:latest".to_owned(),
                    requested: DeploymentBuilder::Dockerfile,
                    environment: Vec::new(),
                    build_command: None,
                    start_command: None,
                },
                &reporter,
            )
            .await;
        reporter.flush().await.expect("flush");
        let sent = serde_json::to_string(&*requests.lock().expect("request lock")).expect("json");
        // `//` is not a comment once the first line is ours, so that repository
        // fails on its own Dockerfile. What matters is that no frontend was looked up.
        assert!(
            !sent.contains("docker.io/sakala-agent-sm/this-frontend"),
            "{directive} chose a frontend: {sent}"
        );
        match directive {
            "// syntax=" => {
                assert!(result.is_err(), "{directive}");
                assert!(sent.contains("unknown instruction"), "{sent}");
                println!("{directive} line: build fails on the line itself");
            }
            _ => {
                result.unwrap_or_else(|error| panic!("{directive} must be ignored: {error}"));
            }
        }
    }
    println!("# syntax= lines ignored: true");
}

/// The Railpack frontend ignores `--no-cache` and `--pull` (see
/// `docs/RAILPACK_STRATEGY.md`), so between projects only the keyed `secrets-hash`
/// stands between a build step and a layer built with someone else's secret.
#[tokio::test]
#[ignore = "needs a local Docker daemon, network access, and a working builder"]
async fn railpack_projects_keep_their_own_secrets() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    let image = build_sandbox_image();
    let temp = TempDir::new().expect("temp directory");
    let runner: Arc<dyn ProcessRunner> =
        Arc::new(TokioProcessRunner::new(Duration::from_secs(590)));
    let builder = ImageBuildService::new(
        Arc::clone(&runner),
        DockerRuntimeConfig::default().railpack_frontend,
        RailpackSandbox::new(runner, image, SANDBOX_NETWORK.to_owned()),
    );
    let (server, _requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = ApiRuntimeReporter::new(client, Uuid::new_v4(), LogBounds::default());
    let source = temp.path().join("source");
    // Same source and same plan for both projects, so a build step's key can match.
    write_repository(
        &source,
        &[(
            "package.json",
            r#"{"name":"same","version":"1.0.0","scripts":{"start":"node index.js"}}"#,
        )],
    );
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755))
            .expect("source permissions");
    }
    let mut seen = Vec::new();
    for (name, secret) in [
        ("one", "railpack-secret-one"),
        ("two", "railpack-secret-two"),
    ] {
        let tag = format!("sakala-agent-sm/railpack-{name}:latest");
        builder
            .build(
                &BuildRequest {
                    project_id: Uuid::new_v4(),
                    deployment_id: Uuid::new_v4(),
                    workspace: temp.path().to_path_buf(),
                    source: source.clone(),
                    image: tag.clone(),
                    requested: DeploymentBuilder::Railpack,
                    environment: vec![("S".to_owned(), SecretString::new(secret))],
                    build_command: Some("echo \"$S\" > baked.txt".to_owned()),
                    start_command: None,
                },
                &reporter,
            )
            .await
            .expect("build");
        // Railpack images run their command through `bash -c`, which would swallow a path.
        let baked = docker(&[
            "run",
            "--rm",
            "--pull=never",
            "--entrypoint",
            "cat",
            &tag,
            "/app/baked.txt",
        ])
        .trim()
        .to_owned();
        println!("project {name} holds {baked}");
        seen.push(baked);
    }
    assert_eq!(seen, ["railpack-secret-one", "railpack-secret-two"]);
}

/// Runs a real workload under `SAKALA_SMOKE_CONTAINER_RUNTIME`, on whichever daemon
/// `DOCKER_HOST` points at. `caddy:2.11.3-alpine` and `node:24-alpine` must be loaded
/// there. Set `SAKALA_SMOKE_LIMITS_ENFORCED=1` only where cgroups apply: the check
/// lets a process grow until something stops it.
#[tokio::test]
#[ignore = "needs a daemon with the runtime registered, and SAKALA_SMOKE_CONTAINER_RUNTIME"]
async fn a_workload_runs_under_the_configured_runtime() {
    let runtime =
        std::env::var("SAKALA_SMOKE_CONTAINER_RUNTIME").expect("SAKALA_SMOKE_CONTAINER_RUNTIME");
    let limits_enforced = std::env::var_os("SAKALA_SMOKE_LIMITS_ENFORCED").is_some();
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let temp = TempDir::new().expect("temp directory");

    // A name Docker does not know stops the node before it takes any work.
    let refused = DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            railpack_image: "sha256:unused-by-this-test".to_owned(),
            workspace_root: temp.path().join("builds"),
            caddy_sites_dir: temp.path().join("sites"),
            caddy_container: ROUTER.to_owned(),
            runtime_network: NETWORK.to_owned(),
            railpack_network: SANDBOX_NETWORK.to_owned(),
            container_runtime: Some("sakala-agent-not-registered".to_owned()),
            ..DockerRuntimeConfig::default()
        },
        Arc::new(TokioProcessRunner::default()),
    );
    let report = refused.preflight().await.expect("preflight");
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "container-runtime")
        .expect("check");
    println!("unregistered runtime: {}", check.detail);
    assert!(check.fatal && !check.ready, "{check:?}");

    let harness =
        Harness::start_configured(&temp, "sha256:unused-by-this-test", None, Some(&runtime)).await;
    let report = harness.executor.preflight().await.expect("preflight");
    for check in report
        .checks
        .iter()
        .filter(|check| check.name.starts_with("container-"))
    {
        println!(
            "preflight {}: ready={} fatal={} {}",
            check.name, check.ready, check.fatal, check.detail
        );
    }
    let telemetry = harness.executor.node_telemetry().await.expect("telemetry");
    println!(
        "heartbeat: container_runtime={} limits_enforced={} operational={:?}",
        telemetry.runtime_dependencies["container_runtime"],
        telemetry.runtime_dependencies["container_runtime_limits_enforced"],
        telemetry.runtime_operational
    );
    assert_eq!(telemetry.runtime_dependencies["container_runtime"], runtime);

    // The route only activates after the router got an HTTP answer from it.
    let run = harness
        .deploy_with_memory(
            "undergvisor",
            &format!("echo hello-from-the-app; {}", serve(200)),
            256,
            &[],
        )
        .await;
    run.print("app under the configured runtime");
    run.assert_completed();
    run.assert_logged("[runtime] hello-from-the-app");
    let name = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{}", run.project_prefix()),
        "--format",
        "{{.Names}}",
    ]);
    let name = name.trim();
    let config = docker(&[
        "inspect",
        "--format",
        "runtime={{.HostConfig.Runtime}} memory={{.HostConfig.Memory}} pids={{.HostConfig.PidsLimit}} cpus={{.HostConfig.NanoCpus}}",
        name,
    ]);
    println!("container: {}", config.trim());
    assert!(config.contains(&format!("runtime={runtime} ")), "{config}");
    let kernel = docker(&["exec", name, "dmesg"]);
    println!("dmesg: {}", kernel.lines().next().unwrap_or_default());
    assert!(kernel.contains("gVisor"), "not under gVisor: {kernel}");

    // Every lifecycle operation waits for the app to answer through the router again.
    harness
        .executor
        .restart_project(run.lifecycle(), harness.reporter())
        .await
        .expect("restart");
    println!("restart: answered");
    harness
        .executor
        .sleep_project(run.lifecycle(), harness.reporter())
        .await
        .expect("sleep");
    let status = docker(&["inspect", "--format", "{{.State.Status}}", name]);
    assert_eq!(status.trim(), "exited");
    harness
        .executor
        .wake_project(run.lifecycle(), harness.reporter())
        .await
        .expect("wake");
    println!("sleep and wake: answered");
    let (status, crashed) = harness.crash_state_of(&run).await;
    println!("health snapshot: {status}, crashed: {crashed:?}");
    assert!(status.starts_with("Up"), "{status}");

    let run = harness
        .deploy("crashes", "echo about-to-exit; exit 5", &[])
        .await;
    run.print("app that exits under the configured runtime");
    run.assert_failed("runtime_app_crashed", &["5"]);
    run.assert_logged("[runtime] about-to-exit");

    if limits_enforced {
        let run = harness
            .deploy_with_memory(
                "oom",
                "echo allocating; exec node -e 'const a=[];for(;;)a.push(Buffer.alloc(8<<20,1))'",
                128,
                &[],
            )
            .await;
        run.print("app over its memory limit");
        run.assert_failed("runtime_app_out_of_memory", &["128 MB"]);
    } else {
        println!("memory limit not exercised: this daemon does not enforce it");
    }
    harness.executor.shutdown().await.expect("shutdown");
}

/// Railpack in its sandbox, end to end, on a real repository.
#[tokio::test]
#[ignore = "needs a local Docker daemon, network access, and moby/buildkit:buildx-stable-1"]
async fn a_real_repository_deploys_with_railpack_in_the_sandbox() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let image = build_sandbox_image();
    start_host_network_builder();
    let commit = String::from_utf8_lossy(
        &Command::new("git")
            .args([
                "ls-remote",
                "https://github.com/heroku/node-js-getting-started.git",
                "HEAD",
            ])
            .output()
            .expect("git")
            .stdout,
    )
    .split_whitespace()
    .next()
    .expect("commit")
    .to_owned();
    println!("heroku/node-js-getting-started at {commit}");

    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start_with(&temp, &image, Some(HOST_NETWORK_BUILDER)).await;
    harness.runner.use_real_git();

    let inspection = harness
        .inspect(
            "https://github.com/heroku/node-js-getting-started.git",
            &commit,
        )
        .await;
    println!(
        "inspection: {}",
        inspection["railpack"]["detectedProviders"]
    );
    assert_eq!(inspection["railpack"]["detectedProviders"][0], "node");

    let run = harness
        .run_custom(
            "herokunode",
            json!({
                "repository_url": "https://github.com/heroku/node-js-getting-started.git",
                "commit_sha": commit,
                "builder": "auto",
                "timeouts": {
                    "build_timeout_seconds": 590,
                    "start_timeout_seconds": 60,
                    "command_timeout_seconds": 800
                },
            }),
            CancellationToken::new(),
        )
        .await;
    println!("elapsed: {:.1} s", run.elapsed.as_secs_f64());
    println!("stages: {:?}", run.stages);
    if let Some(failure) = &run.failure {
        for line in run.logs.iter().rev().take(40).rev() {
            println!("log: {line}");
        }
        panic!("deployment failed: {failure}");
    }
    assert!(
        run.stages
            .iter()
            .any(|stage| stage == "deployment.analysis.started")
    );
    let (status, _) = harness.crash_state_of(&run).await;
    println!("container status: {status}");
    assert!(status.starts_with("Up"), "{status}");
    harness.executor.shutdown().await.expect("shutdown");
}

/// Laravel at a fixed commit, built by Railpack and started by Railpack's own
/// `/start-container.sh`, which ends in FrankenPHP. The FrankenPHP binary carries
/// the file capability `cap_net_bind_service=ep`, and the kernel refuses to exec
/// such a binary when the capability is missing from the bounding set, so with
/// `--cap-drop ALL` alone every Laravel app exited 126 with
/// `exec: frankenphp: Operation not permitted`.
#[tokio::test]
#[ignore = "needs a local Docker daemon, network access, and moby/buildkit:buildx-stable-1"]
async fn a_laravel_app_boots_with_the_railpack_start_command() {
    const REPOSITORY: &str = "https://github.com/laravel/laravel.git";
    const COMMIT: &str = "def7ce14811161f3c689a71ca8abc1369da44bd6";
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let image = build_sandbox_image();
    start_host_network_builder();

    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start_with(&temp, &image, Some(HOST_NETWORK_BUILDER)).await;
    harness.runner.use_real_git();
    let run = harness
        .run_payload(
            "laravel",
            Uuid::new_v4(),
            Uuid::new_v4(),
            512,
            json!({
                "APP_KEY": "base64:2fl+Ktvkfl+Fuz4Qp/A75G2RTiWVA/ZoKZvp6fiiM10=",
                "APP_ENV": "production",
                "LOG_CHANNEL": "stderr",
                "DB_CONNECTION": "sqlite",
                "SESSION_DRIVER": "file",
                "CACHE_STORE": "file",
                "RAILPACK_SKIP_MIGRATIONS": "true",
            }),
            json!({
                "repository_url": REPOSITORY,
                "commit_sha": COMMIT,
                "builder": "auto",
                "timeouts": {
                    "build_timeout_seconds": 590,
                    "start_timeout_seconds": 90,
                    "command_timeout_seconds": 800
                },
            }),
            CancellationToken::new(),
        )
        .await;
    println!("elapsed: {:.1} s", run.elapsed.as_secs_f64());
    println!("stages: {:?}", run.stages);
    if let Some(failure) = &run.failure {
        for line in run.logs.iter().rev().take(40).rev() {
            println!("log: {line}");
        }
        panic!("deployment failed: {failure}");
    }
    let (status, _) = harness.crash_state_of(&run).await;
    println!("container status: {status}");
    assert!(status.starts_with("Up"), "{status}");

    let container = docker(&[
        "ps",
        "--filter",
        &format!("label=dev.sakala.project-id={}", run.project_id),
        "--format",
        "{{.Names}}",
    ])
    .trim()
    .to_owned();
    let capabilities = docker(&[
        "inspect",
        "--format",
        "{{json .HostConfig.CapAdd}} {{json .HostConfig.CapDrop}} {{json .HostConfig.SecurityOpt}}",
        &container,
    ]);
    println!("container options: {}", capabilities.trim());
    // Evidence only: read the capability from the image's own file.
    let frankenphp = Command::new("docker")
        .args([
            "exec",
            &container,
            "sh",
            "-c",
            "getcap \"$(command -v frankenphp)\" 2>&1 || echo getcap unavailable",
        ])
        .output()
        .expect("docker");
    println!(
        "frankenphp file capability: {}",
        String::from_utf8_lossy(&frankenphp.stdout).trim()
    );
    // Laravel's own health route, through the router as a visitor would reach it.
    let health = docker(&[
        "exec",
        ROUTER,
        "wget",
        "-q",
        "-O-",
        &format!("http://{container}:3000/up"),
    ]);
    assert!(health.contains("Application up"), "{health}");
    assert_no_platform_details(&run, &temp);
    harness.executor.shutdown().await.expect("shutdown");
}

/// A static site through Railpack's staticfile provider, which serves it with a
/// Caddy binary of its own.
#[tokio::test]
#[ignore = "needs a local Docker daemon, network access, and moby/buildkit:buildx-stable-1"]
async fn a_static_site_deploys_with_railpack() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    start_router();
    let image = build_sandbox_image();
    start_host_network_builder();

    let temp = TempDir::new().expect("temp directory");
    let harness = Harness::start_with(&temp, &image, Some(HOST_NETWORK_BUILDER)).await;
    harness.runner.use_real_build(vec![(
        "index.html".to_owned(),
        "<h1>static site served</h1>\n".to_owned(),
    )]);
    let run = harness
        .run_custom(
            "static",
            json!({
                "builder": "railpack",
                "timeouts": {
                    "build_timeout_seconds": 590,
                    "start_timeout_seconds": 60,
                    "command_timeout_seconds": 800
                },
            }),
            CancellationToken::new(),
        )
        .await;
    println!("elapsed: {:.1} s", run.elapsed.as_secs_f64());
    if let Some(failure) = &run.failure {
        for line in run.logs.iter().rev().take(40).rev() {
            println!("log: {line}");
        }
        panic!("deployment failed: {failure}");
    }
    let container = docker(&[
        "ps",
        "--filter",
        &format!("label=dev.sakala.project-id={}", run.project_id),
        "--format",
        "{{.Names}}",
    ])
    .trim()
    .to_owned();
    let page = docker(&[
        "exec",
        ROUTER,
        "wget",
        "-q",
        "-O-",
        &format!("http://{container}:3000/"),
    ]);
    assert!(page.contains("static site served"), "{page}");
    assert_no_platform_details(&run, &temp);
    harness.executor.shutdown().await.expect("shutdown");
}

/// What the deployment's own log must not carry: the shared router's output and
/// the node's paths.
fn assert_no_platform_details(run: &Deployment, temp: &TempDir) {
    let caddy = run
        .logs
        .iter()
        .filter(|line| line.starts_with("[caddy-"))
        .collect::<Vec<_>>();
    assert!(
        caddy.is_empty(),
        "router output reached the tenant: {caddy:?}"
    );
    let node_path = temp.path().to_string_lossy();
    if let Some(at) = run.sent.find(node_path.as_ref()) {
        let start = run.sent[..at]
            .char_indices()
            .rev()
            .nth(200)
            .map_or(0, |(index, _)| index);
        let end = (at + node_path.len() + 200).min(run.sent.len());
        let end = (end..=run.sent.len())
            .find(|index| run.sent.is_char_boundary(*index))
            .unwrap_or(run.sent.len());
        panic!(
            "a node path reached the control plane: {}",
            &run.sent[start..end]
        );
    }
}

/// Image GC against the real daemon. Each deployment image keeps its own tag, so
/// none becomes dangling, and the old `docker image prune` without `-a` removed
/// nothing; the old listing format did not even parse.
#[tokio::test]
#[ignore = "needs a local Docker daemon and node:24-alpine"]
async fn image_gc_removes_old_project_images_and_keeps_active_and_previous() {
    let _serial = SERIAL.lock().await;
    let _cleanup = Cleanup;
    Cleanup::remove_everything();
    // GC acts on every Sakala image on the daemon, so it runs only where the
    // test owns all of them.
    let foreign = docker(&[
        "image",
        "ls",
        "--filter",
        "label=dev.sakala.managed=true",
        "--format",
        "{{.Repository}}",
    ]);
    assert!(
        foreign.trim().is_empty(),
        "refusing to run: this daemon holds Sakala images the test does not own: {foreign}"
    );
    let project = Uuid::new_v4();
    let other = Uuid::new_v4();
    let commit = |name: &str, labels: &str| {
        let source = format!("{PREFIX}-gc-source");
        docker(&[
            "create",
            "--name",
            &source,
            "--pull=never",
            APP_IMAGE,
            "true",
        ]);
        let image = name.to_owned();
        docker(&["commit", "--change", labels, &source, &image]);
        docker(&["rm", "-f", "-v", &source]);
        // Distinct creation times, which decide what "previous" means.
        std::thread::sleep(Duration::from_millis(1_100));
        image
    };
    let owned = |project: Uuid, deployment: &str| {
        format!(
            "LABEL dev.sakala.managed=true dev.sakala.project-id={project} dev.sakala.deployment-id={}",
            Uuid::new_v4()
                .to_string()
                .replace(&Uuid::nil().to_string()[..8], deployment)
        )
    };
    let oldest = commit(
        &format!("sakala/project-{project}:{PREFIX}-oldest"),
        &owned(project, "1"),
    );
    let previous = commit(
        &format!("sakala/project-{project}:{PREFIX}-previous"),
        &owned(project, "2"),
    );
    let active = commit(
        &format!("sakala/project-{project}:{PREFIX}-active"),
        &owned(project, "3"),
    );
    let abandoned = commit(
        &format!("sakala/project-{project}:{PREFIX}-abandoned"),
        &owned(project, "4"),
    );
    let other_old = commit(
        &format!("sakala/project-{other}:{PREFIX}-old"),
        &owned(other, "5"),
    );
    let other_newest = commit(
        &format!("sakala/project-{other}:{PREFIX}-newest"),
        &owned(other, "6"),
    );
    let unmanaged = commit(&format!("{PREFIX}/unmanaged:latest"), "LABEL dev.example=1");
    docker(&[
        "run",
        "-d",
        "--name",
        &format!("{PREFIX}-gc-active"),
        &active,
        "sleep",
        "300",
    ]);

    let temp = TempDir::new().expect("temp directory");
    let config = DockerRuntimeConfig {
        agent_id: format!("{PREFIX}-agent"),
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        image_gc_max_age: Duration::ZERO,
        // `other` has no container, and its images are younger than this.
        orphaned_image_grace: Duration::from_secs(3_600),
        ..DockerRuntimeConfig::default()
    };
    std::fs::create_dir_all(&config.workspace_root).expect("workspace");
    std::fs::create_dir_all(&config.caddy_sites_dir).expect("sites");
    let executor = DockerRuntimeExecutor::with_runner(
        config,
        Arc::new(TokioProcessRunner::new(Duration::from_secs(60))),
    );
    let report = executor.reconcile().await.expect("reconcile");
    let mut reported = report
        .stale_images
        .iter()
        .map(|image| image.image_id.clone())
        .collect::<Vec<_>>();
    reported.sort();
    let id_of = |image: &str| {
        docker(&["image", "inspect", "--format", "{{.Id}}", image])
            .trim()
            .to_owned()
    };
    let mut expected = [&oldest, &abandoned]
        .iter()
        .map(|image| id_of(image))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(reported, expected, "reconcile reports what cleanup removes");

    let (server, _requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let output = executor
        .cleanup_runtime(
            CleanupRuntimeRequest {
                command_id: Uuid::new_v4(),
                approved: true,
                targets: vec![RuntimeCleanupTarget::StaleImages],
                cancellation: CancellationToken::new(),
            },
            Arc::new(ApiRuntimeReporter::new(
                client,
                Uuid::new_v4(),
                LogBounds::default(),
            )),
        )
        .await
        .expect("cleanup");
    println!("cleanup result: {}", output.result);
    assert_eq!(output.result["removed_images"], 2);

    let exists = |image: &str| {
        Command::new("docker")
            .args(["image", "inspect", image])
            .output()
            .expect("docker")
            .status
            .success()
    };
    for image in [&oldest, &abandoned] {
        assert!(!exists(image), "{image} should have been removed");
    }
    for image in [&previous, &active, &other_old, &other_newest, &unmanaged] {
        assert!(exists(image), "{image} must be kept");
    }
}

/// Containers and helper images of the project-removal smoke test start with
/// this. Its deployment images are `sakala/project-<random id>:sakala-imgfix-*`,
/// which no other work on the daemon can share.
const IMGFIX: &str = "sakala-imgfix";

/// Removes only what the project-removal smoke test created.
struct ImgfixCleanup {
    projects: Vec<Uuid>,
}

impl ImgfixCleanup {
    fn run(&self) {
        let list = |args: &[&str]| {
            Command::new("docker")
                .args(args)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default()
        };
        let filter = format!("name=^/{IMGFIX}-");
        for name in list(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}}"]).lines() {
            assert!(
                name.starts_with(&format!("{IMGFIX}-")),
                "refusing to remove {name}"
            );
            Command::new("docker")
                .args(["rm", "-f", "-v", name])
                .output()
                .ok();
        }
        for image in list(&["images", "--format", "{{.Repository}}:{{.Tag}}"]).lines() {
            let ours = image.starts_with(&format!("{IMGFIX}/"))
                || self.projects.iter().any(|project| {
                    image.starts_with(&format!("sakala/project-{project}:{IMGFIX}-"))
                });
            if ours {
                Command::new("docker")
                    .args(["image", "rm", "-f", image])
                    .output()
                    .ok();
            }
        }
    }
}

impl Drop for ImgfixCleanup {
    fn drop(&mut self) {
        self.run();
    }
}

/// Deleting a project used to leave its last images on the node for good.
/// The API now asks for `remove_images` on that stop; the other project, and
/// every image outside `sakala/project-<id>`, must stay.
#[tokio::test]
#[ignore = "needs a local Docker daemon and node:24-alpine"]
async fn deleting_a_project_removes_its_images_and_leaves_the_other_project_alone() {
    let deleted = Uuid::new_v4();
    let kept = Uuid::new_v4();
    let orphan = Uuid::new_v4();
    let cleanup = ImgfixCleanup {
        projects: vec![deleted, kept, orphan],
    };
    cleanup.run();
    let commit = |image: &str, labels: &str| {
        let source = format!("{IMGFIX}-source");
        docker(&[
            "create",
            "--name",
            &source,
            "--pull=never",
            APP_IMAGE,
            "true",
        ]);
        docker(&["commit", "--change", labels, &source, image]);
        docker(&["rm", "-f", "-v", &source]);
        image.to_owned()
    };
    let owned = |project: Uuid| {
        format!(
            "LABEL dev.sakala.managed=true dev.sakala.project-id={project} dev.sakala.deployment-id={}",
            Uuid::new_v4()
        )
    };
    let deployment = |project: Uuid, tag: &str| format!("sakala/project-{project}:{IMGFIX}-{tag}");
    let deleted_previous = commit(&deployment(deleted, "previous"), &owned(deleted));
    let deleted_active = commit(&deployment(deleted, "active"), &owned(deleted));
    let kept_previous = commit(&deployment(kept, "previous"), &owned(kept));
    let kept_active = commit(&deployment(kept, "active"), &owned(kept));
    // Labelled like the deleted project, but named elsewhere or not managed.
    let foreign_name = commit(&format!("{IMGFIX}/retagged:latest"), &owned(deleted));
    let unmanaged = commit(
        &format!("{IMGFIX}/unmanaged:latest"),
        &format!("LABEL dev.sakala.project-id={deleted}"),
    );
    let deleted_deployment = Uuid::new_v4();
    let workload = |name: &str, project: Uuid, deployment: Uuid, image: &str| {
        docker(&[
            "run",
            "-d",
            "--name",
            &format!("{IMGFIX}-{name}"),
            "--label",
            "dev.sakala.managed=true",
            "--label",
            &format!("dev.sakala.project-id={project}"),
            "--label",
            &format!("dev.sakala.deployment-id={deployment}"),
            "--label",
            "dev.sakala.domain=imgfix.test",
            "--label",
            "dev.sakala.port=3000",
            image,
            "sleep",
            "300",
        ]);
    };
    workload("deleted", deleted, deleted_deployment, &deleted_active);
    workload("kept", kept, Uuid::new_v4(), &kept_active);

    let temp = TempDir::new().expect("temp directory");
    let config = DockerRuntimeConfig {
        agent_id: format!("{IMGFIX}-agent"),
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        orphaned_image_grace: Duration::from_secs(1),
        ..DockerRuntimeConfig::default()
    };
    std::fs::create_dir_all(&config.workspace_root).expect("workspace");
    std::fs::create_dir_all(&config.caddy_sites_dir).expect("sites");
    let executor = DockerRuntimeExecutor::with_runner(
        config,
        Arc::new(TokioProcessRunner::new(Duration::from_secs(60))),
    );
    let (server, _requests) = recording_api().await;
    let client = ApiClient::new(server.uri(), "smoke", "test-agent-token").expect("client");
    let reporter = Arc::new(ApiRuntimeReporter::new(
        client,
        Uuid::new_v4(),
        LogBounds::default(),
    ));
    let exists = |reference: &str| {
        Command::new("docker")
            .args(["image", "inspect", reference])
            .output()
            .expect("docker")
            .status
            .success()
    };
    let container_exists = |name: &str| {
        Command::new("docker")
            .args(["container", "inspect", &format!("{IMGFIX}-{name}")])
            .output()
            .expect("docker")
            .status
            .success()
    };

    let output = executor
        .stop_project(
            WorkloadLifecycleRequest {
                command_id: Uuid::new_v4(),
                project_id: deleted,
                deployment_id: deleted_deployment,
                remove_images: true,
                cancellation: CancellationToken::new(),
            },
            reporter,
        )
        .await
        .expect("stop of the deleted project");
    println!("stop result: {}", output.result);

    assert_eq!(output.result["status"], "stopped");
    assert_eq!(output.result["removed_images"], 2);
    assert!(!container_exists("deleted"));
    for image in [&deleted_previous, &deleted_active] {
        assert!(!exists(image), "{image} should have been removed");
    }
    for image in [&kept_previous, &kept_active, &foreign_name, &unmanaged] {
        assert!(exists(image), "{image} must be kept");
    }
    assert!(container_exists("kept"));

    // The periodic GC removes a project with no container once its images pass
    // the grace period. It acts on every Sakala image of the daemon, so it only
    // runs where the test owns all of them.
    let orphan_image = commit(&deployment(orphan, "only"), &owned(orphan));
    let managed = docker(&[
        "image",
        "ls",
        "--filter",
        "label=dev.sakala.managed=true",
        "--format",
        "{{.Repository}}:{{.Tag}}",
    ]);
    let foreign = managed
        .lines()
        .filter(|image| {
            !image.starts_with(&format!("{IMGFIX}/"))
                && ![kept, orphan]
                    .iter()
                    .any(|project| image.starts_with(&format!("sakala/project-{project}:")))
        })
        .collect::<Vec<_>>();
    if !foreign.is_empty() {
        println!("skipping the periodic GC part: foreign Sakala images {foreign:?}");
        return;
    }
    std::thread::sleep(Duration::from_millis(1_500));
    let collected = executor
        .collect_stale_images()
        .await
        .expect("periodic image GC");
    println!(
        "periodic GC removed {} image(s), {} bytes",
        collected.removed_images, collected.reclaimed_bytes
    );
    assert_eq!(collected.removed_images, 1);
    assert!(
        !exists(&orphan_image),
        "{orphan_image} should have been removed"
    );
    for image in [&kept_previous, &kept_active, &foreign_name, &unmanaged] {
        assert!(exists(image), "{image} must be kept");
    }
    drop(cleanup);
}

/// Downloads inside the default BuildKit stall on some machines.
fn start_host_network_builder() {
    docker(&[
        "buildx",
        "create",
        "--name",
        HOST_NETWORK_BUILDER,
        "--driver",
        "docker-container",
        "--driver-opt",
        "network=host",
    ]);
    docker(&[
        "buildx",
        "inspect",
        "--bootstrap",
        "--builder",
        HOST_NETWORK_BUILDER,
    ]);
}

async fn recording_api() -> (MockServer, Arc<Mutex<Vec<(String, Value)>>>) {
    let server = MockServer::start().await;
    let requests: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let recorded = Arc::clone(&requests);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let count = ["events", "logs"]
                .iter()
                .find_map(|key| body[*key].as_array().map(Vec::len));
            recorded
                .lock()
                .expect("request lock")
                .push((request.url.path().to_owned(), body));
            match count {
                Some(count) => ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "accepted_count": count,
                        "duplicate_count": 0,
                        "first_sequence": 1,
                        "last_sequence": count,
                    }
                })),
                None => ResponseTemplate::new(204),
            }
        })
        .mount(&server)
        .await;
    (server, requests)
}

struct Harness {
    server: MockServer,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    executor: Arc<dyn RuntimeExecutor>,
    runner: Arc<SmokeRunner>,
}

#[derive(Clone)]
struct Deployment {
    name: String,
    project_id: Uuid,
    deployment_id: Uuid,
    elapsed: Duration,
    failure: Option<Value>,
    logs: Vec<String>,
    sent: String,
    stages: Vec<String>,
}

impl Deployment {
    fn print(&self, title: &str) {
        println!("\n--- {title} [{}]", self.name);
        println!("elapsed: {:.1} s", self.elapsed.as_secs_f64());
        match &self.failure {
            Some(failure) => println!("fail body: {failure}"),
            None => println!("completed"),
        }
        for line in &self.logs {
            println!("log: {line}");
        }
    }

    fn assert_completed(&self) {
        assert!(self.failure.is_none(), "{}: {:?}", self.name, self.failure);
    }

    fn assert_failed(&self, code: &str, fragments: &[&str]) {
        let failure = self
            .failure
            .as_ref()
            .unwrap_or_else(|| panic!("{} should fail", self.name));
        assert_eq!(failure["error_code"], code, "{failure}");
        let message = failure["error_message"].as_str().expect("message");
        for fragment in fragments {
            assert!(message.contains(fragment), "{message}");
        }
    }

    fn assert_logged(&self, line: &str) {
        assert!(
            self.logs.iter().any(|logged| logged == line),
            "{line} missing from {:?}",
            self.logs
        );
    }

    fn project_prefix(&self) -> String {
        format!("{APP_NAME_PREFIX}{}", &self.project_id.to_string()[..8])
    }

    fn lifecycle(&self) -> WorkloadLifecycleRequest {
        WorkloadLifecycleRequest {
            command_id: Uuid::new_v4(),
            project_id: self.project_id,
            deployment_id: self.deployment_id,
            remove_images: false,
            cancellation: CancellationToken::new(),
        }
    }

    async fn with_environment(self, harness: &Harness, environment: Value) -> Self {
        harness
            .run_command(&self.name, Uuid::new_v4(), Uuid::new_v4(), 256, environment)
            .await
    }
}

impl Harness {
    async fn start(temp: &TempDir) -> Self {
        Self::start_with(temp, "sha256:unused-by-this-test", None).await
    }

    async fn start_with(temp: &TempDir, railpack_image: &str, builder: Option<&str>) -> Self {
        Self::start_configured(temp, railpack_image, builder, None).await
    }

    async fn start_configured(
        temp: &TempDir,
        railpack_image: &str,
        builder: Option<&str>,
        container_runtime: Option<&str>,
    ) -> Self {
        let (server, requests) = recording_api().await;
        let runner = Arc::new(SmokeRunner {
            builder: builder.map(str::to_owned),
            ..SmokeRunner::default()
        });
        std::fs::create_dir_all(temp.path().join("builds")).expect("workspace");
        let config = DockerRuntimeConfig {
            agent_id: format!("{PREFIX}-agent"),
            workspace_root: temp.path().join("builds"),
            caddy_sites_dir: temp.path().join("sites"),
            caddy_container: ROUTER.to_owned(),
            runtime_network: NETWORK.to_owned(),
            min_workspace_free_bytes: 0,
            health_attempts: 4,
            health_interval: Duration::from_millis(500),
            crash_stable_window: STABLE_WINDOW,
            railpack_image: railpack_image.to_owned(),
            railpack_network: SANDBOX_NETWORK.to_owned(),
            container_runtime: container_runtime.map(str::to_owned),
            ..DockerRuntimeConfig::default()
        };
        let executor: Arc<dyn RuntimeExecutor> = Arc::new(DockerRuntimeExecutor::with_runner(
            config,
            runner.clone() as Arc<dyn ProcessRunner>,
        ));
        let preflight = executor.preflight().await.expect("preflight");
        let probe_check = preflight
            .checks
            .iter()
            .find(|check| check.name == "caddy-http-probe")
            .expect("probe check");
        println!("preflight caddy-http-probe ready: {}", probe_check.ready);
        assert!(probe_check.ready);
        Self {
            server,
            requests,
            executor,
            runner,
        }
    }

    async fn deploy(&self, name: &str, script: &str, extra: &[&str]) -> Deployment {
        self.deploy_with_memory(name, script, 256, extra).await
    }

    async fn deploy_with_memory(
        &self,
        name: &str,
        script: &str,
        memory_mb: u64,
        extra: &[&str],
    ) -> Deployment {
        prepare_image(name, script, extra);
        self.runner.select_image(name);
        self.run_command(name, Uuid::new_v4(), Uuid::new_v4(), memory_mb, json!({}))
            .await
    }

    async fn run_command(
        &self,
        name: &str,
        project_id: Uuid,
        deployment_id: Uuid,
        memory_mb: u64,
        environment: Value,
    ) -> Deployment {
        self.run_payload(
            name,
            project_id,
            deployment_id,
            memory_mb,
            environment,
            json!({}),
            CancellationToken::new(),
        )
        .await
    }

    async fn inspect(&self, repository_url: &str, commit: &str) -> Value {
        let command: AgentCommand = serde_json::from_value(json!({
            "id": Uuid::new_v4(),
            "type": "InspectProject",
            "status": "Pending",
            "project_id": Uuid::new_v4(),
            "deployment_id": null,
            "payload": { "repository_url": repository_url, "commit_sha": commit },
        }))
        .expect("command");
        self.requests.lock().expect("request lock").clear();
        let client =
            ApiClient::new(self.server.uri(), "smoke", "test-agent-token").expect("client");
        CommandProcessor::new(client, Arc::clone(&self.executor), Duration::from_secs(600))
            .process(&command, CancellationToken::new())
            .await
            .expect("inspection");
        let requests = self.requests.lock().expect("request lock");
        requests
            .iter()
            .find(|(path, _)| path.ends_with("/complete"))
            .map(|(_, body)| body["result"].clone())
            .expect("inspection result")
    }

    async fn run_custom(
        &self,
        name: &str,
        extra: Value,
        cancellation: CancellationToken,
    ) -> Deployment {
        self.run_payload(
            name,
            Uuid::new_v4(),
            Uuid::new_v4(),
            256,
            json!({}),
            extra,
            cancellation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_payload(
        &self,
        name: &str,
        project_id: Uuid,
        deployment_id: Uuid,
        memory_mb: u64,
        environment: Value,
        extra: Value,
        cancellation: CancellationToken,
    ) -> Deployment {
        self.requests.lock().expect("request lock").clear();
        let mut command: AgentCommand = serde_json::from_value(json!({
            "id": Uuid::new_v4(),
            "type": "DeployProject",
            "status": "Pending",
            "project_id": project_id,
            "deployment_id": deployment_id,
            "payload": {
                "repository_url": "https://github.com/gmedia/example-app.git",
                "commit_sha": "0123456789abcdef0123456789abcdef01234567",
                "domain": format!("{name}.run.sakala.localhost"),
                "container_port": 3000,
                "environment": environment,
                "resources": { "memory_mb": memory_mb, "cpu_millis": 500, "pids_limit": 128 },
                "timeouts": {
                    "build_timeout_seconds": 30,
                    "start_timeout_seconds": 6,
                    "command_timeout_seconds": 60
                },
                "log_bounds": { "max_line_length": 4096, "max_batch_lines": 500, "max_total_bytes": 1048576 }
            }
        }))
        .expect("command");
        for (key, value) in extra.as_object().expect("extra payload") {
            command.payload[key] = value.clone();
        }
        let client =
            ApiClient::new(self.server.uri(), "smoke", "test-agent-token").expect("client");
        let processor =
            CommandProcessor::new(client, Arc::clone(&self.executor), Duration::from_secs(900));
        let started = Instant::now();
        let _ = processor.process(&command, cancellation).await;
        let elapsed = started.elapsed();
        let requests = self.requests.lock().expect("request lock").clone();
        let logs = requests
            .iter()
            .filter(|(path, _)| path.ends_with("/logs"))
            .flat_map(|(_, body)| body["logs"].as_array().cloned().unwrap_or_default())
            .filter_map(|log| log["message"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        Deployment {
            name: name.to_owned(),
            project_id,
            deployment_id,
            elapsed,
            failure: requests
                .iter()
                .find(|(path, _)| path.ends_with("/fail"))
                .map(|(_, body)| body.clone()),
            logs,
            stages: requests
                .iter()
                .filter(|(path, _)| path.ends_with("/events"))
                .flat_map(|(_, body)| body["events"].as_array().cloned().unwrap_or_default())
                .filter_map(|event| event["type"].as_str().map(str::to_owned))
                .filter(|stage| stage.ends_with(".started") || stage.ends_with(".ready"))
                .collect(),
            sent: serde_json::to_string(&requests).expect("requests serialize"),
        }
    }

    fn reporter(&self) -> Arc<dyn RuntimeReporter> {
        let client =
            ApiClient::new(self.server.uri(), "smoke", "test-agent-token").expect("client");
        Arc::new(ApiRuntimeReporter::new(
            client,
            Uuid::new_v4(),
            LogBounds::default(),
        ))
    }

    /// The Docker status of a workload and its entry in the crash report.
    async fn crash_state_of(&self, run: &Deployment) -> (String, Option<CrashedWorkloadDetail>) {
        let snapshots = self.executor.health_snapshot().await.expect("snapshot");
        let status = snapshots
            .iter()
            .find(|snapshot| snapshot.workload.project_id == run.project_id)
            .map_or_else(String::new, |snapshot| snapshot.workload.status.clone());
        let crashed = self
            .executor
            .crashed_workloads(&snapshots)
            .await
            .expect("crash details");
        let detail = crashed
            .into_iter()
            .find(|detail| detail.project_id == run.project_id);
        (status, detail)
    }

    async fn wait_for_running_crash_loop(&self, run: &Deployment) -> CrashedWorkloadDetail {
        for _ in 0..160 {
            if let (_, Some(detail)) = self.crash_state_of(run).await
                && detail.running
            {
                return detail;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("{} never reached a reported crash loop", run.name);
    }

    async fn wait_for_crash(&self, crashing: &Deployment) -> Vec<CrashedWorkloadDetail> {
        for _ in 0..60 {
            let snapshots = self.executor.health_snapshot().await.expect("snapshot");
            let crashed = self
                .executor
                .crashed_workloads(&snapshots)
                .await
                .expect("crash details");
            if crashed
                .iter()
                .any(|detail| detail.project_id == crashing.project_id)
            {
                return crashed;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("the crash was never reported");
    }
}

/// Real processes, except that Git and image builds are replaced, and every
/// name the runtime generates is rewritten into the `sakala-agent-sm` namespace.
#[derive(Default)]
struct SmokeRunner {
    image: Mutex<String>,
    /// Repository files for a real build. Without them a build only tags an image.
    real_build_files: Mutex<Option<Vec<(String, String)>>>,
    /// Checks out and builds for real, with nothing replaced.
    real_git: std::sync::atomic::AtomicBool,
    /// Buildx builder for real builds.
    builder: Option<String>,
}

impl SmokeRunner {
    fn use_real_git(&self) {
        self.real_git
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn use_real_build(&self, files: Vec<(String, String)>) {
        *self.real_build_files.lock().expect("files lock") = Some(files);
    }

    fn select_image(&self, name: &str) {
        *self.image.lock().expect("image lock") = format!("{PREFIX}/{name}:latest");
    }
}

#[async_trait]
impl ProcessRunner for SmokeRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let real_git = self.real_git.load(std::sync::atomic::Ordering::SeqCst);
        if real_git && spec.program == "git" {
            return TokioProcessRunner::new(Duration::from_secs(300))
                .run(spec, sink)
                .await;
        }
        if spec.program == "git" {
            if spec.args.iter().any(|argument| argument == "init") {
                let source = spec.args.last().expect("git init target");
                let source = std::path::Path::new(source);
                std::fs::create_dir_all(source).expect("source");
                std::fs::write(source.join("Dockerfile"), "FROM scratch\n").expect("Dockerfile");
                if let Some(files) = &*self.real_build_files.lock().expect("files lock") {
                    for (path, content) in files {
                        let file = source.join(path);
                        std::fs::create_dir_all(file.parent().expect("parent")).expect("parent");
                        std::fs::write(file, content).expect("file");
                    }
                }
            }
            return Ok(ProcessOutput {
                success: true,
                code: Some(0),
                ..ProcessOutput::default()
            });
        }
        let mut real = spec.clone();
        real.args = spec
            .args
            .iter()
            .map(|argument| {
                argument
                    .to_string_lossy()
                    .replace("sakala-app-", APP_NAME_PREFIX)
                    .replace("sakala-project-", PROJECT_NETWORK_PREFIX)
                    .replace("sakala-agent-railpack-", APP_NAME_PREFIX)
                    .replace("sakala/project-", &format!("{PREFIX}/project-"))
                    .into()
            })
            .collect();
        let replace_build =
            !real_git && self.real_build_files.lock().expect("files lock").is_none();
        if replace_build
            && real.args.iter().any(|argument| argument == "buildx")
            && real.args.iter().any(|argument| argument == "build")
        {
            let position = real
                .args
                .iter()
                .position(|argument| argument == "--tag")
                .expect("build tag");
            let target = real.args[position + 1].clone();
            let source = self.image.lock().expect("image lock").clone();
            real = CommandSpec::new("docker")
                .arg("tag")
                .arg(source)
                .arg(target);
        }
        if let Some(builder) = &self.builder {
            real = real.env("BUILDX_BUILDER", builder.clone());
        }
        // Follows the command's own deadline; real builds outlast the default.
        let mut output = TokioProcessRunner::new(Duration::from_secs(600))
            .run(&real, sink)
            .await?;
        // The runtime compares these names with the ones it generated.
        output.stdout = output
            .stdout
            .replace(PROJECT_NETWORK_PREFIX, "sakala-project-")
            .replace(APP_NAME_PREFIX, "sakala-app-");
        Ok(output)
    }
}

fn prepare_image(name: &str, script: &str, extra: &[&str]) {
    let builder = format!("{PREFIX}-builder-{name}");
    docker(&["create", "--name", &builder, "--pull=never", APP_IMAGE]);
    let image = format!("{PREFIX}/{name}:latest");
    let mut args = vec!["commit".to_owned()];
    for change in [
        "ENTRYPOINT [\"sh\", \"-c\"]".to_owned(),
        format!("CMD {}", json!([script])),
    ]
    .into_iter()
    .chain(extra.iter().map(|change| (*change).to_owned()))
    {
        args.push("--change".to_owned());
        args.push(change);
    }
    args.push(builder.clone());
    args.push(image);
    docker(&args.iter().map(String::as_str).collect::<Vec<_>>());
    docker(&["rm", "-f", "-v", &builder]);
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker should be installed");
    assert!(
        output.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_no_containers_named(prefix: &str) {
    let found = docker(&[
        "ps",
        "-a",
        "--filter",
        &format!("name=^/{prefix}"),
        "--format",
        "{{.Names}}",
    ]);
    assert!(
        found.trim().is_empty(),
        "candidate was not cleaned up: {found}"
    );
}

struct Cleanup;

impl Cleanup {
    fn remove_everything() {
        let quiet = |args: &[&str]| {
            Command::new("docker").args(args).output().ok();
        };
        let list = |args: &[&str]| {
            Command::new("docker")
                .args(args)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default()
        };
        for name in list(&[
            "ps",
            "-a",
            "--filter",
            "name=^/sakala-agent-sm-",
            "--format",
            "{{.Names}}",
        ])
        .lines()
        {
            assert!(
                name.starts_with("sakala-agent-sm-"),
                "refusing to remove {name}"
            );
            quiet(&["rm", "-f", "-v", name]);
        }
        for image in list(&["images", "--format", "{{.Repository}}:{{.Tag}}"]).lines() {
            let deployment_named =
                image.starts_with("sakala/project-") && image.contains(":sakala-agent-sm-");
            if image.starts_with("sakala-agent-sm/") || deployment_named {
                quiet(&["image", "rm", "-f", image]);
            }
        }
        for network in list(&["network", "ls", "--format", "{{.Name}}"]).lines() {
            if network.starts_with(PROJECT_NETWORK_PREFIX) {
                quiet(&["network", "rm", network]);
            }
        }
        quiet(&["network", "rm", NETWORK]);
        quiet(&["network", "rm", SANDBOX_NETWORK]);
        quiet(&["buildx", "rm", "--force", HOST_NETWORK_BUILDER]);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        Self::remove_everything();
    }
}
