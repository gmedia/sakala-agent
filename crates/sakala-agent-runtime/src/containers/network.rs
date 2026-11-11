//! One bridge network per project.
//!
//! On a shared network every workload can open connections to every other
//! workload, bypassing the router. A network per project leaves a workload with
//! only its own project's containers, which during a redeploy means the previous
//! deployment and the candidate, plus the router. Docker isolates bridge networks
//! from each other, so neither a name nor an address of another project answers.

use std::{collections::HashSet, sync::Arc};

use sakala_agent_core::ports::RuntimePreflightCheck;
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{CommandSpec, NullOutputSink, ProcessOutput, ProcessRunner, RuntimeError};

const NETWORK_PREFIX: &str = "sakala-project-";
const PREFLIGHT_NETWORK: &str = "sakala-project-preflight";
const MANAGED_LABEL: &str = "dev.sakala.managed=true";
const PROJECT_KIND_LABEL: &str = "dev.sakala.network-kind=project";
const PREFLIGHT_KIND_LABEL: &str = "dev.sakala.network-kind=preflight";
const PROJECT_LABEL: &str = "dev.sakala.project-id";
const AGENT_LABEL: &str = "dev.sakala.agent-id";
const ROUTER_NETWORKS_FORMAT: &str =
    "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}}{{\"\\n\"}}{{end}}";
/// What Docker hands out without `default-address-pools`: 172.17-19.0.0/16, three
/// /14 split into /16, and 192.168.0.0/16 split into /20.
const DOCKER_DEFAULT_POOL_NETWORKS: u64 = 31;
const DOCS: &str = "docs/DOCKER_RUNTIME.md#network-per-project";

/// The network a project's workloads join. Deterministic, so every command for a
/// project finds it again without state.
#[must_use]
pub fn project_network_name(project_id: Uuid) -> String {
    format!("{NETWORK_PREFIX}{project_id}")
}

/// What startup reconciliation changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetworkReconciliation {
    pub removed: usize,
    pub router_attached: usize,
}

pub(crate) struct ProjectNetworks {
    runner: Arc<dyn ProcessRunner>,
    router: String,
    agent_id: String,
}

impl ProjectNetworks {
    pub(crate) fn new(runner: Arc<dyn ProcessRunner>, router: String, agent_id: String) -> Self {
        Self {
            runner,
            router,
            agent_id,
        }
    }

    /// Creates the project's network when it is missing and connects the router,
    /// so the router can probe and route to the workload by its name.
    pub(crate) async fn prepare(&self, project_id: Uuid) -> Result<String, RuntimeError> {
        let name = project_network_name(project_id);
        let create = CommandSpec::new("docker")
            .arg("network")
            .arg("create")
            .arg("--driver")
            .arg("bridge")
            .arg("--label")
            .arg(MANAGED_LABEL)
            .arg("--label")
            .arg(PROJECT_KIND_LABEL)
            .arg("--label")
            .arg(format!("{PROJECT_LABEL}={project_id}"))
            .arg("--label")
            .arg(format!("{AGENT_LABEL}={}", self.agent_id))
            .arg(&name);
        let created = self.docker(&create).await?;
        if !created.success && !created.stderr.contains("already exists") {
            warn!(
                network = %name,
                detail = %first_line(&created.stderr),
                "Docker could not create a project network"
            );
            return Err(creation_error(&created));
        }
        if !self.connect_router(&name).await? {
            return Err(RuntimeError::Container(
                "the project network disappeared while it was being prepared".to_owned(),
            ));
        }
        Ok(name)
    }

    /// Connects the router again, for example after its container was recreated.
    /// A workload from an older Agent still lives on the shared network and has no
    /// network of its own, which needs nothing.
    pub(crate) async fn attach_router(&self, project_id: Uuid) -> Result<(), RuntimeError> {
        self.connect_router(&project_network_name(project_id))
            .await
            .map(|_| ())
    }

