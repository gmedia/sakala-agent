use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::{AppliedRuntimeResources, LogBounds, RuntimeResourceLimits};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeReconciliationReport, RuntimeStaleImage,
};

mod docker;
pub(crate) mod limits;

pub use docker::DockerContainerEngine;
pub use limits::ResourceSafetyConfig;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunContainerRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub name: String,
    pub image: String,
    pub workspace: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub resources: AppliedRuntimeResources,
    pub domain: String,
    pub port: u16,
    pub log_bounds: LogBounds,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkload {
    pub container_id: String,
    pub status: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub port: u16,
    pub command_id: Option<Uuid>,
    pub log_bounds: LogBounds,
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

    /// Starts at most one follower for a container. Returns whether a new
    /// follower was registered.
    fn start_log_follower(&self, container: &str, reporter: Arc<dyn RuntimeReporter>) -> bool;

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

    /// Inventories dangling Sakala-managed images before any deletion runs.
    async fn detect_stale_images(
        &self,
        _max_age: std::time::Duration,
    ) -> Result<Vec<RuntimeStaleImage>, RuntimeError> {
        Ok(Vec::new())
    }

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

/// Names a managed workload so the runtime router can reach it by name.
///
/// The name doubles as the Caddy upstream host, so it must stay inside the
/// 63-octet DNS label limit. Two full UUIDs plus the prefix are 84 octets,
/// which Docker accepts as a container name but its embedded DNS server
/// cannot answer for, so every route resolved to nothing and returned 502.
/// The deployment UUID alone identifies the workload; a short project prefix
/// is kept so `docker ps` still groups a project's containers visibly.
#[must_use]
pub fn container_name(project_id: Uuid, deployment_id: Uuid) -> String {
    let project = project_id.to_string();
    format!("sakala-app-{}-{deployment_id}", &project[..8])
}

#[cfg(test)]
mod tests {
    use super::container_name;
    use uuid::Uuid;

    #[test]
    fn container_name_fits_a_dns_label() {
        let name = container_name(
            Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID"),
            Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID"),
        );

        // Caddy resolves this name through Docker's embedded DNS, which cannot
        // answer for a label longer than 63 octets.
        assert!(name.len() <= 63, "{name} is {} octets", name.len());
        assert_eq!(
            name,
            "sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993"
        );
    }
}
