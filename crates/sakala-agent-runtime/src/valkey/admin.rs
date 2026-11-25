//! [`ValkeyAdmin`] over the Docker CLI, and over `valkey-cli` run with
//! `docker exec` inside each store's own container.
//!
//! Process arguments are only names derived from a store's UUID, addresses,
//! and integers. The admin password reaches `valkey-cli` through `-e NAME`
//! with the value in the child's environment, never in an argument; tenant
//! passwords never leave the agent except as the SHA-256 hash in `users.acl`.
//!
//! Every operation runs one at a time and converges: it writes the store's
//! record and files first, then makes Docker and Valkey match them, so sending
//! a command again after a failure finishes it. A heartbeat read that finds a
//! command running is skipped, not failed.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use ipnet::Ipv4Net;
use sakala_agent_core::ports::{RuntimeExecutionError, VALKEY_USAGE_BUSY, ValkeyAdmin};
use sakala_agent_protocol::{
    CreateValkeyPayload, ManagedValkeyUsage, ResizeValkeyPayload, RotateValkeyPasswordPayload,
    VALKEY_NAME_PREFIX, VALKEY_PORT, ValkeyEndpoint, ValkeyMemoryMb, ValkeyMode,
    ValkeyTargetPayload,
    valkey::{
        CreateValkeyResult, DeleteValkeyResult, ResizeValkeyResult, RotateValkeyPasswordResult,
        VALKEY_STATUS_MISSING, ValkeyLockResult, error_codes,
    },
    valkey_resource_name,
};
use serde::Deserialize;
use tokio::{sync::Mutex, time::Instant};
use tracing::{info, warn};
use uuid::Uuid;

use super::{
    ADMIN_USER, VALKEY_CONTAINER_CONF_DIR, VALKEY_CONTAINER_DATA_DIR, VALKEY_CONTAINER_USER,
    VALKEY_CPU_MILLIS, VALKEY_ID_LABEL, VALKEY_PIDS_LIMIT, budget_mb,
    files::{StoreFiles, StoreRecord, maxmemory_bytes, sha256_hex},
    info::{INFO_SECTIONS, StoreInfo},
    memory_limit_mb,
    network::{
        NetworkState, STORE_NETWORK_FORMAT, VALKEY_NETWORK_KIND, ValkeyNetworkPool, parse_routes,
        parse_subnets, store_host,
    },
};
use crate::{
    CommandSpec, NullOutputSink, ProcessOutput, ProcessRunner, TokioProcessRunner,
    containers::limits::docker_cpu_value, error::describe_exit,
};

/// Carries the admin password into the container, by name only.
const CLI_AUTH_ENV: &str = "VALKEYCLI_AUTH";

const MANAGED_LABEL: &str = "dev.sakala.managed=true";
const NETWORK_KIND_LABEL_KEY: &str = "dev.sakala.network-kind";
const AGENT_ID_LABEL_KEY: &str = "dev.sakala.agent-id";
const MODE_LABEL_KEY: &str = "dev.sakala.valkey-mode";

/// A store restarts with the node and after a crash, like a workload.
const RESTART_POLICY: &str = "unless-stopped";

/// The only writable path besides the data volume.
const TMPFS_MOUNT: &str = "/tmp:rw,noexec,nosuid,size=16m";

/// Docker's `State.Status` of a running store, and of a container that holds
/// a store network as a running project would.
const RUNNING_STATUS: &str = "running";
const ACTIVE_STATES: [&str; 3] = [RUNNING_STATUS, "restarting", "paused"];

/// `docker inspect` of a store's container.
const CONTAINER_FORMAT: &str =
    "{{.Name}}\t{{.State.Status}}\t{{.RestartCount}}\t{{.Config.Image}}\t{{.HostConfig.Memory}}";

/// `docker ps` of the containers on a store network.
const NETWORK_USERS_FORMAT: &str = "{{.Names}}\t{{.State}}";

/// Where the kernel lists this host's routes.
const ROUTE_TABLE: &str = "/proc/net/route";

/// What Docker's built-in address pools cover when `default-address-pools`
/// is not set: 172.17-31.0.0/16 and 192.168.0.0/16.
const DOCKER_DEFAULT_POOLS: [&str; 2] = ["172.16.0.0/12", "192.168.0.0/16"];

/// Bounds one Docker call.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds `docker run`, which may pull the image first.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);
/// Bounds one `valkey-cli` call; the container stops it this much earlier.
const CLI_TIMEOUT: Duration = Duration::from_secs(15);
const CLI_SERVER_MARGIN: Duration = Duration::from_secs(5);
/// How long a started store may take to answer, under gVisor with an AOF to
/// load.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL: Duration = Duration::from_millis(500);
/// Bounds one `INFO` of the heartbeat read.
const USAGE_CLI_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a heartbeat read waits for a running command before it is
/// skipped.
const USAGE_LOCK_WAIT: Duration = Duration::from_secs(2);
/// Left of the node's command timeout for reporting the result.
const COMMAND_MARGIN: Duration = Duration::from_secs(15);
/// A step that would get less time than this is not started.
const MIN_STEP: Duration = Duration::from_secs(5);

/// Most characters of Docker's or Valkey's own message kept in a failure.
const DETAIL_MAX_LENGTH: usize = 300;

/// Replies of a store that is starting, restarting, or refuses the admin user;
/// what a client prints when it cannot connect; and what Docker says about a
/// container it cannot exec into or a daemon it cannot reach.
const UNAVAILABLE_MARKERS: [&str; 9] = [
    "LOADING",
    "NOAUTH",
    "WRONGPASS",
    "Could not connect",
    "Connection refused",
    "is not running",
    "No such container",
    "Cannot connect to the Docker daemon",
    "error during connect",
];

