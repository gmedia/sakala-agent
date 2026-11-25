use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    num::NonZeroU32,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::ports::{
    RuntimeCapacity, RuntimeHealthSnapshot, RuntimeNetworkReconciliation, RuntimeOrphan,
    RuntimeReconciliationReport, RuntimeStaleImage, RuntimeWorkload,
};
use sakala_agent_protocol::{
    AppliedRuntimeResources, CrashedWorkloadDetail, LogBounds, RuntimeResourceLimits,
    ValkeyAttachment, WorkloadMetrics,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    task::JoinHandle,
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    containers::limits::docker_cpu_value,
    containers::{
        ClaimProject, ContainerEngine, FailureOutput, ImageCleanup, ImageRetention, IsActiveOneOff,
        ListedReplica, ManagedWorkload, OneOffContainerRequest, OneOffExit, OneOffRole,
        ResourceSafetyConfig, RoutedWorkload, RunContainerRequest,
        images::{
            CONTAINER_IMAGE_FORMAT, ContainerUsage, IMAGE_INSPECT_FORMAT, IMAGE_LABELS_FORMAT,
            IMAGE_LIST_FORMAT, InspectedImage, ListedImage, NO_SUCH_IMAGE,
            PROJECT_CONTAINER_FORMAT, Retention, StaleImage, adopt_unlabelled_images,
            owned_references, parse_container_usage, parse_image_inspect, parse_image_labels,
            parse_image_list, parse_project_containers, previous_image_reference, replaced_image,
            select_project_images, select_stale_images, with_named_images,
        },
        metrics::{
            METRICS_INSPECT_FORMAT, STATS_FORMAT, join_sample, parse_inspected_states,
            parse_metrics_target, parse_stats,
        },
        network::ProjectNetworks,
        replicas::{REPLICA_LABEL, REPLICAS_LABEL, ReplicaSlot},
        workers::{PROCESS_LABEL, WORKLOAD_KIND_LABEL_KEY, process_from_labels, process_labels},
    },
    logs::ReporterOutputSink,
    process::run_checked,
    routing::RouteIdentity,
};

const MANAGED_LABEL: &str = "dev.sakala.managed=true";
const AGENT_ID_LABEL: &str = "dev.sakala.agent-id";
const DOMAIN_LABEL: &str = "dev.sakala.domain";
const PORT_LABEL: &str = "dev.sakala.port";
const COMMAND_ID_LABEL: &str = "dev.sakala.command-id";
const LOG_MAX_LINE_LABEL: &str = "dev.sakala.log-max-line-length";
const LOG_MAX_BATCH_LABEL: &str = "dev.sakala.log-max-batch-lines";
const LOG_MAX_TOTAL_LABEL: &str = "dev.sakala.log-max-total-bytes";
const PROJECT_ID_LABEL: &str = "dev.sakala.project-id";
const DEPLOYMENT_ID_LABEL: &str = "dev.sakala.deployment-id";
/// Marks a one-off container, `dev.sakala.role=<role>` with the role of
/// [`OneOffRole::as_str`]: `release` or `job`. The Agent never sets
/// `MANAGED_LABEL` on it, but Docker copies the image's labels to the
/// container and every Sakala image carries `MANAGED_LABEL`. So every listing
/// filtered on `MANAGED_LABEL` also prints this label and skips a container
/// that has it (see [`OneOffRole::is_labelled`]).
const ROLE_LABEL_KEY: &str = "dev.sakala.role";
/// `docker ps` of one-off containers by name, for their removal.
const ONE_OFF_NAME_FORMAT: &str = "{{.Names}}";
/// Why a stopped container no sleeping route points at is an orphan.
const STALE_STOPPED_REASON: &str = "stale stopped deployment container";
/// Restart policy of a workload, web or worker. A one-off container keeps
/// Docker's default, which never restarts: it runs once.
const WORKLOAD_RESTART_POLICY: &str = "unless-stopped";
/// Keeps a job from ever reaching a registry: its image is local or missing.
const JOB_PULL_POLICY: &str = "never";
/// What Docker says when `--pull never` finds no local image.
const NO_SUCH_IMAGE_MESSAGE: &str = "no such image";
/// How long a web replica gets between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_WEB_STOP_GRACE: Duration = Duration::from_secs(10);
/// Default of `SAKALA_WORKER_STOP_GRACE_SECONDS`: how long a worker gets
/// between `SIGTERM` and `SIGKILL`, to finish the job in hand (ADR-022).
pub const DEFAULT_WORKER_STOP_GRACE: Duration = Duration::from_secs(30);
/// What `docker run` exits with when it cannot invoke the command (126) or
/// cannot find it (127), such as `sh` missing from the image.
const COMMAND_NOT_RUNNABLE_EXIT_CODES: [i32; 2] = [126, 127];
/// What Docker says, in lower case, about a container that is gone: `No such
/// container` from `docker rm`, `no such object` from `docker inspect`.
const NO_SUCH_MESSAGE: &str = "no such";
/// What `docker rm` says, in lower case, while another removal of the
/// container runs.
const REMOVAL_IN_PROGRESS_MESSAGE: &str = "is already in progress";
/// How long a removal of a one-off container that Docker already runs is
/// awaited. Docker waits about ten seconds for a killed container to exit,
/// and gVisor on a loaded node can take that long.
const ONE_OFF_REMOVAL_IN_PROGRESS_WAIT: Duration = Duration::from_secs(30);
/// How often that removal is checked.
const ONE_OFF_REMOVAL_POLL: Duration = Duration::from_millis(500);
/// `docker inspect` of how a one-off container ended.
pub(crate) const ONE_OFF_INSPECT_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_LOG_TAIL_LINES: u64 = 100;
const FAILURE_LOG_TIMEOUT: Duration = Duration::from_secs(10);
const CRASH_INSPECT_TIMEOUT: Duration = Duration::from_secs(1);
const EXIT_EVENTS_TIMEOUT: Duration = Duration::from_secs(1);
/// From this many restarts, a running workload counts as crash-looping rather than
/// as an app that crashed once and recovered.
const CRASH_LOOP_MIN_RESTARTS: u32 = 3;
const ENVIRONMENT_INSPECT_TIMEOUT: Duration = Duration::from_secs(5);
/// `docker image inspect` of the image a deployment asks to reuse. Past it the
/// deployment builds instead.
const IMAGE_LABELS_TIMEOUT: Duration = Duration::from_secs(10);
/// `docker ps` of a metrics sample.
const METRICS_LISTING_TIMEOUT: Duration = Duration::from_secs(2);
/// `docker stats --no-stream` waits for a second reading about a second after
/// the first to compute CPU usage, so it takes about two seconds on an idle node.
const METRICS_STATS_TIMEOUT: Duration = Duration::from_secs(8);
/// `docker inspect` of a metrics sample.
const METRICS_INSPECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Set by the image or by Sakala itself, and common in ordinary log text.
const NON_SECRET_ENVIRONMENT_KEYS: [&str; 3] = ["PATH", "HOME", "PORT"];
/// `docker inspect` of a container the new deployment replaces: whether it
/// runs, its name, the worker process it runs, empty for a web replica, and
/// its one-off role, empty for a workload.
const PREVIOUS_CONTAINER_FORMAT: &str = "{{.State.Running}}\t{{.Name}}\t{{index .Config.Labels \"dev.sakala.process\"}}\t{{index .Config.Labels \"dev.sakala.role\"}}";
const CRASH_STATE_FORMAT: &str = "{{.Id}}\t{{.State.Status}}\t{{.RestartCount}}\t{{.State.ExitCode}}\t{{.State.OOMKilled}}\t{{.State.StartedAt}}\t{{.State.FinishedAt}}";
const EXIT_EVENTS_FORMAT: &str = "{{.Actor.ID}}\t{{.Action}}\t{{.Actor.Attributes.exitCode}}";
/// Exit code of a SIGKILL, which is what the kernel OOM killer sends.
const SIGKILL_EXIT_CODE: i32 = 137;
/// The exit of a worker that ended on purpose, which is never a crash.
const EXPECTED_WORKER_EXIT_CODE: i32 = 0;
/// The only Linux capabilities a workload keeps after `--cap-drop ALL`.
///
/// Most images start as root and then switch to an unprivileged user: the
/// official nginx image chowns its cache directories to uid 101 before its
/// workers drop to that user, and httpd, `gosu`/`su-exec` entrypoints,
/// supervisord, and php-fpm pools do the same. Without CHOWN, SETUID, and
/// SETGID nginx exits 1 at start, and httpd keeps serving as root because its
/// workers cannot drop. The rest of Docker's default set stays dropped:
/// NET_RAW (forged packets on the shared bridge), MKNOD, SYS_CHROOT,
/// AUDIT_WRITE, SETFCAP, and SETPCAP. NET_BIND_SERVICE is here because the
/// kernel refuses to exec a binary marked `cap_net_bind_service=ep`, such as
/// FrankenPHP or Caddy, when it is outside the bounding set.
/// `no-new-privileges` stays on, so a non-root process gains none of these
/// through a file. See docs/DOCKER_RUNTIME.md#capability-workload.
const WORKLOAD_CAPABILITIES: [&str; 8] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "NET_BIND_SERVICE",
];

/// The name servers workloads use instead of Docker's embedded DNS. The file outlives
/// every workspace, because Docker reads it again whenever a container restarts.
pub struct ContainerDns {
    path: std::path::PathBuf,
    content: String,
}

impl ContainerDns {
    /// `None` when there are no name servers, which leaves DNS to Docker.
    #[must_use]
    pub fn new(workspace_root: &Path, servers: Vec<std::net::IpAddr>) -> Option<Self> {
        (!servers.is_empty()).then(|| Self {
            path: workspace_root.join(".sakala-resolv.conf"),
            content: servers
                .iter()
                .map(|server| format!("nameserver {server}\n"))
                .collect(),
        })
    }

