//! [`ObjectStorageAdmin`] over RustFS's CLI `rc`, run with `docker exec` inside
//! the instance's own container.
//!
//! The root key never enters the agent: every call builds the `rc` alias inside
//! the container from the mounted secret files, the same way as sakala-deployment's
//! `scripts/object-storage.sh`. Process arguments are only validated bucket names,
//! access key IDs, and integers; nothing is ever interpolated into a script. A
//! tenant secret reaches `docker exec` through `-e NAME` with the value in the
//! child's environment, never in an argument.
//!
//! Killing `docker exec` does not stop the process it started in the container,
//! so every `rc` runs under the container's own `timeout`, which stops it a few
//! seconds before the agent gives up waiting. Every command gets a time budget
//! inside the node's command timeout, cut into steps. Commands run one at a
//! time; a usage read that finds a command running is skipped, not failed.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::{
    logs::redactor::ValueRedactor,
    ports::{BUCKET_USAGE_BUSY, ObjectStorageAdmin, RuntimeExecutionError, SecretString},
};
use sakala_agent_protocol::{
    AccessKeyId, BucketKeyPayload, BucketName, CreateBucketPayload, DeleteBucketPayload,
    ManagedBucketUsage, RotateBucketKeyPayload, SecretAccessKey, SetBucketQuotaPayload,
    buckets::{
        BucketLockResult, CreateBucketResult, DeleteBucketResult, RotateBucketKeyResult,
        SetBucketQuotaResult, error_codes,
    },
};
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::Mutex, time::Instant};
use tracing::warn;
use uuid::Uuid;

use crate::{CommandSpec, NullOutputSink, ProcessOutput, ProcessRunner, TokioProcessRunner};

/// The `rc` alias the fixed scripts define.
pub const RC_ALIAS: &str = "sakala";

/// Carries a tenant secret into the container, by name only.
pub const BUCKET_SECRET_ENV: &str = "SAKALA_BUCKET_SECRET";

/// Carries the bucket policy document into the container.
pub const BUCKET_POLICY_ENV: &str = "SAKALA_BUCKET_POLICY";

/// Runs `rc "$@"` against the instance as root, stopped by the container's
/// `timeout` after `$1` seconds. The alias URL is built inside the container
/// from the mounted root key files, as in `rc()` of sakala-deployment's
/// `scripts/object-storage.sh`, so the root key is in no argument list and
/// never leaves the container.
const RC_SCRIPT: &str = r#"limit=$1; shift; RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS" exec timeout "$limit" rc --no-color --no-progress "$@""#;

/// [`RC_SCRIPT`] with the policy document from [`BUCKET_POLICY_ENV`] on stdin,
/// for `rc admin policy create ... /dev/stdin`.
const RC_POLICY_SCRIPT: &str = r#"limit=$1; shift; printf '%s' "$SAKALA_BUCKET_POLICY" | RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS" timeout "$limit" rc --no-color --no-progress "$@""#;

/// Prints the bucket listing, then the quota and usage of every bucket, as a
/// stream of JSON documents, within `$1` seconds in total. Bucket names cannot
/// hold quotes or spaces, so the listing's `"key"` fields are safe to cut out
/// with `grep` and `sed`, and the agent checks that every listed bucket got its
/// document. A failed or late call fails the whole script.
const USAGE_SCRIPT: &str = r#"set -e
end=$(( $(date +%s) + $1 ))
left() { echo $(( end - $(date +%s) )); }
export RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS"
t=$(left); [ "$t" -gt 0 ] || exit 124
listing=$(timeout "$t" rc --no-color --no-progress ls sakala/ --json)
printf '%s\n' "$listing"
for name in $(printf '%s\n' "$listing" | grep -o '"key": *"[^"]*"' | sed -e 's/^"key": *"//' -e 's/"$//' -e 's:/$::'); do
  t=$(left); [ "$t" -gt 0 ] || exit 124
  timeout "$t" rc --no-color --no-progress quota info "sakala/$name" --json
done"#;

/// Lists bucket `$3` with access key `$2` and the secret in
/// [`BUCKET_SECRET_ENV`], signed like a tenant's request, within `$1` seconds.
/// Prints the response body, then the HTTP status on its own line. The key
/// pair goes to curl on stdin, never as an argument.
const VERIFY_SCRIPT: &str = r#"printf 'user = "%s:%s"\n' "$2" "$SAKALA_BUCKET_SECRET" | curl -sS --max-time "$1" -K - --aws-sigv4 "aws:amz:${RUSTFS_REGION:-us-east-1}:s3" -w '\n%{http_code}' "http://$RUSTFS_ADDRESS/$3?list-type=2&max-keys=0""#;

/// `rc` exit statuses (rc 0.1.37): the instance could not be reached, refused
/// the root key, or does not have the named object.
const RC_EXIT_NETWORK: i32 = 3;
const RC_EXIT_AUTH: i32 = 4;
const RC_EXIT_NOT_FOUND: i32 = 5;

