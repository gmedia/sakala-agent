//! Proves the managed Valkey role (ADR-023) against a real Docker and the
//! pinned `valkey/valkey` image, through the core dispatcher, the executor's
//! own deploy and job code, and the heartbeat read: a cache store and a
//! persistent store start on internal networks at fixed addresses; a web
//! replica and a job created with the store network reach their store with
//! the password and nothing else does; rotation closes connections that were
//! logged in; a lock refuses the tenant and keeps the data, and a repeated
//! create keeps the lock; a persistent store keeps its data across a restart
//! and a cache store does not; a resize changes both limits in place, and a
//! persistent store refuses a size below its data; a store that a running
//! container still uses is not deleted, and deleting one that a sleeping
//! project used removes everything and lets the project wake.
//!
//! No checkout and no build: the app image is a labelled copy of
//! `busybox:1.37`, whose `nc` speaks to Valkey in inline commands. Opt-in,
//! because it needs a daemon plus `caddy:2.11.3-alpine`, `busybox:1.37`, and
//! the pinned Valkey image (pulled when missing):
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test valkey_smoke_test -- --ignored --nocapture
//! ```
//!
//! It runs on Docker's default runtime (runc). The gVisor runtime of the
//! hosted nodes (`SAKALA_CONTAINER_RUNTIME=runsc`) is not exercised here;
//! spike S1 proved the network shape under runsc on staging.
//!
//! Everything it creates starts with `sakala-smvk-`, carries the fresh project
//! UUID, or is named after one of its fresh store UUIDs, and it removes only
//! those.