    /// Written through a rename, so a container starting now never reads half a file.
    async fn ensure_mount(&self) -> Result<std::ffi::OsString, RuntimeError> {
        if fs::read_to_string(&self.path).await.ok().as_deref() != Some(self.content.as_str()) {
            let partial = self.path.with_extension(format!("{}", Uuid::new_v4()));
            fs::write(&partial, &self.content).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o644)).await?;
            }
            fs::rename(&partial, &self.path).await?;
        }
        let mut mount = std::ffi::OsString::from("type=bind,src=");
        mount.push(&self.path);
        mount.push(",dst=/etc/resolv.conf,readonly");
        Ok(mount)
    }
}

/// How long each kind of workload gets between `SIGTERM` and `SIGKILL` when
/// the Agent stops or restarts it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopGrace {
    pub web: Duration,
    pub worker: Duration,
}

impl Default for StopGrace {
    fn default() -> Self {
        Self {
            web: DEFAULT_WEB_STOP_GRACE,
            worker: DEFAULT_WORKER_STOP_GRACE,
        }
    }
}

impl StopGrace {
    /// The grace of the process a container runs, in whole seconds for
    /// `docker stop --time`.
    fn seconds_for(self, process: Option<&str>) -> u64 {
        match process {
            None => self.web.as_secs(),
            Some(_) => self.worker.as_secs(),
        }
    }
}

/// What a node adds to every workload it starts, as opposed to what a command asks for.
#[derive(Default)]
pub struct WorkloadIsolation {
    pub container_runtime: Option<String>,
    pub container_dns: Option<ContainerDns>,
}

pub struct DockerContainerEngine {
    runner: Arc<dyn ProcessRunner>,
    networks: ProjectNetworks,
    resource_safety: ResourceSafetyConfig,
    max_active_containers: u32,
    agent_id: String,
    crash_stable_window: Duration,
    container_runtime: Option<String>,
    container_dns: Option<ContainerDns>,
    stop_grace: StopGrace,
    /// Restarts already written to the operator log, by container.
    logged_restarts: Mutex<HashMap<String, u32>>,
    log_followers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl DockerContainerEngine {
    /// What image GC may remove now, of every project or of one. See
    /// [`super::images`] for the rules.
    async fn stale_images(
        &self,
        project_id: Option<Uuid>,
        retention: ImageRetention,
    ) -> Result<Vec<StaleImage>, RuntimeError> {
        let retention = Retention {
            orphaned_grace: time::Duration::try_from(retention.orphaned_grace).map_err(|_| {
                RuntimeError::Configuration("image GC retention is too large".to_owned())
            })?,
        };
        let Some((listed, inspected, usage)) = self.image_inventory(project_id).await? else {
            return Ok(Vec::new());
        };
        let mut stale = select_stale_images(
            &listed,
            &inspected,
            &usage,
            OffsetDateTime::now_utc(),
            retention,
        );
        // The listing is already filtered by the project's label; this keeps a
        // claim on one project from ever covering another's images.
        if let Some(project_id) = project_id {
            stale.retain(|image| image.project_id == project_id);
        }
        Ok(stale)
    }

    /// Managed images, of one project or all, with their names. A project's
    /// listing filters on its label, so every image in it carries that label.
    async fn image_listing(
        &self,
        project_id: Option<Uuid>,
    ) -> Result<Vec<ListedImage>, RuntimeError> {
        let mut list = CommandSpec::new("docker")
            .arg("image")
            .arg("ls")
            .arg("--no-trunc")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"));
        if let Some(project_id) = project_id {
            list = list
                .arg("--filter")
                .arg(format!("label={PROJECT_ID_LABEL}={project_id}"));
        }
        let list = list.arg("--format").arg(IMAGE_LIST_FORMAT);
        Ok(parse_image_list(
            &self.read_docker(&list, "Sakala image listing").await?,
        ))
    }

    /// Managed images, of one project or all, with what containers use, plus
    /// the unlabelled images named in a project's repository. `None` when there
    /// is no such image.
    async fn image_inventory(
        &self,
        project_id: Option<Uuid>,
    ) -> Result<Option<(Vec<ListedImage>, Vec<InspectedImage>, ContainerUsage)>, RuntimeError> {
        let labelled = self.image_listing(project_id).await?;
        // Unfiltered: a `reference` filter would hide an image's other names,
        // and those decide that the image is not the project's.
        let all = CommandSpec::new("docker")
            .arg("image")
            .arg("ls")
            .arg("--no-trunc")
            .arg("--format")
            .arg(IMAGE_LIST_FORMAT);
        let all = parse_image_list(&self.read_docker(&all, "image listing").await?);
        let listed = with_named_images(labelled, all, project_id);
        if listed.is_empty() {
            return Ok(None);
        }
        let mut inspect = CommandSpec::new("docker")
            .arg("image")
            .arg("inspect")
            .arg("--format")
            .arg(IMAGE_INSPECT_FORMAT);
        for image in &listed {
            inspect = inspect.arg(&image.id);
        }
        let mut inspected = parse_image_inspect(
            &self
                .read_docker(&inspect, "Sakala image inspection")
                .await?,
        )?;
        adopt_unlabelled_images(&listed, &mut inspected);

        // Every container counts, Sakala's or not, stopped or running.
        let containers = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--quiet")
            .arg("--no-trunc");
        let container_ids = self.read_docker(&containers, "container listing").await?;
        let mut usage = ContainerUsage::default();
        if !container_ids.trim().is_empty() {
            let mut images = CommandSpec::new("docker")
                .arg("container")
                .arg("inspect")
                .arg("--format")
                .arg(CONTAINER_IMAGE_FORMAT);
            for id in container_ids.split_whitespace() {
                images = images.arg(id);
            }
            usage = parse_container_usage(
                &self
                    .read_docker(&images, "container image inspection")
                    .await?,
            );
        }
        Ok(Some((listed, inspected, usage)))
    }

    /// Removes each image by its names. Without `--force`, Docker refuses an
    /// image a container started using after the selection, so a race keeps
    /// the image. A failure is logged and the rest are still attempted.
    async fn remove_images(&self, images: Vec<StaleImage>) -> ImageCleanup {
        let mut cleanup = ImageCleanup::default();
        for image in images {
            let mut command = CommandSpec::new("docker").arg("image").arg("rm");
            for reference in &image.references {
                command = command.arg(reference);
            }
            match self.runner.run(&command, &NullOutputSink).await {
                Ok(output) if output.success => {
                    cleanup.removed_images += 1;
                    cleanup.reclaimed_bytes += image.size_bytes;
                }
                Ok(output) => warn!(
                    image = %image.id,
                    project_id = %image.project_id,
                    status = %crate::error::describe_exit(output.code),
                    "Docker kept a Sakala image"
                ),
                Err(error) => warn!(
                    image = %image.id,
                    project_id = %image.project_id,
                    %error,
                    "Sakala image removal failed"
                ),
            }
        }
        cleanup
    }

    async fn read_docker(&self, command: &CommandSpec, what: &str) -> Result<String, RuntimeError> {
        let output = self.runner.run(command, &NullOutputSink).await?;
        if output.success {
            Ok(output.stdout)
        } else {
            Err(RuntimeError::Container(format!(
                "{what} failed with {}",
                crate::error::describe_exit(output.code)
            )))
        }
    }

    /// `router_container` is the runtime Caddy container, which joins every
    /// project network so it can reach each workload by name.
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        router_container: String,
        resource_safety: ResourceSafetyConfig,
        max_active_containers: u32,
        agent_id: String,
        crash_stable_window: Duration,
        isolation: WorkloadIsolation,
    ) -> Self {
        Self {
            networks: ProjectNetworks::new(Arc::clone(&runner), router_container, agent_id.clone()),
            runner,
            resource_safety,
            max_active_containers,
            agent_id,
            crash_stable_window,
            container_runtime: isolation.container_runtime,
            container_dns: isolation.container_dns,
            stop_grace: StopGrace::default(),
            logged_restarts: Mutex::new(HashMap::new()),
            log_followers: Mutex::new(HashMap::new()),
        }
    }

    /// Replaces the default [`StopGrace`].
    #[must_use]
    pub fn with_stop_grace(mut self, stop_grace: StopGrace) -> Self {
        self.stop_grace = stop_grace;
        self
    }

    /// The names of the one-off containers, running or not, that match every
    /// `docker ps` filter in `filters`.
    async fn one_off_names(&self, filters: &[String]) -> Result<Vec<String>, RuntimeError> {
        let mut list = CommandSpec::new("docker").arg("ps").arg("--all");
        for filter in filters {
            list = list.arg("--filter").arg(filter);
        }
        let list = list.arg("--format").arg(ONE_OFF_NAME_FORMAT);
        let listed = self.read_docker(&list, "docker one-off listing").await?;
        Ok(non_empty_lines(&listed).map(str::to_owned).collect())
    }

    /// Running containers that count against the node's maximum, by project:
    /// managed workloads, then one-off containers with their role.
    async fn running_containers(&self) -> Result<RunningContainers, RuntimeError> {
        let workloads = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg(format!(
                "{{{{.Label \"{PROJECT_ID_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"
            ));
        let workloads = self
            .read_docker(&workloads, "docker capacity inspection")
            .await?;
        // A one-off container is not a managed workload, but it runs on the
        // node like one until its command exits.
        let one_offs = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={ROLE_LABEL_KEY}"))
            .arg("--format")
            .arg(format!(
                "{{{{.Label \"{PROJECT_ID_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"
            ));
        let one_offs = self
            .read_docker(&one_offs, "docker one-off capacity inspection")
            .await?;
        Ok(RunningContainers {
            // A one-off container inherits the managed label from its image;
            // it is counted once, with the one-off containers below.
            workloads: non_empty_lines(&workloads)
                .filter_map(|line| {
                    let (project, role) = line.split_once('\t').unwrap_or((line, ""));
                    (!OneOffRole::is_labelled(Some(role))).then(|| project.trim().to_owned())
                })
                .collect(),
            one_offs: non_empty_lines(&one_offs)
                .map(|line| {
                    let (project, role) = line.split_once('\t').unwrap_or((line, ""));
                    (project.trim().to_owned(), role.trim().to_owned())
                })
                .collect(),
        })
    }