/// The container's `timeout` (coreutils) stopped `rc`.
const TIMEOUT_EXIT: i32 = 124;

/// `rc admin` commands exit 1 when the instance cannot be reached or refuses
/// the root key; these mark that in their output.
const RC_UNREACHABLE_MARKERS: [&str; 4] = [
    "\"type\": \"network_error\"",
    "\"type\": \"auth_error\"",
    "Network error",
    "Authentication failed",
];

/// `docker exec` could not run the command in the container: Docker itself
/// failed (125), the command cannot be executed (126), or it is missing (127).
const DOCKER_EXEC_FAILURES: std::ops::RangeInclusive<i32> = 125..=127;

/// Docker 29 reports a missing or stopped container and an unreachable daemon
/// with exit status 1 and one of these on stderr.
const DOCKER_DAEMON_ERRORS: [&str; 3] = [
    "Error response from daemon",
    "Cannot connect to the Docker daemon",
    "error during connect",
];

/// S3 error codes a signed request answers when the secret is not the key's
/// current one, and when the key exists but is disabled. RustFS 1.0.6 checks
/// the signature before the key's status.
const SIGNATURE_MISMATCH: &str = "<Code>SignatureDoesNotMatch</Code>";
const KEY_REFUSED: &str = "<Code>InvalidAccessKeyId</Code>";
const HTTP_OK: &str = "200";

/// RustFS user status of a locked key.
const USER_STATUS_DISABLED: &str = "disabled";

/// Bounds one `rc` call.
const RC_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds removing a bucket with its objects, which grows with the object
/// count.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(600);

/// Bounds the signed request that checks a rotated secret.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);

/// Bounds the usage read, inside the heartbeat's own 20-second limit.
const USAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a usage read waits for a running command before it is skipped.
const USAGE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Bounds `df`.
const DISK_TIMEOUT: Duration = Duration::from_secs(10);

/// Left of the node's command timeout for reporting the result.
const COMMAND_MARGIN: Duration = Duration::from_secs(15);

/// The container stops `rc` this long before the agent stops waiting.
const SERVER_MARGIN: Duration = Duration::from_secs(5);

/// A step that would get less time than this is not started.
const MIN_STEP: Duration = Duration::from_secs(10);

const MIB: u64 = 1_024 * 1_024;

/// Length of the secret a new user starts with before the real one replaces
/// it, matching the 40 characters the control plane generates.
const THROWAWAY_SECRET_LENGTH: usize = 40;

/// Lines and bytes of `rc` stderr kept in a failure message.
const STDERR_TAIL_LINES: usize = 5;
const STDERR_TAIL_MAX_BYTES: usize = 1_000;

/// The instance and the node's capacity limits.
#[derive(Clone, Debug)]
pub struct RustfsAdminConfig {
    /// The container that runs RustFS and holds the root key files.
    pub container: String,
    /// The instance's data directory on this host, read with `df`.
    pub data_dir: PathBuf,
    /// The most all bucket quotas may add up to, in MiB.
    pub total_quota_mb: u64,
    /// Disk to leave free when every bucket is full, in MiB.
    pub min_free_mb: u64,
    /// `SAKALA_COMMAND_TIMEOUT_SECONDS`: every command finishes inside it.
    pub command_timeout: Duration,
}

pub struct RcObjectStorageAdmin {
    runner: Arc<dyn ProcessRunner>,
    config: RustfsAdminConfig,
    /// Serializes every operation and usage read on the instance.
    operations: Mutex<()>,
}

/// The time one command has left, cut into steps.
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

    /// `preferred`, or less so that `reserve` stays for the steps after it.
    fn step(
        self,
        preferred: Duration,
        reserve: Duration,
        operation: &str,
    ) -> Result<Duration, RuntimeExecutionError> {
        let left = self
            .deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(reserve);
        let step = preferred.min(left);
        if step < MIN_STEP {
            return Err(unavailable(format!(
                "the command ran out of time before {operation}"
            )));
        }
        Ok(step)
    }
}

/// Whether a rotated secret is the key's current one.
#[derive(Debug, Eq, PartialEq)]
enum SecretCheck {
    InEffect,
    NotInEffect,
    Unknown(String),
}

/// A bucket on the instance, managed or not, with its quota and usage.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BucketAllocation {
    name: String,
    /// 0 when the bucket has no quota.
    quota_bytes: u64,
    usage_bytes: u64,
}

/// `rc ls sakala/ --json`.
#[derive(Deserialize)]
struct Listing {
    items: Vec<ListingItem>,
}

#[derive(Deserialize)]
struct ListingItem {
    key: String,
}

/// `rc quota info sakala/<bucket> --json`.
#[derive(Deserialize)]
struct QuotaInfo {
    bucket: String,
    #[serde(default)]
    quota: Option<u64>,
    #[serde(default)]
    usage: Option<u64>,
}

