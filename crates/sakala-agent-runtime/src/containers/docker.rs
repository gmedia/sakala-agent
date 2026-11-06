use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeOrphan, RuntimeReconciliationReport,
    RuntimeStaleImage, RuntimeWorkload,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, LogBounds, RuntimeResourceLimits,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    task::JoinHandle,
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    containers::limits::docker_cpu_value,
    containers::{ContainerEngine, ManagedWorkload, ResourceSafetyConfig, RunContainerRequest},
    logs::ReporterOutputSink,
    process::run_checked,
};

const MANAGED_LABEL: &str = "dev.sakala.managed=true";
const AGENT_ID_LABEL: &str = "dev.sakala.agent-id";
const WORKLOAD_KIND_LABEL: &str = "dev.sakala.workload-kind=web";
const DOMAIN_LABEL: &str = "dev.sakala.domain";
const PORT_LABEL: &str = "dev.sakala.port";
const COMMAND_ID_LABEL: &str = "dev.sakala.command-id";
const LOG_MAX_LINE_LABEL: &str = "dev.sakala.log-max-line-length";
const LOG_MAX_BATCH_LABEL: &str = "dev.sakala.log-max-batch-lines";
const LOG_MAX_TOTAL_LABEL: &str = "dev.sakala.log-max-total-bytes";
const STARTUP_LOG_TAIL_LINES: u64 = 100;
const FAILURE_LOG_TIMEOUT: Duration = Duration::from_secs(10);
const CRASH_INSPECT_TIMEOUT: Duration = Duration::from_secs(1);
const EXIT_EVENTS_TIMEOUT: Duration = Duration::from_secs(1);
/// From this many restarts, a running workload counts as crash-looping rather than
/// as an app that crashed once and recovered.
const CRASH_LOOP_MIN_RESTARTS: u32 = 3;
const ENVIRONMENT_INSPECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Set by the image or by Sakala itself, and common in ordinary log text.
const NON_SECRET_ENVIRONMENT_KEYS: [&str; 3] = ["PATH", "HOME", "PORT"];
const CRASH_STATE_FORMAT: &str = "{{.Id}}\t{{.State.Status}}\t{{.RestartCount}}\t{{.State.ExitCode}}\t{{.State.OOMKilled}}\t{{.State.StartedAt}}\t{{.State.FinishedAt}}";
const EXIT_EVENTS_FORMAT: &str = "{{.Actor.ID}}\t{{.Action}}\t{{.Actor.Attributes.exitCode}}";
/// Exit code of a SIGKILL, which is what the kernel OOM killer sends.
const SIGKILL_EXIT_CODE: i32 = 137;

/// The name servers workloads use instead of Docker's embedded DNS. The file outlives
/// every workspace, because Docker reads it again whenever a container restarts.
pub struct ContainerDns {
    path: std::path::PathBuf,
    content: String,
}

impl ContainerDns {
    /// `None` when there are no name servers, which leaves DNS to Docker.
    #[must_use]
    pub fn new(workspace_root: &Path, servers: Vec<std::net::IpAddr>) -> Option<Self> {
        (!servers.is_empty()).then(|| Self {
            path: workspace_root.join(".sakala-resolv.conf"),
            content: servers
                .iter()
                .map(|server| format!("nameserver {server}\n"))
                .collect(),
        })
    }

    /// Written through a rename, so a container starting now never reads half a file.
    async fn ensure_mount(&self) -> Result<std::ffi::OsString, RuntimeError> {
        if fs::read_to_string(&self.path).await.ok().as_deref() != Some(self.content.as_str()) {
            let partial = self.path.with_extension(format!("{}", Uuid::new_v4()));
            fs::write(&partial, &self.content).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o644)).await?;
            }
            fs::rename(&partial, &self.path).await?;
        }
        let mut mount = std::ffi::OsString::from("type=bind,src=");
        mount.push(&self.path);
        mount.push(",dst=/etc/resolv.conf,readonly");
        Ok(mount)
    }
}

/// What a node adds to every workload it starts, as opposed to what a command asks for.
#[derive(Default)]
pub struct WorkloadIsolation {
    pub container_runtime: Option<String>,
    pub container_dns: Option<ContainerDns>,
}

pub struct DockerContainerEngine {
    runner: Arc<dyn ProcessRunner>,
    runtime_network: String,
    resource_safety: ResourceSafetyConfig,
    max_active_containers: u32,
    agent_id: String,
    crash_stable_window: Duration,
    container_runtime: Option<String>,
    container_dns: Option<ContainerDns>,
    /// Restarts already written to the operator log, by container.
    logged_restarts: Mutex<HashMap<String, u32>>,
    log_followers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl DockerContainerEngine {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        runtime_network: String,
        resource_safety: ResourceSafetyConfig,
        max_active_containers: u32,
        agent_id: String,
        crash_stable_window: Duration,
        isolation: WorkloadIsolation,
    ) -> Self {
        Self {
            runner,
            runtime_network,
            resource_safety,
            max_active_containers,
            agent_id,
            crash_stable_window,
            container_runtime: isolation.container_runtime,
            container_dns: isolation.container_dns,
            logged_restarts: Mutex::new(HashMap::new()),
            log_followers: Mutex::new(HashMap::new()),
        }
    }

