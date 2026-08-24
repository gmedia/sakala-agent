use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::DeploymentBuilder;
use tokio::fs;
use uuid::Uuid;

use crate::{ProcessRunner, RuntimeError, RuntimeReporter, process::run_checked};

mod dockerfile;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildRequest {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub workspace: PathBuf,
    pub source: PathBuf,
    pub image: String,
    pub requested: DeploymentBuilder,
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
}

impl ImageBuildService {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, railpack_frontend: String) -> Self {
        Self {
            runner,
            railpack_frontend,
        }
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

        match builder {
            DeploymentBuilder::Dockerfile => {
                let command = dockerfile::build_command(
                    &request.source,
                    &request.source.join("Dockerfile"),
                    &request.image,
                    request.project_id,
                    request.deployment_id,
                );
                run_checked(self.runner.as_ref(), &command, "docker-build", reporter).await?;
            }
            DeploymentBuilder::Railpack => {
                let plan = request.workspace.join("railpack-plan.json");
                let info = request.workspace.join("railpack-info.json");
                let prepare = crate::railpack::prepare_command(&request.source, &plan, &info);
                run_checked(self.runner.as_ref(), &prepare, "railpack-prepare", reporter).await?;
                let build = crate::railpack::build_command(
                    &request.source,
                    &plan,
                    &request.image,
                    &self.railpack_frontend,
                    request.project_id,
                    request.deployment_id,
                );
                run_checked(self.runner.as_ref(), &build, "railpack-build", reporter).await?;
            }
            DeploymentBuilder::Auto => unreachable!("auto builder must be resolved before build"),
        }

        Ok(BuildOutput { builder })
    }
}