/// The exit status of the container's `timeout` when it stopped the command.
const TIMEOUT_EXIT: i32 = 124;

const MIB: u64 = 1_024 * 1_024;

/// The Valkey host role's settings.
#[derive(Clone, Debug)]
pub struct ValkeyAdminConfig {
    /// `SAKALA_AGENT_ID`, on every network, container, and volume of a store.
    pub agent_id: String,
    /// `SAKALA_VALKEY_IMAGE`, pinned by digest.
    pub image: String,
    /// `SAKALA_VALKEY_DATA_DIR`: the stores' records and configuration.
    pub data_dir: PathBuf,
    /// `SAKALA_VALKEY_NETWORK_POOL`.
    pub network_pool: ValkeyNetworkPool,
    /// `SAKALA_VALKEY_TOTAL_MEMORY_MB`: the most Σ(`maxmemory` + 64) may reach.
    pub total_memory_mb: u64,
    /// `SAKALA_CONTAINER_RUNTIME`, the workloads' runtime, such as `runsc`.
    pub container_runtime: Option<String>,
    /// `SAKALA_COMMAND_TIMEOUT_SECONDS`: every command finishes inside it.
    pub command_timeout: Duration,
}

pub struct DockerValkeyAdmin {
    runner: Arc<dyn ProcessRunner>,
    config: ValkeyAdminConfig,
    files: StoreFiles,
    /// Serializes every command, the heartbeat read, and the reconcile.
    operations: Mutex<()>,
}

/// The time one command has left.
#[derive(Clone, Copy, Debug)]
struct Budget {
    deadline: Instant,
}

impl Budget {
    fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total,
        }
    }

    fn left(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// `preferred`, or less when the command has less time left.
    fn step(self, preferred: Duration, operation: &str) -> Result<Duration, RuntimeExecutionError> {
        let step = preferred.min(self.left());
        if step < MIN_STEP {
            return Err(unavailable(format!(
                "the command ran out of time before {operation}"
            )));
        }
        Ok(step)
    }
}

/// A store's container as Docker reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ContainerState {
    status: String,
    restart_count: u64,
    image: String,
    memory_bytes: u64,
}

impl ContainerState {
    fn running(&self) -> bool {
        self.status == RUNNING_STATUS
    }
}

/// One entry of `docker info`'s `DefaultAddressPools`.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AddressPool {
    base: String,
}

impl DockerValkeyAdmin {
    #[must_use]
    pub fn new(config: ValkeyAdminConfig) -> Self {
        Self::with_runner(Arc::new(TokioProcessRunner::default()), config)
    }

    #[must_use]
    pub fn with_runner(runner: Arc<dyn ProcessRunner>, config: ValkeyAdminConfig) -> Self {
        Self {
            files: StoreFiles::new(config.data_dir.clone()),
            runner,
            config,
            operations: Mutex::new(()),
        }
    }

    fn budget(&self) -> Budget {
        Budget::new(self.config.command_timeout.saturating_sub(COMMAND_MARGIN))
    }

    async fn run(&self, spec: CommandSpec) -> Result<ProcessOutput, RuntimeExecutionError> {
        self.runner
            .run(&spec, &NullOutputSink)
            .await
            .map_err(|error| unavailable(format!("docker could not be run: {error}")))
    }

    async fn docker(
        &self,
        args: &[&str],
        limit: Duration,
    ) -> Result<ProcessOutput, RuntimeExecutionError> {
        let mut spec = CommandSpec::new("docker");
        for arg in args {
            spec = spec.arg(*arg);
        }
        self.run(spec.timeout(limit)).await
    }

    /// A Docker call that must succeed.
    async fn docker_ok(
        &self,
        args: &[&str],
        limit: Duration,
        operation: &str,
    ) -> Result<ProcessOutput, RuntimeExecutionError> {
        let output = self.docker(args, limit).await?;
        if output.success {
            Ok(output)
        } else {
            Err(failure(&output, operation))
        }
    }

    /// `valkey-cli` as the admin user inside the store's container. Returns
    /// the reply; an error reply, a store that does not answer, and a
    /// container that is not running are errors.
    async fn cli(
        &self,
        valkey_id: Uuid,
        args: &[&str],
        limit: Duration,
    ) -> Result<String, RuntimeExecutionError> {
        let admin = self
            .files
            .admin_password(valkey_id)
            .await
            .map_err(|error| self.files_unreadable(&error))?
            .ok_or_else(|| unavailable("the store has no admin password on this node"))?;
        let server_limit = limit
            .saturating_sub(CLI_SERVER_MARGIN)
            .max(Duration::from_secs(1));
        let mut spec = CommandSpec::new("docker")
            .arg("exec")
            .arg("-e")
            .arg(CLI_AUTH_ENV)
            .arg(valkey_resource_name(valkey_id))
            .arg("timeout")
            .arg(server_limit.as_secs().to_string())
            .arg("valkey-cli")
            .arg("--no-auth-warning")
            // An error reply exits 1 instead of 0.
            .arg("-e")
            .arg("--user")
            .arg(ADMIN_USER);
        for arg in args {
            spec = spec.arg(*arg);
        }
        let output = self
            .run(spec.secret_env(CLI_AUTH_ENV, admin).timeout(limit))
            .await?;
        let reply = output.stdout.trim().to_owned();
        if output.success && !reply.is_empty() {
            return Ok(reply);
        }
        let detail = if reply.is_empty() {
            first_line(&output.stderr).to_owned()
        } else {
            first_line(&reply).to_owned()
        };
        let operation = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let message = format!(
            "the store did not answer {operation}: {}",
            if detail.is_empty() {
                describe_exit(output.code)
            } else {
                bounded(&detail)
            }
        );
        if UNAVAILABLE_MARKERS
            .iter()
            .any(|marker| detail.contains(marker))
            || output.code == Some(TIMEOUT_EXIT)
        {
            Err(unavailable(message))
        } else {
            Err(operation_failed(message))
        }
    }