    /// Disconnects the router and removes the project's network once no container
    /// uses it, stopped ones included: a sleeping workload needs its network to
    /// start again. Returns whether the network is gone.
    pub(crate) async fn release(&self, project_id: Uuid) -> Result<bool, RuntimeError> {
        let name = project_network_name(project_id);
        let users = CommandSpec::new("docker")
            .arg("ps")
            .arg("--all")
            .arg("--filter")
            .arg(format!("network={name}"))
            .arg("--format")
            .arg("{{.Names}}");
        let listed = self.docker(&users).await?;
        if !listed.success {
            return Err(RuntimeError::Container(format!(
                "project network user listing failed with {}",
                crate::error::describe_exit(listed.code)
            )));
        }
        if listed
            .stdout
            .lines()
            .map(str::trim)
            .any(|user| !user.is_empty() && user != self.router)
        {
            return Ok(false);
        }

        let disconnect = CommandSpec::new("docker")
            .arg("network")
            .arg("disconnect")
            .arg(&name)
            .arg(&self.router);
        let disconnected = self.docker(&disconnect).await?;
        if !disconnected.success
            && !disconnected.stderr.contains("is not connected")
            && !disconnected.stderr.contains("No such container")
            && !network_missing(&disconnected.stderr)
        {
            warn!(
                network = %name,
                detail = %first_line(&disconnected.stderr),
                "the router could not leave a project network"
            );
            return Err(RuntimeError::Container(format!(
                "the router could not leave the project network: {}",
                crate::error::describe_exit(disconnected.code)
            )));
        }

        let remove = CommandSpec::new("docker")
            .arg("network")
            .arg("rm")
            .arg(&name);
        let removed = self.docker(&remove).await?;
        if removed.success || network_missing(&removed.stderr) {
            return Ok(true);
        }
        warn!(
            network = %name,
            detail = %first_line(&removed.stderr),
            "Docker could not remove a project network"
        );
        Err(RuntimeError::Container(format!(
            "project network removal failed with {}",
            crate::error::describe_exit(removed.code)
        )))
    }

    /// Runs once at startup, before commands are accepted. Removes this Agent's
    /// project networks that no container uses, and reconnects the router to the
    /// others, since a recreated router container keeps only the networks its own
    /// Compose file names. One network failing does not stop the others.
    pub(crate) async fn reconcile(&self) -> Result<NetworkReconciliation, RuntimeError> {
        let list = CommandSpec::new("docker")
            .arg("network")
            .arg("ls")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}"))
            .arg("--filter")
            .arg(format!("label={PROJECT_KIND_LABEL}"))
            .arg("--filter")
            .arg(format!("label={AGENT_LABEL}={}", self.agent_id))
            .arg("--format")
            .arg("{{.Name}}");
        let listed = self.docker(&list).await?;
        if !listed.success {
            return Err(RuntimeError::Container(format!(
                "project network listing failed with {}",
                crate::error::describe_exit(listed.code)
            )));
        }
        let attached = self.router_networks().await;
        let mut result = NetworkReconciliation::default();
        for name in listed.stdout.lines().map(str::trim) {
            let Some(project_id) = name
                .strip_prefix(NETWORK_PREFIX)
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            match self.release(project_id).await {
                Ok(true) => result.removed += 1,
                Ok(false) if attached.contains(name) => {}
                Ok(false) => match self.connect_router(name).await {
                    Ok(true) => result.router_attached += 1,
                    Ok(false) => {}
                    Err(error) => warn!(network = %name, %error, "router reattachment failed"),
                },
                Err(error) => warn!(network = %name, %error, "project network cleanup failed"),
            }
        }
        if result != NetworkReconciliation::default() {
            info!(
                removed = result.removed,
                router_attached = result.router_attached,
                "project networks reconciled"
            );
        }
        Ok(result)
    }

    /// Returns `false` when the network does not exist.
    async fn connect_router(&self, network: &str) -> Result<bool, RuntimeError> {
        let connect = CommandSpec::new("docker")
            .arg("network")
            .arg("connect")
            .arg(network)
            .arg(&self.router);
        let connected = self.docker(&connect).await?;
        if connected.success || connected.stderr.contains("already exists in network") {
            return Ok(true);
        }
        if network_missing(&connected.stderr) {
            return Ok(false);
        }
        warn!(
            network,
            detail = %first_line(&connected.stderr),
            "the router could not join a project network"
        );
        Err(RuntimeError::Dependency(format!(
            "the runtime router could not join the project network: {}",
            crate::error::describe_exit(connected.code)
        )))
    }

    /// Empty when the router cannot be inspected, which makes reconciliation
    /// try to connect it everywhere, harmlessly.
    async fn router_networks(&self) -> HashSet<String> {
        let inspect = CommandSpec::new("docker")
            .arg("container")
            .arg("inspect")
            .arg("--format")
            .arg(ROUTER_NETWORKS_FORMAT)
            .arg(&self.router);
        match self.docker(&inspect).await {
            Ok(output) if output.success => output
                .stdout
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => HashSet::new(),
        }
    }

    async fn docker(&self, command: &CommandSpec) -> Result<ProcessOutput, RuntimeError> {
        self.runner.run(command, &NullOutputSink).await
    }
}