    /// Railpack images serve on `$PORT` (its Caddyfile listens on
    /// `:{$PORT:80}`, its Node starters read `process.env.PORT`), and the
    /// generated route points at the port the control plane chose. Without
    /// exporting it the workload listened on its own default while the route
    /// pointed elsewhere, so the edge returned 502. `validate_payload` already
    /// rejects a conflicting `PORT` from the deployment environment, so this
    /// only fills in the value.
    fn effective_environment(
        environment: &BTreeMap<String, String>,
        port: u16,
    ) -> BTreeMap<String, String> {
        let mut effective = environment.clone();
        effective
            .entry("PORT".to_owned())
            .or_insert_with(|| port.to_string());
        effective
    }

    async fn write_env_file(
        workspace: &Path,
        environment: &BTreeMap<String, String>,
    ) -> Result<Option<std::path::PathBuf>, RuntimeError> {
        if environment.is_empty() {
            return Ok(None);
        }

        let path = workspace.join("runtime.env");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await?;
        for (key, value) in environment {
            file.write_all(format!("{key}={value}\n").as_bytes())
                .await?;
        }
        file.flush().await?;
        drop(file);

        Ok(Some(path))
    }
}

#[async_trait]
impl ContainerEngine for DockerContainerEngine {
    fn resolve_resources(
        &self,
        requested: RuntimeResourceLimits,
    ) -> Result<AppliedRuntimeResources, RuntimeError> {
        self.resource_safety.resolve(requested)
    }

    async fn ensure_capacity(&self, project_id: Uuid) -> Result<(), RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg("{{.Label \"dev.sakala.project-id\"}}");
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker capacity inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let active_projects = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        let project_id = project_id.to_string();
        let replacement = active_projects.iter().any(|active| *active == project_id);

        if !replacement && active_projects.len() >= self.max_active_containers as usize {
            return Err(RuntimeError::Capacity(format!(
                "node already runs {} managed containers; configured maximum is {}",
                active_projects.len(),
                self.max_active_containers
            )));
        }