    /// A `valkey-cli` call whose reply must be `expected`.
    async fn cli_expect(
        &self,
        valkey_id: Uuid,
        args: &[&str],
        expected: &str,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let operation = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let reply = self
            .cli(valkey_id, args, budget.step(CLI_TIMEOUT, &operation)?)
            .await?;
        if reply == expected {
            Ok(())
        } else {
            Err(operation_failed(format!(
                "the store answered {operation} with {}",
                bounded(first_line(&reply))
            )))
        }
    }

    fn files_unreadable(&self, error: &io::Error) -> RuntimeExecutionError {
        unavailable(format!(
            "the store files under {} could not be read or written: {error}",
            self.files.root().display()
        ))
    }

    async fn record(&self, valkey_id: Uuid) -> Result<StoreRecord, RuntimeExecutionError> {
        self.files
            .read_record(valkey_id)
            .await
            .map_err(|error| self.files_unreadable(&error))?
            .ok_or_else(|| {
                RuntimeExecutionError::new(
                    error_codes::VALKEY_NOT_FOUND,
                    format!("no Valkey store {valkey_id} on this node"),
                )
            })
    }

    async fn records(&self) -> Result<Vec<StoreRecord>, RuntimeExecutionError> {
        self.files
            .records()
            .await
            .map_err(|error| self.files_unreadable(&error))
    }

    async fn write(&self, record: &StoreRecord) -> Result<(), RuntimeExecutionError> {
        self.files
            .write(record)
            .await
            .map_err(|error| self.files_unreadable(&error))
    }

    /// Refuses a store of `maxmemory_mb` for `valkey_id` that would take the
    /// node's stores past `SAKALA_VALKEY_TOTAL_MEMORY_MB`. The store's own
    /// current size is replaced, not added to.
    fn check_budget(
        &self,
        records: &[StoreRecord],
        valkey_id: Uuid,
        maxmemory_mb: ValkeyMemoryMb,
    ) -> Result<(), RuntimeExecutionError> {
        let needed = records
            .iter()
            .filter(|record| record.valkey_id != valkey_id)
            .map(|record| budget_mb(record.maxmemory_mb))
            .sum::<u64>()
            + budget_mb(maxmemory_mb);
        if needed > self.config.total_memory_mb {
            return Err(capacity_exceeded(format!(
                "stores would need {needed} MiB (maxmemory_mb plus {} MiB each), above SAKALA_VALKEY_TOTAL_MEMORY_MB={}",
                super::VALKEY_BUDGET_OVERHEAD_MB,
                self.config.total_memory_mb
            )));
        }
        Ok(())
    }

    /// The first /27 of the pool that no Docker network, host route, or other
    /// store uses.
    async fn free_subnet(
        &self,
        records: &[StoreRecord],
        budget: Budget,
    ) -> Result<Ipv4Net, RuntimeExecutionError> {
        let mut taken = records
            .iter()
            .map(|record| record.subnet)
            .collect::<Vec<_>>();
        taken.extend(self.docker_subnets(budget).await?);
        taken.extend(host_routes().await);
        self.config.network_pool.free_subnet(&taken).ok_or_else(|| {
            capacity_exceeded(format!(
                "SAKALA_VALKEY_NETWORK_POOL {} has no free /27 left for another store network",
                self.config.network_pool
            ))
        })
    }