    /// Every managed workload container, running or not, as orphan detection
    /// sorts it, and how many one-off containers run. A one-off container
    /// carries the managed label it inherits from its image, but it is never
    /// a workload or an orphan: the sweep of one-off containers removes it
    /// once its command ended.
    async fn managed_containers(
        &self,
        sleeping_routes: &HashSet<RouteIdentity>,
    ) -> Result<ManagedContainers, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg(format!("{{{{.ID}}}}\t{{{{.Status}}}}\t{{{{.Label \"{PROJECT_ID_LABEL}\"}}}}\t{{{{.Label \"{DEPLOYMENT_ID_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"));
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker orphan inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let mut report = RuntimeReconciliationReport::default();
        let mut running_one_offs = 0;
        for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
            let fields = line.split('\t').collect::<Vec<_>>();
            let status = fields.get(1).copied().unwrap_or_default();
            if OneOffRole::is_labelled(fields.get(4).copied()) {
                if status.starts_with("Up") {
                    running_one_offs += 1;
                }
                continue;
            }
            report.inspected_containers += 1;
            let container_id = fields.first().copied().unwrap_or_default().to_owned();
            let project_id = fields.get(2).and_then(|value| Uuid::parse_str(value).ok());
            let deployment_id = fields.get(3).and_then(|value| Uuid::parse_str(value).ok());
            let stopped = status.starts_with("Exited") || status.starts_with("Dead");
            // Idle sleep stops the active deployment's containers and keeps
            // them for the wake; its sleeping route is the local record of that.
            if let (true, Some(project_id), Some(deployment_id)) =
                (stopped, project_id, deployment_id)
                && sleeping_routes.contains(&RouteIdentity {
                    project_id,
                    deployment_id: Some(deployment_id),
                })
            {
                report.sleeping_workloads.push(RuntimeWorkload {
                    container_id,
                    project_id,
                    deployment_id,
                    status: status.to_owned(),
                });
                continue;
            }
            let reason = if project_id.is_none() {
                Some("managed container has an unknown project identity")
            } else if deployment_id.is_none() {
                Some("managed container has incomplete deployment identity labels")
            } else if status.starts_with("Created") {
                Some("dangling candidate container was never started")
            } else if stopped {
                Some(STALE_STOPPED_REASON)
            } else {
                None
            };