        Ok(())
    }

    async fn detect_orphans(&self) -> Result<RuntimeReconciliationReport, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg("{{.ID}}\t{{.Status}}\t{{.Label \"dev.sakala.project-id\"}}\t{{.Label \"dev.sakala.deployment-id\"}}");
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker orphan inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let mut report = RuntimeReconciliationReport::default();
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            report.inspected_containers += 1;
            let fields = line.split('\t').collect::<Vec<_>>();
            let container_id = fields.first().copied().unwrap_or_default().to_owned();
            let status = fields.get(1).copied().unwrap_or_default();
            let project_id = fields.get(2).and_then(|value| Uuid::parse_str(value).ok());
            let deployment_id = fields.get(3).and_then(|value| Uuid::parse_str(value).ok());
            let reason = if project_id.is_none() {
                Some("managed container has an unknown project identity")
            } else if deployment_id.is_none() {
                Some("managed container has incomplete deployment identity labels")
            } else if status.starts_with("Created") {
                Some("dangling candidate container was never started")
            } else if status.starts_with("Exited") || status.starts_with("Dead") {
                Some("stale stopped deployment container")
            } else {
                None
            };

            if let Some(reason) = reason {
                report.orphans.push(RuntimeOrphan {
                    container_id,
                    project_id,
                    reason: reason.to_owned(),
                });
            } else if let (Some(project_id), Some(deployment_id)) = (project_id, deployment_id) {
                report.workloads.push(RuntimeWorkload {
                    container_id,
                    project_id,
                    deployment_id,
                    status: status.to_owned(),
                });
            }
        }

        Ok(report)
    }

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeError> {
        let report = self.detect_orphans().await?;
        Ok(RuntimeCapacity {
            active_workloads: Some(report.workloads.len()),
            stopped_workloads: Some(
                report
                    .orphans
                    .iter()
                    .filter(|orphan| orphan.reason == "stale stopped deployment container")
                    .count(),
            ),
            maximum_active_workloads: Some(self.max_active_containers as usize),
            active_builds: None,
            maximum_concurrent_builds: None,
        })
    }

    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeError> {
        // `docker ps` deliberately excludes stopped workloads. Mereka tidak
        // seharusnya terus diperiksa oleh worker kesehatan runtime.
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg("{{.ID}}\t{{.Status}}\t{{.Label \"dev.sakala.project-id\"}}\t{{.Label \"dev.sakala.deployment-id\"}}");
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker health inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let mut snapshots = Vec::new();
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let fields = line.split('\t').collect::<Vec<_>>();
            let Some(project_id) = fields.get(2).and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            let Some(deployment_id) = fields.get(3).and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            let status = fields.get(1).copied().unwrap_or_default().to_owned();
            let (ready, reason) = health_state(&status);
            snapshots.push(RuntimeHealthSnapshot {
                workload: RuntimeWorkload {
                    container_id: fields.first().copied().unwrap_or_default().to_owned(),
                    project_id,
                    deployment_id,
                    status,
                },
                ready,
                reason,
            });
        }

        Ok(snapshots)
    }

    /// A workload counts as crashed while Docker is restarting it, and while it
    /// is crash-looping: restarted at least `CRASH_LOOP_MIN_RESTARTS` times and up
    /// for less than `crash_stable_window`. An app that crashed once and answers
    /// again is not reported, only logged for the operator, or the UI would say it
    /// is down while it serves. Docker resets `RestartCount` on `docker restart`
    /// and `docker start`, so a restart or wake through the agent never counts. An
    /// `exited` container is left out on purpose: `docker stop`, `docker kill`, and
    /// a sleep all end there with any exit code, so it cannot be told apart.
    async fn crashed_workloads(
        &self,
        snapshots: &[RuntimeHealthSnapshot],
    ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeError> {
        let listed = snapshots
            .iter()
            .filter(|snapshot| {
                let status = snapshot.workload.status.to_ascii_lowercase();
                status.starts_with("up") || status.starts_with("restarting")
            })
            .collect::<Vec<_>>();
        if listed.is_empty() {
            return Ok(Vec::new());
        }

        let mut command = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(CRASH_STATE_FORMAT)
            .timeout(CRASH_INSPECT_TIMEOUT);
        for snapshot in &listed {
            command = command.arg(&snapshot.workload.container_id);
        }
        let output = self.runner.run(&command, &NullOutputSink).await?;
        // `docker inspect` exits non-zero when one of several containers is gone
        // but still prints the others, and a container removed since the listing
        // is simply not crashed anymore.
        if !output.success
            && output.stdout.trim().is_empty()
            && !output.stderr.to_ascii_lowercase().contains("no such")
        {
            return Err(RuntimeError::Container(format!(
                "docker crash inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let states = listed
            .iter()
            .filter_map(|snapshot| {
                let state = output
                    .stdout
                    .lines()
                    .filter_map(CrashState::parse)
                    .find(|state| state.id.starts_with(&snapshot.workload.container_id))?;
                Some((*snapshot, state))
            })
            .collect::<Vec<_>>();
        self.log_new_restarts(&states);

        let now = OffsetDateTime::now_utc();
        let crashed = states
            .into_iter()
            .filter(|(_, state)| state.is_crashed(now, self.crash_stable_window))
            .collect::<Vec<_>>();
        let last_exits = self.last_exits(&crashed).await;

        Ok(crashed
            .iter()
            .map(|(snapshot, state)| {
                let last_exit = last_exits.get(&state.id);
                CrashedWorkloadDetail {
                    project_id: snapshot.workload.project_id,
                    deployment_id: snapshot.workload.deployment_id,
                    exit_code: state.exit_code(last_exit),
                    oom_killed: state.oom_killed(last_exit),
                    finished_at: state.finished_at,
                    restart_count: state.restart_count,
                    running: state.running,
                }
            })
            .collect())
    }

    async fn workload(
        &self,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> Result<Option<ManagedWorkload>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--filter")
            .arg(format!("label=dev.sakala.project-id={project_id}"))
            .arg("--filter")
            .arg(format!("label=dev.sakala.deployment-id={deployment_id}"))
            .arg("--format")
            .arg("{{.ID}}\t{{.Status}}\t{{.Label \"dev.sakala.domain\"}}\t{{.Label \"dev.sakala.port\"}}\t{{.Label \"dev.sakala.command-id\"}}\t{{.Label \"dev.sakala.log-max-line-length\"}}\t{{.Label \"dev.sakala.log-max-batch-lines\"}}\t{{.Label \"dev.sakala.log-max-total-bytes\"}}");
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker workload lookup failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        let Some(line) = output.stdout.lines().find(|line| !line.trim().is_empty()) else {
            return Ok(None);
        };
        let fields = line.split('\t').collect::<Vec<_>>();
        let domain = fields
            .get(2)
            .copied()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                RuntimeError::Container("managed workload is missing its domain label".to_owned())
            })?;
        let port = fields
            .get(3)
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                RuntimeError::Container("managed workload has an invalid port label".to_owned())
            })?;
        Ok(Some(ManagedWorkload {
            container_id: fields.first().copied().unwrap_or_default().to_owned(),
            status: fields.get(1).copied().unwrap_or_default().to_owned(),
            project_id,
            deployment_id,
            domain: domain.to_owned(),
            port,
            command_id: fields.get(4).and_then(|value| Uuid::parse_str(value).ok()),
            log_bounds: sakala_agent_protocol::LogBounds {
                max_line_length: parse_optional_label(fields.get(5).copied()),
                max_batch_lines: parse_optional_label(fields.get(6).copied()),
                max_total_bytes: parse_optional_label(fields.get(7).copied()),
            },
        }))
    }

    async fn restart(
        &self,
        workload: &ManagedWorkload,
        grace_seconds: u64,
    ) -> Result<(), RuntimeError> {
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("restart")
                .arg("--time")
                .arg(grace_seconds.to_string())
                .arg(&workload.container_id),
            "docker-restart",
        )
        .await
    }

    async fn stop(
        &self,
        workload: &ManagedWorkload,
        grace_seconds: u64,
    ) -> Result<(), RuntimeError> {
        if !workload.status.to_ascii_lowercase().starts_with("up") {
            return Ok(());
        }
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("stop")
                .arg("--time")
                .arg(grace_seconds.to_string())
                .arg(&workload.container_id),
            "docker-stop",
        )
        .await
    }

    async fn start_existing(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError> {
        if workload.status.to_ascii_lowercase().starts_with("up") {
            return Ok(());
        }
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("start")
                .arg(&workload.container_id),
            "docker-start",
        )
        .await
    }

    async fn remove(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError> {
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("rm")
                .arg(&workload.container_id),
            "docker-remove",
        )
        .await
    }

    async fn start(
        &self,
        request: &RunContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let environment = Self::effective_environment(&request.environment, request.port);
        let env_file = Self::write_env_file(&request.workspace, &environment).await?;
        let mut command = CommandSpec::new("docker")
            .arg("run")
            .arg("--detach")
            .arg("--name")
            .arg(&request.name)
            .arg("--network")
            .arg(&self.runtime_network);
        // Docker refuses an unknown runtime, and the failure stays a failure: a
        // workload must not end up on a weaker runtime than the node promises.
        if let Some(container_runtime) = &self.container_runtime {
            command = command.arg("--runtime").arg(container_runtime);
        }
        if let Some(container_dns) = &self.container_dns {
            command = command
                .arg("--mount")
                .arg(container_dns.ensure_mount().await?);
        }
        command = command
            .arg("--restart")
            .arg("unless-stopped")
            .arg("--security-opt")
            .arg("no-new-privileges:true")
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--memory")
            .arg(format!("{}m", request.resources.memory_mb))
            .arg("--memory-swap")
            .arg(format!("{}m", request.resources.memory_mb))
            .arg("--cpus")
            .arg(docker_cpu_value(request.resources.cpu_millis))
            .arg("--pids-limit")
            .arg(request.resources.pids_limit.to_string())
            .arg("--label")
            .arg(MANAGED_LABEL)
            .arg("--label")
            .arg(format!("dev.sakala.project-id={}", request.project_id))
            .arg("--label")
            .arg(format!(
                "dev.sakala.deployment-id={}",
                request.deployment_id
            ))
            .arg("--label")
            .arg(format!("{DOMAIN_LABEL}={}", request.domain))
            .arg("--label")
            .arg(format!("{PORT_LABEL}={}", request.port))
            .arg("--label")
            .arg(format!("{COMMAND_ID_LABEL}={}", request.command_id))
            .arg("--label")
            .arg(WORKLOAD_KIND_LABEL)
            .arg("--label")
            .arg(format!("{AGENT_ID_LABEL}={}", self.agent_id));
        for (label, value) in [
            (LOG_MAX_LINE_LABEL, request.log_bounds.max_line_length),
            (LOG_MAX_BATCH_LABEL, request.log_bounds.max_batch_lines),
            (LOG_MAX_TOTAL_LABEL, request.log_bounds.max_total_bytes),
        ] {
            if let Some(value) = value {
                command = command.arg("--label").arg(format!("{label}={value}"));
            }
        }
        if let Some(env_file) = &env_file {
            command = command.arg("--env-file").arg(env_file.as_os_str());
        }
        // The command runs in the container's own `sh`, never in one on the host.
        // An image without `sh` fails to start, which readiness reports as a crash.
        if request.start_command.is_some() {
            command = command.arg("--entrypoint").arg("sh");
        }
        command = command.arg(&request.image);
        if let Some(start_command) = &request.start_command {
            command = command.arg("-c").arg(start_command);
        }

        let result = run_checked(self.runner.as_ref(), &command, "docker-run", reporter).await;
        if let Some(env_file) = env_file {
            let remove_result = fs::remove_file(env_file).await;
            if result.is_ok() {
                remove_result?;
            }
        }
        result.map(|_| ())
    }

    async fn report_startup_logs(
        &self,
        container: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("logs")
            .arg("--tail")
            .arg(STARTUP_LOG_TAIL_LINES.to_string())
            .arg(container);
        run_checked(self.runner.as_ref(), &command, "runtime", reporter)
            .await
            .map(|_| ())
    }

    async fn report_failure_logs(
        &self,
        container: &str,
        log_bounds: LogBounds,
        reporter: &dyn RuntimeReporter,
    ) -> Result<bool, RuntimeError> {
        let tail = log_bounds
            .max_batch_lines
            .map_or(STARTUP_LOG_TAIL_LINES, |lines| {
                lines.min(STARTUP_LOG_TAIL_LINES)
            });
        if tail == 0 {
            return Ok(false);
        }
        let command = CommandSpec::new("docker")
            .arg("logs")
            .arg("--tail")
            .arg(tail.to_string())
            .arg(container)
            .timeout(FAILURE_LOG_TIMEOUT);
        let output = run_checked(self.runner.as_ref(), &command, "runtime", reporter).await?;
        Ok(!output.stdout.trim().is_empty() || !output.stderr.trim().is_empty())
    }

    async fn environment_values(&self, container: &str) -> Result<Vec<String>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg("{{json .Config.Env}}")
            .arg(container)
            .timeout(ENVIRONMENT_INSPECT_TIMEOUT);
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker environment inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        // The parse error is not reported: it could quote part of the output.
        let entries: Vec<String> = serde_json::from_str(output.stdout.trim()).map_err(|_| {
            RuntimeError::Container("docker returned an unreadable environment".to_owned())
        })?;
        Ok(entries
            .into_iter()
            .filter_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                (!NON_SECRET_ENVIRONMENT_KEYS.contains(&key)).then(|| value.to_owned())
            })
            .collect())
    }

    fn start_log_follower(&self, container: &str, reporter: Arc<dyn RuntimeReporter>) -> bool {
        let mut followers = self.log_followers.lock().expect("log follower lock");
        followers.retain(|_, follower| !follower.is_finished());
        if followers.contains_key(container) {
            return false;
        }

        let runner = Arc::clone(&self.runner);
        let container = container.to_owned();
        let follower_container = container.clone();
        let handle = tokio::spawn(async move {
            let command = CommandSpec::new("docker")
                .arg("logs")
                .arg("--follow")
                .arg("--tail")
                .arg("0")
                .arg(&follower_container)
                .without_timeout();
            let sink = ReporterOutputSink::new(reporter.as_ref(), "runtime");

            match runner.run(&command, &sink).await {
                Ok(output) => debug!(
                    container = %follower_container,
                    status = ?output.code,
                    "container log follower stopped"
                ),
                Err(error) => warn!(
                    container = %follower_container,
                    %error,
                    "container log follower failed"
                ),
            }
        });

        followers.insert(container.clone(), handle);
        debug!(%container, "container log follower started");
        true
    }

    async fn cleanup_previous(
        &self,
        project_id: Uuid,
        current: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let list = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--quiet")
            .arg("--filter")
            .arg(format!("label=dev.sakala.project-id={project_id}"))
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"));
        let output = self.runner.run(&list, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker-list-previous failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        for container_id in output.stdout.lines().filter(|id| !id.trim().is_empty()) {
            let inspect = CommandSpec::new("docker")
                .arg("inspect")
                .arg("--format")
                .arg("{{.State.Running}}\t{{.Name}}")
                .arg(container_id);
            let inspected_name = self.runner.run(&inspect, &NullOutputSink).await?;
            if !inspected_name.success {
                return Err(RuntimeError::Container(format!(
                    "docker-inspect-previous failed with {}",
                    crate::error::describe_exit(inspected_name.code)
                )));
            }
            let mut fields = inspected_name.stdout.trim().split('\t');
            let running = matches!(fields.next(), Some("true"));
            let name = fields.next().unwrap_or_default().trim_start_matches('/');
            if name == current {
                continue;
            }
            if running {
                let stop = CommandSpec::new("docker")
                    .arg("stop")
                    .arg("--time")
                    .arg("10")
                    .arg(container_id);
                run_checked(
                    self.runner.as_ref(),
                    &stop,
                    "docker-stop-previous",
                    reporter,
                )
                .await?;
            }
            let remove = CommandSpec::new("docker").arg("rm").arg(container_id);
            run_checked(
                self.runner.as_ref(),
                &remove,
                "docker-remove-previous",
                reporter,
            )
            .await?;
        }

        Ok(())
    }

    async fn cleanup_candidate(&self, container: &str, image: &str) -> Result<(), RuntimeError> {
        let mut failures = Vec::new();
        for (artifact, command) in [
            (
                "candidate container",
                CommandSpec::new("docker")
                    .arg("rm")
                    .arg("--force")
                    .arg(container),
            ),
            (
                "candidate image",
                CommandSpec::new("docker")
                    .arg("image")
                    .arg("rm")
                    .arg("--force")
                    .arg(image),
            ),
        ] {
            match self.runner.run(&command, &NullOutputSink).await {
                Ok(output) if output.success => {}
                Ok(output) => failures.push(format!(
                    "{artifact} failed with {}",
                    crate::error::describe_exit(output.code)
                )),
                Err(error) => failures.push(format!("{artifact}: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Container(format!(
                "candidate cleanup incomplete: {}",
                failures.join("; ")
            )))
        }
    }

    async fn detect_stale_images(
        &self,
        max_age: std::time::Duration,
    ) -> Result<Vec<RuntimeStaleImage>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("image")
            .arg("ls")
            .arg("--filter")
            .arg("dangling=true")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg("{{.ID}}\t{{.Label \"dev.sakala.project-id\"}}\t{{.Label \"dev.sakala.deployment-id\"}}");
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "Sakala stale image inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        let minimum_age = time::Duration::try_from(max_age).map_err(|_| {
            RuntimeError::Configuration("image GC maximum age is too large".to_owned())
        })?;
        let now = OffsetDateTime::now_utc();
        let mut stale = Vec::new();
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let fields = line.split('\t').collect::<Vec<_>>();
            let image_id = fields.first().copied().unwrap_or_default().to_owned();
            let inspect = CommandSpec::new("docker")
                .arg("image")
                .arg("inspect")
                .arg("--format")
                .arg("{{.Created}}")
                .arg(&image_id);
            let inspected = self.runner.run(&inspect, &NullOutputSink).await?;
            if !inspected.success {
                return Err(RuntimeError::Container(format!(
                    "Sakala image age inspection failed with {}",
                    crate::error::describe_exit(inspected.code)
                )));
            }
            let created =
                OffsetDateTime::parse(inspected.stdout.trim(), &Rfc3339).map_err(|error| {
                    RuntimeError::Container(format!(
                        "Docker returned an invalid image creation timestamp: {error}"
                    ))
                })?;
            if now - created < minimum_age {
                continue;
            }
            stale.push(RuntimeStaleImage {
                image_id,
                project_id: fields.get(1).and_then(|value| Uuid::parse_str(value).ok()),
                deployment_id: fields.get(2).and_then(|value| Uuid::parse_str(value).ok()),
            });
        }
        Ok(stale)
    }

    async fn cleanup_stale_images(
        &self,
        max_age: std::time::Duration,
    ) -> Result<u64, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("image")
            .arg("prune")
            .arg("--force")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--filter")
            .arg(format!("until={}s", max_age.as_secs()));
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "Sakala image GC failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        match parse_reclaimed_bytes(&output.stdout) {
            Ok(bytes) => Ok(bytes),
            Err(error) => {
                tracing::warn!(%error, "Docker image prune succeeded but reclaimed-space telemetry was not parseable");
                Ok(0)
            }
        }
    }

    async fn shutdown(&self) {
        let followers = {
            let mut followers = self.log_followers.lock().expect("log follower lock");
            std::mem::take(&mut *followers)
        };
        for follower in followers.values() {
            follower.abort();
        }
        for (_, follower) in followers {
            let _ = follower.await;
        }
    }
}

