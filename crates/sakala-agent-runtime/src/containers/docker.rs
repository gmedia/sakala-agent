use std::{collections::BTreeMap, path::Path, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::{AppliedRuntimeResources, RuntimeResourceLimits};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};
use uuid::Uuid;

use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    containers::limits::docker_cpu_value,
    containers::{ContainerEngine, ResourceSafetyConfig, RunContainerRequest},
    process::run_checked,
};

const MANAGED_LABEL: &str = "dev.sakala.managed=true";

pub struct DockerContainerEngine {
    runner: Arc<dyn ProcessRunner>,
    runtime_network: String,
    resource_safety: ResourceSafetyConfig,
}

impl DockerContainerEngine {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        runtime_network: String,
        resource_safety: ResourceSafetyConfig,
    ) -> Self {
        Self {
            runner,
            runtime_network,
            resource_safety,
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
            return Err(RuntimeError::Execution(format!(
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
}
