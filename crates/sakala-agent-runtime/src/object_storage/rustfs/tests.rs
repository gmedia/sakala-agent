//! The adapter against a simulated `rc`: every call goes through a fake
//! [`ProcessRunner`] that answers like rc 0.1.37 did against RustFS 1.0.6.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use sakala_agent_protocol::{BucketQuotaMb, MANAGED_S3_CAPABILITY};
use serde_json::Value;

use super::*;
use crate::{ProcessOutputSink, RuntimeError};
use sakala_agent_core::ports::BUCKET_USAGE_BUSY;

const BUCKET: &str = "b-8k2m4q7x9z1c3v5b6n0d";
const OTHER_BUCKET: &str = "b-q1w2e3r4t5y6u7i8o9p0";
const KEY: &str = "SK7Q2M9X4K1C8V5B3N0D";
const SECRET: &str = "c2VjcmV0YWNjZXNza2V5dmFsdWUxMjM0NTY3ODkw";
const ROTATED: &str = "cm90YXRlZHNlY3JldGFjY2Vzc2tleXZhbHVlMTIz";
const CONTAINER: &str = "sakala-object-storage-rustfs-1";

#[derive(Clone, Debug)]
struct User {
    disabled: bool,
    policies: Vec<String>,
    secret: String,
}

#[derive(Clone, Debug, Default)]
struct Bucket {
    quota_bytes: u64,
    usage_bytes: u64,
}

/// The instance as `rc` sees it, plus how the next calls should fail.
#[derive(Default)]
struct Instance {
    buckets: BTreeMap<String, Bucket>,
    users: BTreeMap<String, User>,
    policies: BTreeMap<String, String>,
    anonymous_private: BTreeSet<String>,
    free_mb: u64,
    /// The rc subcommand (first arguments joined by spaces) that fails, with
    /// its exit status and stderr.
    fail: Option<(String, i32, String)>,
    /// The failing subcommand still takes effect, as when rc is stopped after
    /// RustFS applied the change.
    fail_after_applying: bool,
    /// The subcommand whose `docker exec` the runner cannot finish.
    runner_error: Option<String>,
}

/// One `docker exec` or `df` call as the test sees it.
#[derive(Clone, Debug)]
struct Call {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    secret_env: BTreeMap<String, String>,
}

impl Call {
    fn script(&self) -> Option<&str> {
        let sh = self.args.iter().position(|arg| arg == "sh")?;
        self.args.get(sh + 2).map(String::as_str)
    }

    /// The arguments after `rc` and its time limit.
    fn rc_args(&self) -> Vec<&str> {
        let Some(sh) = self.args.iter().position(|arg| arg == "sh") else {
            return Vec::new();
        };
        self.args[sh + 5..].iter().map(String::as_str).collect()
    }

    /// The seconds after which the container stops the script's work.
    fn server_limit(&self) -> u64 {
        let sh = self.args.iter().position(|arg| arg == "sh").expect("sh");
        self.args[sh + 4].parse().expect("limit")
    }