fn parse_optional_label(value: Option<&str>) -> Option<u64> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
}

fn parse_reclaimed_bytes(output: &str) -> Result<u64, RuntimeError> {
    let Some(value) = output
        .lines()
        .find_map(|line| line.strip_prefix("Total reclaimed space: "))
    else {
        return Ok(0);
    };
    let value = value.trim();
    let split = value
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(value.len());
    let number = value[..split].parse::<f64>().map_err(|_| {
        RuntimeError::Container(
            "Docker image GC returned an invalid reclaimed-space value".to_owned(),
        )
    })?;
    let multiplier = match value[split..].trim().to_ascii_lowercase().as_str() {
        "b" | "bytes" | "" => 1,
        "kb" | "kib" => 1_024,
        "mb" | "mib" => 1_024 * 1_024,
        "gb" | "gib" => 1_024 * 1_024 * 1_024,
        _ => {
            return Err(RuntimeError::Container(
                "Docker image GC returned an unknown reclaimed-space unit".to_owned(),
            ));
        }
    };
    let bytes = number * multiplier as f64;
    if !bytes.is_finite() || bytes.is_sign_negative() || bytes > u64::MAX as f64 {
        return Err(RuntimeError::Container(
            "Docker image GC reclaimed-space value overflowed".to_owned(),
        ));
    }
    Ok(bytes.round() as u64)
}