/// `rc admin user info sakala <key> --json`.
#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    status: String,
    #[serde(default)]
    policies: Vec<String>,
}

impl UserInfo {
    /// Refuses a user that carries any policy besides the bucket's own, which
    /// would give the key access the control plane does not know about, or
    /// means the key belongs to another bucket.
    fn ensure_only(&self, bucket: &BucketName) -> Result<(), RuntimeExecutionError> {
        let foreign = self
            .policies
            .iter()
            .filter(|policy| policy.as_str() != bucket.as_str())
            .map(String::as_str)
            .collect::<Vec<_>>();
        if foreign.is_empty() {
            Ok(())
        } else {
            Err(RuntimeExecutionError::new(
                error_codes::BUCKET_CONFLICT,
                format!(
                    "the key's user carries policies besides {bucket}: {}; nothing was changed",
                    foreign.join(", ")
                ),
            ))
        }
    }

    fn holds(&self, bucket: &BucketName) -> bool {
        self.policies.iter().any(|policy| policy == bucket.as_str())
    }

    fn disabled(&self) -> bool {
        self.status == USER_STATUS_DISABLED
    }
}

impl RcObjectStorageAdmin {
    #[must_use]
    pub fn new(config: RustfsAdminConfig) -> Self {
        Self::with_runner(Arc::new(TokioProcessRunner::default()), config)
    }

    #[must_use]
    pub fn with_runner(runner: Arc<dyn ProcessRunner>, config: RustfsAdminConfig) -> Self {
        Self {
            runner,
            config,
            operations: Mutex::new(()),
        }
    }

    fn budget(&self) -> Budget {
        Budget::new(self.config.command_timeout.saturating_sub(COMMAND_MARGIN))
    }

    /// `docker exec [-e NAME]... <container> sh -c <script> rc <seconds> <args>...`,
    /// where the container stops the script's work after `<seconds>`, a little
    /// before the agent stops waiting at `limit`.
    fn exec(
        &self,
        env_names: &[&str],
        script: &str,
        limit: Duration,
        args: &[&str],
    ) -> CommandSpec {
        let server_limit = limit
            .saturating_sub(SERVER_MARGIN)
            .max(Duration::from_secs(1));
        let mut spec = CommandSpec::new("docker").arg("exec");
        for name in env_names {
            spec = spec.arg("-e").arg(*name);
        }
        spec = spec
            .arg(&self.config.container)
            .arg("sh")
            .arg("-c")
            .arg(script)
            .arg("rc")
            .arg(server_limit.as_secs().to_string());
        for arg in args {
            spec = spec.arg(*arg);
        }
        spec.timeout(limit)
    }

    /// One `rc` call with the step's share of the budget.
    fn rc(&self, budget: Budget, args: &[&str]) -> Result<CommandSpec, RuntimeExecutionError> {
        let limit = budget.step(
            RC_TIMEOUT,
            Duration::ZERO,
            args.first().copied().unwrap_or("rc"),
        )?;
        Ok(self.exec(&[], RC_SCRIPT, limit, args))
    }

    async fn run(&self, spec: &CommandSpec) -> Result<ProcessOutput, RuntimeExecutionError> {
        self.runner
            .run(spec, &NullOutputSink)
            .await
            .map_err(|error| unavailable(format!("docker exec could not run rc: {error}")))
    }

    /// Runs a call that must succeed.
    async fn run_ok(
        &self,
        spec: &CommandSpec,
        operation: &str,
        redactor: &ValueRedactor,
    ) -> Result<ProcessOutput, RuntimeExecutionError> {
        let output = self.run(spec).await?;
        if output.success {
            Ok(output)
        } else {
            Err(failure(&output, operation, redactor))
        }
    }

    /// An `rc` call that must succeed.
    async fn rc_ok(
        &self,
        budget: Budget,
        args: &[&str],
        operation: &str,
        redactor: &ValueRedactor,
    ) -> Result<(), RuntimeExecutionError> {
        self.run_ok(&self.rc(budget, args)?, operation, redactor)
            .await
            .map(drop)
    }

    /// The user named by `key`, or `None` when RustFS does not know it.
    async fn user(
        &self,
        budget: Budget,
        key: &AccessKeyId,
    ) -> Result<Option<UserInfo>, RuntimeExecutionError> {
        let output = self
            .run(&self.rc(
                budget,
                &["admin", "user", "info", RC_ALIAS, key.as_str(), "--json"],
            )?)
            .await?;
        if output.code == Some(RC_EXIT_NOT_FOUND) {
            return Ok(None);
        }
        if !output.success {
            return Err(failure(
                &output,
                "reading the key's user",
                &ValueRedactor::default(),
            ));
        }
        parse(&output.stdout, "rc admin user info").map(Some)
    }

