//! Proves the object storage host role against a real RustFS: commands go
//! through the same dispatcher the agent uses, and tenant requests are signed
//! with the keys the control plane would inject.
//!
//! Opt-in, because it needs a Docker daemon and the RustFS image:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test object_storage_smoke_test -- --ignored --nocapture
//! ```
//!
//! `SAKALA_SMOKE_RUSTFS_IMAGE` replaces the image, which defaults to the
//! version and digest sakala-deployment pins. Run it on every RustFS bump: IAM
//! enforcement and `rc`'s output are re-proven here, nowhere else.
//!
//! It starts one container named `sakala-smoke-object-storage` the way
//! `compose.object-storage.yaml` does (read-only root, no capabilities, root key
//! from mounted files, S3 API on the container's loopback only), and removes it
//! at the end. Tenant requests run inside the container with
//! `curl --aws-sigv4`, the tenant secret passed by environment.

use std::{
    collections::BTreeSet,
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::{
    commands::CommandDispatcher,
    ports::{CommandOutput, ObjectStorageAdmin, RuntimeExecutionError, RuntimeReporter},
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, DeploymentEvent, DeploymentLog, buckets::error_codes,
};
use sakala_agent_runtime::{
    CommandSpec, NoopRuntimeExecutor, ProcessOutput, ProcessOutputSink, ProcessRunner,
    RuntimeError, TokioProcessRunner,
    object_storage::{RcObjectStorageAdmin, RustfsAdminConfig},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CONTAINER: &str = "sakala-smoke-object-storage";
const DEFAULT_IMAGE: &str =
    "rustfs/rustfs:1.0.6@sha256:ee717072290bec9b11abfc0365f8310b00df1cd9b3639943edae47bdef05044a";
const IMAGE_ENV: &str = "SAKALA_SMOKE_RUSTFS_IMAGE";
const S3_ADDRESS: &str = "127.0.0.1:9000";
const READY_ATTEMPTS: usize = 60;
const TOTAL_QUOTA_MB: u64 = 64;
const MIB: u64 = 1_024 * 1_024;
/// A bucket that predates the agent, like the staging script's `w-...` ones.
const UNMANAGED_BUCKET: &str = "w-smoke-unlimited";
/// Attempts of a write answered `503 SlowDown`. RustFS 1.0.6 usually answers
/// one write after a quota change that way, but under load more than three in
/// a row were seen, so the test retries longer than an SDK's standard mode.
const SLOWDOWN_ATTEMPTS: u32 = 5;
/// The pause before retry `n` is `n` times this, as SDKs back off.
const SLOWDOWN_BACKOFF: Duration = Duration::from_millis(500);

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

struct Container;

impl Drop for Container {
    fn drop(&mut self) {
        // The data files belong to the container's user; open them up so the
        // temporary directory can be removed.
        let _ = Command::new("docker")
            .args(["exec", CONTAINER, "chmod", "-R", "a+rwX", "/data"])
            .output();
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .output();
    }
}

/// A tenant: bucket, key, and secret, random per run.
#[derive(Clone)]
struct Tenant {
    bucket: String,
    key: String,
    secret: String,
}

impl Tenant {
    fn random() -> Self {
        let id = Uuid::new_v4().simple().to_string();
        Self {
            bucket: format!("b-smoke{}", &id[..13]),
            key: format!("SKSMOKE{}", id[13..26].to_ascii_uppercase()),
            secret: random_secret(),
        }
    }

    fn create(&self, quota_mb: u64) -> Value {
        json!({
            "bucket": self.bucket,
            "access_key_id": self.key,
            "secret_access_key": self.secret,
            "quota_mb": quota_mb,
        })
    }

    fn key_payload(&self) -> Value {
        json!({ "bucket": self.bucket, "access_key_id": self.key })
    }
}

/// 40 alphanumeric characters, like the control plane's secrets.
fn random_secret() -> String {
    let mut secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    secret.truncate(40);
    secret
}

fn docker(arguments: &[&str]) -> std::process::Output {
    Command::new("docker")
        .args(arguments)
        .output()
        .expect("docker runs")
}

/// Runs docker and returns its stdout. A failure names only `what` and the
/// exit status: arguments and stderr can carry key file paths and presigned
/// URLs, so they never reach the test output.
fn docker_ok(what: &str, arguments: &[&str]) -> String {
    let output = docker(arguments);
    assert!(
        output.status.success(),
        "docker could not {what} ({:?}); rerun the step by hand to see its output",
        output.status.code()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn start_rustfs(secrets: &Path, data: &Path) -> Container {
    let _ = docker(&["rm", "-f", CONTAINER]);
    let image = std::env::var(IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    let access_key_mount = format!(
        "{}:/run/secrets/rustfs-access-key:ro",
        secrets.join("access-key").display()
    );
    let root_key_mount = format!(
        "{}:/run/secrets/rustfs-secret-key:ro",
        secrets.join("secret-key").display()
    );
    let volume = format!("{}:/data", data.display());
    docker_ok(
        "start the RustFS container",
        &[
            "run",
            "-d",
            "--name",
            CONTAINER,
            "--read-only",
            "--tmpfs",
            "/tmp:size=64m,mode=1777",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges:true",
            "--ulimit",
            "nofile=65536:65536",
            "-e",
            "RUSTFS_VOLUMES=/data",
            "-e",
            &format!("RUSTFS_ADDRESS={S3_ADDRESS}"),
            "-e",
            "RUSTFS_REGION=us-east-1",
            "-e",
            "RUSTFS_CONSOLE_ENABLE=false",
            "-e",
            "RUSTFS_ACCESS_KEY_FILE=/run/secrets/rustfs-access-key",
            "-e",
            "RUSTFS_SECRET_KEY_FILE=/run/secrets/rustfs-secret-key",
            "-e",
            "RUSTFS_OBS_LOG_DIRECTORY=",
            "-e",
            "RUSTFS_OBS_LOGGER_LEVEL=info",
            "-e",
            "RC_CONFIG_DIR=/tmp/rc",
            "-v",
            &access_key_mount,
            "-v",
            &root_key_mount,
            "-v",
            &volume,
            &image,
        ],
    );
    Container
}

fn wait_until_ready() {
    let url = format!("http://{S3_ADDRESS}/health/ready");
    for _ in 0..READY_ATTEMPTS {
        if docker(&["exec", CONTAINER, "curl", "-fsS", &url])
            .status
            .success()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("RustFS did not become ready in time");
}

/// `rc` as root, the way the agent and the operator script run it.
fn rc_root(arguments: &[&str]) -> std::process::Output {
    let mut command = vec![
        "exec",
        CONTAINER,
        "sh",
        "-c",
        r#"RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS" exec rc --no-color --no-progress "$@""#,
        "rc",
    ];
    command.extend_from_slice(arguments);
    docker(&command)
}

/// Runs a shell script inside the container with the tenant's key in its
/// environment, never in an argument.
fn in_container(tenant: &Tenant, script: &str, arguments: &[&str]) -> String {
    let mut command = Command::new("docker");
    command
        .args([
            "exec", "-e", "AK", "-e", "SK", CONTAINER, "sh", "-c", script, "sh",
        ])
        .args(arguments)
        .env("AK", &tenant.key)
        .env("SK", &tenant.secret);
    let output = command.output().expect("docker exec runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A signed S3 request: the status code and the body.
const SIGNED_REQUEST: &str = r#"printf 'user = "%s:%s"\n' "$AK" "$SK" | curl -sS -K - --aws-sigv4 "aws:amz:us-east-1:s3" -D /tmp/s3-headers -o /tmp/s3-body -w '%{http_code}' "$@" > /tmp/s3-code 2>/tmp/s3-error; printf '%s\n' "$(cat /tmp/s3-code)"; cat /tmp/s3-headers; printf '\n--body--\n'; cat /tmp/s3-body"#;

struct S3Response {
    status: u16,
    headers: String,
    body: String,
}

fn s3(tenant: &Tenant, arguments: &[&str]) -> S3Response {
    let output = in_container(tenant, SIGNED_REQUEST, arguments);
    let (head, body) = output.split_once("\n--body--\n").unwrap_or((&output, ""));
    let (status, headers) = head.split_once('\n').unwrap_or((head, ""));
    S3Response {
        status: status.trim().parse().unwrap_or(0),
        headers: headers.to_owned(),
        body: body.to_owned(),
    }
}

fn url(path: &str) -> String {
    format!("http://{S3_ADDRESS}/{path}")
}

/// A PUT, retried on `503 SlowDown` with a growing pause, the way S3 SDKs
/// retry it: RustFS 1.0.6 answers the first write after every quota change
/// with that. `slowdowns` counts the retries.
fn put(tenant: &Tenant, path: &str, arguments: &[&str], slowdowns: &Mutex<u32>) -> S3Response {
    let target = url(path);
    let mut request = vec!["-X", "PUT"];
    request.extend_from_slice(arguments);
    request.push(&target);
    let mut response = s3(tenant, &request);
    for attempt in 1..SLOWDOWN_ATTEMPTS {
        if !(response.status == 503 && response.body.contains("SlowDown")) {
            break;
        }
        *slowdowns.lock().expect("slowdowns") += 1;
        std::thread::sleep(SLOWDOWN_BACKOFF * attempt);
        response = s3(tenant, &request);
    }
    response
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A presigned GET for `path`, signed by the tenant's key.
fn presign(tenant: &Tenant, path: &str) -> String {
    let output = in_container(
        tenant,
        r#"RC_HOST_t="http://$AK:$SK@$RUSTFS_ADDRESS" exec rc --no-color share "t/$1" --expire 1h --json"#,
        &[path],
    );
    let share: Value = serde_json::from_str(&output).expect("rc share output");
    share["url"].as_str().expect("url").to_owned()
}

fn anonymous_status(target: &str) -> u16 {
    let output = docker_ok(
        "send an anonymous request",
        &[
            "exec",
            CONTAINER,
            "curl",
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            target,
        ],
    );
    output.trim().parse().expect("status")
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

/// What happens to the next `rc admin user passwd` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Passwd {
    Answered,
    /// It runs, but the agent never hears back, like a client timeout after
    /// RustFS applied the change.
    LostAfterRunning,
    /// It never reaches the container.
    LostBeforeRunning,
}

/// The real process runner, except that it can lose a `passwd` call.
struct LosingRunner {
    inner: TokioProcessRunner,
    passwd: Mutex<Passwd>,
}

impl LosingRunner {
    fn new() -> Self {
        Self {
            inner: TokioProcessRunner::default(),
            passwd: Mutex::new(Passwd::Answered),
        }
    }

    fn set(&self, passwd: Passwd) {
        *self.passwd.lock().expect("passwd") = passwd;
    }
}

#[async_trait]
impl ProcessRunner for LosingRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let passwd = spec.args.iter().any(|arg| arg == "passwd");
        let mode = *self.passwd.lock().expect("passwd");
        let lost = || RuntimeError::Timeout {
            operation: "docker".to_owned(),
            seconds: 60,
        };
        match (passwd, mode) {
            (true, Passwd::LostBeforeRunning) => Err(lost()),
            (true, Passwd::LostAfterRunning) => {
                self.inner.run(spec, sink).await?;
                Err(lost())
            }
            _ => self.inner.run(spec, sink).await,
        }
    }
}

/// Samples the host's process list for any of `secrets` until stopped.
struct ProcessWatch {
    stop: Arc<AtomicBool>,
    hits: Arc<Mutex<BTreeSet<String>>>,
    samples: Arc<Mutex<u32>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProcessWatch {
    fn start(secrets: Arc<Mutex<Vec<String>>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(Mutex::new(BTreeSet::new()));
        let samples = Arc::new(Mutex::new(0));
        let thread = {
            let (stop, hits, samples) =
                (Arc::clone(&stop), Arc::clone(&hits), Arc::clone(&samples));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(output) = Command::new("ps").args(["-eww", "-o", "args"]).output() {
                        let listing = String::from_utf8_lossy(&output.stdout);
                        *samples.lock().expect("samples") += 1;
                        for secret in secrets.lock().expect("secrets").iter() {
                            if listing.contains(secret.as_str()) {
                                hits.lock().expect("hits").insert(secret.clone());
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        Self {
            stop,
            hits,
            samples,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> (BTreeSet<String>, u32) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("ps watcher");
        }
        let hits = self.hits.lock().expect("hits").clone();
        let samples = *self.samples.lock().expect("samples");
        (hits, samples)
    }
}

#[tokio::test]
#[ignore = "needs Docker and the RustFS image"]
#[allow(clippy::too_many_lines)]
async fn the_object_storage_role_isolates_tenants_on_a_real_rustfs() {
    let secrets_dir = tempfile::tempdir().expect("secrets dir");
    let data_dir = tempfile::tempdir().expect("data dir");
    let root_access = Uuid::new_v4().simple().to_string()[..20].to_ascii_uppercase();
    let root_secret = random_secret();
    std::fs::write(secrets_dir.path().join("access-key"), &root_access).expect("access key");
    std::fs::write(secrets_dir.path().join("secret-key"), &root_secret).expect("secret key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The container runs as its own uid, as on the host where the files
        // belong to the deploy account it runs as.
        for file in ["access-key", "secret-key"] {
            std::fs::set_permissions(
                secrets_dir.path().join(file),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("chmod");
        }
        std::fs::set_permissions(data_dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("chmod");
    }
    let _container = start_rustfs(secrets_dir.path(), data_dir.path());
    wait_until_ready();

    let a = Tenant::random();
    let b = Tenant::random();
    let rotated_secret = random_secret();
    let watched = Arc::new(Mutex::new(vec![
        a.secret.clone(),
        b.secret.clone(),
        rotated_secret.clone(),
        root_secret.clone(),
    ]));
    let watch = ProcessWatch::start(Arc::clone(&watched));
    let slowdowns = Mutex::new(0_u32);

    let admin_config = RustfsAdminConfig {
        container: CONTAINER.to_owned(),
        data_dir: data_dir.path().to_owned(),
        total_quota_mb: TOTAL_QUOTA_MB,
        min_free_mb: 0,
        command_timeout: Duration::from_secs(900),
    };
    let admin = Arc::new(RcObjectStorageAdmin::new(admin_config.clone()));
    admin.check().await.expect("admin check");
    let dispatcher = CommandDispatcher::new(Arc::new(NoopRuntimeExecutor))
        .with_object_storage_admin(Arc::clone(&admin) as Arc<dyn ObjectStorageAdmin>);

    // Create, then create again: the second run changes nothing.
    let first = dispatch(&dispatcher, CommandType::CreateBucket, a.create(6))
        .await
        .expect("create a");
    let again = dispatch(&dispatcher, CommandType::CreateBucket, a.create(6))
        .await
        .expect("create a again");
    assert_eq!(
        first,
        json!({ "bucket": a.bucket, "access_key_id": a.key, "quota_mb": 6, "created": true })
    );
    assert_eq!(again["created"], false);
    dispatch(&dispatcher, CommandType::CreateBucket, b.create(8))
        .await
        .expect("create b");

    // The key can put, get, list, and delete in its own bucket.
    let object = format!("{}/hello.txt", a.bucket);
    let put_hello = put(&a, &object, &["--data-binary", "hello"], &slowdowns);
    assert_eq!(put_hello.status, 200, "{}", put_hello.body);
    let got = s3(&a, &[&url(&object)]);
    assert_eq!((got.status, got.body.as_str()), (200, "hello"));
    let listed = s3(&a, &[&url(&format!("{}?list-type=2", a.bucket))]);
    assert_eq!(listed.status, 200);
    assert!(
        listed.body.contains("<Key>hello.txt</Key>"),
        "{}",
        listed.body
    );
    let deleted = s3(&a, &["-X", "DELETE", &url(&object)]);
    assert_eq!(deleted.status, 204);
    let kept = format!("{}/keep.txt", a.bucket);
    assert_eq!(
        put(&a, &kept, &["--data-binary", "kept"], &slowdowns).status,
        200
    );
    println!(
        "first write after a quota change answered 503 SlowDown: {} time(s) so far",
        slowdowns.lock().expect("slowdowns")
    );

    // The key reaches nothing else.
    let b_object = format!("{}/private.txt", b.bucket);
    assert_eq!(
        put(&b, &b_object, &["--data-binary", "b"], &slowdowns).status,
        200
    );
    for (what, request) in [
        ("read another bucket", vec![url(&b_object)]),
        (
            "write another bucket",
            vec![
                "-X".into(),
                "PUT".into(),
                "--data-binary".into(),
                "x".into(),
                url(&format!("{}/evil", b.bucket)),
            ],
        ),
        (
            "list another bucket",
            vec![url(&format!("{}?list-type=2", b.bucket))],
        ),
        (
            "create a bucket",
            vec!["-X".into(), "PUT".into(), url("b-smokeevilevilevil")],
        ),
        (
            "delete its own bucket",
            vec!["-X".into(), "DELETE".into(), url(&a.bucket)],
        ),
    ] {
        let arguments = request.iter().map(String::as_str).collect::<Vec<_>>();
        let response = s3(&a, &arguments);
        assert_eq!(response.status, 403, "{what}: {}", response.body);
    }
    let public_policy = json!({
        "Version": "2012-10-17",
        "Statement": [{ "Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": format!("arn:aws:s3:::{}/*", a.bucket) }],
    })
    .to_string();
    let lifecycle = "<LifecycleConfiguration><Rule><ID>x</ID><Status>Enabled</Status><Filter><Prefix></Prefix></Filter><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>";
    let cors = "<CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>";
    let versioning = "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>";
    for (subresource, document) in [
        ("policy", public_policy.as_str()),
        ("versioning", versioning),
        ("lifecycle", lifecycle),
        ("cors", cors),
    ] {
        let target = url(&format!("{}?{subresource}", a.bucket));
        let response = s3(&a, &["-X", "PUT", "--data-binary", document, &target]);
        assert_eq!(response.status, 403, "put {subresource}: {}", response.body);
    }
    // ListBuckets is allowed by RustFS but lists only the key's own bucket.
    let buckets = s3(&a, &[&url("")]);
    println!(
        "ListBuckets with a tenant key: HTTP {} listing only its own bucket: {}",
        buckets.status,
        buckets.body.contains(&a.bucket) && !buckets.body.contains(&b.bucket)
    );
    assert!(
        buckets.status == 403
            || (buckets.status == 200
                && buckets.body.contains(&a.bucket)
                && !buckets.body.contains(&b.bucket)),
        "ListBuckets must be refused or show only the key's bucket: {}",
        buckets.body
    );

    // Anonymous access is refused; a presigned GET works.
    assert_eq!(anonymous_status(&url(&kept)), 403);
    assert_eq!(
        anonymous_status(&url(&format!("{}?list-type=2", a.bucket))),
        403
    );
    let presigned = presign(&a, &kept);
    assert_eq!(anonymous_status(&presigned), 200);

    // The hard quota (6 MiB) refuses a single PUT over it...
    docker_ok(
        "write the upload test files",
        &[
            "exec",
            CONTAINER,
            "sh",
            "-c",
            "head -c 7340032 /dev/zero > /tmp/seven-mib; head -c 5242880 /dev/urandom > /tmp/part-one; head -c 2097152 /dev/urandom > /tmp/part-two",
        ],
    );
    let too_big = put(
        &a,
        &format!("{}/too-big", a.bucket),
        &["-T", "/tmp/seven-mib"],
        &slowdowns,
    );
    println!(
        "single PUT over quota: HTTP {} {}",
        too_big.status,
        too_big.body.trim()
    );
    assert!(!(200..300).contains(&too_big.status), "{}", too_big.body);
    assert!(too_big.body.contains("quota"), "{}", too_big.body);
    // ...and a multipart upload whose completion would pass it. Its parts are
    // accepted and not counted while the upload is incomplete.
    let upload = format!("{}/multipart", a.bucket);
    let started = s3(&a, &["-X", "POST", &url(&format!("{upload}?uploads"))]);
    assert_eq!(started.status, 200, "{}", started.body);
    let upload_id = started
        .body
        .split("<UploadId>")
        .nth(1)
        .and_then(|rest| rest.split("</UploadId>").next())
        .expect("upload id")
        .to_owned();
    let mut etags = Vec::new();
    for (number, file) in [(1, "/tmp/part-one"), (2, "/tmp/part-two")] {
        let part = s3(
            &a,
            &[
                "-X",
                "PUT",
                "-T",
                file,
                &url(&format!(
                    "{upload}?partNumber={number}&uploadId={upload_id}"
                )),
            ],
        );
        assert_eq!(part.status, 200, "part {number}: {}", part.body);
        etags.push(header(&part.headers, "etag").expect("etag").to_owned());
    }
    let usage = admin.bucket_usage().await.expect("usage");
    let a_usage = usage
        .iter()
        .find(|bucket| bucket.name == a.bucket)
        .expect("a in usage");
    println!(
        "usage with 7 MiB of incomplete parts: {} bytes (parts not counted: {})",
        a_usage.size_bytes,
        a_usage.size_bytes < MIB
    );
    assert!(
        a_usage.size_bytes < MIB,
        "incomplete parts counted: {}",
        a_usage.size_bytes
    );
    let complete = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        etags[0], etags[1]
    );
    let completed = s3(
        &a,
        &[
            "-X",
            "POST",
            "--data-binary",
            &complete,
            &url(&format!("{upload}?uploadId={upload_id}")),
        ],
    );
    println!(
        "multipart completion over quota: HTTP {} {}",
        completed.status,
        completed.body.trim()
    );
    assert!(
        !(200..300).contains(&completed.status),
        "{}",
        completed.body
    );
    assert!(completed.body.contains("quota"), "{}", completed.body);

    // Usage lists managed buckets only, unmanaged ones never.
    assert!(
        rc_root(&["mb", &format!("sakala/{UNMANAGED_BUCKET}"), "-q"])
            .status
            .success()
    );
    let usage = admin.bucket_usage().await.expect("usage");
    let names = usage
        .iter()
        .map(|bucket| bucket.name.as_str())
        .collect::<Vec<_>>();
    let mut expected = vec![a.bucket.as_str(), b.bucket.as_str()];
    expected.sort_unstable();
    assert_eq!(names, expected);

    // Capacity: an unlimited bucket on the instance, the node total, and the
    // free-disk floor each refuse.
    let c = Tenant::random();
    let unlimited = dispatch(&dispatcher, CommandType::CreateBucket, c.create(1))
        .await
        .expect_err("unlimited bucket present");
    assert_eq!(unlimited.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert!(
        unlimited.to_string().contains(UNMANAGED_BUCKET),
        "{unlimited}"
    );
    assert!(
        rc_root(&["rb", &format!("sakala/{UNMANAGED_BUCKET}"), "-q"])
            .status
            .success()
    );
    let total = dispatch(
        &dispatcher,
        CommandType::CreateBucket,
        c.create(TOTAL_QUOTA_MB),
    )
    .await
    .expect_err("above the node total");
    assert_eq!(total.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    let increase = dispatch(
        &dispatcher,
        CommandType::SetBucketQuota,
        json!({ "bucket": a.bucket, "quota_mb": TOTAL_QUOTA_MB - 7 }),
    )
    .await
    .expect_err("an increase above the node total");
    assert_eq!(increase.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    let floored = CommandDispatcher::new(Arc::new(NoopRuntimeExecutor)).with_object_storage_admin(
        Arc::new(RcObjectStorageAdmin::new(RustfsAdminConfig {
            min_free_mb: u64::from(u32::MAX),
            ..admin_config.clone()
        })),
    );
    let floor = dispatch(&floored, CommandType::CreateBucket, c.create(1))
        .await
        .expect_err("below the disk floor");
    assert_eq!(floor.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert!(
        rc_root(&["admin", "user", "info", "sakala", &c.key, "--json"])
            .status
            .code()
            == Some(5)
    );

    // Quota change, then the write after it.
    let resized = dispatch(
        &dispatcher,
        CommandType::SetBucketQuota,
        json!({ "bucket": a.bucket, "quota_mb": 10 }),
    )
    .await
    .expect("quota");
    assert_eq!(resized, json!({ "bucket": a.bucket, "quota_mb": 10 }));
    let quota: Value = serde_json::from_slice(
        &rc_root(&["quota", "info", &format!("sakala/{}", a.bucket), "--json"]).stdout,
    )
    .expect("quota info");
    assert_eq!(quota["quota"], 10 * MIB);
    assert_eq!(
        put(
            &a,
            &format!("{}/after-resize", a.bucket),
            &["--data-binary", "x"],
            &slowdowns
        )
        .status,
        200
    );

    // Rotation: the old secret is refused at once, the new one works.
    let rotated_tenant = Tenant {
        secret: rotated_secret.clone(),
        ..a.clone()
    };
    let rotated = dispatch(
        &dispatcher,
        CommandType::RotateBucketKey,
        json!({ "bucket": a.bucket, "access_key_id": a.key, "secret_access_key": rotated_secret }),
    )
    .await
    .expect("rotate");
    assert_eq!(
        rotated,
        json!({ "bucket": a.bucket, "access_key_id": a.key })
    );
    assert_eq!(
        s3(&a, &[&url(&kept)]).status,
        403,
        "the old secret still works"
    );
    assert_eq!(s3(&rotated_tenant, &[&url(&kept)]).status, 200);
    let presigned = presign(&rotated_tenant, &kept);

    // Lock: the key and its presigned URLs are refused, and a repeated create
    // keeps it locked. Unlock restores both.
    let locked = dispatch(&dispatcher, CommandType::LockBucket, a.key_payload())
        .await
        .expect("lock");
    assert_eq!(locked["locked"], true);
    assert_eq!(s3(&rotated_tenant, &[&url(&kept)]).status, 403);
    assert_eq!(
        anonymous_status(&presigned),
        403,
        "a presigned URL outlived the lock"
    );
    dispatch(
        &dispatcher,
        CommandType::CreateBucket,
        json!({ "bucket": a.bucket, "access_key_id": a.key, "secret_access_key": rotated_secret, "quota_mb": 10 }),
    )
    .await
    .expect("create while locked");
    assert_eq!(
        s3(&rotated_tenant, &[&url(&kept)]).status,
        403,
        "a repeated create unlocked the key"
    );
    let user: Value = serde_json::from_slice(
        &rc_root(&["admin", "user", "info", "sakala", &a.key, "--json"]).stdout,
    )
    .expect("user info");
    assert_eq!(user["status"], "disabled");
    let unlocked = dispatch(&dispatcher, CommandType::UnlockBucket, a.key_payload())
        .await
        .expect("unlock");
    assert_eq!(unlocked["locked"], false);
    assert_eq!(s3(&rotated_tenant, &[&url(&kept)]).status, 200);
    assert_eq!(anonymous_status(&presigned), 200);

    // A rotation whose answer is lost is checked against the instance: applied
    // means complete, not applied means fail with the old secret still alive,
    // and a locked key is checked too.
    let losing = Arc::new(LosingRunner::new());
    let checked = RcObjectStorageAdmin::with_runner(
        Arc::clone(&losing) as Arc<dyn ProcessRunner>,
        admin_config.clone(),
    );
    let rotate = |secret: &str| {
        sakala_agent_protocol::RotateBucketKeyPayload::from_value(&json!({
            "bucket": a.bucket,
            "access_key_id": a.key,
            "secret_access_key": secret,
        }))
        .expect("payload")
    };
    let applied = Tenant {
        secret: random_secret(),
        ..a.clone()
    };
    let lost = random_secret();
    let locked_applied = Tenant {
        secret: random_secret(),
        ..a.clone()
    };
    watched.lock().expect("secrets").extend([
        applied.secret.clone(),
        lost.clone(),
        locked_applied.secret.clone(),
    ]);
    losing.set(Passwd::LostAfterRunning);
    checked
        .rotate_bucket_key(&rotate(&applied.secret))
        .await
        .expect("applied, so complete");
    assert_eq!(s3(&applied, &[&url(&kept)]).status, 200);
    losing.set(Passwd::LostBeforeRunning);
    let error = checked
        .rotate_bucket_key(&rotate(&lost))
        .await
        .expect_err("not applied, so fail");
    assert_eq!(
        error.code(),
        error_codes::BUCKET_ADMIN_UNAVAILABLE,
        "{error}"
    );
    assert_eq!(
        s3(&applied, &[&url(&kept)]).status,
        200,
        "the old secret died"
    );
    dispatch(&dispatcher, CommandType::LockBucket, a.key_payload())
        .await
        .expect("lock");
    losing.set(Passwd::LostAfterRunning);
    checked
        .rotate_bucket_key(&rotate(&locked_applied.secret))
        .await
        .expect("applied on a locked key, so complete");
    dispatch(&dispatcher, CommandType::UnlockBucket, a.key_payload())
        .await
        .expect("unlock");
    assert_eq!(s3(&locked_applied, &[&url(&kept)]).status, 200);
    assert_eq!(s3(&applied, &[&url(&kept)]).status, 403);
    println!("lost passwd answers were settled by a signed check: applied, not applied, locked");

    // Delete removes objects (incomplete uploads included), the key, the
    // policy, and the bucket; a repeat reports nothing deleted.
    let removed = dispatch(&dispatcher, CommandType::DeleteBucket, a.key_payload())
        .await
        .expect("delete");
    assert_eq!(removed, json!({ "bucket": a.bucket, "deleted": true }));
    assert_eq!(
        rc_root(&["admin", "user", "info", "sakala", &a.key, "--json"])
            .status
            .code(),
        Some(5)
    );
    assert_eq!(
        rc_root(&["admin", "policy", "info", "sakala", &a.bucket, "--json"])
            .status
            .code(),
        Some(5)
    );
    let listing =
        String::from_utf8_lossy(&rc_root(&["ls", "sakala/", "--json"]).stdout).into_owned();
    assert!(!listing.contains(&a.bucket), "{listing}");
    assert!(listing.contains(&b.bucket), "{listing}");
    let removed_again = dispatch(&dispatcher, CommandType::DeleteBucket, a.key_payload())
        .await
        .expect("delete again");
    assert_eq!(
        removed_again,
        json!({ "bucket": a.bucket, "deleted": false })
    );
    for (command_type, payload) in [
        (CommandType::LockBucket, a.key_payload()),
        (CommandType::UnlockBucket, a.key_payload()),
        (
            CommandType::RotateBucketKey,
            json!({ "bucket": a.bucket, "access_key_id": a.key, "secret_access_key": random_secret() }),
        ),
        (
            CommandType::SetBucketQuota,
            json!({ "bucket": a.bucket, "quota_mb": 1 }),
        ),
    ] {
        let error = dispatch(&dispatcher, command_type, payload)
            .await
            .expect_err("gone");
        assert_eq!(
            error.code(),
            error_codes::BUCKET_NOT_FOUND,
            "{command_type:?}"
        );
    }
    dispatch(&dispatcher, CommandType::DeleteBucket, b.key_payload())
        .await
        .expect("delete b");

    // A container that is not there makes the node unavailable, not empty.
    let missing = RcObjectStorageAdmin::new(RustfsAdminConfig {
        container: format!("{CONTAINER}-missing"),
        ..admin_config.clone()
    });
    let unavailable = missing.bucket_usage().await.expect_err("no container");
    assert_eq!(
        unavailable.code(),
        error_codes::BUCKET_ADMIN_UNAVAILABLE,
        "{unavailable}"
    );
    let unavailable = dispatch(
        &CommandDispatcher::new(Arc::new(NoopRuntimeExecutor))
            .with_object_storage_admin(Arc::new(missing)),
        CommandType::LockBucket,
        b.key_payload(),
    )
    .await
    .expect_err("no container");
    assert_eq!(unavailable.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);

    // No secret in the process list or in RustFS's log.
    let (hits, samples) = watch.finish();
    println!("ps samples taken during the run: {samples}");
    assert!(samples > 0);
    assert!(
        hits.is_empty(),
        "a secret appeared in the host process list"
    );
    let logs = Command::new("docker")
        .args(["logs", CONTAINER])
        .output()
        .expect("docker logs");
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    for secret in watched.lock().expect("secrets").iter() {
        assert!(
            !logs.contains(secret.as_str()),
            "a secret appeared in the RustFS log"
        );
    }
    println!(
        "writes answered 503 SlowDown and retried: {}",
        slowdowns.lock().expect("slowdowns")
    );
}
