use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeOrphan, RuntimeReconciliationReport,
    RuntimeStaleImage, RuntimeWorkload,
};
use sakala_agent_protocol::{AppliedRuntimeResources, RuntimeResourceLimits};
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

pub struct DockerContainerEngine {
    runner: Arc<dyn ProcessRunner>,
    runtime_network: String,
    resource_safety: ResourceSafetyConfig,
    max_active_containers: u32,
    agent_id: String,
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
    ) -> Self {
        Self {
            runner,
            runtime_network,
            resource_safety,
            max_active_containers,
            agent_id,
            log_followers: Mutex::new(HashMap::new()),
        }
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
                "docker capacity inspection exited with status {:?}",
                output.code
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
                "docker orphan inspection exited with status {:?}",
                output.code
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
                "docker health inspection exited with status {:?}",
                output.code
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
                "docker workload lookup exited with status {:?}",
                output.code
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
        let env_file = Self::write_env_file(&request.workspace, &request.environment).await?;
        let mut command = CommandSpec::new("docker")
            .arg("run")
            .arg("--detach")
            .arg("--name")
            .arg(&request.name)
            .arg("--network")
            .arg(&self.runtime_network)
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
        command = command.arg(&request.image);

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
            .arg("100")
            .arg(container);
        run_checked(self.runner.as_ref(), &command, "runtime", reporter)
            .await
            .map(|_| ())
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
                "docker-list-previous exited with status {:?}",
                output.code
            )));
        }

        for container_id in output.stdout.lines().filter(|id| !id.trim().is_empty()) {
            let inspect = CommandSpec::new("docker")
                .arg("inspect")
                .arg("--format")
                .arg("{{.State.Running}}\t{{.Name}}")
                .arg(container_id);
            let inspected_name = self.runner.run(&inspect, &NullOutputSink).await?;
            let mut fields = inspected_name.stdout.trim().split('\t');
            let running = matches!(fields.next(), Some("true"));
            let name = fields.next().unwrap_or_default().trim_start_matches('/');
            if running || name == current {
                continue;
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
                Ok(output) => {
                    failures.push(format!("{artifact} exited with status {:?}", output.code))
                }
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

    async fn detect_stale_images(&self) -> Result<Vec<RuntimeStaleImage>, RuntimeError> {
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
                "Sakala stale image inspection exited with status {:?}",
                output.code
            )));
        }
        Ok(output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let fields = line.split('\t').collect::<Vec<_>>();
                RuntimeStaleImage {
                    image_id: fields.first().copied().unwrap_or_default().to_owned(),
                    project_id: fields.get(1).and_then(|value| Uuid::parse_str(value).ok()),
                    deployment_id: fields.get(2).and_then(|value| Uuid::parse_str(value).ok()),
                }
            })
            .collect())
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
                "Sakala image GC exited with status {:?}",
                output.code
            )));
        }
        parse_reclaimed_bytes(&output.stdout)
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
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..split].parse::<u64>().map_err(|_| {
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
    number.checked_mul(multiplier).ok_or_else(|| {
        RuntimeError::Container("Docker image GC reclaimed-space value overflowed".to_owned())
    })
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
