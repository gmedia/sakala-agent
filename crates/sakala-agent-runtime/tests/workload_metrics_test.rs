//! `RuntimeExecutor::workload_metrics` of the Docker runtime, driven through a
//! scripted process runner that answers the three Docker calls of one sample.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sakala_agent_core::ports::RuntimeExecutor;
use sakala_agent_runtime::{
    CommandSpec, DockerRuntimeConfig, DockerRuntimeExecutor, ProcessOutput, ProcessOutputSink,
    ProcessRunner, RuntimeError,
};
use tempfile::TempDir;

const PROJECT: &str = "ff66ed4a-6303-4be6-8ef4-63c28b112680";
const DEPLOYMENT: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";
const OTHER_PROJECT: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_DEPLOYMENT: &str = "22222222-2222-4222-8222-222222222222";

/// What `docker ps`, `docker stats`, and `docker inspect` answer.
#[derive(Clone, Default)]
struct Script {
    listing: String,
    stats: Option<String>,
    inspect: String,
    inspect_stderr: String,
}

struct ScriptedRunner {
    script: Script,
    commands: Mutex<Vec<Vec<String>>>,
}

impl ScriptedRunner {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            commands: Mutex::new(Vec::new()),
        })
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().expect("command lock").clone()
    }
}

fn output(success: bool, stdout: &str, stderr: &str) -> ProcessOutput {
    ProcessOutput {
        success,
        code: Some(i32::from(!success)),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

#[async_trait]
impl ProcessRunner for ScriptedRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let args = spec
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.commands
            .lock()
            .expect("command lock")
            .push(args.clone());
        Ok(match args.first().map(String::as_str) {
            Some("ps") => output(true, &self.script.listing, ""),
            Some("stats") => match &self.script.stats {
                Some(stats) => output(true, stats, ""),
                None => output(false, "", "Cannot connect to the Docker daemon"),
            },
            Some("inspect") => output(
                self.script.inspect_stderr.is_empty(),
                &self.script.inspect,
                &self.script.inspect_stderr,
            ),
            _ => output(false, "", "unexpected command"),
        })
    }
}

fn executor(runner: Arc<ScriptedRunner>) -> (DockerRuntimeExecutor, TempDir) {
    let temp = TempDir::new().expect("temp directory");
    let config = DockerRuntimeConfig {
        workspace_root: temp.path().join("builds"),
        caddy_sites_dir: temp.path().join("sites"),
        ..DockerRuntimeConfig::default()
    };
    (DockerRuntimeExecutor::with_runner(config, runner), temp)
}

fn stats_line(id: &str, cpu: &str, memory: &str) -> String {
    format!(
        r#"{{"BlockIO":"0B / 0B","CPUPerc":"{cpu}","Container":"{id}","ID":"{id}","MemPerc":"1.00%","MemUsage":"{memory}","Name":"n","NetIO":"0B / 0B","PIDs":"3"}}"#
    )
}

/// Two replicas of one deployment, one single-replica deployment, and a
/// release container, which a managed-label listing never shows but which
/// must be skipped even if it did.
fn listing() -> String {
    [
        format!("aaaaaaaaaaaa\tUp 5 minutes\t{PROJECT}\t{DEPLOYMENT}\t1\t2\t"),
        format!("bbbbbbbbbbbb\tUp 5 minutes\t{PROJECT}\t{DEPLOYMENT}\t0\t2\t"),
        format!("cccccccccccc\tUp 2 hours (healthy)\t{OTHER_PROJECT}\t{OTHER_DEPLOYMENT}\t\t\t"),
        format!("dddddddddddd\tUp 3 seconds\t{OTHER_PROJECT}\t{OTHER_DEPLOYMENT}\t\t\trelease"),
    ]
    .join("\n")
}

fn stats() -> String {
    [
        stats_line("aaaaaaaaaaaa", "112.50%", "200MiB / 256MiB"),
        stats_line("bbbbbbbbbbbb", "0.00%", "35.8MiB / 256MiB"),
        stats_line("cccccccccccc", "3.10%", "1.5GiB / 7.6GiB"),
        stats_line("dddddddddddd", "99.00%", "10MiB / 256MiB"),
        // The router and anything else on the node, not a workload.
        stats_line("eeeeeeeeeeee", "0.40%", "8.4MiB / 1.917GiB"),
    ]
    .join("\n")
}

