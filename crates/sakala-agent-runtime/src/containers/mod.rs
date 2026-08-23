use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::{AppliedRuntimeResources, RuntimeResourceLimits};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeReconciliationReport,
};

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
    pub domain: String,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkload {
    pub container_id: String,
    pub status: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub port: u16,
}

#[async_trait]
pub trait ContainerEngine: Send + Sync {
    fn resolve_resources(
        &self,
        requested: RuntimeResourceLimits,
    ) -> Result<AppliedRuntimeResources, RuntimeError>;

    async fn ensure_capacity(&self, project_id: Uuid) -> Result<(), RuntimeError>;

    async fn detect_orphans(&self) -> Result<RuntimeReconciliationReport, RuntimeError>;

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeError>;

    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeError>;

    async fn workload(
        &self,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> Result<Option<ManagedWorkload>, RuntimeError>;

    async fn restart(
        &self,
        workload: &ManagedWorkload,
        grace_seconds: u64,
    ) -> Result<(), RuntimeError>;

    async fn stop(
        &self,
        workload: &ManagedWorkload,
        grace_seconds: u64,
    ) -> Result<(), RuntimeError>;

    async fn start_existing(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError>;

    async fn remove(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError>;

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

    fn start_log_follower(&self, container: &str, reporter: Arc<dyn RuntimeReporter>);

    async fn cleanup_previous(
        &self,
        project_id: Uuid,
        current: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Attempts every candidate cleanup action. A failure is returned only
    /// after all owned artifacts have been attempted, so callers can report
    /// partial cleanup without replacing the primary deployment error.
    async fn cleanup_candidate(&self, container: &str, image: &str) -> Result<(), RuntimeError>;

    /// Reclaims only dangling images explicitly labeled as Sakala-managed.
    /// Docker itself refuses images referenced by any container.
    async fn cleanup_stale_images(&self, max_age: std::time::Duration)
    -> Result<u64, RuntimeError>;

    async fn shutdown(&self);
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