    /// `rc` subcommand words without flags and targets, such as `admin user add`.
    fn summary(&self) -> String {
        if self.program == "df" {
            return "df".to_owned();
        }
        if self.script() == Some(USAGE_SCRIPT) {
            return "usage".to_owned();
        }
        if self.script() == Some(VERIFY_SCRIPT) {
            return "verify".to_owned();
        }
        let args = self.rc_args();
        let words = match args.first().copied() {
            Some("admin") => 3,
            Some("quota" | "anonymous") => 2,
            _ => 1,
        };
        args.iter()
            .take(words)
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

struct FakeRunner {
    instance: StdMutex<Instance>,
    calls: StdMutex<Vec<Call>>,
    active: AtomicUsize,
    most_active: AtomicUsize,
    delay: Duration,
}

impl FakeRunner {
    fn new(instance: Instance) -> Arc<Self> {
        Self::with_delay(instance, Duration::ZERO)
    }

    fn with_delay(instance: Instance, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            instance: StdMutex::new(instance),
            calls: StdMutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            most_active: AtomicUsize::new(0),
            delay,
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

    fn instance(&self) -> std::sync::MutexGuard<'_, Instance> {
        self.instance.lock().expect("instance")
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

fn exit(code: i32, stderr: impl Into<String>) -> ProcessOutput {
    ProcessOutput {
        success: false,
        code: Some(code),
        stdout: String::new(),
        stderr: stderr.into(),
    }
}

fn bucket_of(target: &str) -> String {
    target.trim_start_matches("sakala/").to_owned()
}

impl Instance {
    fn quota_document(name: &str, bucket: &Bucket) -> String {
        json!({
            "bucket": name,
            "quota": bucket.quota_bytes,
            "quotaHuman": "ignored",
            "usage": bucket.usage_bytes,
            "usageHuman": "ignored",
            "quotaType": "HARD",
        })
        .to_string()
    }

    fn answer(&mut self, call: &Call) -> ProcessOutput {
        if call.program == "df" {
            return ok(format!(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 60000000 1000 {} 50% /\n",
                self.free_mb * 1_024
            ));
        }
        if let Some((summary, code, stderr)) = self.fail.clone()
            && call.summary() == summary
        {
            if self.fail_after_applying {
                self.fail = None;
                let _ = self.answer(call);
                self.fail = Some((summary, code, stderr.clone()));
            }
            return exit(code, stderr);
        }
        if call.script() == Some(VERIFY_SCRIPT) {
            let args = call.rc_args();
            let secret = &call.secret_env[BUCKET_SECRET_ENV];
            let body = match self.users.get(args[0]) {
                Some(user) if user.secret == *secret && !user.disabled => {
                    return ok("<ListBucketResult></ListBucketResult>\n200");
                }
                Some(user) if user.secret == *secret => {
                    "<Error><Code>InvalidAccessKeyId</Code><Message>check key failed</Message></Error>"
                }
                Some(_) => "<Error><Code>SignatureDoesNotMatch</Code></Error>",
                None => "<Error><Code>InvalidAccessKeyId</Code></Error>",
            };
            return ok(format!("{body}\n403"));
        }
        if call.script() == Some(USAGE_SCRIPT) {
            let listing = json!({
                "items": self.buckets.keys().map(|name| json!({ "key": name, "is_dir": true })).collect::<Vec<_>>(),
                "truncated": false,
            });
            let mut out = format!("{listing:#}\n");
            for (name, bucket) in &self.buckets {
                out.push_str(&Self::quota_document(name, bucket));
                out.push('\n');
            }
            return ok(out);
        }
        let args = call.rc_args();
        match args.as_slice() {
            ["admin", "info", "cluster", ..] => ok(r#"{"rustfsVersion":"1.0.6","onlineDisks":1}"#),
            ["ls", "sakala/", "--json"] => ok(json!({
                "items": self.buckets.keys().map(|name| json!({ "key": format!("{name}/") })).collect::<Vec<_>>(),
            })
            .to_string()),
            ["mb", target, "-q"] => {
                self.buckets.entry(bucket_of(target)).or_default();
                ok("")
            }
            ["quota", "set", target, size, "-q"] => match self.buckets.get_mut(&bucket_of(target)) {
                Some(bucket) => {
                    let mib: u64 = size.trim_end_matches("MiB").parse().expect("MiB");
                    bucket.quota_bytes = mib * MIB;
                    ok("")
                }
                None => exit(3, "✗ Failed to set bucket quota: Network error: HTTP 500"),
            },
            ["admin", "policy", "create", "sakala", name, "/dev/stdin", "-q"] => {
                let document = call.env.get(BUCKET_POLICY_ENV).expect("policy in env");
                self.policies.insert((*name).to_owned(), document.clone());
                ok("")
            }
            ["admin", "policy", "info", "sakala", name, "--json"] => {
                if self.policies.contains_key(*name) {
                    ok("{}")
                } else {
                    exit(5, "")
                }
            }
            ["admin", "policy", "rm", "sakala", name, "-q"] => {
                self.policies.remove(*name);
                ok("")
            }
            ["admin", "policy", "attach", "sakala", name, "--user", key, "-q"] => {
                let user = self.users.get_mut(*key).expect("attach to a user");
                if !user.policies.iter().any(|policy| policy == name) {
                    user.policies.push((*name).to_owned());
                }
                ok("")
            }
            ["admin", "user", "info", "sakala", key, "--json"] => match self.users.get(*key) {
                Some(user) => ok(json!({
                    "accessKey": key,
                    "status": if user.disabled { "disabled" } else { "enabled" },
                    "policies": user.policies,
                    "memberOf": [],
                })
                .to_string()),
                None => exit(5, format!("{{\"error\": \"User '{key}' not found\"}}")),
            },
            ["admin", "user", "add", "sakala", key, secret] => {
                // Like rc 0.1.37: an existing user is reset and enabled.
                let user = self.users.entry((*key).to_owned()).or_insert(User {
                    disabled: false,
                    policies: Vec::new(),
                    secret: String::new(),
                });
                user.secret = (*secret).to_owned();
                user.disabled = false;
                ok(format!(r#"{{"secret_key": "{secret}"}}"#))
            }
            ["admin", "user", "passwd", "sakala", key, "--password-from-env", name, "-q"] => {
                let secret = call.secret_env.get(*name).expect("secret in env").clone();
                match self.users.get_mut(*key) {
                    Some(user) => {
                        user.secret = secret;
                        ok("")
                    }
                    None => exit(5, "not found"),
                }
            }
            ["admin", "user", action @ ("disable" | "enable"), "sakala", key, "-q"] => {
                match self.users.get_mut(*key) {
                    Some(user) => {
                        user.disabled = *action == "disable";
                        ok("")
                    }
                    None => exit(1, "failed to set user status: user does not exist"),
                }
            }
            ["admin", "user", "rm", "sakala", key, "-q"] => match self.users.remove(*key) {
                Some(_) => ok(""),
                None => exit(5, "not found"),
            },
            ["anonymous", "set", "private", target, "-q"] => {
                self.anonymous_private.insert(bucket_of(target));
                ok("")
            }
            ["rb", "--force", "--dangerous", "--yes", target, "-q"] => {
                match self.buckets.remove(&bucket_of(target)) {
                    Some(_) => ok(""),
                    None => exit(5, "Bucket not found"),
                }
            }
            other => panic!("unexpected rc call {other:?}"),
        }
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        _sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let call = Call {
            program: spec.program.clone(),
            args: spec
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            env: spec.environment.clone(),
            secret_env: spec
                .secret_environment_names()
                .map(|name| {
                    (
                        name.to_owned(),
                        spec.secret_environment_value(name)
                            .expect("value")
                            .to_owned(),
                    )
                })
                .collect(),
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_active.fetch_max(active, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let runner_error = self.instance().runner_error.clone();
        if runner_error.as_deref() == Some(call.summary().as_str()) {
            // The change lands, but the agent never hears back.
            let _ = self.instance().answer(&call);
            self.calls.lock().expect("calls").push(call);
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(RuntimeError::Timeout {
                operation: "docker".to_owned(),
                seconds: 60,
            });
        }
        let output = self.instance().answer(&call);
        self.calls.lock().expect("calls").push(call);
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(output)
    }
}

fn config() -> RustfsAdminConfig {
    RustfsAdminConfig {
        container: CONTAINER.to_owned(),
        data_dir: PathBuf::from("/var/lib/sakala/object-storage"),
        total_quota_mb: 1_024,
        min_free_mb: 1_024,
        command_timeout: Duration::from_secs(900),
    }
}

fn admin(runner: &Arc<FakeRunner>) -> RcObjectStorageAdmin {
    RcObjectStorageAdmin::with_runner(Arc::clone(runner) as Arc<dyn ProcessRunner>, config())
}

fn roomy() -> Instance {
    Instance {
        free_mb: 10_000,
        ..Instance::default()
    }
}

fn name(value: &str) -> BucketName {
    BucketName::parse(value).expect("bucket")
}

fn key(value: &str) -> AccessKeyId {
    AccessKeyId::parse(value).expect("key")
}

fn create(quota_mb: u64) -> CreateBucketPayload {
    CreateBucketPayload {
        bucket: name(BUCKET),
        access_key_id: key(KEY),
        secret_access_key: SecretAccessKey::parse(SECRET).expect("secret"),
        quota_mb: BucketQuotaMb::new(quota_mb).expect("quota"),
    }
}

fn key_payload() -> BucketKeyPayload {
    BucketKeyPayload {
        bucket: name(BUCKET),
        access_key_id: key(KEY),
    }
}

fn quota(quota_mb: u64) -> SetBucketQuotaPayload {
    SetBucketQuotaPayload {
        bucket: name(BUCKET),
        quota_mb: BucketQuotaMb::new(quota_mb).expect("quota"),
    }
}

fn existing_bucket(instance: &mut Instance, bucket: &str, quota_mb: u64, usage_mb: u64) {
    instance.buckets.insert(
        bucket.to_owned(),
        Bucket {
            quota_bytes: quota_mb * MIB,
            usage_bytes: usage_mb * MIB,
        },
    );
}

#[test]
fn the_capability_matches_the_contract() {
    assert_eq!(MANAGED_S3_CAPABILITY, "managed-s3");
}

#[tokio::test]
async fn create_runs_the_documented_rc_calls_in_order() {
    let runner = FakeRunner::new(roomy());
    let result = admin(&runner)
        .create_bucket(&create(256))
        .await
        .expect("create");

    assert_eq!(
        result,
        CreateBucketResult {
            bucket: name(BUCKET),
            access_key_id: key(KEY),
            quota_mb: BucketQuotaMb::new(256).expect("quota"),
            created: true,
        }
    );
    assert_eq!(
        runner.summaries(),
        [
            "usage",
            "admin user info",
            "df",
            "mb",
            "quota set",
            "admin policy create",
            "admin user add",
            "admin user passwd",
            "admin policy attach",
            "anonymous set",
        ]
    );
    let calls = runner.calls();
    let rc = |summary: &str| {
        calls
            .iter()
            .find(|call| call.summary() == summary)
            .expect(summary)
            .rc_args()
            .join(" ")
    };
    assert_eq!(rc("mb"), format!("mb sakala/{BUCKET} -q"));
    assert_eq!(
        rc("quota set"),
        format!("quota set sakala/{BUCKET} 256MiB -q")
    );
    assert_eq!(
        rc("admin policy create"),
        format!("admin policy create sakala {BUCKET} /dev/stdin -q")
    );
    assert_eq!(
        rc("admin user passwd"),
        format!("admin user passwd sakala {KEY} --password-from-env SAKALA_BUCKET_SECRET -q")
    );
    assert_eq!(
        rc("admin policy attach"),
        format!("admin policy attach sakala {BUCKET} --user {KEY} -q")
    );
    assert_eq!(
        rc("anonymous set"),
        format!("anonymous set private sakala/{BUCKET} -q")
    );
    let df = calls.iter().find(|call| call.program == "df").expect("df");
    assert_eq!(df.args, ["-Pk", "/var/lib/sakala/object-storage"]);
    for call in calls.iter().filter(|call| call.program == "docker") {
        assert_eq!(call.args[0], "exec");
        let container = call
            .args
            .iter()
            .position(|arg| arg == CONTAINER)
            .expect("container");
        assert_eq!(&call.args[container + 1..container + 3], ["sh", "-c"]);
        assert_eq!(call.args[container + 4], "rc");
    }

    let instance = runner.instance();
    let user = instance.users.get(KEY).expect("user");
    assert_eq!(user.secret, SECRET);
    assert_eq!(user.policies, [BUCKET]);
    assert!(!user.disabled);
    assert_eq!(instance.buckets[BUCKET].quota_bytes, 256 * MIB);
    assert!(instance.anonymous_private.contains(BUCKET));
    let policy: Value = serde_json::from_str(&instance.policies[BUCKET]).expect("policy");
    assert_eq!(
        policy["Statement"][0]["Resource"],
        json!([format!("arn:aws:s3:::{BUCKET}")])
    );
    assert_eq!(
        policy["Statement"][1]["Resource"],
        json!([format!("arn:aws:s3:::{BUCKET}/*")])
    );
    assert_eq!(
        policy["Statement"][1]["Action"],
        json!([
            "s3:GetObject",
            "s3:PutObject",
            "s3:DeleteObject",
            "s3:AbortMultipartUpload",
            "s3:ListMultipartUploadParts"
        ])
    );
}

#[tokio::test]
async fn secrets_and_the_policy_travel_in_the_child_environment_never_in_arguments() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    admin
        .rotate_bucket_key(&RotateBucketKeyPayload {
            bucket: name(BUCKET),
            access_key_id: key(KEY),
            secret_access_key: SecretAccessKey::parse(ROTATED).expect("secret"),
        })
        .await
        .expect("rotate");

    let calls = runner.calls();
    for call in &calls {
        let argv = call.args.join(" ");
        assert!(!argv.contains(SECRET), "{argv}");
        assert!(!argv.contains(ROTATED), "{argv}");
        assert!(
            !argv.contains("Statement"),
            "the policy is passed by env: {argv}"
        );
        assert!(!call.env.values().any(|value| value.contains(SECRET)));
    }
    let passwd = calls
        .iter()
        .filter(|call| call.summary() == "admin user passwd")
        .collect::<Vec<_>>();
    assert_eq!(passwd.len(), 2);
    for (call, secret) in passwd.iter().zip([SECRET, ROTATED]) {
        assert_eq!(&call.args[..3], ["exec", "-e", BUCKET_SECRET_ENV]);
        assert_eq!(call.secret_env[BUCKET_SECRET_ENV], secret);
    }
    let policy = calls
        .iter()
        .find(|call| call.summary() == "admin policy create")
        .expect("policy");
    assert_eq!(&policy.args[..3], ["exec", "-e", BUCKET_POLICY_ENV]);
    assert_eq!(policy.script(), Some(RC_POLICY_SCRIPT));
    assert!(policy.secret_env.is_empty());
    for call in calls.iter().filter(|call| call.program == "docker") {
        let script = call.script().expect("script");
        assert!(script.contains("/run/secrets/rustfs-secret-key"));
        if call.summary() != "admin user passwd" {
            assert!(call.secret_env.is_empty(), "{}", call.summary());
        }
    }
    assert_eq!(runner.instance().users[KEY].secret, ROTATED);
}

#[tokio::test]
async fn a_new_user_starts_with_a_throwaway_secret_that_is_replaced_at_once() {
    let runner = FakeRunner::new(roomy());
    admin(&runner)
        .create_bucket(&create(64))
        .await
        .expect("create");

    let summaries = runner.summaries();
    let add = summaries
        .iter()
        .position(|summary| summary == "admin user add")
        .expect("add");
    assert_eq!(summaries[add + 1], "admin user passwd");
    let calls = runner.calls();
    let throwaway = calls[add].rc_args()[5].to_owned();
    assert_eq!(throwaway.len(), THROWAWAY_SECRET_LENGTH);
    assert!(throwaway.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    assert_ne!(throwaway, SECRET);
    assert_eq!(runner.instance().users[KEY].secret, SECRET);
}

#[tokio::test]
async fn a_repeated_create_changes_nothing_and_keeps_a_locked_key_locked() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    admin.lock_bucket(&key_payload()).await.expect("lock");
    runner.clear();

    let again = admin
        .create_bucket(&create(64))
        .await
        .expect("create again");

    assert!(!again.created);
    assert_eq!(
        runner.summaries(),
        [
            "usage",
            "admin user info",
            "df",
            "quota set",
            "admin policy create",
            "admin user passwd",
            "anonymous set",
        ],
        "no mb, no user add (which would unlock the key), no second attach"
    );
    let instance = runner.instance();
    assert!(instance.users[KEY].disabled, "the key stays locked");
    assert_eq!(instance.users[KEY].policies, [BUCKET]);
}

#[tokio::test]
async fn a_half_created_bucket_is_completed_on_retry() {
    let mut instance = roomy();
    // `mb` succeeded but the quota did not: the bucket has none yet.
    existing_bucket(&mut instance, BUCKET, 0, 0);
    let runner = FakeRunner::new(instance);

    let result = admin(&runner)
        .create_bucket(&create(64))
        .await
        .expect("create");

    assert!(!result.created);
    assert_eq!(runner.instance().buckets[BUCKET].quota_bytes, 64 * MIB);
}

#[tokio::test]
async fn a_user_with_a_foreign_policy_is_a_conflict_and_nothing_changes() {
    let mut instance = roomy();
    instance.users.insert(
        KEY.to_owned(),
        User {
            disabled: false,
            policies: vec![BUCKET.to_owned(), "readwrite".to_owned()],
            secret: "old".to_owned(),
        },
    );
    let runner = FakeRunner::new(instance);
    let admin = admin(&runner);

    for result in [
        admin.create_bucket(&create(64)).await.map(drop),
        admin
            .rotate_bucket_key(&RotateBucketKeyPayload {
                bucket: name(BUCKET),
                access_key_id: key(KEY),
                secret_access_key: SecretAccessKey::parse(ROTATED).expect("secret"),
            })
            .await
            .map(drop),
        admin.delete_bucket(&key_payload()).await.map(drop),
    ] {
        let error = result.expect_err("conflict");
        assert_eq!(error.code(), error_codes::BUCKET_CONFLICT);
        assert!(error.to_string().contains("readwrite"));
    }
    let mutating = runner
        .summaries()
        .into_iter()
        .filter(|summary| !matches!(summary.as_str(), "usage" | "admin user info" | "df"))
        .collect::<Vec<_>>();
    assert!(mutating.is_empty(), "{mutating:?}");
    let instance = runner.instance();
    assert!(instance.buckets.is_empty());
    assert_eq!(instance.users[KEY].secret, "old");
}

#[tokio::test]
async fn capacity_counts_every_bucket_and_replaces_this_buckets_quota() {
    // Total 1024 MiB: 700 MiB elsewhere leaves 324 MiB.
    let mut instance = roomy();
    existing_bucket(&mut instance, OTHER_BUCKET, 700, 0);
    existing_bucket(&mut instance, "w-01a0d73b-uploads", 0, 0);
    let runner = FakeRunner::new(instance);
    let admin = admin(&runner);

    let error = admin
        .create_bucket(&create(64))
        .await
        .expect_err("unlimited");
    assert_eq!(error.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert!(error.to_string().contains("w-01a0d73b-uploads"));
    runner.instance().buckets.remove("w-01a0d73b-uploads");

    let error = admin.create_bucket(&create(325)).await.expect_err("total");
    assert_eq!(error.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert!(error.to_string().contains("1025 MiB"), "{error}");
    admin
        .create_bucket(&create(324))
        .await
        .expect("fits exactly");

    // A resize replaces this bucket's 324 MiB rather than adding to it.
    admin.set_bucket_quota(&quota(324)).await.expect("same");
    admin.set_bucket_quota(&quota(100)).await.expect("smaller");
    admin.set_bucket_quota(&quota(324)).await.expect("back up");
    let error = admin
        .set_bucket_quota(&quota(325))
        .await
        .expect_err("above total");
    assert_eq!(error.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert_eq!(
        runner.instance().buckets[BUCKET].quota_bytes,
        324 * MIB,
        "a refused increase leaves the quota alone"
    );
}

#[tokio::test]
async fn capacity_keeps_the_free_disk_floor_when_every_bucket_is_full() {
    // 1500 MiB free; 200 MiB of the 500 MiB quota elsewhere already used, so
    // 300 MiB more may still be written there.
    let mut instance = Instance {
        free_mb: 1_500,
        ..Instance::default()
    };
    existing_bucket(&mut instance, OTHER_BUCKET, 500, 200);
    let runner = FakeRunner::new(instance);
    let admin = RcObjectStorageAdmin::with_runner(
        Arc::clone(&runner) as Arc<dyn ProcessRunner>,
        RustfsAdminConfig {
            total_quota_mb: 100_000,
            ..config()
        },
    );

    // 1500 - (300 + 177) = 1023 < 1024.
    let error = admin.create_bucket(&create(177)).await.expect_err("floor");
    assert_eq!(error.code(), error_codes::BUCKET_CAPACITY_EXCEEDED);
    assert!(error.to_string().contains("1023 MiB"), "{error}");
    // 1500 - (300 + 176) = 1024.
    admin
        .create_bucket(&create(176))
        .await
        .expect("on the floor");
}

#[tokio::test]
async fn a_quota_decrease_needs_no_capacity_check() {
    let mut instance = Instance {
        free_mb: 0,
        ..Instance::default()
    };
    existing_bucket(&mut instance, BUCKET, 256, 10);
    let runner = FakeRunner::new(instance);

    admin(&runner)
        .set_bucket_quota(&quota(128))
        .await
        .expect("decrease on a full disk");

    assert_eq!(runner.summaries(), ["usage", "quota set"]);
    assert_eq!(runner.instance().buckets[BUCKET].quota_bytes, 128 * MIB);
}

#[tokio::test]
async fn missing_buckets_and_keys_are_not_found() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);

    let quota = admin.set_bucket_quota(&quota(64)).await.expect_err("quota");
    let rotate = admin
        .rotate_bucket_key(&RotateBucketKeyPayload {
            bucket: name(BUCKET),
            access_key_id: key(KEY),
            secret_access_key: SecretAccessKey::parse(ROTATED).expect("secret"),
        })
        .await
        .expect_err("rotate");
    let lock = admin.lock_bucket(&key_payload()).await.expect_err("lock");
    let unlock = admin
        .unlock_bucket(&key_payload())
        .await
        .expect_err("unlock");
    for error in [quota, rotate, lock, unlock] {
        assert_eq!(error.code(), error_codes::BUCKET_NOT_FOUND, "{error}");
    }
    assert!(
        runner
            .summaries()
            .iter()
            .all(|summary| matches!(summary.as_str(), "usage" | "admin user info")),
        "{:?}",
        runner.summaries()
    );
}

#[tokio::test]
async fn lock_and_unlock_switch_the_key_and_repeat_harmlessly() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");

    for _ in 0..2 {
        let locked = admin.lock_bucket(&key_payload()).await.expect("lock");
        assert!(locked.locked);
        assert!(runner.instance().users[KEY].disabled);
    }
    for _ in 0..2 {
        let unlocked = admin.unlock_bucket(&key_payload()).await.expect("unlock");
        assert!(!unlocked.locked);
        assert!(!runner.instance().users[KEY].disabled);
    }
    let lock_call = runner
        .calls()
        .into_iter()
        .find(|call| call.summary() == "admin user disable")
        .expect("disable");
    assert_eq!(
        lock_call.rc_args(),
        ["admin", "user", "disable", "sakala", KEY, "-q"]
    );
}

#[tokio::test]
async fn delete_stops_the_key_first_removes_everything_and_repeats_as_not_deleted() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    runner
        .instance()
        .buckets
        .get_mut(BUCKET)
        .expect("bucket")
        .usage_bytes = 5 * MIB;
    runner.clear();

    let deleted = admin.delete_bucket(&key_payload()).await.expect("delete");
    assert!(deleted.deleted);
    assert_eq!(
        runner.summaries(),
        [
            "admin user info",
            "admin user disable",
            "admin user rm",
            "ls",
            "rb",
            "admin policy info",
            "admin policy rm",
        ]
    );
    let rb = runner
        .calls()
        .into_iter()
        .find(|call| call.summary() == "rb")
        .expect("rb");
    assert_eq!(
        rb.rc_args(),
        [
            "rb",
            "--force",
            "--dangerous",
            "--yes",
            &format!("sakala/{BUCKET}"),
            "-q"
        ]
    );
    {
        let instance = runner.instance();
        assert!(instance.buckets.is_empty());
        assert!(instance.users.is_empty());
        assert!(instance.policies.is_empty());
    }

    let again = admin.delete_bucket(&key_payload()).await.expect("again");
    assert!(!again.deleted);
}

#[tokio::test]
async fn usage_reports_managed_buckets_only_sorted_by_name() {
    let mut instance = roomy();
    existing_bucket(&mut instance, OTHER_BUCKET, 64, 3);
    existing_bucket(&mut instance, BUCKET, 64, 1);
    existing_bucket(&mut instance, "w-01a0d73b-uploads", 64, 9);
    let runner = FakeRunner::new(instance);

    let usage = admin(&runner).bucket_usage().await.expect("usage");

    assert_eq!(
        usage,
        [
            ManagedBucketUsage {
                name: BUCKET.to_owned(),
                size_bytes: MIB,
            },
            ManagedBucketUsage {
                name: OTHER_BUCKET.to_owned(),
                size_bytes: 3 * MIB,
            },
        ]
    );
    let calls = runner.calls();
    assert_eq!(calls.len(), 1, "one docker exec per read");
    assert_eq!(calls[0].script(), Some(USAGE_SCRIPT));
}

#[test]
fn a_usage_read_without_a_quota_for_a_listed_bucket_is_refused() {
    let listing = format!(r#"{{"items":[{{"key":"{BUCKET}/"}},{{"key":"{OTHER_BUCKET}"}}]}}"#);
    let one = Instance::quota_document(BUCKET, &Bucket::default());
    let error = parse_allocations(&format!("{listing}\n{one}\n")).expect_err("missing");
    assert_eq!(error.code(), error_codes::BUCKET_OPERATION_FAILED);
    assert!(error.to_string().contains(OTHER_BUCKET));

    assert_eq!(
        parse_allocations(r#"{"items": [], "truncated": false}"#).expect("empty"),
        []
    );
    for garbage in ["", "not json", r#"{"items": 7}"#] {
        assert_eq!(
            parse_allocations(garbage).expect_err("garbage").code(),
            error_codes::BUCKET_OPERATION_FAILED,
            "{garbage:?}"
        );
    }
}

#[tokio::test]
async fn failures_map_to_codes_and_never_carry_the_secret() {
    let cases = [
        (
            "mb",
            125,
            "docker: Error response from daemon: No such container",
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
        ),
        (
            "mb",
            1,
            "Error response from daemon: container abc is not running",
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
        ),
        (
            "mb",
            127,
            "sh: rc: not found",
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
        ),
        (
            "mb",
            3,
            "✗ Network error: Connection refused",
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
        ),
        (
            "mb",
            4,
            "✗ Authentication failed: InvalidAccessKeyId",
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
        ),
        (
            "admin user passwd",
            1,
            "✗ rejected secret c2VjcmV0YWNjZXNza2V5dmFsdWUxMjM0NTY3ODkw",
            error_codes::BUCKET_OPERATION_FAILED,
        ),
    ];
    for (summary, code, stderr, expected) in cases {
        let mut instance = roomy();
        instance.fail = Some((summary.to_owned(), code, stderr.to_owned()));
        let runner = FakeRunner::new(instance);
        let error = admin(&runner)
            .create_bucket(&create(64))
            .await
            .expect_err("failure");
        assert_eq!(error.code(), expected, "{summary} {code}");
        assert!(!error.to_string().contains(SECRET), "{error}");
    }

    let mut instance = roomy();
    instance.fail = Some((
        "usage".to_owned(),
        1,
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock".to_owned(),
    ));
    let error = admin(&FakeRunner::new(instance))
        .bucket_usage()
        .await
        .expect_err("down");
    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);
}

#[test]
fn the_stderr_tail_is_bounded_and_redacted() {
    let redactor = ValueRedactor::new([SECRET.to_owned()]);
    let stderr = (0..20)
        .map(|line| format!("line {line} {SECRET}"))
        .collect::<Vec<_>>()
        .join("\n");
    let tail = stderr_tail(&stderr, &redactor);
    assert!(!tail.contains(SECRET));
    assert!(tail.starts_with("line 15"), "{tail}");
    assert!(stderr_tail(&"é".repeat(2_000), &redactor).len() <= STDERR_TAIL_MAX_BYTES);
}

#[tokio::test]
async fn operations_and_usage_reads_run_one_at_a_time() {
    let runner = FakeRunner::with_delay(roomy(), Duration::from_millis(5));
    let admin = Arc::new(admin(&runner));

    let mut tasks = Vec::new();
    for index in 0..4 {
        let admin = Arc::clone(&admin);
        tasks.push(tokio::spawn(async move {
            if index % 2 == 0 {
                admin.bucket_usage().await.map(drop)
            } else {
                admin.create_bucket(&create(8)).await.map(drop)
            }
        }));
    }
    for task in tasks {
        task.await.expect("task").expect("operation");
    }

    assert_eq!(runner.most_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn check_asks_the_instance_for_its_state() {
    let runner = FakeRunner::new(roomy());
    admin(&runner).check().await.expect("check");
    assert_eq!(
        runner.calls()[0].rc_args(),
        ["admin", "info", "cluster", "sakala", "--json"]
    );
}

#[test]
fn the_scripts_build_the_alias_like_the_deployment_operator_script() {
    // `rc()` in sakala-deployment's scripts/object-storage.sh.
    let alias = r#"RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS""#;
    for script in [RC_SCRIPT, RC_POLICY_SCRIPT, USAGE_SCRIPT] {
        assert!(script.contains(alias), "{script}");
        assert!(script.contains("timeout \"$"), "{script}");
    }
    assert!(RC_SCRIPT.ends_with(r#"exec timeout "$limit" rc --no-color --no-progress "$@""#));
    assert!(RC_POLICY_SCRIPT.contains(r#"printf '%s' "$SAKALA_BUCKET_POLICY" | "#));
    assert!(USAGE_SCRIPT.starts_with("set -e\n"));
    assert!(VERIFY_SCRIPT.contains("--max-time"));
    assert!(VERIFY_SCRIPT.contains("-K -"), "the key pair goes on stdin");
}

fn rotate(secret: &str) -> RotateBucketKeyPayload {
    RotateBucketKeyPayload {
        bucket: name(BUCKET),
        access_key_id: key(KEY),
        secret_access_key: SecretAccessKey::parse(secret).expect("secret"),
    }
}

#[tokio::test]
async fn rc_stops_inside_the_container_before_the_agent_stops_waiting() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    admin.delete_bucket(&key_payload()).await.expect("delete");

    for call in runner
        .calls()
        .iter()
        .filter(|call| call.program == "docker")
    {
        let script = call.script().expect("script");
        assert!(script.contains("timeout"), "{}", call.summary());
        let limit = call.server_limit();
        let summary = call.summary();
        let expected = if summary == "rb" { 595 } else { 55 };
        assert_eq!(limit, expected, "{summary}");
    }
}

#[tokio::test]
async fn every_step_fits_the_command_timeout() {
    // 900 s command timeout: 885 s budget, plenty for every step.
    let runner = FakeRunner::new(roomy());
    admin(&runner)
        .create_bucket(&create(64))
        .await
        .expect("create");

    // A 100 s command timeout: 85 s to spend, every step cut to fit.
    let short = RcObjectStorageAdmin::with_runner(
        Arc::clone(&runner) as Arc<dyn ProcessRunner>,
        RustfsAdminConfig {
            command_timeout: Duration::from_secs(100),
            ..config()
        },
    );
    runner.clear();
    short.delete_bucket(&key_payload()).await.expect("delete");
    let rb = runner
        .calls()
        .into_iter()
        .find(|call| call.summary() == "rb")
        .expect("rb");
    // 85 s, less 20 s for the two policy calls after it, less the 5 s the
    // container stops rc before the agent stops waiting.
    assert!(
        (59..=60).contains(&rb.server_limit()),
        "{}",
        rb.server_limit()
    );
    assert_eq!(
        runner.summaries().last().map(String::as_str),
        Some("admin policy rm")
    );

    // Too short to start anything: refused before any call.
    let none = RcObjectStorageAdmin::with_runner(
        Arc::clone(&runner) as Arc<dyn ProcessRunner>,
        RustfsAdminConfig {
            command_timeout: Duration::from_secs(20),
            ..config()
        },
    );
    runner.clear();
    let error = none.create_bucket(&create(8)).await.expect_err("no time");
    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn a_rotation_that_rc_reports_failed_but_took_effect_completes() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    {
        let mut instance = runner.instance();
        instance.fail = Some(("admin user passwd".to_owned(), TIMEOUT_EXIT, String::new()));
        instance.fail_after_applying = true;
    }
    runner.clear();

    admin
        .rotate_bucket_key(&rotate(ROTATED))
        .await
        .expect("rotated");

    assert_eq!(
        runner.summaries(),
        ["admin user info", "admin user passwd", "verify"]
    );
    let verify = runner
        .calls()
        .into_iter()
        .find(|call| call.summary() == "verify")
        .expect("verify");
    assert_eq!(&verify.args[..3], ["exec", "-e", BUCKET_SECRET_ENV]);
    assert!(!verify.args.join(" ").contains(ROTATED));
    assert_eq!(verify.secret_env[BUCKET_SECRET_ENV], ROTATED);
    assert_eq!(runner.instance().users[KEY].secret, ROTATED);
}

#[tokio::test]
async fn a_rotation_of_a_locked_key_is_checked_too() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    admin.lock_bucket(&key_payload()).await.expect("lock");
    {
        let mut instance = runner.instance();
        instance.runner_error = Some("admin user passwd".to_owned());
    }

    admin
        .rotate_bucket_key(&rotate(ROTATED))
        .await
        .expect("rotated");
    assert!(runner.instance().users[KEY].disabled, "still locked");
}

#[tokio::test]
async fn a_rotation_that_did_not_take_effect_fails_with_the_old_secret_alive() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    runner.instance().fail = Some(("admin user passwd".to_owned(), TIMEOUT_EXIT, String::new()));

    let error = admin
        .rotate_bucket_key(&rotate(ROTATED))
        .await
        .expect_err("not applied");

    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);
    assert_eq!(runner.instance().users[KEY].secret, SECRET);
}

#[tokio::test]
async fn a_rotation_whose_outcome_cannot_be_checked_asks_for_the_same_command_again() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    {
        let mut instance = runner.instance();
        instance.runner_error = Some("admin user passwd".to_owned());
    }
    // The check cannot reach the instance either.
    runner.instance().fail = Some((
        "verify".to_owned(),
        1,
        "Error response from daemon: container is not running".to_owned(),
    ));

    let error = admin
        .rotate_bucket_key(&rotate(ROTATED))
        .await
        .expect_err("unknown");

    assert_eq!(error.code(), error_codes::BUCKET_OUTCOME_UNKNOWN);
    assert!(
        error
            .to_string()
            .contains("send the same RotateBucketKey again")
    );
    assert!(!error.to_string().contains(ROTATED));
}

#[test]
fn signed_request_answers_tell_which_secret_works() {
    assert_eq!(
        secret_check("<ListBucketResult/>\n200\n", false),
        SecretCheck::InEffect
    );
    assert_eq!(
        secret_check(
            "<Error><Code>SignatureDoesNotMatch</Code></Error>\n403",
            true
        ),
        SecretCheck::NotInEffect
    );
    assert_eq!(
        secret_check("<Error><Code>InvalidAccessKeyId</Code></Error>\n403", true),
        SecretCheck::InEffect
    );
    for (stdout, disabled) in [
        ("<Error><Code>InvalidAccessKeyId</Code></Error>\n403", false),
        ("<Error><Code>InternalError</Code></Error>\n500", false),
        ("", false),
    ] {
        assert!(
            matches!(secret_check(stdout, disabled), SecretCheck::Unknown(_)),
            "{stdout:?}"
        );
    }
}

#[tokio::test]
async fn admin_commands_that_cannot_reach_the_instance_are_unavailable() {
    // `rc admin ...` exits 1, not 3, when the instance is unreachable.
    let stderr = r#"{
  "error": "Failed to get user info: Network error: Request failed: error sending request",
  "details": { "type": "network_error", "retryable": true }
}"#;
    let mut instance = roomy();
    instance.fail = Some(("admin user info".to_owned(), 1, stderr.to_owned()));
    let runner = FakeRunner::new(instance);
    let admin = admin(&runner);

    for error in [
        admin
            .create_bucket(&create(8))
            .await
            .map(drop)
            .expect_err("create"),
        admin
            .lock_bucket(&key_payload())
            .await
            .map(drop)
            .expect_err("lock"),
        admin
            .rotate_bucket_key(&rotate(ROTATED))
            .await
            .map(drop)
            .expect_err("rotate"),
    ] {
        assert_eq!(
            error.code(),
            error_codes::BUCKET_ADMIN_UNAVAILABLE,
            "{error}"
        );
    }

    let mut instance = roomy();
    instance.fail = Some(("admin info cluster".to_owned(), 1, stderr.to_owned()));
    let error = super::RcObjectStorageAdmin::with_runner(
        FakeRunner::new(instance) as Arc<dyn ProcessRunner>,
        config(),
    )
    .check()
    .await
    .expect_err("check");
    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);

    let mut instance = roomy();
    instance.fail = Some(("mb".to_owned(), TIMEOUT_EXIT, String::new()));
    let error = admin_for(instance)
        .create_bucket(&create(8))
        .await
        .expect_err("late");
    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);
    assert!(error.to_string().contains("did not finish in time"));
}

fn admin_for(instance: Instance) -> RcObjectStorageAdmin {
    RcObjectStorageAdmin::with_runner(
        FakeRunner::new(instance) as Arc<dyn ProcessRunner>,
        config(),
    )
}

#[tokio::test]
async fn a_runner_error_is_unavailable() {
    let mut instance = roomy();
    instance.runner_error = Some("usage".to_owned());
    let error = admin_for(instance)
        .bucket_usage()
        .await
        .expect_err("timeout");
    assert_eq!(error.code(), error_codes::BUCKET_ADMIN_UNAVAILABLE);
}

#[tokio::test]
async fn lock_and_unlock_refuse_another_buckets_key_and_a_missing_bucket() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");

    let other = BucketKeyPayload {
        bucket: name(OTHER_BUCKET),
        access_key_id: key(KEY),
    };
    for error in [
        admin.lock_bucket(&other).await.expect_err("lock"),
        admin.unlock_bucket(&other).await.expect_err("unlock"),
    ] {
        assert_eq!(error.code(), error_codes::BUCKET_CONFLICT, "{error}");
    }
    assert!(!runner.instance().users[KEY].disabled);

    runner.instance().buckets.remove(BUCKET);
    let error = admin
        .lock_bucket(&key_payload())
        .await
        .expect_err("missing");
    assert_eq!(error.code(), error_codes::BUCKET_NOT_FOUND);
    assert!(!runner.instance().users[KEY].disabled);
}

#[tokio::test]
async fn a_usage_read_while_a_command_holds_the_instance_is_skipped_not_failed() {
    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    // A long DeleteBucket holds the lock for as long as it runs, up to minutes.
    let held = admin.operations.lock().await;

    let started = std::time::Instant::now();
    let error = admin.bucket_usage().await.expect_err("busy");
    assert_eq!(error.code(), BUCKET_USAGE_BUSY);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited too long"
    );
    assert!(runner.calls().is_empty());

    drop(held);
    admin.bucket_usage().await.expect("read after the command");
}

/// `examples/failures/bucket-failures.json`: the `fail` bodies the control
/// plane copies, by code.
fn fixture_message(code: &str) -> String {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../examples/failures/bucket-failures.json"
    ))
    .expect("fixture");
    fixture
        .as_array()
        .expect("list")
        .iter()
        .find(|entry| entry["body"]["error_code"] == code)
        .and_then(|entry| entry["body"]["error_message"].as_str())
        .unwrap_or_else(|| panic!("no {code} in the fixture"))
        .to_owned()
}

#[tokio::test]
async fn the_failure_fixture_holds_what_the_adapter_sends() {
    const STOPPED: &str = "Error response from daemon: container 3f2a is not running";

    let runner = FakeRunner::new(roomy());
    let admin = admin(&runner);
    admin.create_bucket(&create(64)).await.expect("create");
    runner.instance().runner_error = Some("admin user passwd".to_owned());
    runner.instance().fail = Some(("verify".to_owned(), 1, STOPPED.to_owned()));
    let unknown = admin
        .rotate_bucket_key(&rotate(ROTATED))
        .await
        .expect_err("unknown");

    let mut full = roomy();
    existing_bucket(&mut full, OTHER_BUCKET, 1_000, 0);
    let capacity = admin_for(full)
        .create_bucket(&create(64))
        .await
        .expect_err("capacity");

    let mut foreign = roomy();
    foreign.users.insert(
        KEY.to_owned(),
        User {
            disabled: false,
            policies: vec![OTHER_BUCKET.to_owned()],
            secret: String::new(),
        },
    );
    let conflict = admin_for(foreign)
        .create_bucket(&create(64))
        .await
        .expect_err("conflict");

    let missing = admin_for(roomy())
        .lock_bucket(&key_payload())
        .await
        .expect_err("missing");

    let mut stopped = roomy();
    stopped.fail = Some(("usage".to_owned(), 1, STOPPED.to_owned()));
    let unavailable = admin_for(stopped)
        .create_bucket(&create(64))
        .await
        .expect_err("stopped");

    let mut refused = roomy();
    refused.fail = Some((
        "mb".to_owned(),
        1,
        "✗ Failed to create bucket: Access Denied".to_owned(),
    ));
    let operation = admin_for(refused)
        .create_bucket(&create(64))
        .await
        .expect_err("refused");

    for error in [unknown, capacity, conflict, missing, unavailable, operation] {
        assert_eq!(
            error.to_string(),
            fixture_message(error.code()),
            "{}",
            error.code()
        );
    }
}