fn inspect() -> String {
    [
        format!("aaaaaaaaaaaa{}\t0\t268435456", "0".repeat(52)),
        format!("bbbbbbbbbbbb{}\t3\t268435456", "0".repeat(52)),
        format!("cccccccccccc{}\t0\t0", "0".repeat(52)),
    ]
    .join("\n")
}

#[tokio::test]
async fn one_entry_per_running_replica_with_release_and_other_containers_left_out() {
    let runner = ScriptedRunner::new(Script {
        listing: listing(),
        stats: Some(stats()),
        inspect: inspect(),
        ..Script::default()
    });
    let (executor, _temp) = executor(Arc::clone(&runner));

    let metrics = RuntimeExecutor::workload_metrics(&executor)
        .await
        .expect("the sample succeeds");

    let summary = metrics
        .iter()
        .map(|item| {
            (
                item.project_id.to_string(),
                item.replica,
                item.cpu_percent,
                item.memory_bytes,
                item.memory_limit_bytes,
                item.restart_count,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            (OTHER_PROJECT.to_owned(), 0, 3.1, 1_610_612_736, None, 0),
            (PROJECT.to_owned(), 0, 0.0, 37_539_021, Some(268_435_456), 3),
            (
                PROJECT.to_owned(),
                1,
                112.5,
                209_715_200,
                Some(268_435_456),
                0
            ),
        ]
    );
    assert!(
        metrics
            .iter()
            .all(|item| item.sampled_at == metrics[0].sampled_at)
    );

    let commands = runner.commands();
    assert_eq!(commands.len(), 3, "one listing, one stats, one inspect");
    assert!(
        commands[0]
            .iter()
            .any(|argument| argument == "label=dev.sakala.managed=true")
    );
    assert_eq!(
        commands[1],
        ["stats", "--no-stream", "--format", "{{json .}}"]
    );
    let inspected = &commands[2][3..];
    assert_eq!(inspected, ["aaaaaaaaaaaa", "bbbbbbbbbbbb", "cccccccccccc"]);
}

#[tokio::test]
async fn no_running_workload_is_an_empty_sample_without_docker_stats() {
    let runner = ScriptedRunner::new(Script::default());
    let (executor, _temp) = executor(Arc::clone(&runner));

    let metrics = RuntimeExecutor::workload_metrics(&executor)
        .await
        .expect("nothing to sample is not a failure");

    assert!(metrics.is_empty());
    assert_eq!(runner.commands().len(), 1);
}

#[tokio::test]
async fn a_docker_stats_failure_fails_the_whole_sample() {
    let runner = ScriptedRunner::new(Script {
        listing: listing(),
        stats: None,
        inspect: inspect(),
        ..Script::default()
    });
    let (executor, _temp) = executor(runner);

    assert!(RuntimeExecutor::workload_metrics(&executor).await.is_err());
}

#[tokio::test]
async fn a_runtime_reporting_no_usage_fails_instead_of_sending_zeros() {
    // What Docker prints for containers it has no cgroup reading for.
    let zeros = ["aaaaaaaaaaaa", "bbbbbbbbbbbb", "cccccccccccc"]
        .map(|id| stats_line(id, "0.00%", "0B / 0B"))
        .join("\n");
    let runner = ScriptedRunner::new(Script {
        listing: listing(),
        stats: Some(zeros),
        inspect: inspect(),
        ..Script::default()
    });
    let (executor, _temp) = executor(runner);

    assert!(RuntimeExecutor::workload_metrics(&executor).await.is_err());
}

#[tokio::test]
async fn a_container_removed_during_the_sample_is_left_out() {
    let runner = ScriptedRunner::new(Script {
        listing: listing(),
        stats: Some(stats()),
        inspect: format!("cccccccccccc{}\t0\t0", "0".repeat(52)),
        inspect_stderr: "Error: No such object: aaaaaaaaaaaa".to_owned(),
    });
    let (executor, _temp) = executor(runner);

    let metrics = RuntimeExecutor::workload_metrics(&executor)
        .await
        .expect("the remaining container is sampled");

    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].project_id.to_string(), OTHER_PROJECT);
}
