use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use sakala_agent_protocol::{AppliedRuntimeResources, RuntimeResourceLimits};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};

mod docker;
pub(crate) mod limits;

pub use docker::DockerContainerEngine;
pub use limits::ResourceSafetyConfig;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunContainerRequest {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub name: String,
    pub image: String,
    pub workspace: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub resources: AppliedRuntimeResources,
}

#[async_trait]
pub trait ContainerEngine: Send + Sync {
    fn resolve_resources(
        &self,
        requested: RuntimeResourceLimits,
    ) -> Result<AppliedRuntimeResources, RuntimeError>;

    async fn start(
        &self,
        request: &RunContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    async fn report_startup_logs(
        &self,
        container: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    async fn cleanup_previous(
        &self,
        project_id: Uuid,
        current: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    async fn cleanup_candidate(&self, container: &str, image: &str);
}

#[must_use]
pub fn image_name(project_id: Uuid, deployment_id: Uuid, commit_sha: &str) -> String {
    format!(
        "sakala/project-{project_id}:{}-{}",
        &commit_sha[..12],
        &deployment_id.to_string()[..8]
    )
}

#[must_use]
pub fn container_name(project_id: Uuid, deployment_id: Uuid) -> String {
    format!("sakala-app-{project_id}-{deployment_id}")
}