    async fn bucket_exists(
        &self,
        budget: Budget,
        bucket: &BucketName,
    ) -> Result<bool, RuntimeExecutionError> {
        let output = self
            .run_ok(
                &self.rc(budget, &["ls", &format!("{RC_ALIAS}/"), "--json"])?,
                "listing buckets",
                &ValueRedactor::default(),
            )
            .await?;
        let listing: Listing = parse(&output.stdout, "rc ls")?;
        Ok(listing
            .items
            .iter()
            .any(|item| bucket_key(&item.key) == bucket.as_str()))
    }

    /// Every bucket on the instance with its quota and usage, managed or not.
    async fn allocations(
        &self,
        limit: Duration,
    ) -> Result<Vec<BucketAllocation>, RuntimeExecutionError> {
        let spec = self.exec(&[], USAGE_SCRIPT, limit, &[]);
        let output = self
            .run_ok(&spec, "reading bucket quotas", &ValueRedactor::default())
            .await?;
        parse_allocations(&output.stdout)
    }

    async fn free_mb(&self) -> Result<u64, RuntimeExecutionError> {
        let spec = CommandSpec::new("df")
            .arg("-Pk")
            .arg(self.config.data_dir.as_os_str())
            .timeout(DISK_TIMEOUT);
        let output = self
            .runner
            .run(&spec, &NullOutputSink)
            .await
            .map_err(|error| self.disk_unreadable(&error.to_string()))?;
        if !output.success {
            return Err(self.disk_unreadable(&crate::error::describe_exit(output.code)));
        }
        crate::workspace::parse_available_disk_bytes(&output.stdout)
            .map(|bytes| bytes / MIB)
            .map_err(|error| self.disk_unreadable(&error.to_string()))
    }

    fn disk_unreadable(&self, reason: &str) -> RuntimeExecutionError {
        unavailable(format!(
            "free space of {} could not be read: {reason}",
            self.config.data_dir.display()
        ))
    }

    /// Refuses a quota for `bucket` of `requested_mb` that would take the sum of
    /// all quotas above the node's total, or leave less than the free-disk
    /// floor once every bucket is full. `bucket`'s current quota is replaced,
    /// not added to.
    async fn check_capacity(
        &self,
        allocations: &[BucketAllocation],
        bucket: &BucketName,
        requested_mb: u64,
    ) -> Result<(), RuntimeExecutionError> {
        let others = || {
            allocations
                .iter()
                .filter(|other| other.name != bucket.as_str())
        };
        let unlimited = others()
            .filter(|other| other.quota_bytes == 0)
            .map(|other| other.name.as_str())
            .collect::<Vec<_>>();
        if !unlimited.is_empty() {
            warn!(
                buckets = %unlimited.join(", "),
                "buckets without a quota on the object storage instance; give each one a quota before new buckets or quota increases are accepted"
            );
            return Err(capacity_exceeded(format!(
                "bucket(s) without a quota: {}; the node cannot account for their size",
                unlimited.join(", ")
            )));
        }
        let committed_after = others()
            .map(|other| other.quota_bytes.div_ceil(MIB))
            .sum::<u64>()
            .saturating_add(requested_mb);
        if committed_after > self.config.total_quota_mb {
            return Err(capacity_exceeded(format!(
                "quotas would total {committed_after} MiB, above SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB={}",
                self.config.total_quota_mb
            )));
        }
        let used_mb = allocations
            .iter()
            .map(|allocation| allocation.usage_bytes)
            .sum::<u64>()
            .div_ceil(MIB);
        let free_mb = self.free_mb().await?;
        let free_when_full =
            i128::from(free_mb) - (i128::from(committed_after) - i128::from(used_mb));
        if free_when_full < i128::from(self.config.min_free_mb) {
            return Err(capacity_exceeded(format!(
                "with every quota used the disk would have {free_when_full} MiB free, below SAKALA_OBJECT_STORAGE_MIN_FREE_MB={}",
                self.config.min_free_mb
            )));
        }
        Ok(())
    }

    async fn set_quota(
        &self,
        budget: Budget,
        bucket: &BucketName,
        quota_mb: u32,
    ) -> Result<(), RuntimeExecutionError> {
        self.rc_ok(
            budget,
            &[
                "quota",
                "set",
                &target(bucket),
                &format!("{quota_mb}MiB"),
                "-q",
            ],
            "setting the bucket quota",
            &ValueRedactor::default(),
        )
        .await
    }