    /// The IPv4 subnets of every Docker network.
    async fn docker_subnets(&self, budget: Budget) -> Result<Vec<Ipv4Net>, RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "listing Docker networks")?;
        let listed = self
            .docker_ok(
                &["network", "ls", "--quiet"],
                limit,
                "listing Docker networks",
            )
            .await?;
        let ids = listed
            .stdout
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![
            "network",
            "inspect",
            "--format",
            "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
        ];
        args.extend(ids);
        // A network removed between the two calls makes inspect fail for it
        // only; the others still print.
        let inspected = self.docker(&args, limit).await?;
        if !inspected.success && inspected.stdout.trim().is_empty() {
            return Err(failure(&inspected, "reading Docker network subnets"));
        }
        Ok(parse_subnets(&inspected.stdout))
    }

    async fn inspect_network(
        &self,
        name: &str,
        budget: Budget,
    ) -> Result<Option<NetworkState>, RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "inspecting the store network")?;
        let output = self
            .docker(
                &["network", "inspect", "--format", STORE_NETWORK_FORMAT, name],
                limit,
            )
            .await?;
        if !output.success {
            if missing(&output.stderr) {
                return Ok(None);
            }
            return Err(failure(&output, "inspecting the store network"));
        }
        Ok(Some(NetworkState::parse(&output.stdout)))
    }

    async fn inspect_container(
        &self,
        name: &str,
        budget: Budget,
    ) -> Result<Option<ContainerState>, RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "inspecting the store container")?;
        Ok(self
            .container_states(&[name.to_owned()], limit)
            .await?
            .remove(name))
    }

    /// The containers among `names` that exist, by name.
    async fn container_states(
        &self,
        names: &[String],
        limit: Duration,
    ) -> Result<HashMap<String, ContainerState>, RuntimeExecutionError> {
        let mut args = vec![
            "inspect",
            "--type",
            "container",
            "--format",
            CONTAINER_FORMAT,
        ];
        args.extend(names.iter().map(String::as_str));
        let output = self.docker(&args, limit).await?;
        if !output.success && !missing(&output.stderr) {
            return Err(failure(&output, "inspecting the store containers"));
        }
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| {
                let fields = line.trim().split('\t').collect::<Vec<_>>();
                let name = fields.first()?.trim_start_matches('/').to_owned();
                Some((
                    name,
                    ContainerState {
                        status: (*fields.get(1)?).to_owned(),
                        restart_count: fields.get(2)?.parse().ok()?,
                        image: (*fields.get(3)?).to_owned(),
                        memory_bytes: fields.get(4)?.parse().ok()?,
                    },
                ))
            })
            .collect())
    }

    async fn volume_exists(
        &self,
        name: &str,
        budget: Budget,
    ) -> Result<bool, RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "inspecting the store volume")?;
        let output = self
            .docker(&["volume", "inspect", "--format", "{{.Name}}", name], limit)
            .await?;
        if output.success {
            return Ok(true);
        }
        if missing(&output.stderr) {
            return Ok(false);
        }
        Err(failure(&output, "inspecting the store volume"))
    }

    /// Creates the store network when missing. An existing one must be this
    /// agent's store network with the record's subnet.
    async fn ensure_network(
        &self,
        record: &StoreRecord,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let name = valkey_resource_name(record.valkey_id);
        if let Some(network) = self.inspect_network(&name, budget).await? {
            return self.check_network(record, &network);
        }
        let limit = budget.step(DOCKER_TIMEOUT, "creating the store network")?;
        let subnet = record.subnet.to_string();
        let id_label = format!("{VALKEY_ID_LABEL}={}", record.valkey_id);
        let kind_label = format!("{NETWORK_KIND_LABEL_KEY}={VALKEY_NETWORK_KIND}");
        let agent_label = format!("{AGENT_ID_LABEL_KEY}={}", self.config.agent_id);
        let output = self
            .docker(
                &[
                    "network",
                    "create",
                    "--driver",
                    "bridge",
                    // No route out: the store, and Lua that escapes it, reach
                    // nothing but the containers on this network.
                    "--internal",
                    "--subnet",
                    &subnet,
                    "--label",
                    MANAGED_LABEL,
                    "--label",
                    &kind_label,
                    "--label",
                    &id_label,
                    "--label",
                    &agent_label,
                    &name,
                ],
                limit,
            )
            .await?;
        if output.success {
            return Ok(());
        }
        if output.stderr.contains("already exists") {
            let network = self.inspect_network(&name, budget).await?.ok_or_else(|| {
                operation_failed("the store network disappeared while it was created")
            })?;
            return self.check_network(record, &network);
        }
        Err(failure(&output, "creating the store network"))
    }

    fn check_network(
        &self,
        record: &StoreRecord,
        network: &NetworkState,
    ) -> Result<(), RuntimeExecutionError> {
        if network.owned_by(&self.config.agent_id) && network.subnets == [record.subnet] {
            return Ok(());
        }
        Err(conflict(format!(
            "network {} exists but is not this agent's store network on {}; nothing was changed",
            valkey_resource_name(record.valkey_id),
            record.subnet
        )))
    }

    async fn ensure_volume(
        &self,
        valkey_id: Uuid,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "creating the store volume")?;
        let id_label = format!("{VALKEY_ID_LABEL}={valkey_id}");
        let agent_label = format!("{AGENT_ID_LABEL_KEY}={}", self.config.agent_id);
        // Idempotent: an existing volume of the same name is kept as it is.
        self.docker_ok(
            &[
                "volume",
                "create",
                "--label",
                &id_label,
                "--label",
                &agent_label,
                &valkey_resource_name(valkey_id),
            ],
            limit,
            "creating the store volume",
        )
        .await
        .map(drop)
    }

    /// `docker run` of a store: the workload hardening, a fixed address on its
    /// own network, its configuration read-only, and its data in its volume.
    fn run_spec(&self, record: &StoreRecord) -> CommandSpec {
        let name = valkey_resource_name(record.valkey_id);
        let limit = format!("{}m", memory_limit_mb(record.maxmemory_mb));
        let mut spec = CommandSpec::new("docker")
            .arg("run")
            .arg("--detach")
            .arg("--name")
            .arg(&name)
            .arg("--network")
            .arg(&name)
            .arg("--ip")
            .arg(store_host(record.subnet).to_string());
        if let Some(runtime) = &self.config.container_runtime {
            spec = spec.arg("--runtime").arg(runtime);
        }
        spec.arg("--restart")
            .arg(RESTART_POLICY)
            .arg("--read-only")
            .arg("--tmpfs")
            .arg(TMPFS_MOUNT)
            .arg("--security-opt")
            .arg("no-new-privileges:true")
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--user")
            .arg(VALKEY_CONTAINER_USER)
            .arg("--memory")
            .arg(&limit)
            .arg("--memory-swap")
            .arg(&limit)
            .arg("--cpus")
            .arg(docker_cpu_value(VALKEY_CPU_MILLIS))
            .arg("--pids-limit")
            .arg(VALKEY_PIDS_LIMIT.to_string())
            .arg("--mount")
            .arg(format!(
                "type=bind,source={},target={VALKEY_CONTAINER_CONF_DIR},readonly",
                self.files.conf_dir(record.valkey_id).display()
            ))
            .arg("--mount")
            .arg(format!(
                "type=volume,source={name},target={VALKEY_CONTAINER_DATA_DIR}"
            ))
            .arg("--label")
            .arg(format!("{VALKEY_ID_LABEL}={}", record.valkey_id))
            .arg("--label")
            .arg(format!("{MODE_LABEL_KEY}={}", record.mode))
            .arg("--label")
            .arg(format!("{AGENT_ID_LABEL_KEY}={}", self.config.agent_id))
            .arg(&self.config.image)
            .arg("valkey-server")
            .arg(format!("{VALKEY_CONTAINER_CONF_DIR}/valkey.conf"))
    }

    async fn run_container(
        &self,
        record: &StoreRecord,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let limit = budget.step(RUN_TIMEOUT, "starting the store container")?;
        let output = self.run(self.run_spec(record).timeout(limit)).await?;
        if output.success {
            Ok(())
        } else {
            Err(failure(&output, "docker run of the store"))
        }
    }

    async fn remove_container(
        &self,
        name: &str,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let limit = budget.step(DOCKER_TIMEOUT, "removing the store container")?;
        let output = self.docker(&["rm", "--force", name], limit).await?;
        if output.success || missing(&output.stderr) {
            Ok(())
        } else {
            Err(failure(&output, "removing the store container"))
        }
    }

    /// Makes the store's container run with the node's image and the record's
    /// memory limit. Returns whether it was started now, which means it read
    /// the store's files as they are.
    async fn ensure_container(
        &self,
        record: &StoreRecord,
        budget: Budget,
    ) -> Result<bool, RuntimeExecutionError> {
        let name = valkey_resource_name(record.valkey_id);
        let expected_memory = memory_limit_mb(record.maxmemory_mb) * MIB;
        match self.inspect_container(&name, budget).await? {
            Some(state)
                if state.image != self.config.image || state.memory_bytes != expected_memory =>
            {
                // A new image, such as a security release, or a resize the
                // runtime could not apply in place. The volume keeps a
                // persistent store's data.
                info!(
                    valkey_id = %record.valkey_id,
                    "recreating a Valkey store container with the node's image and memory limit"
                );
                self.remove_container(&name, budget).await?;
                self.run_container(record, budget).await?;
                Ok(true)
            }
            Some(state) if state.running() => Ok(false),
            Some(_) => {
                let limit = budget.step(DOCKER_TIMEOUT, "starting the store container")?;
                self.docker_ok(&["start", &name], limit, "starting the store container")
                    .await?;
                Ok(true)
            }
            None => {
                self.run_container(record, budget).await?;
                Ok(true)
            }
        }
    }

    /// Fails unless the store's container is running.
    async fn require_running(
        &self,
        valkey_id: Uuid,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        match self
            .inspect_container(&valkey_resource_name(valkey_id), budget)
            .await?
        {
            Some(state) if state.running() => Ok(()),
            Some(state) => Err(unavailable(format!(
                "the store's container is {}; its files are updated and apply when it starts",
                state.status
            ))),
            None => Err(unavailable(
                "the store's container is missing; send CreateValkey again to recreate it",
            )),
        }
    }

    /// Waits until the store answers `PING`, as long as [`READY_TIMEOUT`] and
    /// the budget allow.
    async fn wait_ready(
        &self,
        valkey_id: Uuid,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        let deadline = Instant::now() + READY_TIMEOUT.min(budget.left());
        loop {
            let limit = CLI_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
            let error = match self.cli(valkey_id, &["PING"], limit.max(MIN_STEP)).await {
                Ok(reply) if reply == "PONG" => return Ok(()),
                Ok(reply) => bounded(first_line(&reply)),
                Err(error) => error.to_string(),
            };
            if Instant::now() + READY_POLL >= deadline {
                return Err(unavailable(format!(
                    "the store did not answer within {} seconds: {error}",
                    READY_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Makes the running store read `users.acl` again.
    async fn load_acl(&self, valkey_id: Uuid, budget: Budget) -> Result<(), RuntimeExecutionError> {
        self.cli_expect(valkey_id, &["ACL", "LOAD"], "OK", budget)
            .await
    }

    /// Closes every connection of the tenant. A connection stays logged in
    /// after its user's password changes or the user is turned off.
    async fn disconnect_tenant(
        &self,
        valkey_id: Uuid,
        budget: Budget,
    ) -> Result<u64, RuntimeExecutionError> {
        let reply = self
            .cli(
                valkey_id,
                &["CLIENT", "KILL", "USER", "default"],
                budget.step(CLI_TIMEOUT, "closing the tenant's connections")?,
            )
            .await?;
        reply.parse().map_err(|_| {
            operation_failed(format!(
                "the store answered CLIENT KILL with {}",
                bounded(first_line(&reply))
            ))
        })
    }

    async fn set_maxmemory(
        &self,
        record: &StoreRecord,
        budget: Budget,
    ) -> Result<(), RuntimeExecutionError> {
        self.cli_expect(
            record.valkey_id,
            &[
                "CONFIG",
                "SET",
                "maxmemory",
                &maxmemory_bytes(record.maxmemory_mb).to_string(),
            ],
            "OK",
            budget,
        )
        .await
    }

    async fn store_info(
        &self,
        valkey_id: Uuid,
        limit: Duration,
    ) -> Result<StoreInfo, RuntimeExecutionError> {
        let mut args = vec!["INFO"];
        args.extend(INFO_SECTIONS);
        self.cli(valkey_id, &args, limit)
            .await
            .map(|reply| StoreInfo::parse(&reply))
    }

    /// Applies the record's lock state and closes the tenant's connections on
    /// a lock.
    async fn set_locked(
        &self,
        valkey_id: Uuid,
        locked: bool,
    ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let mut record = self.record(valkey_id).await?;
        record.locked = locked;
        self.write(&record).await?;
        self.require_running(valkey_id, budget).await?;
        self.load_acl(valkey_id, budget).await?;
        let disconnected_clients = if locked {
            self.disconnect_tenant(valkey_id, budget).await?
        } else {
            0
        };
        Ok(ValkeyLockResult {
            valkey_id,
            locked,
            disconnected_clients,
        })
    }

    /// Names of this agent's Docker objects of one kind that carry a store ID,
    /// by store.
    async fn labelled(
        &self,
        args: &[&str],
        limit: Duration,
    ) -> Result<Vec<(String, Uuid)>, RuntimeExecutionError> {
        let output = self.docker_ok(args, limit, "listing store objects").await?;
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter_map(|name| {
                let valkey_id = Uuid::parse_str(name.strip_prefix(VALKEY_NAME_PREFIX)?).ok()?;
                Some((name.to_owned(), valkey_id))
            })
            .collect())
    }
}

#[async_trait]
impl ValkeyAdmin for DockerValkeyAdmin {
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        self.files
            .ensure_root()
            .await
            .map_err(|error| self.files_unreadable(&error))?;
        let output = self
            .docker_ok(
                &["info", "--format", "{{json .DefaultAddressPools}}"],
                DOCKER_TIMEOUT,
                "reading Docker's address pools",
            )
            .await?;
        let pool = self.config.network_pool;
        let docker_pools = serde_json::from_str::<Option<Vec<AddressPool>>>(output.stdout.trim())
            .ok()
            .flatten()
            .filter(|pools| !pools.is_empty())
            .map_or_else(
                || {
                    DOCKER_DEFAULT_POOLS
                        .iter()
                        .filter_map(|pool| pool.parse().ok())
                        .collect::<Vec<Ipv4Net>>()
                },
                |pools| {
                    pools
                        .iter()
                        .filter_map(|pool| pool.base.parse().ok())
                        .collect()
                },
            );
        for overlap in pool.overlapping(&docker_pools) {
            warn!(
                pool = %pool,
                docker_pool = %overlap,
                "SAKALA_VALKEY_NETWORK_POOL overlaps one of Docker's address pools; store networks skip ranges Docker already uses, but choose a range outside its pools"
            );
        }
        for overlap in pool.overlapping(&host_routes().await) {
            warn!(
                pool = %pool,
                route = %overlap,
                "SAKALA_VALKEY_NETWORK_POOL overlaps a route of this host; store networks skip it, but choose a range outside the host's networks"
            );
        }
        let stores = self.records().await?;
        info!(
            stores = stores.len(),
            pool = %pool,
            capacity_mb = self.config.total_memory_mb,
            "managed Valkey role is ready"
        );
        Ok(())
    }

    async fn create_valkey(
        &self,
        payload: &CreateValkeyPayload,
    ) -> Result<CreateValkeyResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let valkey_id = payload.valkey_id;
        let password_sha256 = sha256_hex(payload.password.expose());
        let (record, created, password_changed) = match self
            .files
            .read_record(valkey_id)
            .await
            .map_err(|error| self.files_unreadable(&error))?
        {
            Some(mut record) => {
                if record.mode != payload.mode || record.maxmemory_mb != payload.maxmemory_mb {
                    return Err(conflict(format!(
                        "store {valkey_id} exists as a {} store of {} MiB; its mode and size do not change on CreateValkey, use ResizeValkey for the size",
                        record.mode,
                        record.maxmemory_mb.get()
                    )));
                }
                let changed = record.password_sha256 != password_sha256;
                record.password_sha256 = password_sha256;
                (record, false, changed)
            }
            None => {
                let records = self.records().await?;
                self.check_budget(&records, valkey_id, payload.maxmemory_mb)?;
                let subnet = self.free_subnet(&records, budget).await?;
                (
                    StoreRecord::new(
                        valkey_id,
                        payload.mode,
                        payload.maxmemory_mb,
                        subnet,
                        &payload.password,
                    ),
                    true,
                    false,
                )
            }
        };
        // The record first: a failure below leaves the store's address
        // reserved, and the same command finishes it.
        self.write(&record).await?;
        self.ensure_network(&record, budget).await?;
        self.ensure_volume(valkey_id, budget).await?;
        let started = self.ensure_container(&record, budget).await?;
        self.wait_ready(valkey_id, budget).await?;
        if !started {
            self.load_acl(valkey_id, budget).await?;
            if password_changed {
                self.disconnect_tenant(valkey_id, budget).await?;
            }
        }
        if created {
            info!(%valkey_id, mode = %record.mode, maxmemory_mb = record.maxmemory_mb.get(), subnet = %record.subnet, "Valkey store created");
        }
        Ok(CreateValkeyResult {
            valkey_id,
            mode: record.mode,
            maxmemory_mb: record.maxmemory_mb,
            memory_limit_mb: memory_limit_mb(record.maxmemory_mb),
            network: valkey_resource_name(valkey_id),
            endpoint: ValkeyEndpoint {
                host: store_host(record.subnet),
                port: VALKEY_PORT,
            },
            created,
            locked: record.locked,
        })
    }

    async fn delete_valkey(
        &self,
        payload: &ValkeyTargetPayload,
    ) -> Result<DeleteValkeyResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let valkey_id = payload.valkey_id;
        let name = valkey_resource_name(valkey_id);
        let container = self.inspect_container(&name, budget).await?;
        let network = self.inspect_network(&name, budget).await?;
        if let Some(network) = &network
            && !network.owned_by(&self.config.agent_id)
        {
            return Err(conflict(format!(
                "network {name} is not this agent's store network; nothing was removed"
            )));
        }
        let volume = self.volume_exists(&name, budget).await?;
        let files = self
            .files
            .exists(valkey_id)
            .await
            .map_err(|error| self.files_unreadable(&error))?;
        if container.is_none() && network.is_none() && !volume && !files {
            return Ok(DeleteValkeyResult {
                valkey_id,
                deleted: false,
            });
        }

        if network.is_some() {
            let limit = budget.step(DOCKER_TIMEOUT, "listing the store network's containers")?;
            let filter = format!("network={name}");
            let users = self
                .docker_ok(
                    &[
                        "ps",
                        "--all",
                        "--filter",
                        &filter,
                        "--format",
                        NETWORK_USERS_FORMAT,
                    ],
                    limit,
                    "listing the store network's containers",
                )
                .await?;
            let users = users
                .stdout
                .lines()
                .filter_map(|line| line.trim().split_once('\t'))
                .filter(|(user, _)| *user != name)
                .map(|(user, state)| (user.to_owned(), state.to_owned()))
                .collect::<Vec<_>>();
            let active = users
                .iter()
                .filter(|(_, state)| ACTIVE_STATES.contains(&state.as_str()))
                .map(|(user, _)| user.as_str())
                .collect::<Vec<_>>();
            if !active.is_empty() {
                let count = match active.len() {
                    1 => "1 running container is".to_owned(),
                    count => format!("{count} running containers are"),
                };
                return Err(RuntimeExecutionError::new(
                    error_codes::VALKEY_IN_USE,
                    format!(
                        "{count} still on the store's network: {}; stop or redeploy the projects that used it first",
                        active.join(", ")
                    ),
                ));
            }
            // A stopped or sleeping container of a project that used the
            // store would otherwise fail to start once the network is gone.
            for (user, _) in &users {
                let limit = budget.step(DOCKER_TIMEOUT, "disconnecting a stopped container")?;
                let output = self
                    .docker(&["network", "disconnect", "--force", &name, user], limit)
                    .await?;
                if !output.success
                    && !output.stderr.contains("is not connected")
                    && !missing(&output.stderr)
                {
                    return Err(failure(&output, "disconnecting a stopped container"));
                }
            }
        }
        if container.is_some() {
            self.remove_container(&name, budget).await?;
        }
        if network.is_some() {
            let limit = budget.step(DOCKER_TIMEOUT, "removing the store network")?;
            let output = self.docker(&["network", "rm", &name], limit).await?;
            if !output.success && !missing(&output.stderr) {
                return Err(failure(&output, "removing the store network"));
            }
        }
        if volume {
            let limit = budget.step(DOCKER_TIMEOUT, "removing the store volume")?;
            let output = self.docker(&["volume", "rm", &name], limit).await?;
            if !output.success && !missing(&output.stderr) {
                return Err(failure(&output, "removing the store volume"));
            }
        }
        self.files
            .remove(valkey_id)
            .await
            .map_err(|error| self.files_unreadable(&error))?;
        info!(%valkey_id, "Valkey store deleted");
        Ok(DeleteValkeyResult {
            valkey_id,
            deleted: true,
        })
    }

    async fn rotate_valkey_password(
        &self,
        payload: &RotateValkeyPasswordPayload,
    ) -> Result<RotateValkeyPasswordResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let valkey_id = payload.valkey_id;
        let mut record = self.record(valkey_id).await?;
        record.password_sha256 = sha256_hex(payload.password.expose());
        self.write(&record).await?;
        self.require_running(valkey_id, budget).await?;
        self.load_acl(valkey_id, budget).await?;
        let disconnected_clients = self.disconnect_tenant(valkey_id, budget).await?;
        Ok(RotateValkeyPasswordResult {
            valkey_id,
            disconnected_clients,
        })
    }

    async fn lock_valkey(
        &self,
        payload: &ValkeyTargetPayload,
    ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
        self.set_locked(payload.valkey_id, true).await
    }

    async fn unlock_valkey(
        &self,
        payload: &ValkeyTargetPayload,
    ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
        self.set_locked(payload.valkey_id, false).await
    }

    async fn resize_valkey(
        &self,
        payload: &ResizeValkeyPayload,
    ) -> Result<ResizeValkeyResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let valkey_id = payload.valkey_id;
        let mut record = self.record(valkey_id).await?;
        let previous = record.maxmemory_mb;
        let requested = payload.maxmemory_mb;
        self.check_budget(&self.records().await?, valkey_id, requested)?;
        self.require_running(valkey_id, budget).await?;
        let shrinking = requested < previous;
        if shrinking && record.mode == ValkeyMode::Persistent {
            let info = self
                .store_info(
                    valkey_id,
                    budget.step(CLI_TIMEOUT, "reading the store's memory")?,
                )
                .await?;
            let used = info
                .used_memory
                .ok_or_else(|| operation_failed("the store did not report used_memory"))?;
            if used > maxmemory_bytes(requested) {
                return Err(RuntimeExecutionError::new(
                    error_codes::VALKEY_SIZE_BELOW_USAGE,
                    format!(
                        "the persistent store holds {} MiB, more than the requested {} MiB; nothing was changed",
                        used.div_ceil(MIB),
                        requested.get()
                    ),
                ));
            }
        }
        record.maxmemory_mb = requested;
        self.write(&record).await?;
        let name = valkey_resource_name(valkey_id);
        let limit = format!("{}m", memory_limit_mb(requested));
        // Lower Valkey's limit before the container's, and raise the
        // container's before Valkey's, so Valkey never outgrows its cgroup.
        if shrinking {
            self.set_maxmemory(&record, budget).await?;
        }
        let step = budget.step(DOCKER_TIMEOUT, "changing the store's memory limit")?;
        let updated = self
            .docker(
                &["update", "--memory", &limit, "--memory-swap", &limit, &name],
                step,
            )
            .await?;
        let restarted = if updated.success {
            false
        } else {
            warn!(
                %valkey_id,
                detail = %bounded(first_line(&updated.stderr)),
                "the runtime could not change a Valkey store's memory limit in place; recreating its container"
            );
            self.remove_container(&name, budget).await?;
            self.run_container(&record, budget).await?;
            self.wait_ready(valkey_id, budget).await?;
            true
        };
        if !shrinking && !restarted {
            self.set_maxmemory(&record, budget).await?;
        }
        Ok(ResizeValkeyResult {
            valkey_id,
            maxmemory_mb: requested,
            memory_limit_mb: memory_limit_mb(requested),
            restarted,
        })
    }

    async fn valkey_usage(&self) -> Result<Vec<ManagedValkeyUsage>, RuntimeExecutionError> {
        let Ok(_operation) = tokio::time::timeout(USAGE_LOCK_WAIT, self.operations.lock()).await
        else {
            return Err(RuntimeExecutionError::new(
                VALKEY_USAGE_BUSY,
                "a Valkey command is running",
            ));
        };
        let records = self.records().await?;
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let names = records
            .iter()
            .map(|record| valkey_resource_name(record.valkey_id))
            .collect::<Vec<_>>();
        let mut states = self.container_states(&names, DOCKER_TIMEOUT).await?;
        let mut usage = Vec::with_capacity(records.len());
        for (record, name) in records.into_iter().zip(names) {
            let state = states.remove(&name);
            let info = match &state {
                Some(state) if state.running() => self
                    .store_info(record.valkey_id, USAGE_CLI_TIMEOUT)
                    .await
                    .unwrap_or_default(),
                _ => StoreInfo::default(),
            };
            usage.push(ManagedValkeyUsage {
                valkey_id: record.valkey_id,
                mode: record.mode,
                maxmemory_mb: record.maxmemory_mb,
                locked: record.locked,
                status: state.as_ref().map_or_else(
                    || VALKEY_STATUS_MISSING.to_owned(),
                    |state| state.status.clone(),
                ),
                restart_count: state.as_ref().map_or(0, |state| state.restart_count),
                used_memory: info.used_memory,
                maxmemory: info.maxmemory,
                evicted_keys: info.evicted_keys,
                errorstat_oom: info.errorstat_oom,
                connected_clients: info.connected_clients,
                aof_last_write_status: info.aof_last_write_status,
            });
        }
        Ok(usage)
    }

    async fn reconcile(&self) -> Result<(), RuntimeExecutionError> {
        let Ok(_operation) = self.operations.try_lock() else {
            return Ok(());
        };
        let known = self
            .records()
            .await?
            .into_iter()
            .map(|record| record.valkey_id)
            .collect::<HashSet<_>>();
        let agent_filter = format!("label={AGENT_ID_LABEL_KEY}={}", self.config.agent_id);
        let kind_filter = format!("label={NETWORK_KIND_LABEL_KEY}={VALKEY_NETWORK_KIND}");
        let networks = self
            .labelled(
                &[
                    "network",
                    "ls",
                    "--filter",
                    &kind_filter,
                    "--filter",
                    &agent_filter,
                    "--format",
                    "{{.Name}}",
                ],
                DOCKER_TIMEOUT,
            )
            .await?;
        for (name, valkey_id) in networks.iter().filter(|(_, id)| !known.contains(id)) {
            let filter = format!("network={name}");
            let users = self
                .docker_ok(
                    &["ps", "--all", "--quiet", "--filter", &filter],
                    DOCKER_TIMEOUT,
                    "listing a store network's containers",
                )
                .await?;
            if users.stdout.trim().is_empty() {
                let removed = self
                    .docker(&["network", "rm", name], DOCKER_TIMEOUT)
                    .await?;
                if removed.success {
                    info!(%valkey_id, network = %name, "removed a Valkey store network without a store");
                }
            } else {
                warn!(%valkey_id, network = %name, "a Valkey store network without a store still has containers; it is kept");
            }
        }
        let id_filter = format!("label={VALKEY_ID_LABEL}");
        let containers = self
            .labelled(
                &[
                    "ps",
                    "--all",
                    "--filter",
                    &id_filter,
                    "--filter",
                    &agent_filter,
                    "--format",
                    "{{.Names}}",
                ],
                DOCKER_TIMEOUT,
            )
            .await?;
        let volumes = self
            .labelled(
                &[
                    "volume",
                    "ls",
                    "--filter",
                    &id_filter,
                    "--filter",
                    &agent_filter,
                    "--format",
                    "{{.Name}}",
                ],
                DOCKER_TIMEOUT,
            )
            .await?;
        for (kind, (name, valkey_id)) in containers
            .iter()
            .map(|entry| ("container", entry))
            .chain(volumes.iter().map(|entry| ("volume", entry)))
            .filter(|(_, (_, id))| !known.contains(id))
        {
            warn!(
                %valkey_id,
                kind,
                name = %name,
                "Valkey store {kind} without a store record detected; automatic deletion is disabled, send DeleteValkey to remove it"
            );
        }
        Ok(())
    }
}

