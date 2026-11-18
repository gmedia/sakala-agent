//! Proves `workload_metrics` against a real Docker: two replicas of one
//! deployment, one of them busy on a core, are sampled with real CPU, memory,
//! limit, and restart values, while a release container and an unmanaged
//! container on the same node are left out.
//!
//! Opt-in, because it needs a daemon and `busybox:1.37` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test workload_metrics_smoke_test -- --ignored --nocapture
//! ```
//!
//! Set `SAKALA_SMOKE_CONTAINER_RUNTIME=runsc` to start the containers under
//! gVisor on a host where Docker has that runtime registered.
//!
//! Every container it creates starts with `sakala-smmet-`, and it removes only
//! those.

use std::{process::Command, time::Duration};

use sakala_agent_core::ports::RuntimeExecutor;
use sakala_agent_runtime::{DockerRuntimeConfig, DockerRuntimeExecutor};
use tempfile::TempDir;
use uuid::Uuid;

const PREFIX: &str = "sakala-smmet-";
const IMAGE: &str = "busybox:1.37";
const MEMORY_LIMIT: &str = "64m";
const MEMORY_LIMIT_BYTES: u64 = 64 * 1_024 * 1_024;
/// A replica spinning on one core reports well above this.
const BUSY_CPU_PERCENT_FLOOR: f64 = 50.0;
/// Lets the busy loop run before the sample is taken.
const WARM_UP: Duration = Duration::from_secs(2);
const RUNTIME_ENV: &str = "SAKALA_SMOKE_CONTAINER_RUNTIME";

fn docker(args: &[String]) -> (bool, String) {
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

fn remove_smoke_containers() {
    let (_, ids) = docker(&[
        "ps".into(),
        "--all".into(),
        "--quiet".into(),
        "--filter".into(),
        format!("name={PREFIX}"),
    ]);
    for id in ids.split_whitespace() {
        docker(&["rm".into(), "--force".into(), id.into()]);
    }
}

fn run(name: &str, labels: &[String], command: &str) {
    let mut args = vec![
        "run".to_owned(),
        "--detach".to_owned(),
        "--name".to_owned(),
        format!("{PREFIX}{name}"),
        "--memory".to_owned(),
        MEMORY_LIMIT.to_owned(),
    ];
    if let Ok(runtime) = std::env::var(RUNTIME_ENV) {
        args.extend(["--runtime".to_owned(), runtime]);
    }
    for label in labels {
        args.extend(["--label".to_owned(), label.clone()]);
    }
    args.extend([
        IMAGE.to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        command.to_owned(),
    ]);
    let (started, output) = docker(&args);
    assert!(started, "docker run {name} failed: {output}");
}

#[tokio::test]
#[ignore = "needs a local Docker daemon and busybox:1.37"]
async fn real_docker_reports_usage_per_replica_and_skips_other_containers() {
    remove_smoke_containers();
    let project = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let workload = |index: u32| {
        vec![
            "dev.sakala.managed=true".to_owned(),
            "dev.sakala.workload-kind=web".to_owned(),
            format!("dev.sakala.project-id={project}"),
            format!("dev.sakala.deployment-id={deployment}"),
            format!("dev.sakala.replica={index}"),
            "dev.sakala.replicas=2".to_owned(),
        ]
    };
    run("busy", &workload(0), "while :; do :; done");
    run("idle", &workload(1), "sleep 600");
    // A release container never carries the managed label.
    run(
        "release",
        &[
            "dev.sakala.role=release".to_owned(),
            format!("dev.sakala.project-id={project}"),
            format!("dev.sakala.deployment-id={deployment}"),
        ],
        "while :; do :; done",
    );
    run("unmanaged", &[], "sleep 600");
    tokio::time::sleep(WARM_UP).await;

    let temp = TempDir::new().expect("temp directory");
    let executor = DockerRuntimeExecutor::new(DockerRuntimeConfig {
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        ..DockerRuntimeConfig::default()
    });
    let sample = RuntimeExecutor::workload_metrics(&executor).await;
    remove_smoke_containers();

    let metrics = sample
        .expect("the sample succeeds")
        .into_iter()
        .filter(|item| item.project_id == project)
        .collect::<Vec<_>>();
    for item in &metrics {
        println!("{}", serde_json::to_string(item).expect("serializes"));
    }
    assert_eq!(
        metrics.iter().map(|item| item.replica).collect::<Vec<_>>(),
        [0, 1],
        "one entry per replica, release container left out"
    );
    assert!(metrics[0].cpu_percent > BUSY_CPU_PERCENT_FLOOR);
    assert!(metrics[1].cpu_percent < BUSY_CPU_PERCENT_FLOOR);
    for item in &metrics {
        assert_eq!(item.deployment_id, deployment);
        assert!(item.memory_bytes > 0 && item.memory_bytes < MEMORY_LIMIT_BYTES);
        assert_eq!(item.memory_limit_bytes, Some(MEMORY_LIMIT_BYTES));
        assert_eq!(item.restart_count, 0);
        assert_eq!(item.sampled_at.nanosecond(), 0, "whole seconds on the wire");
    }
}
