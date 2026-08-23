use std::{path::PathBuf, sync::Arc, time::Duration};

use sakala_agent_core::ports::NodeTelemetry;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::{CommandSpec, NullOutputSink, ProcessRunner};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct NodeTelemetryCollector {
    runner: Arc<dyn ProcessRunner>,
    workspace_root: PathBuf,
    runtime_network: String,
    caddy_container: String,
    dependencies: OnceCell<Value>,
}

impl NodeTelemetryCollector {
    pub(crate) fn new(
        runner: Arc<dyn ProcessRunner>,
        workspace_root: PathBuf,
        runtime_network: String,
        caddy_container: String,
    ) -> Self {
        Self {
            runner,
            workspace_root,
            runtime_network,
            caddy_container,
            dependencies: OnceCell::new(),
        }
    }

    pub(crate) async fn snapshot(&self) -> NodeTelemetry {
        let dependencies = self
            .dependencies
            .get_or_init(|| dependency_versions(Arc::clone(&self.runner)))
            .await
            .clone();
        let runtime_operational = live_runtime_operational(
            self.runner.as_ref(),
            &self.workspace_root,
            &self.runtime_network,
            &self.caddy_container,
        )
        .await;
        let memory = read_memory().await;
        let disk = disk_resources(self.runner.as_ref(), &self.workspace_root).await;
        NodeTelemetry {
            hostname: std::env::var("HOSTNAME").ok(),
            uptime_seconds: tokio::fs::read_to_string("/proc/uptime")
                .await
                .ok()
                .and_then(|contents| contents.split_whitespace().next()?.parse::<f64>().ok())
                .map(|seconds| seconds as u64),
            cpu_total: std::thread::available_parallelism().ok().map(usize::from),
            cpu_load_1m: tokio::fs::read_to_string("/proc/loadavg")
                .await
                .ok()
                .and_then(|contents| contents.split_whitespace().next()?.parse::<f64>().ok()),
            memory_total_bytes: memory.0,
            memory_available_bytes: memory.1,
            disk_total_bytes: disk.0,
            disk_available_bytes: disk.1,
            workspace_used_bytes: disk.2,
            runtime_operational: Some(runtime_operational),
            runtime_dependencies: dependencies,
        }
    }
}

async fn live_runtime_operational(
    runner: &dyn ProcessRunner,
    workspace_root: &std::path::Path,
    runtime_network: &str,
    caddy_container: &str,
) -> bool {
    let docker = command_ready(
        runner,
        CommandSpec::new("docker")
            .arg("info")
            .arg("--format")
            .arg("{{.ServerVersion}}"),
        None,
    );
    let caddy = command_ready(
        runner,
        CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg("{{.State.Running}}")
            .arg(caddy_container),
        Some("true"),
    );
    let network = command_ready(
        runner,
        CommandSpec::new("docker")
            .arg("network")
            .arg("inspect")
            .arg(runtime_network),
        None,
    );
    let workspace = workspace_writable(workspace_root);
    let (docker, caddy, network, workspace) = tokio::join!(docker, caddy, network, workspace);
    docker && caddy && network && workspace
}

async fn command_ready(
    runner: &dyn ProcessRunner,
    command: CommandSpec,
    expected_stdout: Option<&str>,
) -> bool {
    runner
        .run(&command.timeout(PROBE_TIMEOUT), &NullOutputSink)
        .await
        .ok()
        .filter(|output| output.success)
        .is_some_and(|output| {
            expected_stdout.is_none_or(|expected| output.stdout.trim() == expected)
        })
}

async fn workspace_writable(workspace_root: &std::path::Path) -> bool {
    let probe = workspace_root.join(format!(".sakala-readiness-{}", uuid::Uuid::new_v4()));
    let created = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .await
        .is_ok();
    if !created {
        return false;
    }
    tokio::fs::remove_file(probe).await.is_ok()
}

async fn dependency_versions(runner: Arc<dyn ProcessRunner>) -> Value {
    let (git, docker, buildx, railpack) = tokio::join!(
        command_version(runner.as_ref(), "git", &["--version"]),
        command_version(
            runner.as_ref(),
            "docker",
            &["version", "--format", "{{.Server.Version}}"]
        ),
        command_version(runner.as_ref(), "docker", &["buildx", "version"]),
        command_version(runner.as_ref(), "railpack", &["--version"]),
    );
    json!({ "git": git, "docker": docker, "buildx": buildx, "railpack": railpack })
}

async fn command_version(
    runner: &dyn ProcessRunner,
    program: &str,
    args: &[&str],
) -> Option<String> {
    let mut command = CommandSpec::new(program).timeout(PROBE_TIMEOUT);
    for arg in args {
        command = command.arg(*arg);
    }
    let output = runner.run(&command, &NullOutputSink).await.ok()?;
    if !output.success {
        return None;
    }
    let value = output.stdout.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

async fn read_memory() -> (Option<u64>, Option<u64>) {
    let contents = tokio::fs::read_to_string("/proc/meminfo")
        .await
        .unwrap_or_default();
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else { continue };
        let value = fields.next().and_then(|value| value.parse::<u64>().ok());
        match key {
            "MemTotal:" => total = value.and_then(|value| value.checked_mul(1_024)),
            "MemAvailable:" => available = value.and_then(|value| value.checked_mul(1_024)),
            _ => {}
        }
    }
    (total, available)
}

async fn disk_resources(
    runner: &dyn ProcessRunner,
    workspace_root: &std::path::Path,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let df = runner
        .run(
            &CommandSpec::new("df")
                .arg("-Pk")
                .arg(workspace_root.as_os_str())
                .timeout(PROBE_TIMEOUT),
            &NullOutputSink,
        )
        .await
        .ok()
        .filter(|output| output.success);
    let fields = df
        .as_ref()
        .and_then(|output| {
            output
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .nth(1)
        })
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    let total = fields
        .get(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1_024));
    let available = fields
        .get(3)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1_024));
    let used = runner
        .run(
            &CommandSpec::new("du")
                .arg("-sk")
                .arg(workspace_root.as_os_str())
                .timeout(PROBE_TIMEOUT),
            &NullOutputSink,
        )
        .await
        .ok()
        .filter(|output| output.success)
        .and_then(|output| output.stdout.split_whitespace().next()?.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1_024));
    (total, available, used)
}