/// Not fatal, like `caddy-http-probe`: workloads already running still need the
/// Agent for stop, sleep, and recovery, while every new deployment would fail
/// with a message naming this cause.
pub(crate) async fn preflight_check(
    runner: &dyn ProcessRunner,
    max_active_containers: u32,
) -> RuntimePreflightCheck {
    let check = |ready: bool, detail: String| RuntimePreflightCheck {
        name: "project-networks".to_owned(),
        fatal: false,
        ready,
        detail,
    };
    let create = CommandSpec::new("docker")
        .arg("network")
        .arg("create")
        .arg("--driver")
        .arg("bridge")
        .arg("--label")
        .arg(MANAGED_LABEL)
        .arg("--label")
        .arg(PREFLIGHT_KIND_LABEL)
        .arg(PREFLIGHT_NETWORK);
    match runner.run(&create, &NullOutputSink).await {
        Ok(output) if output.success || output.stderr.contains("already exists") => {
            let remove = CommandSpec::new("docker")
                .arg("network")
                .arg("rm")
                .arg(PREFLIGHT_NETWORK);
            let _ = runner.run(&remove, &NullOutputSink).await;
        }
        Ok(output) => {
            return check(
                false,
                format!(
                    "Docker cannot create a bridge network, so no project can be deployed: {}. \
                     Each project needs its own network; see {DOCS}.",
                    first_line(&output.stderr)
                ),
            );
        }
        Err(error) => {
            return check(
                false,
                format!("could not try to create a bridge network: {error}"),
            );
        }
    }

    let pools = CommandSpec::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{json .DefaultAddressPools}}");
    let Some(capacity) = runner
        .run(&pools, &NullOutputSink)
        .await
        .ok()
        .filter(|output| output.success)
        .and_then(|output| pool_capacity(&output.stdout))
    else {
        return check(
            true,
            format!("Docker's address pools could not be read; see {DOCS}"),
        );
    };
    let existing = CommandSpec::new("docker")
        .arg("network")
        .arg("ls")
        .arg("--quiet")
        .arg("--filter")
        .arg("driver=bridge");
    let existing = runner
        .run(&existing, &NullOutputSink)
        .await
        .ok()
        .filter(|output| output.success)
        .map_or(0, |output| {
            output
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count() as u64
        });
    let free = capacity.networks.saturating_sub(existing);
    let source = if capacity.docker_defaults {
        "Docker's default address pools"
    } else {
        "default-address-pools"
    };
    if free < u64::from(max_active_containers) {
        return check(
            false,
            format!(
                "Docker can create only about {free} more bridge networks ({} in {source}, \
                 {existing} exist), fewer than SAKALA_MAX_ACTIVE_CONTAINERS ({max_active_containers}). \
                 Each project needs its own network, so deployments will fail once they run out. \
                 Set default-address-pools in Docker's daemon.json; see {DOCS}.",
                capacity.networks
            ),
        );
    }
    check(
        true,
        format!(
            "about {free} more bridge networks available ({} in {source})",
            capacity.networks
        ),
    )
}

