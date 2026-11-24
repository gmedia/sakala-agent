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
pub(crate) mod workers;

pub use docker::{
    ContainerDns, DEFAULT_WEB_STOP_GRACE, DEFAULT_WORKER_STOP_GRACE, DockerContainerEngine,
    StopGrace, WorkloadIsolation,
};
pub use images::{PREVIOUS_IMAGE_TAG, previous_image_reference, project_repository};
pub use limits::ResourceSafetyConfig;
pub use network::project_network_name;
pub use replicas::{MAX_REPLICAS_PER_PROJECT_CEILING, ReplicaSlot};
pub use workers::worker_container_name;

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
    /// to take it at build time. Always set for a worker, which runs its own.
    pub start_command: Option<String>,
    /// The worker process this container runs, `None` for a web replica.
    pub process: Option<String>,
}

/// What a one-off container runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OneOffRole {
    /// A deployment's release command, before its replicas start.
    Release,
    /// A `RunJob` command, from the live image.
    Job,
}

impl OneOffRole {
    /// Every role, for listings that must find any one-off container.
    pub const ALL: [Self; 2] = [Self::Release, Self::Job];

    /// The value of the `dev.sakala.role` label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Release => sakala_agent_protocol::RELEASE_PROCESS,
            Self::Job => sakala_agent_protocol::JOB_PROCESS,
        }
    }

    /// The `process` of the log lines it writes. A release command's lines
    /// stay without one, as before worker processes.
    #[must_use]
    pub const fn log_process(self) -> Option<&'static str> {
        match self {
            Self::Release => None,
            Self::Job => Some(sakala_agent_protocol::JOB_PROCESS),
        }
    }
}

/// The one container that runs a deployment's release command, or one job.
///
/// It runs from a deployment's image with the same environment, resources,
/// runtime, and security options as a replica, on the project's network, but
/// it serves nothing: it has no route, no domain or port label, and no restart
/// policy. It is not a managed workload either, so listings, health checks,
/// replica restore, and orphan detection never see it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OneOffContainerRequest {
    pub role: OneOffRole,
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

/// How a one-off container ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OneOffExit {
    pub exit_code: i32,
    /// Docker killed the container for exceeding its memory limit.
    pub oom_killed: bool,
}

impl OneOffExit {
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
    /// The worker process this container runs, `None` for a web replica.
    pub process: Option<String>,
}

