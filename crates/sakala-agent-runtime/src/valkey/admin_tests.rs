//! The adapter against a simulated Docker: every call goes through a fake
//! [`ProcessRunner`] that keeps networks, volumes, and containers, and answers
//! `valkey-cli` like Valkey 9.2.0 did in the smoke test.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex as StdMutex,
};

use sakala_agent_protocol::{ValkeyPassword, valkey::error_codes};
use tempfile::TempDir;

use super::*;
use crate::{ProcessOutputSink, RuntimeError};

const AGENT: &str = "runtime-01";
const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3Jh";
const ROTATED: &str = "cm90YXRlZHBhc3N3b3JkdmFsdWUxMjM0";
const IMAGE: &str = "valkey/valkey:9.2.0-alpine@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882";

#[derive(Clone, Debug)]
struct Network {
    kind: String,
    agent: String,
    subnet: String,
}

#[derive(Clone, Debug)]
struct Container {
    status: String,
    image: String,
    memory_mb: u64,
}

/// Docker and the stores as the fake sees them, plus how the next calls
/// should fail.
#[derive(Default)]
struct Docker {
    networks: BTreeMap<String, Network>,
    volumes: BTreeSet<String>,
    containers: BTreeMap<String, Container>,
    /// Containers other than the store on a network, with their state.
    network_users: BTreeMap<String, Vec<(String, String)>>,
    used_memory: u64,
    /// `docker update` fails, as when the runtime cannot change a limit.
    update_fails: bool,
    /// The daemon does not answer.
    down: bool,
}

#[derive(Clone, Debug)]
struct Call {
    args: Vec<String>,
    secret_env: BTreeMap<String, String>,
}