#[derive(Debug, PartialEq, Eq)]
struct PoolCapacity {
    networks: u64,
    docker_defaults: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AddressPool {
    base: String,
    size: u32,
}

/// Reads `docker info`'s `DefaultAddressPools`, which is `null` when the daemon
/// uses its built-in pools. IPv6 pools are left out: Sakala networks are IPv4.
fn pool_capacity(stdout: &str) -> Option<PoolCapacity> {
    let pools: Option<Vec<AddressPool>> = serde_json::from_str(stdout.trim()).ok()?;
    let Some(pools) = pools.filter(|pools| !pools.is_empty()) else {
        return Some(PoolCapacity {
            networks: DOCKER_DEFAULT_POOL_NETWORKS,
            docker_defaults: true,
        });
    };
    let networks = pools
        .iter()
        .filter(|pool| !pool.base.contains(':'))
        .filter_map(|pool| {
            let prefix = pool.base.split_once('/')?.1.parse::<u32>().ok()?;
            let bits = pool.size.checked_sub(prefix)?.min(32);
            Some(1_u64 << bits)
        })
        .sum();
    Some(PoolCapacity {
        networks,
        docker_defaults: false,
    })
}

fn creation_error(output: &ProcessOutput) -> RuntimeError {
    let stderr = output.stderr.to_ascii_lowercase();
    if stderr.contains("address pool") || stderr.contains("fully subnetted") {
        RuntimeError::Capacity(
            "Docker has no free address range left for another project network".to_owned(),
        )
    } else {
        RuntimeError::Container(format!(
            "project network creation failed with {}",
            crate::error::describe_exit(output.code)
        ))
    }
}

fn network_missing(stderr: &str) -> bool {
    stderr.contains("No such network")
        || (stderr.contains("network") && stderr.contains("not found"))
}

/// Docker's own message, for the operator log only.
fn first_line(stderr: &str) -> &str {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no error output")
}

#[cfg(test)]
mod tests {
    use super::{PoolCapacity, pool_capacity, project_network_name};
    use uuid::Uuid;

    #[test]
    fn a_project_network_is_named_after_the_project() {
        let project = Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("UUID");
        assert_eq!(
            project_network_name(project),
            "sakala-project-ff66ed4a-6303-4be6-8ef4-63c28b112680"
        );
    }

    #[test]
    fn docker_defaults_hold_about_thirty_networks() {
        let defaults = PoolCapacity {
            networks: 31,
            docker_defaults: true,
        };
        assert_eq!(pool_capacity("null\n"), Some(defaults));
        assert_eq!(
            pool_capacity("[]"),
            Some(PoolCapacity {
                networks: 31,
                docker_defaults: true
            })
        );
    }

    #[test]
    fn configured_pools_count_their_ipv4_subnets() {
        assert_eq!(
            pool_capacity(
                r#"[{"Base":"172.16.0.0/12","Size":24},{"Base":"10.250.0.0/16","Size":28},{"Base":"fd00::/48","Size":64}]"#
            ),
            Some(PoolCapacity {
                networks: 4_096 + 4_096,
                docker_defaults: false
            })
        );
    }

    #[test]
    fn unreadable_pools_are_unknown() {
        assert_eq!(pool_capacity(""), None);
        assert_eq!(pool_capacity("{{json"), None);
    }
}
