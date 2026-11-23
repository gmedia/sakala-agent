//! [`ObjectStorageAdmin`] over RustFS's CLI `rc`, run with `docker exec` inside
//! the instance's own container.
//!
//! The root key never enters the agent: every call builds the `rc` alias inside
//! the container from the mounted secret files, with a fixed script identical to
//! sakala-deployment's `scripts/object-storage.sh`. Process arguments are only
//! validated bucket names, access key IDs, and integers; nothing is ever
//! interpolated into a script. A tenant secret reaches `docker exec` through
//! `-e NAME` with the value in the child's environment, never in an argument.
//! Operations and usage reads run one at a time.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use sakala_agent_core::{
    logs::redactor::ValueRedactor,
    ports::{ObjectStorageAdmin, RuntimeExecutionError, SecretString},
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
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use crate::{CommandSpec, NullOutputSink, ProcessOutput, ProcessRunner, TokioProcessRunner};

/// The `rc` alias the fixed scripts define.
pub const RC_ALIAS: &str = "sakala";

/// Carries a tenant secret into the container, by name only.
pub const BUCKET_SECRET_ENV: &str = "SAKALA_BUCKET_SECRET";

/// Carries the bucket policy document into the container.
pub const BUCKET_POLICY_ENV: &str = "SAKALA_BUCKET_POLICY";

/// Runs `rc "$@"` against the instance as root. The alias URL is built inside
/// the container from the mounted root key files, so the root key is in no
/// argument list and never leaves the container. Identical to `rc()` in
/// sakala-deployment's `scripts/object-storage.sh`.
const RC_SCRIPT: &str = r#"RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS" exec rc --no-color --no-progress "$@""#;

/// [`RC_SCRIPT`] with the policy document from [`BUCKET_POLICY_ENV`] on stdin,
/// for `rc admin policy create ... /dev/stdin`.
const RC_POLICY_SCRIPT: &str = r#"printf '%s' "$SAKALA_BUCKET_POLICY" | RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS" rc --no-color --no-progress "$@""#;

/// Prints the bucket listing, then the quota and usage of every bucket, as a
/// stream of JSON documents. Bucket names cannot hold quotes or spaces, so the
/// listing's `"key"` fields are safe to cut out with `grep` and `sed`, and the
/// agent checks that every listed bucket got its document. A failed listing
/// or quota read fails the whole script with `rc`'s exit status.
const USAGE_SCRIPT: &str = r#"set -e
export RC_HOST_sakala="http://$(cat /run/secrets/rustfs-access-key):$(cat /run/secrets/rustfs-secret-key)@$RUSTFS_ADDRESS"
listing=$(rc --no-color --no-progress ls sakala/ --json)
printf '%s\n' "$listing"
for name in $(printf '%s\n' "$listing" | grep -o '"key": *"[^"]*"' | sed -e 's/^"key": *"//' -e 's/"$//' -e 's:/$::'); do
  rc --no-color --no-progress quota info "sakala/$name" --json
done"#;

/// `rc` exit statuses (rc 0.1.37): the instance could not be reached, refused
/// the root key, or does not have the named object.
const RC_EXIT_NETWORK: i32 = 3;
const RC_EXIT_AUTH: i32 = 4;
const RC_EXIT_NOT_FOUND: i32 = 5;

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

/// Bounds one `rc` call.
const RC_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds the usage read, so a hung instance costs a heartbeat no more.
const USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Bounds removing a bucket with its objects, which grows with the object
/// count. Shorter than the default command timeout of 900 seconds.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(600);

/// Bounds `df`.
const DISK_TIMEOUT: Duration = Duration::from_secs(10);

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
}

pub struct RcObjectStorageAdmin {
    runner: Arc<dyn ProcessRunner>,
    config: RustfsAdminConfig,
    /// Serializes every operation and usage read on the instance.
    operations: Mutex<()>,
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

/// `rc admin user info sakala <key> --json`. Its `status` is never read: no
/// operation depends on it, and `CreateBucket` must leave it as it is.
#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    policies: Vec<String>,
}

impl UserInfo {
    /// Refuses a user that carries any policy besides the bucket's own, which
    /// would give the key access the control plane does not know about.
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

    /// `docker exec [-e NAME]... <container> sh -c <script> rc <args>...`.
    fn exec(&self, env_names: &[&str], script: &str, args: &[&str]) -> CommandSpec {
        let mut spec = CommandSpec::new("docker").arg("exec");
        for name in env_names {
            spec = spec.arg("-e").arg(*name);
        }
        spec = spec
            .arg(&self.config.container)
            .arg("sh")
            .arg("-c")
            .arg(script)
            .arg("rc");
        for arg in args {
            spec = spec.arg(*arg);
        }
        spec.timeout(RC_TIMEOUT)
    }