    /// Sets the key's secret from the environment, leaving `reserve` of the
    /// budget for whatever must follow.
    async fn set_secret(
        &self,
        budget: Budget,
        reserve: Duration,
        key: &AccessKeyId,
        secret: &SecretAccessKey,
        redactor: &ValueRedactor,
    ) -> Result<(), RuntimeExecutionError> {
        let limit = budget.step(RC_TIMEOUT, reserve, "setting the key's secret")?;
        let spec = self
            .exec(
                &[BUCKET_SECRET_ENV],
                RC_SCRIPT,
                limit,
                &[
                    "admin",
                    "user",
                    "passwd",
                    RC_ALIAS,
                    key.as_str(),
                    "--password-from-env",
                    BUCKET_SECRET_ENV,
                    "-q",
                ],
            )
            .secret_env(BUCKET_SECRET_ENV, SecretString::new(secret.expose()));
        self.run_ok(&spec, "setting the key's secret", redactor)
            .await
            .map(drop)
    }

    /// Signs one request with `secret` to learn whether it is the key's current
    /// secret. Used only after setting it failed, when the change may still
    /// have been applied.
    async fn check_secret(
        &self,
        budget: Budget,
        payload: &RotateBucketKeyPayload,
        disabled: bool,
    ) -> SecretCheck {
        let limit = match budget.step(VERIFY_TIMEOUT, Duration::ZERO, "checking the new secret") {
            Ok(limit) => limit,
            Err(error) => return SecretCheck::Unknown(error.to_string()),
        };
        let spec = self
            .exec(
                &[BUCKET_SECRET_ENV],
                VERIFY_SCRIPT,
                limit,
                &[payload.access_key_id.as_str(), payload.bucket.as_str()],
            )
            .secret_env(
                BUCKET_SECRET_ENV,
                SecretString::new(payload.secret_access_key.expose()),
            );
        match self.run(&spec).await {
            Ok(output) if output.success => secret_check(&output.stdout, disabled),
            Ok(output) => SecretCheck::Unknown(format!(
                "the signed request could not be sent ({})",
                crate::error::describe_exit(output.code)
            )),
            Err(error) => SecretCheck::Unknown(error.to_string()),
        }
    }

    async fn set_user_status(
        &self,
        payload: &BucketKeyPayload,
        locked: bool,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let Some(user) = self.user(budget, &payload.access_key_id).await? else {
            return Err(not_found(format!(
                "the key {} of {} does not exist",
                payload.access_key_id, payload.bucket
            )));
        };
        user.ensure_only(&payload.bucket)?;
        if !self.bucket_exists(budget, &payload.bucket).await? {
            return Err(not_found(format!(
                "the bucket {} does not exist",
                payload.bucket
            )));
        }
        let (action, operation) = if locked {
            ("disable", "disabling the key")
        } else {
            ("enable", "enabling the key")
        };
        self.rc_ok(
            budget,
            &[
                "admin",
                "user",
                action,
                RC_ALIAS,
                payload.access_key_id.as_str(),
                "-q",
            ],
            operation,
            &ValueRedactor::default(),
        )
        .await?;
        Ok(BucketLockResult {
            bucket: payload.bucket.clone(),
            access_key_id: payload.access_key_id.clone(),
            locked,
        })
    }
}