async fn run_container_command(
    runner: &dyn ProcessRunner,
    command: CommandSpec,
    phase: &str,
) -> Result<(), RuntimeError> {
    let output = runner.run(&command, &NullOutputSink).await?;
    if output.success {
        Ok(())
    } else {
        Err(RuntimeError::failed_process(
            phase,
            output.code,
            &output.stderr,
        ))
    }
}

/// One line of the batched `docker inspect` behind the crash report.
#[derive(Debug)]
struct CrashState {
    id: String,
    restarting: bool,
    running: bool,
    restart_count: u32,
    exit_code: Option<i32>,
    oom_flag: bool,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl CrashState {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        let id = fields.next()?.to_owned();
        let status = fields.next()?;
        Some(Self {
            id,
            restarting: status == "restarting",
            running: status == "running",
            restart_count: fields.next()?.parse().ok()?,
            exit_code: fields.next()?.parse().ok(),
            oom_flag: fields.next()? == "true",
            started_at: parse_docker_time(fields.next()?),
            finished_at: parse_docker_time(fields.next()?),
        })
    }

    fn is_crashed(&self, now: OffsetDateTime, stable_window: Duration) -> bool {
        self.restarting
            || (self.running
                && self.restart_count >= CRASH_LOOP_MIN_RESTARTS
                && self
                    .started_at
                    .is_some_and(|started_at| now - started_at < stable_window))
    }

