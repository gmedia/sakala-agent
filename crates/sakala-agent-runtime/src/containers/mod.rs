use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, LogBounds, RuntimeResourceLimits,
};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeNetworkReconciliation,
    RuntimeReconciliationReport, RuntimeStaleImage,
};

mod docker;
pub(crate) mod images;
pub(crate) mod limits;
pub(crate) mod network;

pub use docker::{ContainerDns, DockerContainerEngine, WorkloadIsolation};
pub use limits::ResourceSafetyConfig;
pub use network::project_network_name;

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
    /// Replaces the image's entrypoint and command, for images that have no way
    /// to take it at build time.
    pub start_command: Option<String>,
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

/// A running managed workload and the domain it is routed under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedWorkload {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
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

    /// Running managed workloads with their domain label, in one listing.
    async fn routed_workloads(&self) -> Result<Vec<RoutedWorkload>, RuntimeError>;

    /// Resolves the crash details of snapshot workloads Docker is restarting.
    async fn crashed_workloads(
        &self,
        snapshots: &[RuntimeHealthSnapshot],
    ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeError>;

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

    /// Starts a workload on its project's own network, which it creates first
    /// and connects the router to. The workload never joins the shared runtime
    /// network, so it cannot reach other projects.
    async fn start(
        &self,
        request: &RunContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Connects the router to the project's network again, when the project has
    /// one. A workload started by an older Agent lives on the shared network.
    async fn attach_router(&self, project_id: Uuid) -> Result<(), RuntimeError>;

    /// Disconnects the router and removes the project's network once no
    /// container, running or stopped, uses it.
    async fn release_network(&self, project_id: Uuid) -> Result<(), RuntimeError>;

    /// Reconnects the router to the project networks it lost and, with
    /// `remove_unused`, removes the ones no container uses. Removal never
    /// overlaps a workload start.
    async fn reconcile_networks(
        &self,
        remove_unused: bool,
    ) -> Result<RuntimeNetworkReconciliation, RuntimeError>;

    async fn report_startup_logs(
        &self,
        container: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Reports the last lines of a candidate that failed before its route went
    /// live, and returns what they showed.
    async fn report_failure_logs(
        &self,
        container: &str,
        log_bounds: LogBounds,
        reporter: &dyn RuntimeReporter,
    ) -> Result<FailureOutput, RuntimeError>;

    /// Reads a workload's environment values, only so that a log follower rebuilt
    /// after an Agent restart can keep hiding them. The agent keeps no copy of a
    /// command's payload, and the result must never be logged.
    async fn environment_values(&self, container: &str) -> Result<Vec<String>, RuntimeError>;

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

    /// Lists the Sakala images that [`Self::cleanup_stale_images`] would remove,
    /// without removing anything.
    async fn detect_stale_images(
        &self,
        _retention: ImageRetention,
    ) -> Result<Vec<RuntimeStaleImage>, RuntimeError> {
        Ok(Vec::new())
    }

    /// Removes old images of Sakala projects. Per project with a container it
    /// keeps every image a container uses, the previous deployment's image, and
    /// anything younger than `retention.max_age`. A project without containers
    /// keeps only images younger than `retention.orphaned_grace`. Docker itself
    /// still refuses an image a container uses.
    async fn cleanup_stale_images(
        &self,
        retention: ImageRetention,
    ) -> Result<ImageCleanup, RuntimeError>;

    /// Removes every image of one project that no container uses, whatever its
    /// age. Called once the containers of a deleted project are gone.
    async fn remove_project_images(&self, project_id: Uuid) -> Result<ImageCleanup, RuntimeError>;

    async fn shutdown(&self);
}

/// How long image GC keeps the images of Sakala projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRetention {
    /// A project that still has a container keeps every image younger than this.
    pub max_age: std::time::Duration,
    /// A project without any container keeps only images younger than this,
    /// counted from when the image was built.
    pub orphaned_grace: std::time::Duration,
}

/// What one image GC run removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImageCleanup {
    pub removed_images: u64,
    /// Docker's sizes of the removed images added up. Layers shared with images
    /// that remain are counted too, so this is an upper bound.
    pub reclaimed_bytes: u64,
}

/// What the last output of a failed candidate showed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FailureOutput {
    /// The container wrote anything at all.
    pub had_output: bool,
    /// A shell or runc reported that the kernel refused to execute a program.
    pub exec_denied: bool,
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