impl Call {
    /// The command words, such as `network create`, `run`, or `cli ACL LOAD`.
    fn summary(&self) -> String {
        if self.args.first().map(String::as_str) == Some("exec") {
            let at = self
                .args
                .iter()
                .position(|arg| arg == ADMIN_USER)
                .expect("admin user");
            return format!(
                "cli {}",
                self.args[at + 1..]
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        match self.args.first().map(String::as_str) {
            Some("network" | "volume") => self.args[..2].join(" "),
            Some(word) => word.to_owned(),
            None => String::new(),
        }
    }

    fn value_of(&self, flag: &str) -> Option<&str> {
        let at = self.args.iter().position(|arg| arg == flag)?;
        self.args.get(at + 1).map(String::as_str)
    }

    fn values_of(&self, flag: &str) -> Vec<&str> {
        self.args
            .windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .collect()
    }
}

struct FakeDocker {
    docker: StdMutex<Docker>,
    calls: StdMutex<Vec<Call>>,
}

impl FakeDocker {
    fn new(docker: Docker) -> Arc<Self> {
        Arc::new(Self {
            docker: StdMutex::new(docker),
            calls: StdMutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    fn summaries(&self) -> Vec<String> {
        self.calls().iter().map(Call::summary).collect()
    }

    fn clear(&self) {
        self.calls.lock().expect("calls").clear();
    }

    fn with<T>(&self, change: impl FnOnce(&mut Docker) -> T) -> T {
        change(&mut self.docker.lock().expect("docker"))
    }
}

fn ok(stdout: impl Into<String>) -> ProcessOutput {
    ProcessOutput {
        success: true,
        code: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

fn err(code: i32, stderr: impl Into<String>) -> ProcessOutput {
    ProcessOutput {
        success: false,
        code: Some(code),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

#[async_trait]
impl ProcessRunner for FakeDocker {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        assert_eq!(spec.program, "docker");
        let call = Call {
            args: spec
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            secret_env: spec
                .secret_environment_names()
                .map(|name| {
                    (
                        name.to_owned(),
                        spec.secret_environment_value(name)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                })
                .collect(),
        };
        self.calls.lock().expect("calls").push(call.clone());
        let mut docker = self.docker.lock().expect("docker");
        if docker.down {
            return Ok(err(
                1,
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock",
            ));
        }
        let args = call.args.iter().map(String::as_str).collect::<Vec<_>>();
        let last = *args.last().expect("args");
        Ok(match args.as_slice() {
            ["info", ..] => ok("null\n"),
            ["network", "ls", "--quiet"] => ok(docker
                .networks
                .keys()
                .map(|name| format!("{name}\n"))
                .collect::<String>()),
            ["network", "ls", ..] => ok(docker
                .networks
                .iter()
                .filter(|(_, network)| network.kind == "valkey" && network.agent == AGENT)
                .map(|(name, _)| format!("{name}\n"))
                .collect::<String>()),
            ["network", "inspect", "--format", format, names @ ..] => {
                if format.starts_with("{{range") {
                    ok(names
                        .iter()
                        .filter_map(|name| docker.networks.get(*name))
                        .map(|network| format!("{} \n", network.subnet))
                        .collect::<String>())
                } else {
                    match docker.networks.get(last) {
                        Some(network) => ok(format!(
                            "{}\t{}\t{} \n",
                            network.kind, network.agent, network.subnet
                        )),
                        None => err(
                            1,
                            format!("Error response from daemon: network {last} not found"),
                        ),
                    }
                }
            }
            ["network", "create", ..] => {
                let labels = call.values_of("--label");
                let label = |key: &str| {
                    labels
                        .iter()
                        .find_map(|label| label.strip_prefix(&format!("{key}=")))
                        .unwrap_or_default()
                        .to_owned()
                };
                if docker.networks.contains_key(last) {
                    err(
                        1,
                        format!(
                            "Error response from daemon: network with name {last} already exists"
                        ),
                    )
                } else {
                    docker.networks.insert(
                        last.to_owned(),
                        Network {
                            kind: label("dev.sakala.network-kind"),
                            agent: label("dev.sakala.agent-id"),
                            subnet: call.value_of("--subnet").expect("subnet").to_owned(),
                        },
                    );
                    ok(format!("{last}\n"))
                }
            }
            ["network", "rm", name] => {
                if docker.containers.contains_key(*name) {
                    err(
                        1,
                        format!(
                            "Error response from daemon: error while removing network: network {name} has active endpoints"
                        ),
                    )
                } else if docker.networks.remove(*name).is_some() {
                    ok(format!("{name}\n"))
                } else {
                    err(
                        1,
                        format!("Error response from daemon: network {name} not found"),
                    )
                }
            }
            ["network", "disconnect", "--force", network, user] => {
                if let Some(users) = docker.network_users.get_mut(*network) {
                    users.retain(|(name, _)| name != user);
                }
                ok("")
            }
            ["volume", "create", ..] => {
                docker.volumes.insert(last.to_owned());
                ok(format!("{last}\n"))
            }
            ["volume", "inspect", ..] => {
                if docker.volumes.contains(last) {
                    ok(format!("{last}\n"))
                } else {
                    err(
                        1,
                        format!("Error response from daemon: get {last}: no such volume"),
                    )
                }
            }
            ["volume", "rm", name] => {
                docker.volumes.remove(*name);
                ok(format!("{name}\n"))
            }
            ["volume", "ls", ..] => ok(docker
                .volumes
                .iter()
                .map(|name| format!("{name}\n"))
                .collect::<String>()),
            ["inspect", "--type", "container", "--format", _, names @ ..] => {
                let mut stdout = String::new();
                let mut missing = Vec::new();
                for name in names {
                    match docker.containers.get(*name) {
                        Some(container) => stdout.push_str(&format!(
                            "/{name}\t{}\t0\t{}\t{}\n",
                            container.status,
                            container.image,
                            container.memory_mb * MIB
                        )),
                        None => missing.push(format!("Error: No such object: {name}")),
                    }
                }
                ProcessOutput {
                    success: missing.is_empty(),
                    code: Some(if missing.is_empty() { 0 } else { 1 }),
                    stdout,
                    stderr: missing.join("\n"),
                }
            }
            ["run", ..] => {
                let name = call.value_of("--name").expect("name").to_owned();
                let memory_mb = call
                    .value_of("--memory")
                    .and_then(|memory| memory.trim_end_matches('m').parse().ok())
                    .expect("memory");
                let image = args[args.len() - 3].to_owned();
                docker.containers.insert(
                    name,
                    Container {
                        status: "running".to_owned(),
                        image,
                        memory_mb,
                    },
                );
                ok("0123456789ab\n")
            }
            ["rm", "--force", name] => {
                docker.containers.remove(*name);
                ok(format!("{name}\n"))
            }
            ["start", name] => {
                if let Some(container) = docker.containers.get_mut(*name) {
                    container.status = "running".to_owned();
                }
                ok(format!("{name}\n"))
            }
            ["update", "--memory", memory, ..] => {
                if docker.update_fails {
                    err(
                        1,
                        "Error response from daemon: Cannot update container: runtime does not support update",
                    )
                } else {
                    if let Some(container) = docker.containers.get_mut(last) {
                        container.memory_mb = memory.trim_end_matches('m').parse().expect("memory");
                    }
                    ok(format!("{last}\n"))
                }
            }
            ["ps", "--all", "--filter", filter, "--format", _]
                if filter.starts_with("network=") =>
            {
                let network = filter.trim_start_matches("network=");
                let mut stdout = format!("{network}\trunning\n");
                for (user, state) in docker.network_users.get(network).into_iter().flatten() {
                    stdout.push_str(&format!("{user}\t{state}\n"));
                }
                ok(stdout)
            }
            ["ps", "--all", "--quiet", "--filter", filter] => {
                let network = filter.trim_start_matches("network=");
                ok(docker
                    .network_users
                    .get(network)
                    .into_iter()
                    .flatten()
                    .map(|(user, _)| format!("{user}\n"))
                    .collect::<String>())
            }
            ["ps", "--all", ..] => ok(docker
                .containers
                .keys()
                .map(|name| format!("{name}\n"))
                .collect::<String>()),
            ["exec", "-e", "VALKEYCLI_AUTH", name, ..] => {
                if docker
                    .containers
                    .get(*name)
                    .is_none_or(|container| container.status != "running")
                {
                    return Ok(err(
                        1,
                        format!("Error response from daemon: container {name} is not running"),
                    ));
                }
                let at = args
                    .iter()
                    .position(|arg| *arg == ADMIN_USER)
                    .expect("user");
                match &args[at + 1..] {
                    ["PING"] => ok("PONG\n"),
                    ["ACL", "LOAD"] | ["CONFIG", "SET", ..] => ok("OK\n"),
                    ["CLIENT", "KILL", "USER", "default"] => ok("2\n"),
                    ["INFO", ..] => ok(format!(
                        "# Memory\r\nused_memory:{}\r\nmaxmemory:67108864\r\n# Stats\r\nevicted_keys:0\r\n# Clients\r\nconnected_clients:1\r\n# Errorstats\r\n# Persistence\r\naof_enabled:1\r\naof_last_write_status:ok\r\n",
                        docker.used_memory
                    )),
                    other => panic!("unexpected valkey-cli call {other:?}"),
                }
            }
            other => panic!("unexpected docker call {other:?}"),
        })
    }
}

struct Node {
    admin: DockerValkeyAdmin,
    docker: Arc<FakeDocker>,
    _temp: TempDir,
}

fn node_with(docker: Docker, total_memory_mb: u64, pool: &str) -> Node {
    let temp = TempDir::new().expect("temp");
    let docker = FakeDocker::new(docker);
    let admin = DockerValkeyAdmin::with_runner(
        docker.clone(),
        ValkeyAdminConfig {
            agent_id: AGENT.to_owned(),
            image: IMAGE.to_owned(),
            data_dir: temp.path().join("valkey"),
            network_pool: pool.parse().expect("pool"),
            total_memory_mb,
            container_runtime: Some("runsc".to_owned()),
            command_timeout: Duration::from_secs(900),
        },
    );
    Node {
        admin,
        docker,
        _temp: temp,
    }
}

fn node() -> Node {
    node_with(Docker::default(), 384, "10.213.0.0/16")
}

fn store(id: u128) -> Uuid {
    Uuid::from_u128(id)
}

fn create(id: u128, mode: ValkeyMode, maxmemory_mb: u64) -> CreateValkeyPayload {
    CreateValkeyPayload {
        valkey_id: store(id),
        mode,
        maxmemory_mb: ValkeyMemoryMb::new(maxmemory_mb).expect("size"),
        password: ValkeyPassword::parse(PASSWORD).expect("password"),
    }
}

fn target(id: u128) -> ValkeyTargetPayload {
    ValkeyTargetPayload {
        valkey_id: store(id),
    }
}

#[tokio::test]
async fn a_store_runs_hardened_on_its_own_network_at_a_fixed_address() {
    let node = node();
    let result = node
        .admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");

    let name = valkey_resource_name(store(1));
    assert!(result.created);
    assert!(!result.locked);
    assert_eq!(result.network, name);
    assert_eq!(result.endpoint.host.to_string(), "10.213.0.2");
    assert_eq!(result.endpoint.port, 6379);
    assert_eq!(result.memory_limit_mb, 192);

    let calls = node.docker.calls();
    let network = calls
        .iter()
        .find(|call| call.summary() == "network create")
        .expect("network create");
    assert!(network.args.iter().any(|arg| arg == "--internal"));
    assert_eq!(network.value_of("--subnet"), Some("10.213.0.0/27"));
    assert_eq!(network.value_of("--driver"), Some("bridge"));
    for label in [
        "dev.sakala.network-kind=valkey",
        &format!("dev.sakala.valkey-id={}", store(1)),
        &format!("dev.sakala.agent-id={AGENT}"),
    ] {
        assert!(network.values_of("--label").contains(&label), "{label}");
    }

    let run = calls
        .iter()
        .find(|call| call.summary() == "run")
        .expect("docker run");
    assert_eq!(run.value_of("--name"), Some(name.as_str()));
    assert_eq!(run.value_of("--network"), Some(name.as_str()));
    assert_eq!(run.values_of("--network").len(), 1);
    assert_eq!(run.value_of("--ip"), Some("10.213.0.2"));
    assert_eq!(run.value_of("--runtime"), Some("runsc"));
    assert_eq!(run.value_of("--restart"), Some("unless-stopped"));
    assert!(run.args.iter().any(|arg| arg == "--read-only"));
    assert_eq!(
        run.value_of("--tmpfs"),
        Some("/tmp:rw,noexec,nosuid,size=16m")
    );
    assert_eq!(
        run.value_of("--security-opt"),
        Some("no-new-privileges:true")
    );
    assert_eq!(run.value_of("--cap-drop"), Some("ALL"));
    assert!(
        run.value_of("--cap-add").is_none(),
        "a store keeps no capability"
    );
    assert_eq!(run.value_of("--user"), Some("999:1000"));
    assert_eq!(run.value_of("--memory"), Some("192m"));
    assert_eq!(run.value_of("--memory-swap"), Some("192m"));
    assert_eq!(run.value_of("--cpus"), Some("0.25"));
    assert_eq!(run.value_of("--pids-limit"), Some("64"));
    let mounts = run.values_of("--mount");
    assert!(mounts[0].starts_with("type=bind,source="));
    assert!(mounts[0].ends_with(&format!("/{}/conf,target=/etc/valkey,readonly", store(1))));
    assert_eq!(mounts[1], format!("type=volume,source={name},target=/data"));
    assert!(!run.args.iter().any(|arg| arg.contains("dev.sakala.role")));
    assert!(
        !run.args
            .iter()
            .any(|arg| arg.contains("dev.sakala.managed"))
    );
    assert_eq!(
        &run.args[run.args.len() - 3..],
        [IMAGE, "valkey-server", "/etc/valkey/valkey.conf"]
    );

    // The tenant's password never reaches a process, only the admin's does,
    // and only through the environment.
    for call in &calls {
        assert!(!call.args.iter().any(|arg| arg.contains(PASSWORD)));
        assert!(
            !call
                .secret_env
                .values()
                .any(|value| value.contains(PASSWORD))
        );
    }
    let ping = calls
        .iter()
        .find(|call| call.summary() == "cli PING")
        .expect("ping");
    assert_eq!(
        ping.secret_env.keys().collect::<Vec<_>>(),
        ["VALKEYCLI_AUTH"]
    );
    assert_eq!(ping.secret_env["VALKEYCLI_AUTH"].len(), 64);
    assert!(
        !ping
            .args
            .iter()
            .any(|arg| arg == &ping.secret_env["VALKEYCLI_AUTH"])
    );
}

#[tokio::test]
async fn a_repeated_create_converges_without_recreating_and_keeps_a_lock() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect("create");
    node.admin.lock_valkey(&target(1)).await.expect("lock");
    node.docker.clear();

    let again = node
        .admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect("create again");

    assert!(!again.created);
    assert!(again.locked, "a repeated create never unlocks");
    assert_eq!(again.endpoint.host.to_string(), "10.213.0.2");
    let summaries = node.docker.summaries();
    assert!(!summaries.contains(&"run".to_owned()), "{summaries:?}");
    assert!(!summaries.contains(&"network create".to_owned()));
    assert!(summaries.contains(&"cli ACL LOAD".to_owned()));
    assert!(
        !summaries.contains(&"cli CLIENT KILL".to_owned()),
        "the same password closes nothing"
    );
}

#[tokio::test]
async fn stores_take_the_next_free_subnet_and_skip_ranges_docker_uses() {
    let mut docker = Docker::default();
    docker.networks.insert(
        "lan-bridge".to_owned(),
        Network {
            kind: String::new(),
            agent: String::new(),
            subnet: "10.213.0.32/27".to_owned(),
        },
    );
    let node = node_with(docker, 1_024, "10.213.0.0/24");

    let first = node
        .admin
        .create_valkey(&create(1, ValkeyMode::Cache, 32))
        .await
        .expect("first");
    let second = node
        .admin
        .create_valkey(&create(2, ValkeyMode::Persistent, 32))
        .await
        .expect("second");

    assert_eq!(first.endpoint.host.to_string(), "10.213.0.2");
    assert_eq!(second.endpoint.host.to_string(), "10.213.0.66");
}

#[tokio::test]
async fn a_store_past_the_budget_or_the_pool_is_refused_before_anything_is_created() {
    let node = node_with(Docker::default(), 384, "10.213.0.0/26");
    for id in 1..=2 {
        node.admin
            .create_valkey(&create(id, ValkeyMode::Cache, 64))
            .await
            .expect("fits");
    }
    node.docker.clear();
    let error = node
        .admin
        .create_valkey(&create(3, ValkeyMode::Cache, 128))
        .await
        .expect_err("budget");
    assert_eq!(error.code(), error_codes::VALKEY_CAPACITY_EXCEEDED);
    assert_eq!(
        error.to_string(),
        "stores would need 448 MiB (maxmemory_mb plus 64 MiB each), above SAKALA_VALKEY_TOTAL_MEMORY_MB=384"
    );
    assert!(node.docker.calls().is_empty());

    // A /26 holds two /27 networks; the budget would fit a third store.
    let error = node
        .admin
        .create_valkey(&create(3, ValkeyMode::Cache, 64))
        .await
        .expect_err("pool");
    assert_eq!(error.code(), error_codes::VALKEY_CAPACITY_EXCEEDED);
    assert!(error.to_string().contains("no free /27"), "{error}");
    assert!(
        !node
            .docker
            .summaries()
            .contains(&"network create".to_owned()),
        "nothing was created"
    );
}

#[tokio::test]
async fn a_create_with_another_mode_or_size_is_a_conflict() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect("create");
    for payload in [
        create(1, ValkeyMode::Persistent, 64),
        create(1, ValkeyMode::Cache, 128),
    ] {
        let error = node
            .admin
            .create_valkey(&payload)
            .await
            .expect_err("conflict");
        assert_eq!(error.code(), error_codes::VALKEY_CONFLICT);
    }
}

#[tokio::test]
async fn another_agents_network_of_the_same_name_is_never_taken_over() {
    let mut docker = Docker::default();
    docker.networks.insert(
        valkey_resource_name(store(1)),
        Network {
            kind: "valkey".to_owned(),
            agent: "another-agent".to_owned(),
            subnet: "10.213.0.0/27".to_owned(),
        },
    );
    let node = node_with(docker, 384, "10.213.0.0/16");
    let error = node
        .admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect_err("conflict");
    assert_eq!(error.code(), error_codes::VALKEY_CONFLICT);
    assert!(!node.docker.summaries().contains(&"run".to_owned()));
    let error = node
        .admin
        .delete_valkey(&target(1))
        .await
        .expect_err("conflict");
    assert_eq!(error.code(), error_codes::VALKEY_CONFLICT);
}

#[tokio::test]
async fn rotation_and_lock_reload_the_acl_and_close_the_tenants_connections() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    node.docker.clear();

    let rotated = node
        .admin
        .rotate_valkey_password(&RotateValkeyPasswordPayload {
            valkey_id: store(1),
            password: ValkeyPassword::parse(ROTATED).expect("password"),
        })
        .await
        .expect("rotate");
    assert_eq!(rotated.disconnected_clients, 2);
    assert_eq!(
        node.docker.summaries(),
        ["inspect", "cli ACL LOAD", "cli CLIENT KILL"]
    );
    let record = node
        .admin
        .files
        .read_record(store(1))
        .await
        .expect("read")
        .expect("record");
    assert_eq!(record.password_sha256, sha256_hex(ROTATED));

    node.docker.clear();
    let locked = node.admin.lock_valkey(&target(1)).await.expect("lock");
    assert!(locked.locked);
    assert_eq!(locked.disconnected_clients, 2);
    let unlocked = node.admin.unlock_valkey(&target(1)).await.expect("unlock");
    assert!(!unlocked.locked);
    assert_eq!(unlocked.disconnected_clients, 0);
    assert_eq!(
        node.docker.summaries(),
        [
            "inspect",
            "cli ACL LOAD",
            "cli CLIENT KILL",
            "inspect",
            "cli ACL LOAD"
        ]
    );

    for error in [
        node.admin
            .lock_valkey(&target(9))
            .await
            .expect_err("missing"),
        node.admin
            .resize_valkey(&ResizeValkeyPayload {
                valkey_id: store(9),
                maxmemory_mb: ValkeyMemoryMb::new(32).expect("size"),
            })
            .await
            .expect_err("missing"),
    ] {
        assert_eq!(error.code(), error_codes::VALKEY_NOT_FOUND);
    }
}

#[tokio::test]
async fn a_command_on_a_stopped_store_updates_its_files_and_reports_unavailable() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    node.docker.with(|docker| {
        docker
            .containers
            .get_mut(&valkey_resource_name(store(1)))
            .expect("container")
            .status = "exited".to_owned();
    });
    let error = node
        .admin
        .lock_valkey(&target(1))
        .await
        .expect_err("stopped");
    assert_eq!(error.code(), error_codes::VALKEY_UNAVAILABLE);
    assert!(
        node.admin
            .files
            .read_record(store(1))
            .await
            .expect("read")
            .expect("record")
            .locked,
        "the lock applies when the store starts"
    );
}

#[tokio::test]
async fn resize_grows_in_place_and_refuses_to_shrink_a_persistent_store_below_its_data() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    node.docker.clear();

    let grown = node
        .admin
        .resize_valkey(&ResizeValkeyPayload {
            valkey_id: store(1),
            maxmemory_mb: ValkeyMemoryMb::new(128).expect("size"),
        })
        .await
        .expect("grow");
    assert_eq!(grown.memory_limit_mb, 320);
    assert!(!grown.restarted);
    // The container's limit goes up before Valkey's.
    assert_eq!(
        node.docker.summaries(),
        ["inspect", "update", "cli CONFIG SET"]
    );
    let update = &node.docker.calls()[1];
    assert_eq!(update.value_of("--memory"), Some("320m"));
    assert_eq!(update.value_of("--memory-swap"), Some("320m"));

    node.docker.with(|docker| docker.used_memory = 40 * MIB);
    let error = node
        .admin
        .resize_valkey(&ResizeValkeyPayload {
            valkey_id: store(1),
            maxmemory_mb: ValkeyMemoryMb::new(32).expect("size"),
        })
        .await
        .expect_err("below usage");
    assert_eq!(error.code(), error_codes::VALKEY_SIZE_BELOW_USAGE);
    assert_eq!(
        error.to_string(),
        "the persistent store holds 40 MiB, more than the requested 32 MiB; nothing was changed"
    );
    assert_eq!(
        node.admin
            .files
            .read_record(store(1))
            .await
            .expect("read")
            .expect("record")
            .maxmemory_mb
            .get(),
        128
    );

    node.docker.clear();
    node.docker.with(|docker| docker.used_memory = MIB);
    node.admin
        .resize_valkey(&ResizeValkeyPayload {
            valkey_id: store(1),
            maxmemory_mb: ValkeyMemoryMb::new(32).expect("size"),
        })
        .await
        .expect("shrink");
    // Valkey's limit goes down before the container's.
    assert_eq!(
        node.docker.summaries(),
        ["inspect", "cli INFO memory", "cli CONFIG SET", "update"]
    );
}

#[tokio::test]
async fn a_runtime_that_cannot_update_in_place_gets_a_new_container() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect("create");
    node.docker.with(|docker| docker.update_fails = true);
    node.docker.clear();

    let resized = node
        .admin
        .resize_valkey(&ResizeValkeyPayload {
            valkey_id: store(1),
            maxmemory_mb: ValkeyMemoryMb::new(128).expect("size"),
        })
        .await
        .expect("resize");

    assert!(resized.restarted);
    assert_eq!(
        node.docker.summaries(),
        ["inspect", "update", "rm", "run", "cli PING"]
    );
    let run = &node.docker.calls()[3];
    assert_eq!(run.value_of("--memory"), Some("320m"));
}

#[tokio::test]
async fn a_new_image_is_applied_by_a_repeated_create() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    node.docker.with(|docker| {
        docker
            .containers
            .get_mut(&valkey_resource_name(store(1)))
            .expect("container")
            .image = "valkey/valkey:9.1.0-alpine@sha256:old".to_owned();
    });
    node.docker.clear();
    let again = node
        .admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create again");
    assert!(!again.created);
    let summaries = node.docker.summaries();
    assert!(summaries.contains(&"rm".to_owned()), "{summaries:?}");
    assert!(summaries.contains(&"run".to_owned()));
    assert!(
        !summaries.contains(&"volume rm".to_owned()),
        "the data stays"
    );
}

#[tokio::test]
async fn delete_refuses_a_running_user_disconnects_stopped_ones_and_removes_everything() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    let name = valkey_resource_name(store(1));
    node.docker.with(|docker| {
        docker.network_users.insert(
            name.clone(),
            vec![
                ("sakala-web".to_owned(), "running".to_owned()),
                ("sakala-sleeping".to_owned(), "exited".to_owned()),
            ],
        );
    });

    let error = node
        .admin
        .delete_valkey(&target(1))
        .await
        .expect_err("in use");
    assert_eq!(error.code(), error_codes::VALKEY_IN_USE);
    assert!(
        error
            .to_string()
            .starts_with("1 running container is still on the store's network: sakala-web;")
    );
    assert!(
        node.docker
            .with(|docker| docker.containers.contains_key(&name))
    );

    node.docker.with(|docker| {
        docker
            .network_users
            .get_mut(&name)
            .expect("users")
            .retain(|(user, _)| user != "sakala-web");
    });
    node.docker.clear();
    let deleted = node.admin.delete_valkey(&target(1)).await.expect("delete");
    assert!(deleted.deleted);
    let calls = node.docker.calls();
    let disconnect = calls
        .iter()
        .find(|call| call.summary() == "network disconnect")
        .expect("disconnect");
    assert_eq!(
        disconnect.args.last().map(String::as_str),
        Some("sakala-sleeping")
    );
    node.docker.with(|docker| {
        assert!(docker.containers.is_empty());
        assert!(docker.networks.is_empty());
        assert!(docker.volumes.is_empty());
    });
    assert!(!node.admin.files.exists(store(1)).await.expect("exists"));

    let again = node.admin.delete_valkey(&target(1)).await.expect("again");
    assert!(!again.deleted);
}

#[tokio::test]
async fn usage_reports_every_store_and_marks_a_missing_container() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Persistent, 64))
        .await
        .expect("create");
    node.admin
        .create_valkey(&create(2, ValkeyMode::Cache, 32))
        .await
        .expect("create");
    node.docker.with(|docker| {
        docker.used_memory = 1_234;
        docker.containers.remove(&valkey_resource_name(store(2)));
    });

    let mut usage = node.admin.valkey_usage().await.expect("usage");
    usage.sort_by_key(|store| store.valkey_id);

    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].status, "running");
    assert_eq!(usage[0].mode, ValkeyMode::Persistent);
    assert_eq!(usage[0].used_memory, Some(1_234));
    assert_eq!(usage[0].maxmemory, Some(67_108_864));
    assert_eq!(usage[0].errorstat_oom, Some(0));
    assert_eq!(usage[0].connected_clients, Some(1));
    assert_eq!(usage[0].aof_last_write_status.as_deref(), Some("ok"));
    assert_eq!(usage[1].status, "missing");
    assert_eq!(usage[1].maxmemory_mb.get(), 32);
    assert_eq!(usage[1].used_memory, None);

    node.docker.with(|docker| docker.down = true);
    let error = node.admin.valkey_usage().await.expect_err("down");
    assert_eq!(error.code(), error_codes::VALKEY_UNAVAILABLE);
}