    /// Docker clears the exit code and the OOM flag once a restart succeeds, so
    /// a running container's own values say nothing about its last exit.
    fn needs_exit_history(&self) -> bool {
        !self.restarting || (self.exit_code == Some(SIGKILL_EXIT_CODE) && !self.oom_flag)
    }

    fn exit_code(&self, last_exit: Option<&LastExit>) -> Option<i32> {
        if self.restarting {
            self.exit_code
        } else {
            last_exit.and_then(|exit| exit.exit_code)
        }
    }

    fn oom_killed(&self, last_exit: Option<&LastExit>) -> bool {
        (self.restarting && self.oom_flag) || last_exit.is_some_and(|exit| exit.oom_killed)
    }
}

impl DockerContainerEngine {
    /// A restart that is not reported as a crash must still leave a trace.
    fn log_new_restarts(&self, states: &[(&RuntimeHealthSnapshot, CrashState)]) {
        let counts = states
            .iter()
            .map(|(_, state)| (state.id.clone(), state.restart_count))
            .collect::<Vec<_>>();
        let new = {
            let mut logged = self.logged_restarts.lock().expect("restart log lock");
            new_restarts(&mut logged, counts)
        };
        for (snapshot, state) in states.iter().filter(|(_, state)| new.contains(&state.id)) {
            warn!(
                container_id = %snapshot.workload.container_id,
                project_id = %snapshot.workload.project_id,
                deployment_id = %snapshot.workload.deployment_id,
                restart_count = state.restart_count,
                running = state.running,
                "Docker restarted a managed workload after it exited on its own"
            );
        }
    }