#[async_trait]
impl ObjectStorageAdmin for RcObjectStorageAdmin {
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        self.rc_ok(
            self.budget(),
            &["admin", "info", "cluster", RC_ALIAS, "--json"],
            "reading the instance state",
            &ValueRedactor::default(),
        )
        .await
    }

    async fn create_bucket(
        &self,
        payload: &CreateBucketPayload,
    ) -> Result<CreateBucketResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let bucket = &payload.bucket;
        let key = &payload.access_key_id;
        let throwaway = throwaway_secret();
        let redactor = ValueRedactor::new([
            payload.secret_access_key.expose().to_owned(),
            throwaway.clone(),
        ]);

        // Everything that can refuse runs before anything changes.
        let allocations = self
            .allocations(budget.step(RC_TIMEOUT, Duration::ZERO, "reading bucket quotas")?)
            .await?;
        let existed = allocations
            .iter()
            .any(|allocation| allocation.name == bucket.as_str());
        let user = self.user(budget, key).await?;
        if let Some(user) = &user {
            user.ensure_only(bucket)?;
        }
        self.check_capacity(&allocations, bucket, u64::from(payload.quota_mb.get()))
            .await?;

        if !existed {
            self.rc_ok(
                budget,
                &["mb", &target(bucket), "-q"],
                "creating the bucket",
                &redactor,
            )
            .await?;
        }
        self.set_quota(budget, bucket, payload.quota_mb.get())
            .await?;
        let limit = budget.step(RC_TIMEOUT, Duration::ZERO, "writing the bucket policy")?;
        let policy = self
            .exec(
                &[BUCKET_POLICY_ENV],
                RC_POLICY_SCRIPT,
                limit,
                &[
                    "admin",
                    "policy",
                    "create",
                    RC_ALIAS,
                    bucket.as_str(),
                    "/dev/stdin",
                    "-q",
                ],
            )
            .env(BUCKET_POLICY_ENV, bucket_policy(bucket));
        self.run_ok(&policy, "writing the bucket policy", &redactor)
            .await?;
        if user.is_none() {
            // `rc admin user add` takes the secret as an argument, which `ps`
            // shows, so the user starts with a throwaway and gets the real
            // secret from the environment. Its output echoes the throwaway and
            // is discarded. It is never run for an existing user: it would
            // reset that user's secret and enable a locked key.
            self.rc_ok(
                budget,
                &["admin", "user", "add", RC_ALIAS, key.as_str(), &throwaway],
                "creating the key's user",
                &redactor,
            )
            .await?;
        }
        self.set_secret(
            budget,
            Duration::ZERO,
            key,
            &payload.secret_access_key,
            &redactor,
        )
        .await?;
        if !user.as_ref().is_some_and(|user| user.holds(bucket)) {
            self.rc_ok(
                budget,
                &[
                    "admin",
                    "policy",
                    "attach",
                    RC_ALIAS,
                    bucket.as_str(),
                    "--user",
                    key.as_str(),
                    "-q",
                ],
                "attaching the bucket policy to the key",
                &redactor,
            )
            .await?;
        }
        self.rc_ok(
            budget,
            &["anonymous", "set", "private", &target(bucket), "-q"],
            "closing the bucket to anonymous access",
            &redactor,
        )
        .await?;
        Ok(CreateBucketResult {
            bucket: bucket.clone(),
            access_key_id: key.clone(),
            quota_mb: payload.quota_mb,
            created: !existed,
        })
    }

    async fn delete_bucket(
        &self,
        payload: &DeleteBucketPayload,
    ) -> Result<DeleteBucketResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let bucket = &payload.bucket;
        let key = payload.access_key_id.as_str();
        let redactor = ValueRedactor::default();

        let user = self.user(budget, &payload.access_key_id).await?;
        if let Some(user) = &user {
            user.ensure_only(bucket)?;
            // The key stops working before anything else is removed.
            self.rc_ok(
                budget,
                &["admin", "user", "disable", RC_ALIAS, key, "-q"],
                "disabling the key",
                &redactor,
            )
            .await?;
            let removed = self
                .run(&self.rc(budget, &["admin", "user", "rm", RC_ALIAS, key, "-q"])?)
                .await?;
            if !removed.success && removed.code != Some(RC_EXIT_NOT_FOUND) {
                return Err(failure(&removed, "removing the key's user", &redactor));
            }
        }
        let bucket_existed = self.bucket_exists(budget, bucket).await?;
        if bucket_existed {
            // Two quick policy calls follow; the removal leaves each of them
            // the shortest step.
            let limit = budget.step(
                REMOVE_TIMEOUT,
                MIN_STEP * 2,
                "removing the bucket and its objects",
            )?;
            // `--dangerous --yes` also aborts incomplete multipart uploads.
            let spec = self.exec(
                &[],
                RC_SCRIPT,
                limit,
                &[
                    "rb",
                    "--force",
                    "--dangerous",
                    "--yes",
                    &target(bucket),
                    "-q",
                ],
            );
            let removed = self.run(&spec).await?;
            if !removed.success && removed.code != Some(RC_EXIT_NOT_FOUND) {
                return Err(failure(
                    &removed,
                    "removing the bucket and its objects",
                    &redactor,
                ));
            }
        }
        let policy = self
            .run(&self.rc(
                budget,
                &[
                    "admin",
                    "policy",
                    "info",
                    RC_ALIAS,
                    bucket.as_str(),
                    "--json",
                ],
            )?)
            .await?;
        let policy_existed = match policy.code {
            Some(0) => true,
            Some(RC_EXIT_NOT_FOUND) => false,
            _ => return Err(failure(&policy, "reading the bucket policy", &redactor)),
        };
        if policy_existed {
            self.rc_ok(
                budget,
                &["admin", "policy", "rm", RC_ALIAS, bucket.as_str(), "-q"],
                "removing the bucket policy",
                &redactor,
            )
            .await?;
        }
        Ok(DeleteBucketResult {
            bucket: bucket.clone(),
            deleted: user.is_some() || bucket_existed || policy_existed,
        })
    }

    async fn rotate_bucket_key(
        &self,
        payload: &RotateBucketKeyPayload,
    ) -> Result<RotateBucketKeyResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let redactor = ValueRedactor::new([payload.secret_access_key.expose().to_owned()]);
        let Some(user) = self.user(budget, &payload.access_key_id).await? else {
            return Err(not_found(format!(
                "the key {} of {} does not exist",
                payload.access_key_id, payload.bucket
            )));
        };
        user.ensure_only(&payload.bucket)?;
        let result = RotateBucketKeyResult {
            bucket: payload.bucket.clone(),
            access_key_id: payload.access_key_id.clone(),
        };
        // The check below needs its time even when setting the secret uses all
        // of its own.
        let Err(error) = self
            .set_secret(
                budget,
                VERIFY_TIMEOUT,
                &payload.access_key_id,
                &payload.secret_access_key,
                &redactor,
            )
            .await
        else {
            return Ok(result);
        };
        // A failure, a timeout above all, does not prove the change was not
        // applied: RustFS may have taken it before rc stopped. Reporting `fail`
        // while the new secret is live would leave the control plane on a dead
        // one, so ask the instance which secret works.
        match self.check_secret(budget, payload, user.disabled()).await {
            SecretCheck::InEffect => {
                warn!(
                    access_key_id = %payload.access_key_id,
                    %error,
                    "rc reported a failure, but the new secret is in effect; the rotation is complete"
                );
                Ok(result)
            }
            SecretCheck::NotInEffect => Err(error),
            SecretCheck::Unknown(reason) => Err(RuntimeExecutionError::new(
                error_codes::BUCKET_OUTCOME_UNKNOWN,
                format!(
                    "{error}; whether the new secret took effect could not be checked ({reason}); send the same RotateBucketKey again"
                ),
            )),
        }
    }

    async fn set_bucket_quota(
        &self,
        payload: &SetBucketQuotaPayload,
    ) -> Result<SetBucketQuotaResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let budget = self.budget();
        let bucket = &payload.bucket;
        let allocations = self
            .allocations(budget.step(RC_TIMEOUT, Duration::ZERO, "reading bucket quotas")?)
            .await?;
        let Some(current) = allocations
            .iter()
            .find(|allocation| allocation.name == bucket.as_str())
        else {
            return Err(not_found(format!("the bucket {bucket} does not exist")));
        };
        let requested_mb = u64::from(payload.quota_mb.get());
        let current_mb = current.quota_bytes.div_ceil(MIB);
        if current_mb == 0 || requested_mb > current_mb {
            self.check_capacity(&allocations, bucket, requested_mb)
                .await?;
        }
        self.set_quota(budget, bucket, payload.quota_mb.get())
            .await?;
        Ok(SetBucketQuotaResult {
            bucket: bucket.clone(),
            quota_mb: payload.quota_mb,
        })
    }

    async fn lock_bucket(
        &self,
        payload: &BucketKeyPayload,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        self.set_user_status(payload, true).await
    }

    async fn unlock_bucket(
        &self,
        payload: &BucketKeyPayload,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        self.set_user_status(payload, false).await
    }

    /// Skipped with [`BUCKET_USAGE_BUSY`] while a command holds the instance,
    /// which may take minutes for a large `DeleteBucket`: the heartbeat keeps
    /// its last state instead of reporting the instance unavailable.
    async fn bucket_usage(&self) -> Result<Vec<ManagedBucketUsage>, RuntimeExecutionError> {
        let Ok(_operation) = tokio::time::timeout(USAGE_LOCK_WAIT, self.operations.lock()).await
        else {
            return Err(RuntimeExecutionError::new(
                BUCKET_USAGE_BUSY,
                "a bucket command is running; the usage read waits for the next heartbeat",
            ));
        };
        let mut usage = self
            .allocations(USAGE_TIMEOUT)
            .await?
            .into_iter()
            .filter(|allocation| BucketName::parse(&allocation.name).is_ok())
            .map(|allocation| ManagedBucketUsage {
                name: allocation.name,
                size_bytes: allocation.usage_bytes,
            })
            .collect::<Vec<_>>();
        usage.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(usage)
    }
}

