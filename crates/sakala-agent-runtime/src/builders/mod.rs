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
    CommandSpec, ProcessRunner, RuntimeError, RuntimeReporter,
    js_workspace::{self, WorkspaceCommands, in_directory},
    logs::emit_event,
    process::run_checked,
    railpack::{
        BUILD_COMMAND_SETTING, RailpackSandbox, SPA_OUTPUT_DIRECTORY_SETTING, START_COMMAND_SETTING,
    },
};

mod dockerfile;
pub(crate) mod image_labels;

pub(crate) use image_labels::reusable_build;

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

/// Adds one `--label` per value of [`image_labels::image_labels`].
pub(crate) fn with_image_labels(mut command: CommandSpec, labels: &[String]) -> CommandSpec {
    for label in labels {
        command = command.arg("--label").arg(label);
    }
    command
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
    /// The checkout, canonical. A JavaScript workspace root can be anywhere from
    /// here down to `source`.
    pub repository: PathBuf,
    /// The root directory, canonical. The checkout itself when none is set.
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
/// of the same name because the project set them as explicit fields. For a package
/// of a JavaScript workspace, the commands it derives only apply where the project
/// set no variable of that name, which stays the way to override them.
fn railpack_environment(
    request: &BuildRequest,
    workspace: Option<&WorkspaceCommands>,
) -> Vec<(String, SecretString)> {
    let mut environment = request
        .environment
        .iter()
        .cloned()
        .collect::<BTreeMap<_, _>>();
    let (build, start) = match workspace {
        Some(commands) => (&commands.build, &commands.start),
        None => (&request.build_command, &request.start_command),
    };
    for (name, command, explicit) in [
        (
            BUILD_COMMAND_SETTING,
            build,
            request.build_command.is_some(),
        ),
        (
            START_COMMAND_SETTING,
            start,
            request.start_command.is_some(),
        ),
        (
            SPA_OUTPUT_DIRECTORY_SETTING,
            &workspace.and_then(|commands| commands.static_output_directory.clone()),
            false,
        ),
    ] {
        if let Some(command) = command
            && (explicit || !environment.contains_key(name))
        {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOutput {
    pub builder: DeploymentBuilder,
    /// Set when the image was built from a JavaScript workspace root: the package's
    /// directory relative to the image's working directory.
    pub package_directory: Option<String>,
}

impl BuildOutput {
    /// A command of the project, such as the release command, run where it would
    /// run if the package had been built on its own.
    #[must_use]
    pub fn in_package(&self, command: String) -> String {
        match &self.package_directory {
            Some(directory) => in_directory(directory, &command),
            None => command,
        }
    }
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

        let output = match builder {
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
                            process: None,
                        })
                        .await;
                }
                let dockerfile = dockerfile::write_locked_dockerfile(
                    &request.source.join("Dockerfile"),
                    &request.workspace,
                )
                .await?;
                let output = BuildOutput {
                    builder,
                    package_directory: None,
                };
                let command = with_build_secrets(
                    dockerfile::build_command(
                        &request.source,
                        &dockerfile,
                        &request.image,
                        request.project_id,
                        &image_labels::image_labels(
                            request.project_id,
                            request.deployment_id,
                            &output,
                        ),
                    ),
                    &request.environment,
                );
                run_checked(self.runner.as_ref(), &command, "docker-build", reporter).await?;
                output
            }
            DeploymentBuilder::Railpack => {
                emit_event(
                    reporter,
                    "deployment.analysis.started",
                    "Analyzing the repository to plan the build.",
                    json!({ "builder": builder }),
                )
                .await?;
                let workspace = js_workspace::detect(&request.repository, &request.source).await;
                let commands = workspace.as_ref().map(|workspace| {
                    workspace.commands(
                        request.build_command.as_deref(),
                        request.start_command.as_deref(),
                    )
                });
                let context = match (&workspace, &commands) {
                    (Some(workspace), Some(commands)) => {
                        emit_event(
                            reporter,
                            "deployment.workspace.detected",
                            "The root directory is a package of a JavaScript workspace; building from the workspace root.",
                            json!(workspace.summary(commands)),
                        )
                        .await?;
                        workspace.root()
                    }
                    _ => request.source.as_path(),
                };
                let environment = railpack_environment(request, commands.as_ref());
                let plan = self
                    .sandbox
                    .prepare(context, &request.workspace, &environment, reporter)
                    .await?;
                started_build().await?;
                let output = BuildOutput {
                    builder,
                    package_directory: workspace
                        .as_ref()
                        .map(|workspace| workspace.package_directory().to_owned()),
                };
                let unlabelled = crate::railpack::unlabelled_reference(&request.image);
                let build = with_build_secrets(
                    crate::railpack::build_command(
                        context,
                        &plan,
                        &unlabelled,
                        &self.railpack_frontend,
                        request.project_id,
                        self.secrets_fingerprint(&environment).as_deref(),
                    ),
                    &environment,
                );
                run_checked(self.runner.as_ref(), &build, "railpack-build", reporter).await?;
                crate::railpack::label_image(
                    &self.runner,
                    reporter,
                    &request.workspace,
                    &unlabelled,
                    &request.image,
                    &image_labels::image_labels(request.project_id, request.deployment_id, &output),
                )
                .await?;
                output
            }
            DeploymentBuilder::Auto => unreachable!("auto builder must be resolved before build"),
        };

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        environment: &[(&str, &str)],
        build_command: Option<&str>,
        start_command: Option<&str>,
    ) -> BuildRequest {
        BuildRequest {
            project_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            workspace: PathBuf::from("/workspace"),
            repository: PathBuf::from("/workspace/source"),
            source: PathBuf::from("/workspace/source/apps/web"),
            image: "sakala/project-test:latest".to_owned(),
            requested: DeploymentBuilder::Railpack,
            environment: environment
                .iter()
                .map(|(name, value)| ((*name).to_owned(), SecretString::new((*value).to_owned())))
                .collect(),
            build_command: build_command.map(str::to_owned),
            start_command: start_command.map(str::to_owned),
        }
    }

    fn exposed(environment: &[(String, SecretString)]) -> Vec<(&str, &str)> {
        environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.expose()))
            .collect()
    }

    #[test]
    fn a_project_variable_overrides_what_a_workspace_derives_but_not_an_explicit_command() {
        let derived = WorkspaceCommands {
            build: Some("pnpm --filter 'web...' run build".to_owned()),
            start: None,
            static_output_directory: Some("apps/web/dist".to_owned()),
        };
        let request = request(
            &[
                ("RAILPACK_BUILD_CMD", "make web"),
                ("RAILPACK_SPA_OUTPUT_DIR", "out"),
            ],
            None,
            None,
        );
        assert_eq!(
            exposed(&railpack_environment(&request, Some(&derived))),
            [
                ("RAILPACK_BUILD_CMD", "make web"),
                ("RAILPACK_SPA_OUTPUT_DIR", "out")
            ]
        );

        let explicit = WorkspaceCommands {
            build: Some("cd 'apps/web' && pnpm run build".to_owned()),
            ..derived
        };
        let request = BuildRequest {
            build_command: Some("pnpm run build".to_owned()),
            ..request
        };
        assert_eq!(
            exposed(&railpack_environment(&request, Some(&explicit))),
            [
                ("RAILPACK_BUILD_CMD", "cd 'apps/web' && pnpm run build"),
                ("RAILPACK_SPA_OUTPUT_DIR", "out")
            ]
        );
    }

    #[test]
    fn without_a_workspace_the_commands_pass_through_unchanged() {
        let request = request(
            &[("API_URL", "https://example.test")],
            Some("pnpm build"),
            None,
        );
        assert_eq!(
            exposed(&railpack_environment(&request, None)),
            [
                ("API_URL", "https://example.test"),
                ("RAILPACK_BUILD_CMD", "pnpm build")
            ]
        );
    }

    #[test]
    fn a_command_runs_in_the_package_only_for_a_workspace_build() {
        let output = BuildOutput {
            builder: DeploymentBuilder::Railpack,
            package_directory: Some("apps/web".to_owned()),
        };
        assert_eq!(
            output.in_package("pnpm run migrate".to_owned()),
            "cd 'apps/web' && pnpm run migrate"
        );
        let output = BuildOutput {
            builder: DeploymentBuilder::Railpack,
            package_directory: None,
        };
        assert_eq!(
            output.in_package("php artisan migrate".to_owned()),
            "php artisan migrate"
        );
    }
}