#[tokio::test]
async fn a_usage_read_is_skipped_while_a_command_holds_the_stores() {
    let node = node();
    let _held = node.admin.operations.lock().await;
    tokio::time::pause();
    let error = node.admin.valkey_usage().await.expect_err("busy");
    assert_eq!(error.code(), VALKEY_USAGE_BUSY);
}

#[tokio::test]
async fn reconcile_removes_only_unused_store_networks_without_a_store() {
    let node = node();
    node.admin
        .create_valkey(&create(1, ValkeyMode::Cache, 64))
        .await
        .expect("create");
    let orphan = valkey_resource_name(store(5));
    let busy_orphan = valkey_resource_name(store(6));
    node.docker.with(|docker| {
        for (name, subnet) in [(&orphan, "10.213.1.0/27"), (&busy_orphan, "10.213.1.32/27")] {
            docker.networks.insert(
                name.clone(),
                Network {
                    kind: "valkey".to_owned(),
                    agent: AGENT.to_owned(),
                    subnet: subnet.to_owned(),
                },
            );
        }
        docker.network_users.insert(
            busy_orphan.clone(),
            vec![("sakala-web".to_owned(), "running".to_owned())],
        );
        docker.volumes.insert(valkey_resource_name(store(7)));
    });

    node.admin.reconcile().await.expect("reconcile");

    node.docker.with(|docker| {
        assert!(!docker.networks.contains_key(&orphan));
        assert!(docker.networks.contains_key(&busy_orphan));
        assert!(
            docker
                .networks
                .contains_key(&valkey_resource_name(store(1)))
        );
        assert!(
            docker.volumes.contains(&valkey_resource_name(store(7))),
            "data is never removed by the reconcile"
        );
    });
}

#[tokio::test]
async fn check_reports_docker_that_does_not_answer() {
    let node = node();
    node.admin.check().await.expect("check");
    node.docker.with(|docker| docker.down = true);
    let error = node.admin.check().await.expect_err("down");
    assert_eq!(error.code(), error_codes::VALKEY_UNAVAILABLE);
}