/// Reads [`VERIFY_SCRIPT`]'s output: the body, then the HTTP status.
fn secret_check(stdout: &str, disabled: bool) -> SecretCheck {
    let (body, status) = stdout
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", stdout.trim()));
    if status.trim() == HTTP_OK {
        SecretCheck::InEffect
    } else if body.contains(SIGNATURE_MISMATCH) {
        SecretCheck::NotInEffect
    } else if disabled && body.contains(KEY_REFUSED) {
        // A locked key signed with its current secret.
        SecretCheck::InEffect
    } else {
        SecretCheck::Unknown(format!(
            "the signed request answered HTTP {}",
            status.trim()
        ))
    }
}

/// `sakala/<bucket>`.
fn target(bucket: &BucketName) -> String {
    format!("{RC_ALIAS}/{bucket}")
}

/// A listing key without the trailing `/` some `rc` versions add.
fn bucket_key(key: &str) -> String {
    key.trim_end_matches('/').to_owned()
}

/// The bucket's only policy: list and locate the bucket, and read, write,
/// delete, and upload in parts inside it. Exactly the staging document of
/// `scripts/object-storage.sh`.
fn bucket_policy(bucket: &BucketName) -> String {
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:GetBucketLocation", "s3:ListBucket", "s3:ListBucketMultipartUploads"],
                "Resource": [format!("arn:aws:s3:::{bucket}")],
            },
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetObject",
                    "s3:PutObject",
                    "s3:DeleteObject",
                    "s3:AbortMultipartUpload",
                    "s3:ListMultipartUploadParts",
                ],
                "Resource": [format!("arn:aws:s3:::{bucket}/*")],
            },
        ],
    })
    .to_string()
}