    /// Reads the last exit of the crashed workloads Docker's state no longer
    /// describes. This is enrichment: without it a workload is still reported,
    /// so a failure here must not fail the report.
    async fn last_exits(
        &self,
        crashed: &[(&RuntimeHealthSnapshot, CrashState)],
    ) -> HashMap<String, LastExit> {
        let unknown = crashed
            .iter()
            .map(|(_, state)| state)
            .filter(|state| state.needs_exit_history())
            .collect::<Vec<_>>();
        let Some(since) = unknown
            .iter()
            .filter_map(|state| state.finished_at)
            .map(OffsetDateTime::unix_timestamp)
            .min()
        else {
            return HashMap::new();
        };
        let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
        let mut command = CommandSpec::new("docker")
            .arg("events")
            // `finished_at` is rounded down, so this still covers the last exit.
            .arg("--since")
            .arg(since.to_string())
            // A fractional value includes this very second and returns at once.
            .arg("--until")
            .arg(format!(
                "{}.{:09}",
                now / 1_000_000_000,
                now % 1_000_000_000
            ))
            .arg("--filter")
            .arg("event=die")
            .arg("--filter")
            .arg("event=oom")
            .arg("--format")
            .arg(EXIT_EVENTS_FORMAT)
            .timeout(EXIT_EVENTS_TIMEOUT);
        for state in &unknown {
            command = command
                .arg("--filter")
                .arg(format!("container={}", state.id));
        }
        match self.runner.run(&command, &NullOutputSink).await {
            Ok(output) if output.success => parse_last_exits(&output.stdout),
            Ok(output) => {
                debug!(status = ?output.code, "docker events returned no exit history");
                HashMap::new()
            }
            Err(error) => {
                debug!(%error, "docker events did not finish");
                HashMap::new()
            }
        }
    }
}