/// This host's routes, empty when they cannot be read.
async fn host_routes() -> Vec<Ipv4Net> {
    tokio::fs::read_to_string(ROUTE_TABLE)
        .await
        .map(|table| parse_routes(&table))
        .unwrap_or_default()
}

fn missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such") || stderr.contains("not found")
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

fn bounded(text: &str) -> String {
    text.chars().take(DETAIL_MAX_LENGTH).collect()
}

/// A Docker call that failed: unavailable when the daemon does not answer,
/// refused otherwise.
fn failure(output: &ProcessOutput, operation: &str) -> RuntimeExecutionError {
    let detail = first_line(&output.stderr);
    let message = if detail.is_empty() {
        format!("{operation} failed with {}", describe_exit(output.code))
    } else {
        format!(
            "{operation} failed with {}: {}",
            describe_exit(output.code),
            bounded(detail)
        )
    };
    if detail.contains("Cannot connect to the Docker daemon")
        || detail.contains("error during connect")
    {
        unavailable(message)
    } else {
        operation_failed(message)
    }
}

fn unavailable(message: impl Into<String>) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::VALKEY_UNAVAILABLE, message)
}

fn operation_failed(message: impl Into<String>) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::VALKEY_OPERATION_FAILED, message)
}

fn capacity_exceeded(message: impl Into<String>) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::VALKEY_CAPACITY_EXCEEDED, message)
}

fn conflict(message: impl Into<String>) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::VALKEY_CONFLICT, message)
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