impl ManagedWorkload {
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.to_ascii_lowercase().starts_with("up")
    }

    /// A container behind the route, as opposed to a worker's.
    #[must_use]
    pub fn is_web(&self) -> bool {
        self.process.is_none()
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
    /// The worker process the container runs, `None` for a web replica.
    pub process: Option<String>,
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
    /// node would then run more than its maximum. `replicas` counts every
    /// container of the deployment, its workers included. The project's own
    /// running workloads are not counted: a new deployment replaces them, and
    /// the overlap while it starts is the same one a single container always
    /// had. A running one-off container counts as one for as long as its
    /// command runs: a release container of another project, and a job of
    /// any project, because a deployment never replaces a job.
    async fn ensure_capacity(
        &self,
        project_id: Uuid,
        replicas: NonZeroU32,
    ) -> Result<(), RuntimeError>;

    /// Refuses one more container, for a job, when the node would then run
    /// more than its maximum. Every running container counts, the job's own
    /// project included: a job runs next to it.
    async fn ensure_job_capacity(&self) -> Result<(), RuntimeError>;

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

    /// Restarts one container with the stop grace of its process.
    async fn restart(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError>;

    /// Stops the running ones among `workloads` side by side, so the
    /// containers of one process share one grace period: web replicas get
    /// the web grace, workers the worker grace (see [`StopGrace`]).
    async fn stop(&self, workloads: &[ManagedWorkload]) -> Result<(), RuntimeError>;

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

    /// Creates and starts a one-off container, detached, on the project's
    /// network. A command Docker cannot start at all, such as one in an image
    /// without `sh`, is a [`RuntimeError::ReleaseCommandFailed`] for a release
    /// and a [`RuntimeError::JobFailed`] for a job; a job image that is gone is
    /// a [`RuntimeError::JobImageMissing`]. The container may then exist in
    /// `Created` state and still needs [`Self::remove_one_off`].
    async fn start_one_off(
        &self,
        request: &OneOffContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError>;

    /// Reports a one-off container's output as it is written, until the
    /// container exits, and returns how it ended. Has no deadline of its own.
    async fn wait_one_off(
        &self,
        name: &str,
        role: OneOffRole,
        reporter: &dyn RuntimeReporter,
    ) -> Result<OneOffExit, RuntimeError>;

    /// Kills a one-off container when it still runs and removes it with its
    /// anonymous volumes. A container that is already gone is not an error.
    async fn remove_one_off(&self, name: &str) -> Result<(), RuntimeError>;

    /// Removes every one-off container this Agent left behind, running or
    /// not. Called at startup: a one-off container only lives as long as the
    /// command that started it. Returns how many were removed.
    async fn remove_stale_one_offs(&self) -> Result<usize, RuntimeError>;

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
    /// web replicas and workers of the deployment that just went live. Each
    /// gets the stop grace of its process.
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
    /// keeps every image a container uses, in any state, and the previous
    /// image (the one tagged [`PREVIOUS_IMAGE_TAG`]). A project without containers keeps only images
    /// younger than `retention.orphaned_grace`. Docker itself still refuses an
    /// image a container uses.
    ///
    /// The images of a project are removed only while `claim` gives that
    /// project, and are chosen again under the claim, so a command of the
    /// project never runs between the choice and the removal. A project
    /// `claim` refuses keeps its images until the next run.
    async fn cleanup_stale_images(
        &self,
        retention: ImageRetention,
        claim: &ClaimProject<'_>,
    ) -> Result<ImageCleanup, RuntimeError>;

    /// Removes every image of one project that no container uses, whatever its
    /// age. Called once the containers of a deleted project are gone.
    async fn remove_project_images(&self, project_id: Uuid) -> Result<ImageCleanup, RuntimeError>;

    /// Tags the image of the workload a deployment that just went live
    /// replaced as `sakala/project-<project_id>:previous`, when it differs
    /// from the image the new replicas (`current`) run and is provably the
    /// project's. Returns the tagged image ID, or `None` when the previous
    /// image stays as it was. Called with the project held, after the route
    /// moved and before the replaced containers are removed, so a crash in
    /// between leaves the tag right and the containers to orphan cleanup.
    async fn mark_previous_image(
        &self,
        _project_id: Uuid,
        _current: &[String],
    ) -> Result<Option<String>, RuntimeError> {
        Ok(None)
    }

    /// Applies the rule of [`Self::cleanup_stale_images`] to one project, with
    /// the project held by the caller. After a successful deployment that
    /// leaves the live image and the previous image.
    async fn prune_project_images(
        &self,
        _project_id: Uuid,
        _retention: ImageRetention,
    ) -> Result<ImageCleanup, RuntimeError> {
        Ok(ImageCleanup::default())
    }

    async fn shutdown(&self);
}

/// How long image GC keeps the images of a project without containers. A
/// project with containers keeps its live and previous images whatever their
/// age, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageRetention {
    /// A project without any container keeps only images younger than this,
    /// counted from when the image was built.
    pub orphaned_grace: std::time::Duration,
}

/// A project held away from its commands while image GC removes its images.
pub type ProjectClaim = tokio::sync::OwnedMutexGuard<()>;

/// Gives image GC a project, or `None` while a command of that project runs.
pub type ClaimProject<'a> = dyn Fn(Uuid) -> Option<ProjectClaim> + Send + Sync + 'a;

/// What one image GC run removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImageCleanup {
    pub removed_images: u64,
    /// Docker's sizes of the removed images added up. Layers shared with images
    /// that remain are counted too, so this is an upper bound.
    pub reclaimed_bytes: u64,
}

impl std::ops::AddAssign for ImageCleanup {
    fn add_assign(&mut self, other: Self) {
        self.removed_images += other.removed_images;
        self.reclaimed_bytes += other.reclaimed_bytes;
    }
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
        "{}:{}-{}",
        images::project_repository(project_id),
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

/// Names the container of one `RunJob` command. The command ID keeps one
/// command from ever running two containers: Docker refuses a second
/// container under the same name.
#[must_use]
pub fn job_container_name(project_id: Uuid, command_id: Uuid) -> String {
    let project = project_id.to_string();
    format!("sakala-job-{}-{command_id}", &project[..8])
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
