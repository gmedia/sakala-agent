use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use sakala_agent_core::ports::{RuntimeOrphan, RuntimeReconciliationReport};
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
    containers::{ContainerEngine, ResourceSafetyConfig, RunContainerRequest},
    logs::ReporterOutputSink,
    process::run_checked,
};

const MANAGED_LABEL: &str = "dev.sakala.managed=true";

pub struct DockerContainerEngine {
    runner: Arc<dyn ProcessRunner>,
    runtime_network: String,
    resource_safety: ResourceSafetyConfig,
    max_active_containers: u32,
    log_followers: Mutex<Vec<JoinHandle<()>>>,
}

impl DockerContainerEngine {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        runtime_network: String,
        resource_safety: ResourceSafetyConfig,
        max_active_containers: u32,
    ) -> Self {
        Self {
            runner,
            runtime_network,
            resource_safety,
            max_active_containers,
            log_followers: Mutex::new(Vec::new()),
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
            let reason = if project_id.is_none() || deployment_id.is_none() {
                Some("managed container has incomplete Sakala identity labels")
            } else if status.starts_with("Exited")
                || status.starts_with("Dead")
                || status.starts_with("Created")
            {
                Some("managed container is not running")
            } else {
                None
            };

            if let Some(reason) = reason {
                report.orphans.push(RuntimeOrphan {
                    container_id,
                    project_id,
                    reason: reason.to_owned(),
                });
            }
        }

        Ok(report)
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
            ));
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

    fn start_log_follower(&self, container: &str, reporter: Arc<dyn RuntimeReporter>) {
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

        let mut followers = self.log_followers.lock().expect("log follower lock");
        followers.retain(|follower| !follower.is_finished());
        followers.push(handle);
        debug!(%container, "container log follower started");
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
                .arg("{{.Name}}")
                .arg(container_id);
            let inspected_name = self.runner.run(&inspect, &NullOutputSink).await?;
            if inspected_name.stdout.trim().trim_start_matches('/') == current {
                continue;
            }
            let remove = CommandSpec::new("docker")
                .arg("rm")
                .arg("--force")
                .arg(container_id);
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

    async fn cleanup_candidate(&self, container: &str, image: &str) {
        for command in [
            CommandSpec::new("docker")
                .arg("rm")
                .arg("--force")
                .arg(container),
            CommandSpec::new("docker")
                .arg("image")
                .arg("rm")
                .arg("--force")
                .arg(image),
        ] {
            let _ = self.runner.run(&command, &NullOutputSink).await;
        }
    }

    async fn shutdown(&self) {
        let followers = {
            let mut followers = self.log_followers.lock().expect("log follower lock");
            std::mem::take(&mut *followers)
        };
        for follower in &followers {
            follower.abort();
        }
        for follower in followers {
            let _ = follower.await;
        }
    }
}