/// A random alphanumeric secret no one keeps.
fn throwaway_secret() -> String {
    let mut secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    secret.truncate(THROWAWAY_SECRET_LENGTH);
    secret
}

fn parse<T: for<'de> Deserialize<'de>>(
    stdout: &str,
    command: &str,
) -> Result<T, RuntimeExecutionError> {
    serde_json::from_str(stdout).map_err(|error| output_not_understood(command, &error))
}

fn output_not_understood(command: &str, error: &serde_json::Error) -> RuntimeExecutionError {
    RuntimeExecutionError::new(
        error_codes::BUCKET_OPERATION_FAILED,
        format!("the output of {command} was not understood: {error}"),
    )
}

/// Reads [`USAGE_SCRIPT`]'s output: the listing, then one quota document per
/// listed bucket.
fn parse_allocations(stdout: &str) -> Result<Vec<BucketAllocation>, RuntimeExecutionError> {
    let mut documents = serde_json::Deserializer::from_str(stdout).into_iter::<serde_json::Value>();
    let listing = documents
        .next()
        .ok_or_else(|| {
            RuntimeExecutionError::new(
                error_codes::BUCKET_OPERATION_FAILED,
                "the bucket listing was empty",
            )
        })?
        .map_err(|error| output_not_understood("rc ls", &error))?;
    let listing: Listing =
        serde_json::from_value(listing).map_err(|error| output_not_understood("rc ls", &error))?;
    let mut quotas = Vec::new();
    for document in documents {
        let document = document.map_err(|error| output_not_understood("rc quota info", &error))?;
        let info: QuotaInfo = serde_json::from_value(document)
            .map_err(|error| output_not_understood("rc quota info", &error))?;
        quotas.push(info);
    }
    listing
        .items
        .iter()
        .map(|item| {
            let name = bucket_key(&item.key);
            let info = quotas
                .iter()
                .find(|info| info.bucket == name)
                .ok_or_else(|| {
                    RuntimeExecutionError::new(
                        error_codes::BUCKET_OPERATION_FAILED,
                        format!("no quota was read for bucket {name}"),
                    )
                })?;
            Ok(BucketAllocation {
                quota_bytes: info.quota.unwrap_or_default(),
                usage_bytes: info.usage.unwrap_or_default(),
                name,
            })
        })
        .collect()
}

/// Turns a failed `docker exec ... rc` into the failure code the control plane
/// acts on. Docker failures, an `rc` stopped by its time limit, and an
/// unreachable or refusing instance are `bucket_admin_unavailable`; everything
/// else is `bucket_operation_failed` with the redacted end of `rc`'s stderr.
fn failure(
    output: &ProcessOutput,
    operation: &str,
    redactor: &ValueRedactor,
) -> RuntimeExecutionError {
    let docker_failed = output
        .code
        .is_some_and(|code| DOCKER_EXEC_FAILURES.contains(&code))
        || DOCKER_DAEMON_ERRORS
            .iter()
            .any(|marker| output.stderr.contains(marker));
    let unreachable = matches!(output.code, Some(RC_EXIT_NETWORK | RC_EXIT_AUTH))
        || RC_UNREACHABLE_MARKERS
            .iter()
            .any(|marker| output.stderr.contains(marker) || output.stdout.contains(marker));
    let tail = stderr_tail(&output.stderr, redactor);
    let status = crate::error::describe_exit(output.code);
    if docker_failed {
        unavailable(format!(
            "{operation} failed: the object storage container could not run rc ({status}): {tail}"
        ))
    } else if output.code == Some(TIMEOUT_EXIT) {
        unavailable(format!(
            "{operation} failed: rc did not finish in time and was stopped ({status})"
        ))
    } else if unreachable {
        unavailable(format!(
            "{operation} failed: rc could not reach the instance or was refused its root key ({status}): {tail}"
        ))
    } else {
        RuntimeExecutionError::new(
            error_codes::BUCKET_OPERATION_FAILED,
            format!("{operation} failed ({status}): {tail}"),
        )
    }
}

fn stderr_tail(stderr: &str, redactor: &ValueRedactor) -> String {
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let tail = lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join(" | ");
    let mut tail = redactor.redact(&tail);
    if tail.len() > STDERR_TAIL_MAX_BYTES {
        let mut end = STDERR_TAIL_MAX_BYTES;
        while !tail.is_char_boundary(end) {
            end -= 1;
        }
        tail.truncate(end);
    }
    tail
}

fn unavailable(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::BUCKET_ADMIN_UNAVAILABLE, message)
}

fn not_found(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::BUCKET_NOT_FOUND, message)
}

fn capacity_exceeded(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::BUCKET_CAPACITY_EXCEEDED, message)
}

#[cfg(test)]
mod tests;
