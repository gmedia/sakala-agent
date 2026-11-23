use std::{
    collections::{BTreeMap, HashSet},
    num::NonZeroU32,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, LogBounds, RuntimeResourceLimits,
    WorkloadMetrics,
};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter, routing::RouteIdentity};
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeNetworkReconciliation,
    RuntimeReconciliationReport, RuntimeStaleImage,
};

mod docker;
pub(crate) mod images;
pub(crate) mod limits;
pub(crate) mod metrics;
pub(crate) mod network;
pub(crate) mod replicas;

pub use docker::{ContainerDns, DockerContainerEngine, WorkloadIsolation};
pub use limits::ResourceSafetyConfig;
pub use network::project_network_name;
pub use replicas::{MAX_REPLICAS_PER_PROJECT_CEILING, ReplicaSlot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunContainerRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub name: String,
    pub replica: ReplicaSlot,
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

/// The one container that runs a deployment's release command.
///
/// It runs from the deployment's new image with the same environment,
/// resources, runtime, and security options as a replica, on the project's
/// network, but it serves nothing: it has no route, no domain or port label,
/// and no restart policy. It is not a managed workload either, so listings,
/// health checks, replica restore, and orphan detection never see it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseContainerRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub name: String,
    pub image: String,
    pub workspace: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub resources: AppliedRuntimeResources,
    /// Exported as `PORT`, as for the replicas, so a command reading the same
    /// configuration as the app sees the same value.
    pub port: u16,
    /// Runs in the container's own `sh -c`, like a start command.
    pub command: String,
}

/// How a release container ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleaseExit {
    pub exit_code: i32,
    /// Docker killed the container for exceeding its memory limit.
    pub oom_killed: bool,
}

impl ReleaseExit {
    #[must_use]
    pub fn succeeded(self) -> bool {
        self.exit_code == 0
    }
}

/// One container of a managed deployment, as Docker lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkload {
    pub container_id: String,
    /// The container name, which the router uses as the upstream host.
    pub name: String,
    pub replica: ReplicaSlot,
    pub status: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub domain: String,
    pub port: u16,
    pub command_id: Option<Uuid>,
    pub log_bounds: LogBounds,
}

impl ManagedWorkload {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.to_ascii_lowercase().starts_with("up")
    }
}

/// One managed container as a node-wide listing shows it, running or not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedReplica {
    pub container_id: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub replica: ReplicaSlot,
    /// Docker's status column, such as `Up 3 minutes` or `Exited (137) 1 minute ago`.
    pub status: String,
}

impl ListedReplica {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.to_ascii_lowercase().starts_with("up")
    }

    /// Stopped by `docker stop`, `docker kill`, or a sleep, so Docker's restart
    /// policy does not bring it back. `docker start` can.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        self.status.to_ascii_lowercase().starts_with("exited")
    }
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

    /// Refuses `replicas` more running containers for `project_id` when the
    /// node would then run more than its maximum. The project's own running
    /// containers are not counted: a new deployment replaces them, and the
    /// overlap while it starts is the same one a single container always had.
    /// A running release container of another project counts as one, for as
    /// long as its command runs.
    async fn ensure_capacity(
        &self,
        project_id: Uuid,
        replicas: NonZeroU32,
    ) -> Result<(), RuntimeError>;

    /// Sorts every managed container into running workloads, sleeping
    /// workloads, and orphans. A stopped container is sleeping when
    /// `sleeping_routes` holds its project and deployment, and an orphan
    /// otherwise.
    async fn detect_orphans(
        &self,
        sleeping_routes: &HashSet<RouteIdentity>,
    ) -> Result<RuntimeReconciliationReport, RuntimeError>;

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeError>;

    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeError>;

    /// Running managed workloads with their domain label, in one listing.
    async fn routed_workloads(&self) -> Result<Vec<RoutedWorkload>, RuntimeError>;

    /// Resolves the crash details of snapshot workloads Docker is restarting.
    async fn crashed_workloads(
        &self,
        snapshots: &[RuntimeHealthSnapshot],
    ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeError>;

    /// Samples CPU, memory, and restart count of every running managed
    /// workload container, one entry per replica, in one batch. Fails as a
    /// whole when Docker cannot be read or reports no usable values.
    async fn workload_metrics(&self) -> Result<Vec<WorkloadMetrics>, RuntimeError>;

    /// Every managed container on the node, running or not, in one listing.
    async fn replica_listing(&self) -> Result<Vec<ListedReplica>, RuntimeError>;

    /// Every container of one deployment, running or not, in replica order.
    /// Empty when the deployment has none on this node.
    async fn workloads(
        &self,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> Result<Vec<ManagedWorkload>, RuntimeError>;

    async fn restart(
        &self,
        workload: &ManagedWorkload,
        grace_seconds: u64,
    ) -> Result<(), RuntimeError>;

    /// Stops the running ones among `workloads` in one Docker call, which
    /// stops them side by side, so replicas share one grace period.
    async fn stop(
        &self,
        workloads: &[ManagedWorkload],
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

    /// Creates and starts the release container, detached, on the project's
    /// network. A command Docker cannot start at all, such as one in an image
    /// without `sh`, is a [`RuntimeError::ReleaseCommandFailed`]; the container
    /// may then exist in `Created` state and still needs [`Self::remove_release`].
    async fn start_release(
        &self,
        request: &ReleaseContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Reports the release container's output as it is written, until the
    /// container exits, and returns how it ended. Has no deadline of its own.
    async fn wait_release(
        &self,
        name: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ReleaseExit, RuntimeError>;

    /// Kills the release container when it still runs and removes it with its
    /// anonymous volumes. A container that is already gone is not an error.
    async fn remove_release(&self, name: &str) -> Result<(), RuntimeError>;

    /// Removes every release container this Agent left behind, running or
    /// not. Called at startup: a release container only lives as long as the
    /// deployment command that started it. Returns how many were removed.
    async fn remove_stale_releases(&self) -> Result<usize, RuntimeError>;

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

    /// Stops and removes every container of the project except `current`, the
    /// replicas of the deployment that just went live.
    async fn cleanup_previous(
        &self,
        project_id: Uuid,
        current: &[String],
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Attempts every candidate cleanup action, for all replicas of the
    /// candidate. A failure is returned only after all owned artifacts have
    /// been attempted, so callers can report partial cleanup without replacing
    /// the primary deployment error. `image` is the image the candidate built;
    /// `None` for a candidate that reused another deployment's image, which
    /// must stay.
    async fn cleanup_candidate(
        &self,
        containers: &[String],
        image: Option<&str>,
    ) -> Result<(), RuntimeError>;

    /// The labels of the image under `image`, or `None` when the node has no
    /// image under that reference.
    async fn image_labels(
        &self,
        image: &str,
    ) -> Result<Option<BTreeMap<String, String>>, RuntimeError>;

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

/// Names the release container of a deployment. Nothing resolves it, but it
/// stays as short as a workload name so `docker ps` groups it with them.
#[must_use]
pub fn release_container_name(project_id: Uuid, deployment_id: Uuid) -> String {
    let project = project_id.to_string();
    format!("sakala-release-{}-{deployment_id}", &project[..8])
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