/// Returns the containers whose restart count rose since the last call, and
/// forgets the containers that are gone.
fn new_restarts(logged: &mut HashMap<String, u32>, counts: Vec<(String, u32)>) -> Vec<String> {
    let mut new = Vec::new();
    let mut current = HashMap::with_capacity(counts.len());
    for (id, count) in counts {
        if count > logged.get(&id).copied().unwrap_or(0) {
            new.push(id.clone());
        }
        current.insert(id, count);
    }
    *logged = current;
    new
}

#[derive(Debug, Default, Eq, PartialEq)]
struct LastExit {
    exit_code: Option<i32>,
    oom_killed: bool,
}

/// Docker emits `oom` before the `die` it caused, so an `oom` only describes the
/// next `die` of that container. One after the last `die` belongs to the current
/// run, in which a worker was killed but the container survived.
fn parse_last_exits(stdout: &str) -> HashMap<String, LastExit> {
    let mut last_exits = HashMap::<String, LastExit>::new();
    let mut pending_oom = HashSet::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(action)) = (fields.next(), fields.next()) else {
            continue;
        };
        match action {
            "oom" => {
                pending_oom.insert(id);
            }
            "die" => {
                last_exits.insert(
                    id.to_owned(),
                    LastExit {
                        exit_code: fields.next().and_then(|value| value.parse().ok()),
                        oom_killed: pending_oom.remove(id),
                    },
                );
            }
            _ => {}
        }
    }
    last_exits
}

/// Docker prints nanoseconds and uses year 1 for "never". The report keeps whole
/// seconds, and the zero time becomes `None`.
fn parse_docker_time(value: &str) -> Option<OffsetDateTime> {
    let time = OffsetDateTime::parse(value.trim(), &Rfc3339).ok()?;
    (time.year() > 1).then(|| time.replace_nanosecond(0).unwrap_or(time))
}

fn health_state(status: &str) -> (bool, Option<String>) {
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("unhealthy") {
        return (false, Some("Docker health status is unhealthy".to_owned()));
    }
    if normalized.contains("health: starting") {
        return (
            false,
            Some("Docker health check is still starting".to_owned()),
        );
    }
    if normalized.starts_with("up") {
        return (true, None);
    }

    (false, Some(format!("container is not running: {status}")))
}

#[cfg(test)]
mod reclaimed_space_tests {
    use std::collections::{BTreeMap, HashMap};

    use super::{DockerContainerEngine, new_restarts, parse_reclaimed_bytes};

    #[test]
    fn each_new_restart_is_logged_once() {
        let mut logged = HashMap::new();
        let at = |id: &str, count| vec![(id.to_owned(), count)];

        assert!(new_restarts(&mut logged, at("a", 0)).is_empty());
        assert_eq!(new_restarts(&mut logged, at("a", 1)), ["a"]);
        assert!(new_restarts(&mut logged, at("a", 1)).is_empty());
        assert_eq!(new_restarts(&mut logged, at("a", 2)), ["a"]);
        assert!(
            new_restarts(&mut logged, at("a", 0)).is_empty(),
            "a restart on purpose"
        );
        assert_eq!(new_restarts(&mut logged, at("a", 1)), ["a"]);
        assert!(new_restarts(&mut logged, Vec::new()).is_empty());
        assert!(logged.is_empty(), "a removed container is forgotten");
    }

    #[test]
    fn parses_decimal_and_compact_docker_sizes() {
        assert_eq!(
            parse_reclaimed_bytes("Total reclaimed space: 16.43 MB\n").expect("decimal MB"),
            (16.43_f64 * 1_048_576_f64).round() as u64
        );
        assert_eq!(
            parse_reclaimed_bytes("Total reclaimed space: 1.84kB\n").expect("compact kB"),
            (1.84_f64 * 1_024_f64).round() as u64
        );
        assert_eq!(
            parse_reclaimed_bytes("Total reclaimed space: 0B\n").expect("zero bytes"),
            0
        );
    }

    #[test]
    fn effective_environment_exports_the_routed_port() {
        let mut environment = BTreeMap::new();
        environment.insert("APP_ENV".to_owned(), "production".to_owned());

        let effective = DockerContainerEngine::effective_environment(&environment, 3000);

        assert_eq!(effective.get("PORT"), Some(&"3000".to_owned()));
        assert_eq!(effective.get("APP_ENV"), Some(&"production".to_owned()));
    }

    #[test]
    fn effective_environment_keeps_a_matching_deployment_port() {
        let mut environment = BTreeMap::new();
        environment.insert("PORT".to_owned(), "3000".to_owned());

        let effective = DockerContainerEngine::effective_environment(&environment, 3000);

        assert_eq!(effective.get("PORT"), Some(&"3000".to_owned()));
    }
}
