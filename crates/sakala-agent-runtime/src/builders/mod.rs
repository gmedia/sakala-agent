use std::{
    collections::{BTreeMap, hash_map::RandomState},
    hash::BuildHasher,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use sakala_agent_core::ports::SecretString;
use sakala_agent_protocol::{DeploymentBuilder, DeploymentLog, LogStream};
use serde_json::json;
use time::OffsetDateTime;
use tokio::fs;
use uuid::Uuid;

use crate::{
    CommandSpec, ProcessRunner, RuntimeError, RuntimeReporter, logs::emit_event,
    process::run_checked, railpack::RailpackSandbox,
};

mod dockerfile;

/// All projects on a node share one BuildKit, so nothing a build reuses may come
/// from another project. `--no-cache` stops a step from reusing a layer built with
/// another project's secret, because BuildKit does not count a secret's value in
/// its cache key. `--pull` stops `FROM` from reading another project's local image,
/// which only exists on this node and so fails to resolve. Both need BuildKit's own
/// Dockerfile frontend: a pinned external one does not honour `--pull`, and one the
/// repository picks need not honour either. See `write_locked_dockerfile`.
pub(crate) fn with_tenant_isolation(command: CommandSpec) -> CommandSpec {
    command.arg("--no-cache").arg("--pull")
}

/// Hands every variable to BuildKit as a secret, which never enters image layers
/// or history unless a build step copies it there. The carrier name is the
/// agent's own: a project's variable names must not reach the environment of a
/// host process, where `LD_PRELOAD` or `DOCKER_HOST` would change what it does.
pub(crate) fn with_build_secrets(
    mut command: CommandSpec,
    environment: &[(String, SecretString)],
) -> CommandSpec {
    for (index, (name, value)) in environment.iter().enumerate() {
        let carrier = format!("SAKALA_BUILD_SECRET_{index}");
        command = command
            .arg("--secret")
            .arg(format!("id={name},env={carrier}"))
            .secret_env(carrier, value.clone());
    }
    command
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildRequest {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub workspace: PathBuf,
    pub source: PathBuf,
    pub image: String,
    pub requested: DeploymentBuilder,
    /// Project variables offered to the build as secrets, sorted by name.
    pub environment: Vec<(String, SecretString)>,
    pub build_command: Option<String>,
    pub start_command: Option<String>,
}

/// Railpack takes both commands through its own settings. They stay out of
/// process arguments like every other value, and they win over a project variable
/// of the same name because the project set them as explicit fields.
fn railpack_environment(request: &BuildRequest) -> Vec<(String, SecretString)> {
    let mut environment = request
        .environment
        .iter()
        .cloned()
        .collect::<BTreeMap<_, _>>();
    for (name, command) in [
        ("RAILPACK_BUILD_CMD", &request.build_command),
        ("RAILPACK_START_CMD", &request.start_command),
    ] {
        if let Some(command) = command {
            environment.insert(name.to_owned(), SecretString::new(command.clone()));
        }
    }
    environment.into_iter().collect()
}

/// A Dockerfile can be a symlink, including to a file on the host. One that stays
/// in the checkout is fine, which a monorepo may rely on.
async fn ensure_dockerfile_in_workspace(request: &BuildRequest) -> Result<(), RuntimeError> {
    let dockerfile = fs::canonicalize(request.source.join("Dockerfile")).await?;
    let workspace = fs::canonicalize(&request.workspace).await?;
    if dockerfile.starts_with(workspace) {
        Ok(())
    } else {
        Err(RuntimeError::Build(
            "the Dockerfile is a symlink that leads outside the repository".to_owned(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildOutput {
    pub builder: DeploymentBuilder,
}

#[async_trait]
pub trait ImageBuilder: Send + Sync {
    async fn build(
        &self,
        request: &BuildRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<BuildOutput, RuntimeError>;
}

pub struct ImageBuildService {
    runner: Arc<dyn ProcessRunner>,
    railpack_frontend: String,
    sandbox: RailpackSandbox,
    /// Keys the fingerprint that tells Railpack when a value changed. It is
    /// visible in process arguments, so it must not allow guessing a weak value
    /// offline. A new key after an Agent restart only costs a rebuild of cached
    /// steps.
    secrets_fingerprint_key: RandomState,
}

impl ImageBuildService {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        railpack_frontend: String,
        sandbox: RailpackSandbox,
    ) -> Self {
        Self {
            runner,
            railpack_frontend,
            sandbox,
            secrets_fingerprint_key: RandomState::new(),
        }
    }

    fn secrets_fingerprint(&self, environment: &[(String, SecretString)]) -> Option<String> {
        (!environment.is_empty()).then(|| {
            let exposed = environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.expose()))
                .collect::<Vec<_>>();
            format!("{:016x}", self.secrets_fingerprint_key.hash_one(exposed))
        })
    }
}

#[async_trait]
impl ImageBuilder for ImageBuildService {
    async fn build(
        &self,
        request: &BuildRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<BuildOutput, RuntimeError> {
        let dockerfile_found = fs::try_exists(request.source.join("Dockerfile")).await?;
        let builder = match request.requested {
            DeploymentBuilder::Auto if dockerfile_found => DeploymentBuilder::Dockerfile,
            DeploymentBuilder::Auto => DeploymentBuilder::Railpack,
            DeploymentBuilder::Dockerfile if !dockerfile_found => {
                return Err(RuntimeError::InvalidCommand(
                    "Dockerfile builder requested but repository has no root Dockerfile".to_owned(),
                ));
            }
            selected => selected,
        };

        let started_build = || {
            emit_event(
                reporter,
                "deployment.build.started",
                "Preparing application image.",
                json!({ "requested_builder": request.requested, "builder": builder }),
            )
        };

        match builder {
            DeploymentBuilder::Dockerfile => {
                ensure_dockerfile_in_workspace(request).await?;
                started_build().await?;
                if request.build_command.is_some() {
                    let _ = reporter
                        .log(DeploymentLog {
                            stream: LogStream::System,
                            message: "build_command was not used: a Dockerfile defines its own build steps."
                                .to_owned(),
                            recorded_at: OffsetDateTime::now_utc(),
                        })
                        .await;
                }
                let dockerfile = dockerfile::write_locked_dockerfile(
                    &request.source.join("Dockerfile"),
                    &request.workspace,
                )
                .await?;
                let command = with_build_secrets(
                    dockerfile::build_command(
                        &request.source,
                        &dockerfile,
                        &request.image,
                        request.project_id,
                        request.deployment_id,
                    ),
                    &request.environment,
                );
                run_checked(self.runner.as_ref(), &command, "docker-build", reporter).await?;
            }
            DeploymentBuilder::Railpack => {
                emit_event(
                    reporter,
                    "deployment.analysis.started",
                    "Analyzing the repository to plan the build.",
                    json!({ "builder": builder }),
                )
                .await?;
                let environment = railpack_environment(request);
                let plan = self
                    .sandbox
                    .prepare(&request.source, &request.workspace, &environment, reporter)
                    .await?;
                started_build().await?;
                let build = with_build_secrets(
                    crate::railpack::build_command(
                        &request.source,
                        &plan,
                        &request.image,
                        &self.railpack_frontend,
                        request.project_id,
                        request.deployment_id,
                        self.secrets_fingerprint(&environment).as_deref(),
                    ),
                    &environment,
                );
                run_checked(self.runner.as_ref(), &build, "railpack-build", reporter).await?;
            }
            DeploymentBuilder::Auto => unreachable!("auto builder must be resolved before build"),
        }

        Ok(BuildOutput { builder })
    }
}