use std::{
    path::PathBuf,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_core::{
    commands::CommandDispatcher,
    ports::{
        CommandOutput, DeployProjectRequest, RunJobRequest, RuntimeExecutionError, RuntimeExecutor,
        RuntimeReporter, ValkeyAdmin, WorkloadLifecycleRequest,
    },
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, DeploymentEvent, DeploymentLog, SleepingPage,
    ValkeyMode, valkey_resource_name,
};
use sakala_agent_runtime::{
    DockerRuntimeConfig, DockerRuntimeExecutor, NoopRuntimeExecutor, TokioProcessRunner,
    containers::{container_name, image_name, project_network_name},
    valkey::{DEFAULT_VALKEY_IMAGE, DockerValkeyAdmin, ValkeyAdminConfig},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PREFIX: &str = "sakala-smvk-";
const ROUTER: &str = "sakala-smvk-caddy";
const RUNTIME_NETWORK: &str = "sakala-smvk-runtime";
const STRANGER_NETWORK: &str = "sakala-smvk-stranger";
const ROUTER_IMAGE: &str = "caddy:2.11.3-alpine";
const APP_IMAGE: &str = "busybox:1.37";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const DOMAIN: &str = "portfolio.run.sakala.localhost";
const PORT: u16 = 3000;
const WEB_COMMAND: &str = "mkdir -p /www; echo web > /www/index.html; trap 'exit 0' TERM; httpd -f -p \"$PORT\" -h /www & wait";
/// The agent ID of `DockerRuntimeConfig::default()`, which the store
/// networks must carry for the executor to join them.
const AGENT_ID: &str = "local-agent-01";
/// A /24 far from Docker's own pools; the role skips any /27 in use.
const POOL: &str = "10.247.0.0/24";
/// Writes about 30 MB in one script: more than 16 MiB, less than 64.
const FILL_SCRIPT: &str =
    "for i=1,200000 do redis.call('SET','fill:'..i,string.rep('x',100)) end return 1";

const ROUTER_CADDYFILE: &str = "{
\tadmin localhost:2019
\tauto_https off
}

import /etc/caddy/sites/*.Caddyfile

:80 {
\trespond \"Not found\" 404
}
";

fn docker(args: &[&str]) -> (bool, String) {
    let output = Command::new("timeout")
        .arg("120")
        .arg("docker")
        .args(args)
        .output()
        .expect("docker should be installed");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// Names the step only: the arguments can hold a store password.
fn must(step: &str, args: &[&str]) -> String {
    let (ok, text) = docker(args);
    assert!(ok, "docker step `{step}` failed");
    text
}

fn exists(kind: &str, name: &str) -> bool {
    docker(&[kind, "inspect", name]).0
}

fn status_of(name: &str) -> Option<String> {
    let (exists, text) = docker(&["inspect", "--format", "{{.State.Status}}", name]);
    exists.then(|| text.trim().to_owned())
}

fn networks_of(name: &str) -> Vec<String> {
    let mut networks = must(
        "networks",
        &[
            "inspect",
            "--format",
            "{{range $name, $_ := .NetworkSettings.Networks}}{{$name}}\n{{end}}",
            name,
        ],
    )
    .lines()
    .filter(|line| !line.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    networks.sort();
    networks
}

/// Sends `commands` to `host:6379` from `container` as inline commands, after
/// `AUTH <password>` when one is given, and returns what Valkey answered.
fn speak(container: &str, host: &str, password: Option<&str>, commands: &[&str]) -> String {
    let mut script = String::new();
    if password.is_some() {
        script.push_str("printf 'AUTH %s\\r\\n' \"$SMOKE_PASSWORD\"; ");
    }
    for command in commands {
        script.push_str(&format!("printf '%s\\r\\n' '{command}'; "));
    }
    script.push_str("printf 'QUIT\\r\\n'");
    let output = Command::new("timeout")
        .arg("30")
        .arg("docker")
        .arg("exec")
        .arg("-e")
        .arg("SMOKE_PASSWORD")
        .arg(container)
        .arg("sh")
        .arg("-c")
        .arg(format!("({script}) | nc -w 3 {host} 6379"))
        .env("SMOKE_PASSWORD", password.unwrap_or_default())
        .output()
        .expect("docker should be installed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A fresh store password for one run: 32 hex characters, which the
/// password rule accepts.
fn random_password() -> String {
    Uuid::new_v4().simple().to_string()
}

/// The store's own log, which holds no password, for a failed create.
fn store_log(id: Uuid) -> String {
    docker(&["logs", "--tail", "20", &valkey_resource_name(id)]).1
}

struct Store {
    id: Uuid,
    host: String,
    data_dir: PathBuf,
}

impl Store {
    fn name(&self) -> String {
        valkey_resource_name(self.id)
    }

    /// `valkey-cli` inside the store, as the agent's admin user.
    fn admin(&self, args: &[&str]) -> String {
        let secret =
            std::fs::read_to_string(self.data_dir.join(self.id.to_string()).join("admin.secret"))
                .expect("admin secret");
        let output = Command::new("timeout")
            .arg("30")
            .arg("docker")
            .args(["exec", "-e", "VALKEYCLI_AUTH", &self.name()])
            .args(["valkey-cli", "--no-auth-warning", "--user", "sakala-admin"])
            .args(args)
            .env("VALKEYCLI_AUTH", secret.trim())
            .output()
            .expect("docker should be installed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// `valkey-cli` inside the store, as the tenant.
    fn tenant(&self, password: &str, args: &[&str]) -> String {
        let output = Command::new("timeout")
            .arg("30")
            .arg("docker")
            .args(["exec", "-e", "VALKEYCLI_AUTH", &self.name()])
            .args(["valkey-cli", "--no-auth-warning"])
            .args(args)
            .env("VALKEYCLI_AUTH", password)
            .output()
            .expect("docker should be installed");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .trim()
        .to_owned()
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.admin(&["PING"]) != "PONG" {
            assert!(Instant::now() < deadline, "the store did not come back");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

struct Cleanup {
    project_id: Uuid,
    image: String,
    stores: Vec<Uuid>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (_, containers) = docker(&[
            "ps",
            "-a",
            "--filter",
            &format!("name=^/{PREFIX}"),
            "--format",
            "{{.Names}}",
        ]);
        let (_, project) = docker(&[
            "ps",
            "-a",
            "--filter",
            &format!("label=dev.sakala.project-id={}", self.project_id),
            "--format",
            "{{.Names}}",
        ]);
        for name in containers.lines().chain(project.lines()) {
            if !name.is_empty() {
                docker(&["rm", "-f", "-v", name]);
            }
        }
        for id in &self.stores {
            let name = valkey_resource_name(*id);
            docker(&["rm", "-f", &name]);
            docker(&["network", "rm", &name]);
            docker(&["volume", "rm", &name]);
        }
        docker(&["image", "rm", "-f", &self.image]);
        docker(&["network", "rm", &project_network_name(self.project_id)]);
        docker(&["network", "rm", RUNTIME_NETWORK]);
        docker(&["network", "rm", STRANGER_NETWORK]);
    }
}

struct SilentReporter;

#[async_trait]
impl RuntimeReporter for SilentReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

async fn dispatch(
    dispatcher: &CommandDispatcher,
    command_type: CommandType,
    payload: Value,
) -> Result<Value, RuntimeExecutionError> {
    dispatcher
        .dispatch(
            &AgentCommand {
                id: Uuid::new_v4(),
                command_type,
                status: CommandStatus::Claimed,
                project_id: None,
                deployment_id: None,
                payload,
            },
            Arc::new(SilentReporter),
            CancellationToken::new(),
        )
        .await
        .map(|CommandOutput { result }| result)
}

fn admin_config(data_dir: PathBuf) -> ValkeyAdminConfig {
    ValkeyAdminConfig {
        agent_id: AGENT_ID.to_owned(),
        image: DEFAULT_VALKEY_IMAGE.to_owned(),
        data_dir,
        network_pool: POOL.parse().expect("pool"),
        total_memory_mb: 384,
        container_runtime: None,
        command_timeout: Duration::from_secs(600),
    }
}

fn redis_environment(host: &str, password: &str) -> Value {
    json!({
        "REDIS_URL": format!("redis://:{password}@{host}:6379"),
        "REDIS_HOST": host,
        "REDIS_PORT": "6379",
        "REDIS_PASSWORD": password,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local Docker daemon, caddy:2.11.3-alpine, busybox:1.37, and the pinned valkey image"]
async fn real_docker_runs_managed_valkey_stores_and_attached_workloads() {
    let project_id = Uuid::new_v4();
    let image_deployment = Uuid::new_v4();
    let image = image_name(project_id, image_deployment, COMMIT);
    let a_id = Uuid::new_v4();
    let b_id = Uuid::new_v4();
    let password_a = random_password();
    let password_a_rotated = random_password();
    let password_b = random_password();
    let _cleanup = Cleanup {
        project_id,
        image: image.clone(),
        stores: vec![a_id, b_id],
    };
    let temp = TempDir::new().expect("temp directory");
    let data_dir = temp.path().join("valkey");
    let sites = temp.path().join("sites");
    std::fs::create_dir_all(&sites).expect("sites");
    let caddyfile = temp.path().join("Caddyfile");
    std::fs::write(&caddyfile, ROUTER_CADDYFILE).expect("Caddyfile");
    let workspace = temp.path().join("builds");
    std::fs::create_dir_all(&workspace).expect("workspace");

    must("runtime network", &["network", "create", RUNTIME_NETWORK]);
    must("stranger network", &["network", "create", STRANGER_NETWORK]);
    must(
        "router",
        &[
            "run",
            "-d",
            "--name",
            ROUTER,
            "--pull=never",
            "--network",
            RUNTIME_NETWORK,
            "-v",
            &format!("{}:/etc/caddy/Caddyfile:ro", caddyfile.display()),
            "-v",
            &format!("{}:/etc/caddy/sites:ro", sites.display()),
            ROUTER_IMAGE,
        ],
    );
    let builder = format!("{PREFIX}image");
    must(
        "image container",
        &["create", "--name", &builder, "--pull=never", APP_IMAGE],
    );
    must(
        "image commit",
        &[
            "commit",
            "--change",
            "LABEL dev.sakala.managed=true",
            "--change",
            &format!("LABEL dev.sakala.project-id={project_id}"),
            "--change",
            &format!("LABEL dev.sakala.deployment-id={image_deployment}"),
            "--change",
            "LABEL dev.sakala.builder=dockerfile",
            &builder,
            &image,
        ],
    );
    must("image container removal", &["rm", &builder]);
    let stranger = format!("{PREFIX}stranger");
    must(
        "stranger",
        &[
            "run",
            "-d",
            "--name",
            &stranger,
            "--network",
            STRANGER_NETWORK,
            APP_IMAGE,
            "sleep",
            "600",
        ],
    );

    let admin: Arc<dyn ValkeyAdmin> = Arc::new(DockerValkeyAdmin::with_runner(
        Arc::new(TokioProcessRunner::new(Duration::from_secs(600))),
        admin_config(data_dir.clone()),
    ));
    admin.check().await.expect("role check");
    let dispatcher =
        CommandDispatcher::new(Arc::new(NoopRuntimeExecutor)).with_valkey_admin(Arc::clone(&admin));
    let executor = Arc::new(DockerRuntimeExecutor::with_runner(
        DockerRuntimeConfig {
            caddy_container: ROUTER.to_owned(),
            caddy_sites_dir: sites.clone(),
            workspace_root: workspace,
            public_api_url: "https://api.staging.sakala.dev".to_owned(),
            ..DockerRuntimeConfig::default()
        },
        Arc::new(TokioProcessRunner::new(Duration::from_secs(60))),
    ));

    // Create: a cache store and a persistent store, each on its own internal
    // network at the second address of its /27.
    let created_a = dispatch(
        &dispatcher,
        CommandType::CreateValkey,
        json!({ "valkey_id": a_id, "mode": "cache", "maxmemory_mb": 32, "password": password_a }),
    )
    .await
    .unwrap_or_else(|error| panic!("create the cache store: {error}\n{}", store_log(a_id)));
    let created_b = dispatch(
        &dispatcher,
        CommandType::CreateValkey,
        json!({ "valkey_id": b_id, "mode": "persistent", "maxmemory_mb": 64, "password": password_b }),
    )
    .await
    .unwrap_or_else(|error| panic!("create the persistent store: {error}\n{}", store_log(b_id)));
    assert_eq!(created_a["created"], true);
    assert_eq!(created_a["memory_limit_mb"], 128);
    assert_eq!(created_b["memory_limit_mb"], 192);
    let a = Store {
        id: a_id,
        host: created_a["endpoint"]["host"]
            .as_str()
            .expect("host")
            .to_owned(),
        data_dir: data_dir.clone(),
    };
    let b = Store {
        id: b_id,
        host: created_b["endpoint"]["host"]
            .as_str()
            .expect("host")
            .to_owned(),
        data_dir: data_dir.clone(),
    };
    for store in [&a, &b] {
        let last = store
            .host
            .strip_prefix("10.247.0.")
            .and_then(|octet| octet.parse::<u8>().ok())
            .expect("an address of the pool");
        assert_eq!(last % 32, 2, "the second address of a /27: {}", store.host);
    }
    assert_ne!(a.host, b.host);
    assert_eq!(created_a["network"], a.name());
    assert_eq!(
        must(
            "store network",
            &["network", "inspect", "--format", "{{.Internal}}", &a.name()]
        )
        .trim(),
        "true"
    );
    assert_eq!(
        must(
            "store user",
            &["inspect", "--format", "{{.Config.User}} {{.HostConfig.ReadonlyRootfs}} {{.HostConfig.CapDrop}} {{.HostConfig.PidsLimit}} {{.HostConfig.NanoCpus}}", &b.name()]
        )
        .trim(),
        "999:1000 true [ALL] 64 250000000"
    );
    assert_eq!(
        a.admin(&["CONFIG", "GET", "maxmemory-policy"]),
        "maxmemory-policy\nallkeys-lru"
    );
    assert_eq!(
        b.admin(&["CONFIG", "GET", "maxmemory-policy"]),
        "maxmemory-policy\nnoeviction"
    );
    assert_eq!(b.admin(&["CONFIG", "GET", "appendonly"]), "appendonly\nyes");
    assert_eq!(a.admin(&["CONFIG", "GET", "appendonly"]), "appendonly\nno");

    let again = dispatch(
        &dispatcher,
        CommandType::CreateValkey,
        json!({ "valkey_id": a_id, "mode": "cache", "maxmemory_mb": 32, "password": password_a }),
    )
    .await
    .expect("create again");
    assert_eq!(again["created"], false);
    assert_eq!(again["endpoint"], created_a["endpoint"]);

    // Deploy: the web replica is created on the project network and the
    // store network, and reaches the store with the password.
    let deployment = Uuid::new_v4();
    let mut environment = redis_environment(&a.host, &password_a);
    environment["APP_ENV"] = json!("production");
    executor
        .deploy_project(
            DeployProjectRequest {
                command_id: Uuid::new_v4(),
                project_id,
                deployment_id: deployment,
                payload: serde_json::from_value(json!({
                    "repository_url": "https://github.com/gmedia/example-app.git",
                    "commit_sha": COMMIT,
                    "domain": DOMAIN,
                    "container_port": PORT,
                    "builder": "dockerfile",
                    "start_command": WEB_COMMAND,
                    "reuse_image_from_deployment_id": image_deployment,
                    "environment": environment,
                    "valkey": { "network": a.name(), "host": a.host },
                }))
                .expect("deploy payload"),
                repository_credential: None,
                cancellation: CancellationToken::new(),
            },
            Arc::new(SilentReporter),
        )
        .await
        .expect("deploy with a store");
    let web = container_name(project_id, deployment);
    let mut expected = vec![project_network_name(project_id), a.name()];
    expected.sort();
    assert_eq!(networks_of(&web), expected);
    let reply = speak(
        &web,
        &a.host,
        Some(password_a.as_str()),
        &["SET greeting hello", "GET greeting", "EVAL \"return 7\" 0"],
    );
    assert!(
        reply.starts_with("+OK\r\n+OK\r\n$5\r\nhello\r\n:7\r\n"),
        "{reply:?}"
    );
    assert!(
        speak(&web, &a.host, Some(random_password().as_str()), &["PING"]).contains("WRONGPASS")
    );
    assert!(speak(&web, &a.host, None, &["PING"]).contains("NOAUTH"));
    assert!(
        speak(
            &web,
            &a.host,
            Some(password_a.as_str()),
            &["CONFIG SET maxmemory 1gb"]
        )
        .contains("NOPERM")
    );
    // The other store is on another network.
    assert_eq!(
        speak(&web, &b.host, Some(password_b.as_str()), &["PING"]),
        ""
    );

    // A job copies the field and reaches the store the same way.
    let job = executor
        .run_job(
            RunJobRequest {
                command_id: Uuid::new_v4(),
                project_id,
                deployment_id: deployment,
                payload: serde_json::from_value(json!({
                    "command": "(printf 'AUTH %s\\r\\nGET greeting\\r\\nQUIT\\r\\n' \"$REDIS_PASSWORD\") | nc -w 3 \"$REDIS_HOST\" \"$REDIS_PORT\" | grep -q hello",
                    "commit_sha": COMMIT,
                    "reuse_image_from_deployment_id": image_deployment,
                    "container_port": PORT,
                    "environment": redis_environment(&a.host, &password_a),
                    "valkey": { "network": a.name(), "host": a.host },
                    "timeout_seconds": 60,
                }))
                .expect("job payload"),
                cancellation: CancellationToken::new(),
            },
            Arc::new(SilentReporter),
        )
        .await
        .expect("the job reaches the store");
    assert_eq!(job.result["exit_code"], 0);

    // Nothing else reaches a store: a container on another network, and one
    // on Docker's default bridge where builds run.
    assert_eq!(
        speak(&stranger, &a.host, Some(password_a.as_str()), &["PING"]),
        ""
    );
    let (_, bridge) = docker(&[
        "run",
        "--rm",
        APP_IMAGE,
        "sh",
        "-c",
        &format!("printf 'PING\\r\\nQUIT\\r\\n' | nc -w 3 {} 6379", a.host),
    ]);
    assert!(
        !bridge.contains("NOAUTH") && !bridge.contains("PONG"),
        "{bridge}"
    );

    // Rotate: a connection that logged in with the old password is closed.
    must(
        "held connection",
        &[
            "exec",
            "-d",
            "-e",
            &format!("SMOKE_PASSWORD={password_a}"),
            &web,
            "sh",
            "-c",
            &format!(
                "(printf 'AUTH %s\\r\\n' \"$SMOKE_PASSWORD\"; sleep 60) | nc {} 6379 > /tmp/held 2>&1",
                a.host
            ),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !a.admin(&["CLIENT", "LIST"]).contains("user=default") {
        assert!(
            Instant::now() < deadline,
            "the held connection did not log in"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let rotated = dispatch(
        &dispatcher,
        CommandType::RotateValkeyPassword,
        json!({ "valkey_id": a_id, "password": password_a_rotated }),
    )
    .await
    .expect("rotate");
    assert!(
        rotated["disconnected_clients"].as_u64() >= Some(1),
        "{rotated}"
    );
    assert!(!a.admin(&["CLIENT", "LIST"]).contains("user=default"));
    assert!(speak(&web, &a.host, Some(password_a.as_str()), &["PING"]).contains("WRONGPASS"));
    assert!(speak(&web, &a.host, Some(password_a_rotated.as_str()), &["PING"]).contains("+PONG"));

    // Lock: the tenant is refused, the data stays, and a repeated create
    // keeps the lock.
    assert_eq!(b.tenant(&password_b, &["SET", "job:1", "queued"]), "OK");
    let locked = dispatch(
        &dispatcher,
        CommandType::LockValkey,
        json!({ "valkey_id": b_id }),
    )
    .await
    .expect("lock");
    assert_eq!(locked["locked"], true);
    assert!(
        b.tenant(&password_b, &["GET", "job:1"])
            .contains("WRONGPASS")
    );
    assert_eq!(b.admin(&["GET", "job:1"]), "queued");
    let recreated = dispatch(
        &dispatcher,
        CommandType::CreateValkey,
        json!({ "valkey_id": b_id, "mode": "persistent", "maxmemory_mb": 64, "password": password_b }),
    )
    .await
    .expect("create a locked store again");
    assert_eq!(recreated["locked"], true);
    assert!(b.tenant(&password_b, &["PING"]).contains("WRONGPASS"));
    let unlocked = dispatch(
        &dispatcher,
        CommandType::UnlockValkey,
        json!({ "valkey_id": b_id }),
    )
    .await
    .expect("unlock");
    assert_eq!(unlocked["locked"], false);
    assert_eq!(b.tenant(&password_b, &["GET", "job:1"]), "queued");

    // Restart: the persistent store keeps its data, the cache store does not.
    must("restart b", &["restart", &b.name()]);
    must("restart a", &["restart", &a.name()]);
    b.wait_ready();
    a.wait_ready();
    assert_eq!(b.tenant(&password_b, &["GET", "job:1"]), "queued");
    assert_eq!(a.admin(&["EXISTS", "greeting"]), "0");
    assert!(
        b.tenant(&password_b, &["PING"]).contains("PONG"),
        "the password survives a restart"
    );

    // Resize: both limits change in place.
    let resized = dispatch(
        &dispatcher,
        CommandType::ResizeValkey,
        json!({ "valkey_id": a_id, "maxmemory_mb": 64 }),
    )
    .await
    .expect("resize");
    assert_eq!(resized["memory_limit_mb"], 192);
    assert_eq!(resized["restarted"], false);
    assert_eq!(
        a.admin(&["CONFIG", "GET", "maxmemory"]),
        "maxmemory\n67108864"
    );
    assert_eq!(
        must(
            "memory",
            &["inspect", "--format", "{{.HostConfig.Memory}}", &a.name()]
        )
        .trim(),
        (192_u64 * 1_024 * 1_024).to_string()
    );
    assert_eq!(b.tenant(&password_b, &["EVAL", FILL_SCRIPT, "0"]), "1");
    let error = dispatch(
        &dispatcher,
        CommandType::ResizeValkey,
        json!({ "valkey_id": b_id, "maxmemory_mb": 16 }),
    )
    .await
    .expect_err("below usage");
    assert_eq!(error.code(), "valkey_size_below_usage");
    assert_eq!(
        b.admin(&["CONFIG", "GET", "maxmemory"]),
        "maxmemory\n67108864"
    );

    // Heartbeat: every field of both stores, also from an agent that just
    // started.
    for reader in [
        Arc::clone(&admin),
        Arc::new(DockerValkeyAdmin::new(admin_config(data_dir.clone()))) as Arc<dyn ValkeyAdmin>,
    ] {
        let usage = reader.valkey_usage().await.expect("usage");
        let cache = usage
            .iter()
            .find(|store| store.valkey_id == a_id)
            .expect("cache store");
        let persistent = usage
            .iter()
            .find(|store| store.valkey_id == b_id)
            .expect("persistent store");
        assert_eq!(cache.mode, ValkeyMode::Cache);
        assert_eq!(cache.status, "running");
        assert_eq!(cache.maxmemory, Some(64 * 1_024 * 1_024));
        assert_eq!(cache.aof_last_write_status, None);
        assert_eq!(cache.evicted_keys, Some(0));
        assert_eq!(persistent.mode, ValkeyMode::Persistent);
        assert_eq!(persistent.aof_last_write_status.as_deref(), Some("ok"));
        assert!(
            persistent.used_memory > Some(16 * 1_024 * 1_024),
            "{persistent:?}"
        );
        assert_eq!(persistent.errorstat_oom, Some(0));
        assert!(persistent.connected_clients >= Some(1));
        assert!(!persistent.locked);
    }

    // Delete: refused while the running web replica is on the store network.
    let error = dispatch(
        &dispatcher,
        CommandType::DeleteValkey,
        json!({ "valkey_id": a_id }),
    )
    .await
    .expect_err("in use");
    assert_eq!(error.code(), "valkey_in_use");
    assert!(error.to_string().contains(&web), "{error}");
    assert_eq!(status_of(&a.name()).as_deref(), Some("running"));

    // A sleeping project's container is disconnected, so it wakes without
    // the store network.
    let lifecycle = WorkloadLifecycleRequest {
        custom_domains: Default::default(),
        command_id: Uuid::new_v4(),
        project_id,
        deployment_id: deployment,
        remove_images: false,
        sleeping_page: SleepingPage::Waking,
        cancellation: CancellationToken::new(),
    };
    executor
        .sleep_project(lifecycle.clone(), Arc::new(SilentReporter))
        .await
        .expect("sleep");
    let deleted = dispatch(
        &dispatcher,
        CommandType::DeleteValkey,
        json!({ "valkey_id": a_id }),
    )
    .await
    .expect("delete");
    assert_eq!(deleted["deleted"], true);
    assert!(!exists("container", &a.name()));
    assert!(!exists("network", &a.name()));
    assert!(!exists("volume", &a.name()));
    assert!(!data_dir.join(a_id.to_string()).exists());
    executor
        .wake_project(
            WorkloadLifecycleRequest {
                command_id: Uuid::new_v4(),
                ..lifecycle.clone()
            },
            Arc::new(SilentReporter),
        )
        .await
        .expect("wake without the store");
    assert_eq!(status_of(&web).as_deref(), Some("running"));
    assert_eq!(networks_of(&web), [project_network_name(project_id)]);
    let again = dispatch(
        &dispatcher,
        CommandType::DeleteValkey,
        json!({ "valkey_id": a_id }),
    )
    .await
    .expect("delete again");
    assert_eq!(again["deleted"], false);

    dispatch(
        &dispatcher,
        CommandType::DeleteValkey,
        json!({ "valkey_id": b_id }),
    )
    .await
    .expect("delete the persistent store");
    for name in [a.name(), b.name()] {
        assert!(!exists("container", &name));
        assert!(!exists("network", &name));
        assert!(!exists("volume", &name));
    }
    admin.reconcile().await.expect("reconcile");
    assert!(admin.valkey_usage().await.expect("usage").is_empty());
    executor
        .stop_project(
            WorkloadLifecycleRequest {
                command_id: Uuid::new_v4(),
                ..lifecycle
            },
            Arc::new(SilentReporter),
        )
        .await
        .expect("stop");
}