            if let Some(reason) = reason {
                report.orphans.push(RuntimeOrphan {
                    container_id,
                    project_id,
                    deployment_id,
                    reason: reason.to_owned(),
                });
            } else if let (Some(project_id), Some(deployment_id)) = (project_id, deployment_id) {
                report.workloads.push(RuntimeWorkload {
                    container_id,
                    project_id,
                    deployment_id,
                    status: status.to_owned(),
                });
            }
        }

        Ok(ManagedContainers {
            report,
            running_one_offs,
        })
    }

    /// Railpack images serve on `$PORT` (its Caddyfile listens on
    /// `:{$PORT:80}`, its Node starters read `process.env.PORT`), and the
    /// generated route points at the port the control plane chose. Without
    /// exporting it the workload listened on its own default while the route
    /// pointed elsewhere, so the edge returned 502. `validate_payload` already
    /// rejects a conflicting `PORT` from the deployment environment, so this
    /// only fills in the value.
    fn effective_environment(
        environment: &BTreeMap<String, String>,
        port: u16,
    ) -> BTreeMap<String, String> {
        let mut effective = environment.clone();
        effective
            .entry("PORT".to_owned())
            .or_insert_with(|| port.to_string());
        effective
    }

    /// `docker run --detach` with what every container of a deployment gets:
    /// its name, the project's network and its Valkey store network, the node's
    /// runtime and name servers,
    /// the security options, and the resource limits. `restart` is the restart
    /// policy; `None` keeps Docker's default, which never restarts.
    async fn isolated_run(
        &self,
        name: &str,
        network: &str,
        store_network: Option<&str>,
        resources: AppliedRuntimeResources,
        restart: Option<&str>,
    ) -> Result<CommandSpec, RuntimeError> {
        let mut command = CommandSpec::new("docker")
            .arg("run")
            .arg("--detach")
            .arg("--name")
            .arg(name)
            .arg("--network")
            .arg(network);
        // The project network comes first and keeps the only route out: a
        // store network is internal.
        if let Some(store_network) = store_network {
            command = command.arg("--network").arg(store_network);
        }
        // Docker refuses an unknown runtime, and the failure stays a failure: a
        // workload must not end up on a weaker runtime than the node promises.
        if let Some(container_runtime) = &self.container_runtime {
            command = command.arg("--runtime").arg(container_runtime);
        }
        if let Some(container_dns) = &self.container_dns {
            command = command
                .arg("--mount")
                .arg(container_dns.ensure_mount().await?);
        }
        if let Some(restart) = restart {
            command = command.arg("--restart").arg(restart);
        }
        command = command
            .arg("--security-opt")
            .arg("no-new-privileges:true")
            .arg("--cap-drop")
            .arg("ALL");
        for capability in WORKLOAD_CAPABILITIES {
            command = command.arg("--cap-add").arg(capability);
        }
        Ok(command
            .arg("--memory")
            .arg(format!("{}m", resources.memory_mb))
            .arg("--memory-swap")
            .arg(format!("{}m", resources.memory_mb))
            .arg("--cpus")
            .arg(docker_cpu_value(resources.cpu_millis))
            .arg("--pids-limit")
            .arg(resources.pids_limit.to_string()))
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

    async fn ensure_capacity(
        &self,
        project_id: Uuid,
        replicas: NonZeroU32,
    ) -> Result<(), RuntimeError> {
        let running = self.running_containers().await?;
        let project_id = project_id.to_string();
        let other_projects = running
            .workloads
            .iter()
            .filter(|project| **project != project_id)
            .count()
            + running
                .one_offs
                .iter()
                .filter(|(project, role)| {
                    *project != project_id || role == OneOffRole::Job.as_str()
                })
                .count();
        let needed = other_projects.saturating_add(replicas.get() as usize);

        if needed > self.max_active_containers as usize {
            return Err(RuntimeError::Capacity(format!(
                "node already runs {other_projects} managed containers of other projects and this project needs {replicas}; configured maximum is {}",
                self.max_active_containers
            )));
        }

        Ok(())
    }

    async fn ensure_job_capacity(&self) -> Result<(), RuntimeError> {
        let running = self.running_containers().await?;
        let running = running.workloads.len() + running.one_offs.len();
        if running >= self.max_active_containers as usize {
            return Err(RuntimeError::Capacity(format!(
                "node already runs {running} managed containers and this job needs one more; configured maximum is {}",
                self.max_active_containers
            )));
        }
        Ok(())
    }

    async fn detect_orphans(
        &self,
        sleeping_routes: &HashSet<RouteIdentity>,
    ) -> Result<RuntimeReconciliationReport, RuntimeError> {
        Ok(self.managed_containers(sleeping_routes).await?.report)
    }

    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeError> {
        // A sleeping container is still a stopped one, so which of them sleep
        // does not change the count and the route files are not read here.
        let listing = self.managed_containers(&HashSet::new()).await?;
        let report = listing.report;
        let stale_stopped = report
            .orphans
            .iter()
            .filter(|orphan| orphan.reason == STALE_STOPPED_REASON)
            .count();
        Ok(RuntimeCapacity {
            // A running one-off container takes a slot like a workload, as in
            // the capacity guard.
            active_workloads: Some(report.workloads.len() + listing.running_one_offs),
            stopped_workloads: Some(report.sleeping_workloads.len() + stale_stopped),
            maximum_active_workloads: Some(self.max_active_containers as usize),
            active_builds: None,
            maximum_concurrent_builds: None,
            maximum_replicas_per_project: None,
        })
    }

    async fn routed_workloads(&self) -> Result<Vec<RoutedWorkload>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg(format!("{{{{.ID}}}}\t{{{{.Status}}}}\t{{{{.Label \"dev.sakala.project-id\"}}}}\t{{{{.Label \"dev.sakala.deployment-id\"}}}}\t{{{{.Label \"dev.sakala.domain\"}}}}\t{{{{.Label \"{WORKLOAD_KIND_LABEL_KEY}\"}}}}\t{{{{.Label \"{PROCESS_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"));
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker routed workload listing failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| {
                let fields = line.split('\t').collect::<Vec<_>>();
                // A worker has no route, so the router serves nothing for it,
                // and neither does a one-off container.
                if process_from_labels(fields.get(5).copied(), fields.get(6).copied()).is_some()
                    || OneOffRole::is_labelled(fields.get(7).copied())
                {
                    return None;
                }
                Some(RoutedWorkload {
                    project_id: Uuid::parse_str(fields.get(2)?).ok()?,
                    deployment_id: Uuid::parse_str(fields.get(3)?).ok()?,
                    domain: fields
                        .get(4)
                        .filter(|domain| !domain.is_empty())?
                        .to_string(),
                })
            })
            .collect())
    }

    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeError> {
        // `docker ps` deliberately excludes stopped workloads. Mereka tidak
        // seharusnya terus diperiksa oleh worker kesehatan runtime.
        Ok(self
            .list_replicas(false, "docker health inspection")
            .await?
            .into_iter()
            .map(|listed| {
                let (ready, reason) = health_state(&listed.status);
                RuntimeHealthSnapshot {
                    workload: RuntimeWorkload {
                        container_id: listed.container_id,
                        project_id: listed.project_id,
                        deployment_id: listed.deployment_id,
                        status: listed.status,
                    },
                    ready,
                    reason,
                    desired_replicas: listed.replica.count,
                    process: listed.process,
                }
            })
            .collect())
    }

    async fn workload_metrics(&self) -> Result<Vec<WorkloadMetrics>, RuntimeError> {
        let listing = CommandSpec::new("docker")
            .arg("ps")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg(format!(
                "{{{{.ID}}}}\t{{{{.Status}}}}\t{{{{.Label \"{PROJECT_ID_LABEL}\"}}}}\t{{{{.Label \"{DEPLOYMENT_ID_LABEL}\"}}}}\t{{{{.Label \"{REPLICA_LABEL}\"}}}}\t{{{{.Label \"{REPLICAS_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}\t{{{{.Label \"{WORKLOAD_KIND_LABEL_KEY}\"}}}}\t{{{{.Label \"{PROCESS_LABEL}\"}}}}"
            ))
            .timeout(METRICS_LISTING_TIMEOUT);
        let targets = self
            .read_docker(&listing, "docker metrics listing")
            .await?
            .lines()
            .filter_map(parse_metrics_target)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        // Every running container, not only the listed ones: given an ID that
        // is gone, `docker stats` fails as a whole and prints nothing.
        let stats = CommandSpec::new("docker")
            .arg("stats")
            .arg("--no-stream")
            .arg("--format")
            .arg(STATS_FORMAT)
            .timeout(METRICS_STATS_TIMEOUT);
        let readings = parse_stats(&self.read_docker(&stats, "docker stats").await?)?;
        // Whole seconds: the control plane aggregates per minute.
        let sampled_at = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap_or_else(|_| OffsetDateTime::now_utc());

        let mut inspect = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(METRICS_INSPECT_FORMAT)
            .timeout(METRICS_INSPECT_TIMEOUT);
        for target in &targets {
            inspect = inspect.arg(&target.container_id);
        }
        let output = self.runner.run(&inspect, &NullOutputSink).await?;
        // As in the crash inspection: a container removed since the listing
        // makes `docker inspect` exit non-zero while it still prints the others.
        if !output.success
            && output.stdout.trim().is_empty()
            && !output.stderr.to_ascii_lowercase().contains("no such")
        {
            return Err(RuntimeError::Container(format!(
                "docker metrics inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        join_sample(
            &targets,
            &readings,
            &parse_inspected_states(&output.stdout),
            sampled_at,
        )
    }

    async fn replica_listing(&self) -> Result<Vec<ListedReplica>, RuntimeError> {
        self.list_replicas(true, "docker replica listing").await
    }

    /// A workload counts as crashed while Docker is restarting it, and while it
    /// is crash-looping: restarted at least `CRASH_LOOP_MIN_RESTARTS` times and up
    /// for less than `crash_stable_window`. An app that crashed once and answers
    /// again is not reported, only logged for the operator, or the UI would say it
    /// is down while it serves. Docker resets `RestartCount` on `docker restart`
    /// and `docker start`, so a restart or wake through the agent never counts. An
    /// `exited` container is left out on purpose: `docker stop`, `docker kill`, and
    /// a sleep all end there with any exit code, so it cannot be told apart.
    async fn crashed_workloads(
        &self,
        snapshots: &[RuntimeHealthSnapshot],
    ) -> Result<Vec<CrashedWorkloadDetail>, RuntimeError> {
        let listed = snapshots
            .iter()
            .filter(|snapshot| {
                let status = snapshot.workload.status.to_ascii_lowercase();
                status.starts_with("up") || status.starts_with("restarting")
            })
            .collect::<Vec<_>>();
        if listed.is_empty() {
            return Ok(Vec::new());
        }

        let mut command = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(CRASH_STATE_FORMAT)
            .timeout(CRASH_INSPECT_TIMEOUT);
        for snapshot in &listed {
            command = command.arg(&snapshot.workload.container_id);
        }
        let output = self.runner.run(&command, &NullOutputSink).await?;
        // `docker inspect` exits non-zero when one of several containers is gone
        // but still prints the others, and a container removed since the listing
        // is simply not crashed anymore.
        if !output.success
            && output.stdout.trim().is_empty()
            && !output.stderr.to_ascii_lowercase().contains("no such")
        {
            return Err(RuntimeError::Container(format!(
                "docker crash inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let states = listed
            .iter()
            .filter_map(|snapshot| {
                let state = output
                    .stdout
                    .lines()
                    .filter_map(CrashState::parse)
                    .find(|state| state.id.starts_with(&snapshot.workload.container_id))?;
                Some((*snapshot, state))
            })
            .collect::<Vec<_>>();
        self.log_new_restarts(&states);

        let now = OffsetDateTime::now_utc();
        let crashed = states
            .into_iter()
            .filter(|(_, state)| state.is_crashed(now, self.crash_stable_window))
            .collect::<Vec<_>>();
        let last_exits = self.last_exits(&crashed).await;
        // A worker that ends with code 0 does so on purpose, like Laravel's
        // `queue:work --max-time`, and Docker starts it again: not a crash.
        let crashed = crashed
            .into_iter()
            .filter(|(snapshot, state)| {
                snapshot.process.is_none()
                    || state.exit_code(last_exits.get(&state.id)) != Some(EXPECTED_WORKER_EXIT_CODE)
            })
            .collect::<Vec<_>>();
        let crashed = whole_deployments(&listed, crashed);

        Ok(crashed
            .iter()
            .map(|(snapshot, state)| {
                let last_exit = last_exits.get(&state.id);
                CrashedWorkloadDetail {
                    project_id: snapshot.workload.project_id,
                    deployment_id: snapshot.workload.deployment_id,
                    exit_code: state.exit_code(last_exit),
                    oom_killed: state.oom_killed(last_exit),
                    finished_at: state.finished_at,
                    restart_count: state.restart_count,
                    running: state.running,
                    process: snapshot.process.clone(),
                }
            })
            .collect())
    }

    async fn workloads(
        &self,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> Result<Vec<ManagedWorkload>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--filter")
            .arg(format!("label=dev.sakala.project-id={project_id}"))
            .arg("--filter")
            .arg(format!("label=dev.sakala.deployment-id={deployment_id}"))
            .arg("--format")
            .arg(format!(
                "{{{{.ID}}}}\t{{{{.Status}}}}\t{{{{.Label \"dev.sakala.domain\"}}}}\t{{{{.Label \"dev.sakala.port\"}}}}\t{{{{.Label \"dev.sakala.command-id\"}}}}\t{{{{.Label \"dev.sakala.log-max-line-length\"}}}}\t{{{{.Label \"dev.sakala.log-max-batch-lines\"}}}}\t{{{{.Label \"dev.sakala.log-max-total-bytes\"}}}}\t{{{{.Names}}}}\t{{{{.Label \"{REPLICA_LABEL}\"}}}}\t{{{{.Label \"{REPLICAS_LABEL}\"}}}}\t{{{{.Label \"{WORKLOAD_KIND_LABEL_KEY}\"}}}}\t{{{{.Label \"{PROCESS_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"
            ));
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker workload lookup failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        let mut workloads = output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty() && !is_one_off_lookup(line))
            .map(|line| parse_workload(line, project_id, deployment_id))
            .collect::<Result<Vec<_>, _>>()?;
        // Web replicas first, then each worker process by name.
        workloads.sort_by(|left, right| {
            (&left.process, left.replica.index).cmp(&(&right.process, right.replica.index))
        });
        Ok(workloads)
    }

    async fn restart(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError> {
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("restart")
                .arg("--time")
                .arg(
                    self.stop_grace
                        .seconds_for(workload.process.as_deref())
                        .to_string(),
                )
                .arg(&workload.container_id),
            "docker-restart",
        )
        .await
    }

    async fn stop(&self, workloads: &[ManagedWorkload]) -> Result<(), RuntimeError> {
        let stop = |web: bool| {
            let running = workloads
                .iter()
                .filter(|workload| workload.is_running() && workload.is_web() == web)
                .map(|workload| workload.container_id.as_str())
                .collect::<Vec<_>>();
            let grace = if web {
                self.stop_grace.web
            } else {
                self.stop_grace.worker
            };
            async move {
                match stop_command(&running, grace) {
                    Some(command) => {
                        run_container_command(self.runner.as_ref(), command, "docker-stop").await
                    }
                    None => Ok(()),
                }
            }
        };
        // Side by side, so the web replicas do not wait out the workers' grace.
        let (web, workers) = tokio::join!(stop(true), stop(false));
        web.and(workers)
    }

    async fn start_existing(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError> {
        if workload.status.to_ascii_lowercase().starts_with("up") {
            return Ok(());
        }
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("start")
                .arg(&workload.container_id),
            "docker-start",
        )
        .await
    }

    async fn remove(&self, workload: &ManagedWorkload) -> Result<(), RuntimeError> {
        run_container_command(
            self.runner.as_ref(),
            CommandSpec::new("docker")
                .arg("rm")
                .arg(&workload.container_id),
            "docker-remove",
        )
        .await
    }

    async fn check_store_network(&self, attachment: &ValkeyAttachment) -> Result<(), RuntimeError> {
        crate::valkey::check_store_network(self.runner.as_ref(), &self.agent_id, attachment).await
    }

    async fn start(
        &self,
        request: &RunContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _start = self.networks.start_guard().await;
        let network = self.networks.prepare(request.project_id).await?;
        let environment = Self::effective_environment(&request.environment, request.port);
        let env_file = Self::write_env_file(&request.workspace, &environment).await?;
        let mut command = self
            .isolated_run(
                &request.name,
                &network,
                request.valkey_network.as_deref(),
                request.resources,
                Some(WORKLOAD_RESTART_POLICY),
            )
            .await?
            .arg("--label")
            .arg(MANAGED_LABEL)
            .arg("--label")
            .arg(format!("dev.sakala.project-id={}", request.project_id))
            .arg("--label")
            .arg(format!(
                "dev.sakala.deployment-id={}",
                request.deployment_id
            ))
            .arg("--label")
            .arg(format!("{DOMAIN_LABEL}={}", request.domain))
            .arg("--label")
            .arg(format!("{PORT_LABEL}={}", request.port))
            .arg("--label")
            .arg(format!("{COMMAND_ID_LABEL}={}", request.command_id));
        for label in process_labels(request.process.as_deref()) {
            command = command.arg("--label").arg(label);
        }
        command = command
            .arg("--label")
            .arg(format!("{AGENT_ID_LABEL}={}", self.agent_id));
        for label in request.replica.labels() {
            command = command.arg("--label").arg(label);
        }
        for (label, value) in [
            (LOG_MAX_LINE_LABEL, request.log_bounds.max_line_length),
            (LOG_MAX_BATCH_LABEL, request.log_bounds.max_batch_lines),
            (LOG_MAX_TOTAL_LABEL, request.log_bounds.max_total_bytes),
        ] {
            if let Some(value) = value {
                command = command.arg("--label").arg(format!("{label}={value}"));
            }
        }
        if let Some(env_file) = &env_file {
            command = command.arg("--env-file").arg(env_file.as_os_str());
        }
        // The command runs in the container's own `sh`, never in one on the host.
        // An image without `sh` fails to start, which readiness reports as a crash.
        if request.start_command.is_some() {
            command = command.arg("--entrypoint").arg("sh");
        }
        command = command.arg(&request.image);
        if let Some(start_command) = &request.start_command {
            command = command.arg("-c").arg(start_command);
        }

        let result = run_checked(self.runner.as_ref(), &command, "docker-run", reporter).await;
        if let Some(env_file) = env_file {
            let remove_result = fs::remove_file(env_file).await;
            if result.is_ok() {
                remove_result?;
            }
        }
        result.map(|_| ())
    }

    async fn start_one_off(
        &self,
        request: &OneOffContainerRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let _start = self.networks.start_guard().await;
        let network = self.networks.prepare(request.project_id).await?;
        let environment = Self::effective_environment(&request.environment, request.port);
        let env_file = Self::write_env_file(&request.workspace, &environment).await?;
        let mut command = self
            .isolated_run(
                &request.name,
                &network,
                request.valkey_network.as_deref(),
                request.resources,
                None,
            )
            .await?;
        for label in [
            format!("{ROLE_LABEL_KEY}={}", request.role.as_str()),
            format!("{PROJECT_ID_LABEL}={}", request.project_id),
            format!("{DEPLOYMENT_ID_LABEL}={}", request.deployment_id),
            format!("{COMMAND_ID_LABEL}={}", request.command_id),
            format!("{AGENT_ID_LABEL}={}", self.agent_id),
        ] {
            command = command.arg("--label").arg(label);
        }
        if request.role == OneOffRole::Job {
            // The image was checked before; one that went since must fail
            // here, not be looked up in a registry under the project's name.
            command = command.arg("--pull").arg(JOB_PULL_POLICY);
        }
        if let Some(env_file) = &env_file {
            command = command.arg("--env-file").arg(env_file.as_os_str());
        }
        // Shell form, exactly like a start command, and never a shell on the host.
        let command = command
            .arg("--entrypoint")
            .arg("sh")
            .arg(&request.image)
            .arg("-c")
            .arg(&request.command);

        let sink = ReporterOutputSink::new(reporter, "docker-run");
        let result = self.runner.run(&command, &sink).await;
        if let Some(env_file) = env_file {
            let remove_result = fs::remove_file(env_file).await;
            if matches!(result, Ok(ref output) if output.success) {
                remove_result?;
            }
        }
        let output = result?;
        if output.success {
            return Ok(());
        }
        if request.role == OneOffRole::Job
            && output
                .stderr
                .to_ascii_lowercase()
                .contains(NO_SUCH_IMAGE_MESSAGE)
        {
            return Err(RuntimeError::JobImageMissing {
                reason: "the image was removed before the job started",
            });
        }
        match (output.code, request.role) {
            (Some(code), OneOffRole::Release)
                if COMMAND_NOT_RUNNABLE_EXIT_CODES.contains(&code) =>
            {
                Err(RuntimeError::ReleaseCommandFailed {
                    exit_code: code,
                    out_of_memory_limit_mb: None,
                })
            }
            (Some(code), OneOffRole::Job) if COMMAND_NOT_RUNNABLE_EXIT_CODES.contains(&code) => {
                Err(RuntimeError::JobFailed {
                    exit_code: code,
                    out_of_memory_limit_mb: None,
                })
            }
            (code, role) => Err(RuntimeError::failed_process(
                &format!("docker-run-{}", role.as_str()),
                code,
                &output.stderr,
            )),
        }
    }

    async fn wait_one_off(
        &self,
        name: &str,
        role: OneOffRole,
        reporter: &dyn RuntimeReporter,
    ) -> Result<OneOffExit, RuntimeError> {
        // Ends when the container exits. A follower that fails early loses
        // output, not the result, which `docker wait` reads on its own.
        let follow = CommandSpec::new("docker")
            .arg("logs")
            .arg("--follow")
            .arg(name)
            .without_timeout();
        let sink =
            ReporterOutputSink::new(reporter, role.as_str()).with_process(role.log_process());
        match self.runner.run(&follow, &sink).await {
            Ok(output) if output.success => {}
            Ok(output) => warn!(
                container = name,
                role = role.as_str(),
                status = %crate::error::describe_exit(output.code),
                "one-off container output could not be followed"
            ),
            Err(error) => warn!(
                container = name,
                role = role.as_str(),
                %error,
                "one-off container output could not be followed"
            ),
        }
        let wait = CommandSpec::new("docker")
            .arg("wait")
            .arg(name)
            .without_timeout();
        let exit_code = self
            .read_docker(&wait, "docker one-off wait")
            .await?
            .trim()
            .parse::<i32>()
            .map_err(|_| RuntimeError::Container("docker wait returned no exit code".to_owned()))?;
        // Enrichment only: without it a failure still reports its exit code.
        let inspect = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg("{{.State.OOMKilled}}")
            .arg(name)
            .timeout(ONE_OFF_INSPECT_TIMEOUT);
        let oom_killed = match self.runner.run(&inspect, &NullOutputSink).await {
            Ok(output) if output.success => output.stdout.trim() == "true",
            _ => false,
        };
        Ok(OneOffExit {
            exit_code,
            oom_killed,
        })
    }

    async fn remove_one_off(&self, name: &str) -> Result<(), RuntimeError> {
        let remove = CommandSpec::new("docker")
            .arg("rm")
            .arg("--force")
            .arg("--volumes")
            .arg(name);
        let output = self.runner.run(&remove, &NullOutputSink).await?;
        let stderr = output.stderr.to_ascii_lowercase();
        if output.success || stderr.contains(NO_SUCH_MESSAGE) {
            return Ok(());
        }
        // Docker still removes it for an earlier `docker rm` whose client
        // went away, such as one cut off while it waited for the container
        // to exit. Asking again fails at once, so its end is awaited instead.
        if stderr.contains(REMOVAL_IN_PROGRESS_MESSAGE) {
            return self.wait_until_removed(name).await;
        }
        warn!(
            container = name,
            status = %crate::error::describe_exit(output.code),
            detail = output.stderr.lines().next().unwrap_or_default(),
            "Docker did not remove a one-off container"
        );
        Err(RuntimeError::failed_process(
            "docker-remove-one-off",
            output.code,
            &output.stderr,
        ))
    }

    async fn remove_stale_one_offs(
        &self,
        is_active: &IsActiveOneOff<'_>,
    ) -> Result<usize, RuntimeError> {
        let mut removed = 0;
        for role in OneOffRole::ALL {
            let names = self
                .one_off_names(&[
                    format!("label={ROLE_LABEL_KEY}={}", role.as_str()),
                    format!("label={AGENT_ID_LABEL}={}", self.agent_id),
                ])
                .await?;
            for name in names.iter().filter(|name| !is_active(name)) {
                self.remove_one_off(name).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    async fn remove_project_one_offs(&self, project_id: Uuid) -> Result<usize, RuntimeError> {
        // Any Agent's: the project is gone, whoever started its containers.
        let names = self
            .one_off_names(&[
                format!("label={ROLE_LABEL_KEY}"),
                format!("label={PROJECT_ID_LABEL}={project_id}"),
            ])
            .await?;
        for name in &names {
            self.remove_one_off(name).await?;
        }
        Ok(names.len())
    }

    async fn attach_router(&self, project_id: Uuid) -> Result<(), RuntimeError> {
        self.networks.attach_router(project_id).await
    }

    async fn release_network(&self, project_id: Uuid) -> Result<(), RuntimeError> {
        self.networks.release(project_id).await.map(|_| ())
    }

    async fn reconcile_networks(
        &self,
        remove_unused: bool,
    ) -> Result<RuntimeNetworkReconciliation, RuntimeError> {
        self.networks.reconcile(remove_unused).await
    }

    async fn report_startup_logs(
        &self,
        container: &str,
        reporter: &dyn RuntimeReporter,
    ) -> Result<(), RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("logs")
            .arg("--tail")
            .arg(STARTUP_LOG_TAIL_LINES.to_string())
            .arg(container);
        run_checked(self.runner.as_ref(), &command, "runtime", reporter)
            .await
            .map(|_| ())
    }

    async fn report_failure_logs(
        &self,
        container: &str,
        log_bounds: LogBounds,
        reporter: &dyn RuntimeReporter,
    ) -> Result<FailureOutput, RuntimeError> {
        let tail = log_bounds
            .max_batch_lines
            .map_or(STARTUP_LOG_TAIL_LINES, |lines| {
                lines.min(STARTUP_LOG_TAIL_LINES)
            });
        if tail == 0 {
            return Ok(FailureOutput::default());
        }
        let command = CommandSpec::new("docker")
            .arg("logs")
            .arg("--tail")
            .arg(tail.to_string())
            .arg(container)
            .timeout(FAILURE_LOG_TIMEOUT);
        let output = run_checked(self.runner.as_ref(), &command, "runtime", reporter).await?;
        Ok(FailureOutput {
            had_output: !output.stdout.trim().is_empty() || !output.stderr.trim().is_empty(),
            exec_denied: crate::error::reports_exec_denied(&output.stdout)
                || crate::error::reports_exec_denied(&output.stderr),
        })
    }

    async fn environment_values(&self, container: &str) -> Result<Vec<String>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg("{{json .Config.Env}}")
            .arg(container)
            .timeout(ENVIRONMENT_INSPECT_TIMEOUT);
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker environment inspection failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        // The parse error is not reported: it could quote part of the output.
        let entries: Vec<String> = serde_json::from_str(output.stdout.trim()).map_err(|_| {
            RuntimeError::Container("docker returned an unreadable environment".to_owned())
        })?;
        Ok(entries
            .into_iter()
            .filter_map(|entry| {
                let (key, value) = entry.split_once('=')?;
                (!NON_SECRET_ENVIRONMENT_KEYS.contains(&key)).then(|| value.to_owned())
            })
            .collect())
    }

    fn start_log_follower(&self, container: &str, reporter: Arc<dyn RuntimeReporter>) -> bool {
        let mut followers = self.log_followers.lock().expect("log follower lock");
        followers.retain(|_, follower| !follower.is_finished());
        if followers.contains_key(container) {
            return false;
        }

        let runner = Arc::clone(&self.runner);
        let container = container.to_owned();
        let follower_container = container.clone();
        let handle = tokio::spawn(async move {
            let command = CommandSpec::new("docker")
                .arg("logs")
                .arg("--follow")
                .arg("--tail")
                .arg("0")
                .arg(&follower_container)
                .without_timeout();
            let sink = ReporterOutputSink::new(reporter.as_ref(), "runtime");

            match runner.run(&command, &sink).await {
                Ok(output) => debug!(
                    container = %follower_container,
                    status = ?output.code,
                    "container log follower stopped"
                ),
                Err(error) => warn!(
                    container = %follower_container,
                    %error,
                    "container log follower failed"
                ),
            }
        });

        followers.insert(container.clone(), handle);
        debug!(%container, "container log follower started");
        true
    }

    async fn cleanup_previous(
        &self,
        project_id: Uuid,
        current: &[String],
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
            return Err(RuntimeError::Container(format!(
                "docker-list-previous failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }

        let mut previous = Vec::new();
        for container_id in output.stdout.lines().filter(|id| !id.trim().is_empty()) {
            let inspect = CommandSpec::new("docker")
                .arg("inspect")
                .arg("--format")
                .arg(PREVIOUS_CONTAINER_FORMAT)
                .arg(container_id);
            let inspected_name = self.runner.run(&inspect, &NullOutputSink).await?;
            if !inspected_name.success {
                return Err(RuntimeError::Container(format!(
                    "docker-inspect-previous failed with {}",
                    crate::error::describe_exit(inspected_name.code)
                )));
            }
            let mut fields = inspected_name.stdout.trim().split('\t');
            let running = matches!(fields.next(), Some("true"));
            let name = fields.next().unwrap_or_default().trim_start_matches('/');
            if current.iter().any(|current| current == name) {
                continue;
            }
            // The kind label is not read: only an Agent with workers writes
            // a process label, and only on a worker.
            let worker = fields
                .next()
                .is_some_and(|process| !process.trim().is_empty());
            // A job of the project keeps running through its deploy, and a
            // release container is removed by the deployment that ran it.
            if OneOffRole::is_labelled(fields.next()) {
                continue;
            }
            previous.push((container_id, running, worker));
        }

        // One call per process kind stops the old containers side by side, so
        // they share one grace period instead of waiting it out one after
        // another, and the web replicas do not wait out the workers' grace.
        let stop = |worker: bool| {
            let running = previous
                .iter()
                .filter(|(_, running, is_worker)| *running && *is_worker == worker)
                .map(|(container_id, _, _)| *container_id)
                .collect::<Vec<_>>();
            let grace = if worker {
                self.stop_grace.worker
            } else {
                self.stop_grace.web
            };
            async move {
                match stop_command(&running, grace) {
                    Some(command) => run_checked(
                        self.runner.as_ref(),
                        &command,
                        "docker-stop-previous",
                        reporter,
                    )
                    .await
                    .map(|_| ()),
                    None => Ok(()),
                }
            }
        };
        let (web, workers) = tokio::join!(stop(false), stop(true));
        web.and(workers)?;
        for (container_id, _, _) in previous {
            let remove = CommandSpec::new("docker").arg("rm").arg(container_id);
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

    async fn cleanup_candidate(
        &self,
        containers: &[String],
        image: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let mut failures = Vec::new();
        let mut remove_containers = CommandSpec::new("docker").arg("rm").arg("--force");
        for container in containers {
            remove_containers = remove_containers.arg(container);
        }
        let remove_image = image.map(|image| {
            (
                "candidate image",
                CommandSpec::new("docker")
                    .arg("image")
                    .arg("rm")
                    .arg("--force")
                    .arg(image),
            )
        });
        for (artifact, command) in
            std::iter::once(("candidate container", remove_containers)).chain(remove_image)
        {
            match self.runner.run(&command, &NullOutputSink).await {
                Ok(output) if output.success => {}
                Ok(output) => failures.push(format!(
                    "{artifact} failed with {}",
                    crate::error::describe_exit(output.code)
                )),
                Err(error) => failures.push(format!("{artifact}: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Container(format!(
                "candidate cleanup incomplete: {}",
                failures.join("; ")
            )))
        }
    }

    async fn image_labels(
        &self,
        image: &str,
    ) -> Result<Option<BTreeMap<String, String>>, RuntimeError> {
        let command = CommandSpec::new("docker")
            .arg("image")
            .arg("inspect")
            .arg("--format")
            .arg(IMAGE_LABELS_FORMAT)
            .arg(image)
            .timeout(IMAGE_LABELS_TIMEOUT);
        let output = self.runner.run(&command, &NullOutputSink).await?;
        if output.success {
            return parse_image_labels(&output.stdout).map(Some);
        }
        if output.stderr.to_ascii_lowercase().contains(NO_SUCH_IMAGE) {
            return Ok(None);
        }
        Err(RuntimeError::Container(format!(
            "image inspection failed with {}",
            crate::error::describe_exit(output.code)
        )))
    }

    async fn detect_stale_images(
        &self,
        retention: ImageRetention,
    ) -> Result<Vec<RuntimeStaleImage>, RuntimeError> {
        Ok(self
            .stale_images(None, retention)
            .await?
            .into_iter()
            .map(|image| RuntimeStaleImage {
                image_id: image.id,
                project_id: Some(image.project_id),
                deployment_id: image.deployment_id,
            })
            .collect())
    }

    async fn cleanup_stale_images(
        &self,
        retention: ImageRetention,
        claim: &ClaimProject<'_>,
    ) -> Result<ImageCleanup, RuntimeError> {
        // One node-wide pass finds the projects with something to remove. Each
        // of them is chosen again under its claim: a deployment that ran in
        // between may have moved the previous tag or started on an image.
        let projects = self
            .stale_images(None, retention)
            .await?
            .into_iter()
            .map(|image| image.project_id)
            .collect::<BTreeSet<_>>();
        let mut cleanup = ImageCleanup::default();
        for project_id in projects {
            let Some(_claim) = claim(project_id) else {
                debug!(%project_id, "image GC left a project a command holds");
                continue;
            };
            let images = self.stale_images(Some(project_id), retention).await?;
            cleanup += self.remove_images(images).await;
        }
        Ok(cleanup)
    }

    async fn remove_project_images(&self, project_id: Uuid) -> Result<ImageCleanup, RuntimeError> {
        let Some((listed, inspected, usage)) = self.image_inventory(Some(project_id)).await? else {
            return Ok(ImageCleanup::default());
        };
        let images = select_project_images(&listed, &inspected, &usage, project_id);
        Ok(self.remove_images(images).await)
    }

    async fn mark_previous_image(
        &self,
        project_id: Uuid,
        current: &[String],
    ) -> Result<Option<String>, RuntimeError> {
        let list = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--quiet")
            .arg("--no-trunc")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--filter")
            .arg(format!("label={PROJECT_ID_LABEL}={project_id}"));
        let ids = self.read_docker(&list, "project container listing").await?;
        if ids.trim().is_empty() {
            return Ok(None);
        }
        let mut inspect = CommandSpec::new("docker")
            .arg("container")
            .arg("inspect")
            .arg("--format")
            .arg(PROJECT_CONTAINER_FORMAT);
        for id in ids.split_whitespace() {
            inspect = inspect.arg(id);
        }
        // A one-off container runs the image it was given, which says
        // nothing about which image the deployment replaced.
        let containers = parse_project_containers(
            &self
                .read_docker(&inspect, "project container inspection")
                .await?,
        )?
        .into_iter()
        .filter(|container| !container.one_off)
        .collect::<Vec<_>>();
        let Some(image) = replaced_image(&containers, current) else {
            return Ok(None);
        };
        let listed = self.image_listing(Some(project_id)).await?;
        let Some(references) = owned_references(&listed, &image, project_id) else {
            // Labelled for another project, or named outside this project's
            // repository: never this project's to mark.
            warn!(
                %project_id,
                image = %image,
                "the replaced image is not provably the project's; the previous image stays"
            );
            return Ok(None);
        };
        let previous = previous_image_reference(project_id);
        if !references.contains(&previous) {
            let tag = CommandSpec::new("docker")
                .arg("image")
                .arg("tag")
                .arg(&image)
                .arg(&previous);
            self.read_docker(&tag, "previous image tag").await?;
        }
        Ok(Some(image))
    }

    async fn prune_project_images(
        &self,
        project_id: Uuid,
        retention: ImageRetention,
    ) -> Result<ImageCleanup, RuntimeError> {
        let images = self.stale_images(Some(project_id), retention).await?;
        Ok(self.remove_images(images).await)
    }

    async fn shutdown(&self) {
        let followers = {
            let mut followers = self.log_followers.lock().expect("log follower lock");
            std::mem::take(&mut *followers)
        };
        for follower in followers.values() {
            follower.abort();
        }
        for (_, follower) in followers {
            let _ = follower.await;
        }
    }
}

/// One line of [`DockerContainerEngine::list_replicas`].
fn parse_listed_replica(line: &str) -> Option<ListedReplica> {
    if line.trim().is_empty() {
        return None;
    }
    let fields = line.split('\t').collect::<Vec<_>>();
    if OneOffRole::is_labelled(fields.get(8).copied()) {
        return None;
    }
    Some(ListedReplica {
        container_id: fields.first().copied().unwrap_or_default().to_owned(),
        status: fields.get(1).copied().unwrap_or_default().to_owned(),
        project_id: Uuid::parse_str(fields.get(2)?).ok()?,
        deployment_id: Uuid::parse_str(fields.get(3)?).ok()?,
        replica: ReplicaSlot::from_labels(fields.get(4).copied(), fields.get(5).copied()),
        process: process_from_labels(fields.get(6).copied(), fields.get(7).copied()),
    })
}

/// Whether a line of the workload lookup is a one-off container, whose role
/// comes after the process.
fn is_one_off_lookup(line: &str) -> bool {
    OneOffRole::is_labelled(line.split('\t').nth(13))
}

/// One line of the workload lookup. The name and the replica labels come last,
/// so a line without them reads as the single replica it then is.
fn parse_workload(
    line: &str,
    project_id: Uuid,
    deployment_id: Uuid,
) -> Result<ManagedWorkload, RuntimeError> {
    let fields = line.split('\t').collect::<Vec<_>>();
    let domain = fields
        .get(2)
        .copied()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RuntimeError::Container("managed workload is missing its domain label".to_owned())
        })?;
    let port = fields
        .get(3)
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port > 0)
        .ok_or_else(|| {
            RuntimeError::Container("managed workload has an invalid port label".to_owned())
        })?;
    let replica = ReplicaSlot::from_labels(fields.get(9).copied(), fields.get(10).copied());
    let name = fields
        .get(8)
        .map(|names| names.split(',').next().unwrap_or_default().trim())
        .filter(|name| !name.is_empty())
        .map_or_else(
            || replica.container_name(project_id, deployment_id),
            str::to_owned,
        );
    Ok(ManagedWorkload {
        container_id: fields.first().copied().unwrap_or_default().to_owned(),
        name,
        replica,
        status: fields.get(1).copied().unwrap_or_default().to_owned(),
        project_id,
        deployment_id,
        domain: domain.to_owned(),
        port,
        command_id: fields.get(4).and_then(|value| Uuid::parse_str(value).ok()),
        log_bounds: sakala_agent_protocol::LogBounds {
            max_line_length: parse_optional_label(fields.get(5).copied()),
            max_batch_lines: parse_optional_label(fields.get(6).copied()),
            max_total_bytes: parse_optional_label(fields.get(7).copied()),
        },
        process: process_from_labels(fields.get(11).copied(), fields.get(12).copied()),
    })
}

/// Keeps the crashed containers of processes whose every listed container
/// crashed, one per process of a deployment: the one that ended last, as the
/// freshest account of the crash. A web process with a replica still up is
/// serving, and its crashed replicas show up as `ready` below `desired`
/// instead. A worker process is judged on its own containers, so a crashing
/// worker never reads as the web process crashing, and the other way round.
fn whole_deployments<'a>(
    listed: &[&RuntimeHealthSnapshot],
    crashed: Vec<(&'a RuntimeHealthSnapshot, CrashState)>,
) -> Vec<(&'a RuntimeHealthSnapshot, CrashState)> {
    let deployment = |snapshot: &RuntimeHealthSnapshot| {
        (
            snapshot.workload.project_id,
            snapshot.workload.deployment_id,
            snapshot.process.clone(),
        )
    };
    let mut listed_per_deployment = HashMap::<(Uuid, Uuid, Option<String>), usize>::new();
    for snapshot in listed {
        *listed_per_deployment
            .entry(deployment(snapshot))
            .or_default() += 1;
    }
    let mut per_deployment = BTreeMap::<
        (Uuid, Uuid, Option<String>),
        Vec<(&'a RuntimeHealthSnapshot, CrashState)>,
    >::new();
    for entry in crashed {
        per_deployment
            .entry(deployment(entry.0))
            .or_default()
            .push(entry);
    }
    per_deployment
        .into_iter()
        .filter(|(key, entries)| listed_per_deployment.get(key) == Some(&entries.len()))
        .filter_map(|(_, entries)| {
            entries
                .into_iter()
                .max_by_key(|(_, state)| state.finished_at)
        })
        .collect()
}

/// What [`DockerContainerEngine::managed_containers`] found.
struct ManagedContainers {
    report: RuntimeReconciliationReport,
    running_one_offs: usize,
}

/// What [`DockerContainerEngine::running_containers`] found.
struct RunningContainers {
    /// The project label of each running managed workload, web or worker.
    workloads: Vec<String>,
    /// The project and role labels of each running one-off container.
    one_offs: Vec<(String, String)>,
}

fn non_empty_lines(stdout: &str) -> impl Iterator<Item = &str> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

fn parse_optional_label(value: Option<&str>) -> Option<u64> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse().ok())
}

/// `docker stop` of `containers` in one call, which stops them side by side
/// so they share one grace period. `None` when there is nothing to stop.
fn stop_command(containers: &[&str], grace: Duration) -> Option<CommandSpec> {
    if containers.is_empty() {
        return None;
    }
    let mut command = CommandSpec::new("docker")
        .arg("stop")
        .arg("--time")
        .arg(grace.as_secs().to_string());
    for container in containers {
        command = command.arg(container);
    }
    Some(command)
}

async fn run_container_command(
    runner: &dyn ProcessRunner,
    command: CommandSpec,
    phase: &str,
) -> Result<(), RuntimeError> {
    let output = runner.run(&command, &NullOutputSink).await?;
    if output.success {
        Ok(())
    } else {
        Err(RuntimeError::failed_process(
            phase,
            output.code,
            &output.stderr,
        ))
    }
}

/// One line of the batched `docker inspect` behind the crash report.
#[derive(Debug)]
struct CrashState {
    id: String,
    restarting: bool,
    running: bool,
    restart_count: u32,
    exit_code: Option<i32>,
    oom_flag: bool,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl CrashState {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        let id = fields.next()?.to_owned();
        let status = fields.next()?;
        Some(Self {
            id,
            restarting: status == "restarting",
            running: status == "running",
            restart_count: fields.next()?.parse().ok()?,
            exit_code: fields.next()?.parse().ok(),
            oom_flag: fields.next()? == "true",
            started_at: parse_docker_time(fields.next()?),
            finished_at: parse_docker_time(fields.next()?),
        })
    }

    fn is_crashed(&self, now: OffsetDateTime, stable_window: Duration) -> bool {
        self.restarting
            || (self.running
                && self.restart_count >= CRASH_LOOP_MIN_RESTARTS
                && self
                    .started_at
                    .is_some_and(|started_at| now - started_at < stable_window))
    }

    /// Docker clears the exit code and the OOM flag once a restart succeeds, so
    /// a running container's own values say nothing about its last exit.
    fn needs_exit_history(&self) -> bool {
        !self.restarting || (self.exit_code == Some(SIGKILL_EXIT_CODE) && !self.oom_flag)
    }

    fn exit_code(&self, last_exit: Option<&LastExit>) -> Option<i32> {
        if self.restarting {
            self.exit_code
        } else {
            last_exit.and_then(|exit| exit.exit_code)
        }
    }

    fn oom_killed(&self, last_exit: Option<&LastExit>) -> bool {
        (self.restarting && self.oom_flag) || last_exit.is_some_and(|exit| exit.oom_killed)
    }
}

impl DockerContainerEngine {
    /// Waits until Docker no longer knows the container, polling at
    /// [`ONE_OFF_REMOVAL_POLL`] for at most [`ONE_OFF_REMOVAL_IN_PROGRESS_WAIT`].
    async fn wait_until_removed(&self, name: &str) -> Result<(), RuntimeError> {
        let inspect = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg("{{.Id}}")
            .arg(name)
            .timeout(ONE_OFF_INSPECT_TIMEOUT);
        let deadline = tokio::time::Instant::now() + ONE_OFF_REMOVAL_IN_PROGRESS_WAIT;
        loop {
            let output = self.runner.run(&inspect, &NullOutputSink).await?;
            if !output.success && output.stderr.to_ascii_lowercase().contains(NO_SUCH_MESSAGE) {
                return Ok(());
            }
            if tokio::time::Instant::now() + ONE_OFF_REMOVAL_POLL > deadline {
                return Err(RuntimeError::Container(format!(
                    "the removal of one-off container {name} already in progress did not finish within {}s",
                    ONE_OFF_REMOVAL_IN_PROGRESS_WAIT.as_secs()
                )));
            }
            tokio::time::sleep(ONE_OFF_REMOVAL_POLL).await;
        }
    }

    /// Managed containers with their identity and replica labels, in one
    /// `docker ps`. `all` includes the stopped ones. A container without a
    /// readable project or deployment ID is left out, and so is a one-off
    /// container.
    async fn list_replicas(
        &self,
        all: bool,
        what: &str,
    ) -> Result<Vec<ListedReplica>, RuntimeError> {
        let mut command = CommandSpec::new("docker").arg("ps");
        if all {
            command = command.arg("--all");
        }
        let command = command
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--format")
            .arg(format!(
                "{{{{.ID}}}}\t{{{{.Status}}}}\t{{{{.Label \"dev.sakala.project-id\"}}}}\t{{{{.Label \"dev.sakala.deployment-id\"}}}}\t{{{{.Label \"{REPLICA_LABEL}\"}}}}\t{{{{.Label \"{REPLICAS_LABEL}\"}}}}\t{{{{.Label \"{WORKLOAD_KIND_LABEL_KEY}\"}}}}\t{{{{.Label \"{PROCESS_LABEL}\"}}}}\t{{{{.Label \"{ROLE_LABEL_KEY}\"}}}}"
            ));
        let stdout = self.read_docker(&command, what).await?;
        Ok(stdout.lines().filter_map(parse_listed_replica).collect())
    }

    /// A restart that is not reported as a crash must still leave a trace.
    fn log_new_restarts(&self, states: &[(&RuntimeHealthSnapshot, CrashState)]) {
        let counts = states
            .iter()
            .map(|(_, state)| (state.id.clone(), state.restart_count))
            .collect::<Vec<_>>();
        let new = {
            let mut logged = self.logged_restarts.lock().expect("restart log lock");
            new_restarts(&mut logged, counts)
        };
        for (snapshot, state) in states.iter().filter(|(_, state)| new.contains(&state.id)) {
            warn!(
                container_id = %snapshot.workload.container_id,
                project_id = %snapshot.workload.project_id,
                deployment_id = %snapshot.workload.deployment_id,
                restart_count = state.restart_count,
                running = state.running,
                process = snapshot.process.as_deref().unwrap_or(sakala_agent_protocol::WEB_PROCESS),
                "Docker restarted a managed workload after it exited on its own"
            );
        }
    }

    /// Reads the last exit of the crashed workloads Docker's state no longer
    /// describes. This is enrichment: without it a workload is still reported,
    /// so a failure here must not fail the report.
    async fn last_exits(
        &self,
        crashed: &[(&RuntimeHealthSnapshot, CrashState)],
    ) -> HashMap<String, LastExit> {
        let unknown = crashed
            .iter()
            .map(|(_, state)| state)
            .filter(|state| state.needs_exit_history())
            .collect::<Vec<_>>();
        let Some(since) = unknown
            .iter()
            .filter_map(|state| state.finished_at)
            .map(OffsetDateTime::unix_timestamp)
            .min()
        else {
            return HashMap::new();
        };
        let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
        let mut command = CommandSpec::new("docker")
            .arg("events")
            // `finished_at` is rounded down, so this still covers the last exit.
            .arg("--since")
            .arg(since.to_string())
            // A fractional value includes this very second and returns at once.
            .arg("--until")
            .arg(format!(
                "{}.{:09}",
                now / 1_000_000_000,
                now % 1_000_000_000
            ))
            .arg("--filter")
            .arg("event=die")
            .arg("--filter")
            .arg("event=oom")
            .arg("--format")
            .arg(EXIT_EVENTS_FORMAT)
            .timeout(EXIT_EVENTS_TIMEOUT);
        for state in &unknown {
            command = command
                .arg("--filter")
                .arg(format!("container={}", state.id));
        }
        match self.runner.run(&command, &NullOutputSink).await {
            Ok(output) if output.success => parse_last_exits(&output.stdout),
            Ok(output) => {
                debug!(status = ?output.code, "docker events returned no exit history");
                HashMap::new()
            }
            Err(error) => {
                debug!(%error, "docker events did not finish");
                HashMap::new()
            }
        }
    }
}

/// Returns the containers whose restart count rose since the last call, and
/// forgets the containers that are gone.
fn new_restarts(logged: &mut HashMap<String, u32>, counts: Vec<(String, u32)>) -> Vec<String> {
    let mut new = Vec::new();
    let mut current = HashMap::with_capacity(counts.len());
    for (id, count) in counts {
        if count > logged.get(&id).copied().unwrap_or(0) {
            new.push(id.clone());
        }
        current.insert(id, count);
    }
    *logged = current;
    new
}

#[derive(Debug, Default, Eq, PartialEq)]
struct LastExit {
    exit_code: Option<i32>,
    oom_killed: bool,
}

/// Docker emits `oom` before the `die` it caused, so an `oom` only describes the
/// next `die` of that container. One after the last `die` belongs to the current
/// run, in which a worker was killed but the container survived.
fn parse_last_exits(stdout: &str) -> HashMap<String, LastExit> {
    let mut last_exits = HashMap::<String, LastExit>::new();
    let mut pending_oom = HashSet::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(action)) = (fields.next(), fields.next()) else {
            continue;
        };
        match action {
            "oom" => {
                pending_oom.insert(id);
            }
            "die" => {
                last_exits.insert(
                    id.to_owned(),
                    LastExit {
                        exit_code: fields.next().and_then(|value| value.parse().ok()),
                        oom_killed: pending_oom.remove(id),
                    },
                );
            }
            _ => {}
        }
    }
    last_exits
}

/// Docker prints nanoseconds and uses year 1 for "never". The report keeps whole
/// seconds, and the zero time becomes `None`.
fn parse_docker_time(value: &str) -> Option<OffsetDateTime> {
    let time = OffsetDateTime::parse(value.trim(), &Rfc3339).ok()?;
    (time.year() > 1).then(|| time.replace_nanosecond(0).unwrap_or(time))
}

fn health_state(status: &str) -> (bool, Option<String>) {
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("unhealthy") {
        return (false, Some("Docker health status is unhealthy".to_owned()));
    }
    if normalized.contains("health: starting") {
        return (
            false,
            Some("Docker health check is still starting".to_owned()),
        );
    }
    if normalized.starts_with("up") {
        return (true, None);
    }

    (false, Some(format!("container is not running: {status}")))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::{DockerContainerEngine, WORKLOAD_CAPABILITIES, new_restarts};

    #[test]
    fn each_new_restart_is_logged_once() {
        let mut logged = HashMap::new();
        let at = |id: &str, count| vec![(id.to_owned(), count)];

        assert!(new_restarts(&mut logged, at("a", 0)).is_empty());
        assert_eq!(new_restarts(&mut logged, at("a", 1)), ["a"]);
        assert!(new_restarts(&mut logged, at("a", 1)).is_empty());
        assert_eq!(new_restarts(&mut logged, at("a", 2)), ["a"]);
        assert!(
            new_restarts(&mut logged, at("a", 0)).is_empty(),
            "a restart on purpose"
        );
        assert_eq!(new_restarts(&mut logged, at("a", 1)), ["a"]);
        assert!(new_restarts(&mut logged, Vec::new()).is_empty());
        assert!(logged.is_empty(), "a removed container is forgotten");
    }

    #[test]
    fn effective_environment_exports_the_routed_port() {
        let mut environment = BTreeMap::new();
        environment.insert("APP_ENV".to_owned(), "production".to_owned());

        let effective = DockerContainerEngine::effective_environment(&environment, 3000);

        assert_eq!(effective.get("PORT"), Some(&"3000".to_owned()));
        assert_eq!(effective.get("APP_ENV"), Some(&"production".to_owned()));
    }

    #[test]
    fn effective_environment_keeps_a_matching_deployment_port() {
        let mut environment = BTreeMap::new();
        environment.insert("PORT".to_owned(), "3000".to_owned());

        let effective = DockerContainerEngine::effective_environment(&environment, 3000);

        assert_eq!(effective.get("PORT"), Some(&"3000".to_owned()));
    }

    /// The allowlist stays a strict subset of Docker's default set. The six
    /// dropped ones give a tenant nothing a web app needs: NET_RAW forges
    /// packets on the shared bridge, MKNOD creates device nodes, SYS_CHROOT
    /// and AUDIT_WRITE serve login tooling, and SETFCAP and SETPCAP change
    /// capabilities themselves.
    #[test]
    fn the_workload_allowlist_drops_the_risky_part_of_docker_defaults() {
        const DOCKER_DEFAULTS: [&str; 14] = [
            "AUDIT_WRITE",
            "CHOWN",
            "DAC_OVERRIDE",
            "FOWNER",
            "FSETID",
            "KILL",
            "MKNOD",
            "NET_BIND_SERVICE",
            "NET_RAW",
            "SETFCAP",
            "SETGID",
            "SETPCAP",
            "SETUID",
            "SYS_CHROOT",
        ];
        for capability in WORKLOAD_CAPABILITIES {
            assert!(DOCKER_DEFAULTS.contains(&capability), "{capability}");
        }
        for dropped in [
            "NET_RAW",
            "MKNOD",
            "SYS_CHROOT",
            "AUDIT_WRITE",
            "SETFCAP",
            "SETPCAP",
            "SYS_ADMIN",
            "NET_ADMIN",
        ] {
            assert!(!WORKLOAD_CAPABILITIES.contains(&dropped), "{dropped}");
        }
    }
}