    fn rc(&self, args: &[&str]) -> CommandSpec {
        self.exec(&[], RC_SCRIPT, args)
    }

    fn rc_with_secret(&self, secret: &SecretAccessKey, args: &[&str]) -> CommandSpec {
        self.exec(&[BUCKET_SECRET_ENV], RC_SCRIPT, args)
            .secret_env(BUCKET_SECRET_ENV, SecretString::new(secret.expose()))
    }

    fn rc_with_policy(&self, bucket: &BucketName) -> CommandSpec {
        let target = bucket.as_str();
        self.exec(
            &[BUCKET_POLICY_ENV],
            RC_POLICY_SCRIPT,
            &[
                "admin",
                "policy",
                "create",
                RC_ALIAS,
                target,
                "/dev/stdin",
                "-q",
            ],
        )
        .env(BUCKET_POLICY_ENV, bucket_policy(bucket))
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

    /// The user named by `key`, or `None` when RustFS does not know it.
    async fn user(&self, key: &AccessKeyId) -> Result<Option<UserInfo>, RuntimeExecutionError> {
        let output = self
            .run(&self.rc(&["admin", "user", "info", RC_ALIAS, key.as_str(), "--json"]))
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

    async fn bucket_names(&self) -> Result<Vec<String>, RuntimeExecutionError> {
        let output = self
            .run_ok(
                &self.rc(&["ls", &format!("{RC_ALIAS}/"), "--json"]),
                "listing buckets",
                &ValueRedactor::default(),
            )
            .await?;
        let listing: Listing = parse(&output.stdout, "rc ls")?;
        Ok(listing
            .items
            .into_iter()
            .map(|item| bucket_key(&item.key))
            .collect())
    }

    /// Every bucket on the instance with its quota and usage, managed or not.
    async fn allocations(&self) -> Result<Vec<BucketAllocation>, RuntimeExecutionError> {
        let spec = self.exec(&[], USAGE_SCRIPT, &[]).timeout(USAGE_TIMEOUT);
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
        bucket: &BucketName,
        quota_mb: u32,
    ) -> Result<(), RuntimeExecutionError> {
        self.run_ok(
            &self.rc(&[
                "quota",
                "set",
                &target(bucket),
                &format!("{quota_mb}MiB"),
                "-q",
            ]),
            "setting the bucket quota",
            &ValueRedactor::default(),
        )
        .await
        .map(drop)
    }

    async fn set_secret(
        &self,
        key: &AccessKeyId,
        secret: &SecretAccessKey,
        redactor: &ValueRedactor,
    ) -> Result<(), RuntimeExecutionError> {
        self.run_ok(
            &self.rc_with_secret(
                secret,
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
            ),
            "setting the key's secret",
            redactor,
        )
        .await
        .map(drop)
    }

    async fn set_user_status(
        &self,
        payload: &BucketKeyPayload,
        locked: bool,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        if self.user(&payload.access_key_id).await?.is_none() {
            return Err(not_found(format!(
                "the key {} of {} does not exist",
                payload.access_key_id, payload.bucket
            )));
        }
        let action = if locked { "disable" } else { "enable" };
        self.run_ok(
            &self.rc(&[
                "admin",
                "user",
                action,
                RC_ALIAS,
                payload.access_key_id.as_str(),
                "-q",
            ]),
            if locked {
                "disabling the key"
            } else {
                "enabling the key"
            },
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
        self.run_ok(
            &self.rc(&["admin", "info", "cluster", RC_ALIAS, "--json"]),
            "reading the instance state",
            &ValueRedactor::default(),
        )
        .await
        .map(drop)
    }

    async fn create_bucket(
        &self,
        payload: &CreateBucketPayload,
    ) -> Result<CreateBucketResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let bucket = &payload.bucket;
        let key = &payload.access_key_id;
        let throwaway = throwaway_secret();
        let redactor = ValueRedactor::new([
            payload.secret_access_key.expose().to_owned(),
            throwaway.clone(),
        ]);

        // Everything that can refuse runs before anything changes.
        let allocations = self.allocations().await?;
        let existed = allocations
            .iter()
            .any(|allocation| allocation.name == bucket.as_str());
        let user = self.user(key).await?;
        if let Some(user) = &user {
            user.ensure_only(bucket)?;
        }
        self.check_capacity(&allocations, bucket, u64::from(payload.quota_mb.get()))
            .await?;

        if !existed {
            self.run_ok(
                &self.rc(&["mb", &target(bucket), "-q"]),
                "creating the bucket",
                &redactor,
            )
            .await?;
        }
        self.set_quota(bucket, payload.quota_mb.get()).await?;
        self.run_ok(
            &self.rc_with_policy(bucket),
            "writing the bucket policy",
            &redactor,
        )
        .await?;
        if user.is_none() {
            // `rc admin user add` takes the secret as an argument, which `ps`
            // shows, so the user starts with a throwaway and gets the real
            // secret from the environment. Its output echoes the throwaway and
            // is discarded. It is never run for an existing user: it would
            // reset that user's secret and enable a locked key.
            self.run_ok(
                &self.rc(&["admin", "user", "add", RC_ALIAS, key.as_str(), &throwaway]),
                "creating the key's user",
                &redactor,
            )
            .await?;
        }
        self.set_secret(key, &payload.secret_access_key, &redactor)
            .await?;
        if !user.as_ref().is_some_and(|user| user.holds(bucket)) {
            self.run_ok(
                &self.rc(&[
                    "admin",
                    "policy",
                    "attach",
                    RC_ALIAS,
                    bucket.as_str(),
                    "--user",
                    key.as_str(),
                    "-q",
                ]),
                "attaching the bucket policy to the key",
                &redactor,
            )
            .await?;
        }
        self.run_ok(
            &self.rc(&["anonymous", "set", "private", &target(bucket), "-q"]),
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
        let bucket = &payload.bucket;
        let key = payload.access_key_id.as_str();
        let redactor = ValueRedactor::default();

        let user = self.user(&payload.access_key_id).await?;
        if let Some(user) = &user {
            user.ensure_only(bucket)?;
            // The key stops working before anything else is removed.
            self.run_ok(
                &self.rc(&["admin", "user", "disable", RC_ALIAS, key, "-q"]),
                "disabling the key",
                &redactor,
            )
            .await?;
            let removed = self
                .run(&self.rc(&["admin", "user", "rm", RC_ALIAS, key, "-q"]))
                .await?;
            if !removed.success && removed.code != Some(RC_EXIT_NOT_FOUND) {
                return Err(failure(&removed, "removing the key's user", &redactor));
            }
        }
        let bucket_existed = self
            .bucket_names()
            .await?
            .iter()
            .any(|name| name == bucket.as_str());
        if bucket_existed {
            // `--dangerous --yes` also aborts incomplete multipart uploads.
            let spec = self
                .rc(&[
                    "rb",
                    "--force",
                    "--dangerous",
                    "--yes",
                    &target(bucket),
                    "-q",
                ])
                .timeout(REMOVE_TIMEOUT);
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
            .run(&self.rc(&[
                "admin",
                "policy",
                "info",
                RC_ALIAS,
                bucket.as_str(),
                "--json",
            ]))
            .await?;
        let policy_existed = match policy.code {
            Some(0) => true,
            Some(RC_EXIT_NOT_FOUND) => false,
            _ => return Err(failure(&policy, "reading the bucket policy", &redactor)),
        };
        if policy_existed {
            self.run_ok(
                &self.rc(&["admin", "policy", "rm", RC_ALIAS, bucket.as_str(), "-q"]),
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
        let redactor = ValueRedactor::new([payload.secret_access_key.expose().to_owned()]);
        let Some(user) = self.user(&payload.access_key_id).await? else {
            return Err(not_found(format!(
                "the key {} of {} does not exist",
                payload.access_key_id, payload.bucket
            )));
        };
        user.ensure_only(&payload.bucket)?;
        self.set_secret(
            &payload.access_key_id,
            &payload.secret_access_key,
            &redactor,
        )
        .await?;
        Ok(RotateBucketKeyResult {
            bucket: payload.bucket.clone(),
            access_key_id: payload.access_key_id.clone(),
        })
    }

    async fn set_bucket_quota(
        &self,
        payload: &SetBucketQuotaPayload,
    ) -> Result<SetBucketQuotaResult, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let bucket = &payload.bucket;
        let allocations = self.allocations().await?;
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
        self.set_quota(bucket, payload.quota_mb.get()).await?;
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

    async fn bucket_usage(&self) -> Result<Vec<ManagedBucketUsage>, RuntimeExecutionError> {
        let _operation = self.operations.lock().await;
        let mut usage = self
            .allocations()
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
/// acts on. Docker failures and an unreachable or refusing instance are
/// `bucket_admin_unavailable`; everything else is `bucket_operation_failed`
/// with the redacted end of `rc`'s stderr.
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
    let tail = stderr_tail(&output.stderr, redactor);
    let status = crate::error::describe_exit(output.code);
    if docker_failed {
        return unavailable(format!(
            "{operation} failed: the object storage container could not run rc ({status}): {tail}"
        ));
    }
    match output.code {
        Some(RC_EXIT_NETWORK | RC_EXIT_AUTH) => unavailable(format!(
            "{operation} failed: rc could not reach the instance or was refused its root key ({status}): {tail}"
        )),
        _ => RuntimeExecutionError::new(
            error_codes::BUCKET_OPERATION_FAILED,
            format!("{operation} failed ({status}): {tail}"),
        ),
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
