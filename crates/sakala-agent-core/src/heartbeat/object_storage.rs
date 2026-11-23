//! `metadata.object_storage` and `metadata.buckets` of an object storage host
//! (ADR-021).
//!
//! `metadata.object_storage` goes into every heartbeat: the public endpoint
//! and signing region tenants use, the node's allocatable capacity, and whether
//! the instance answered the last usage read. `metadata.buckets` lists every
//! managed bucket with the bytes it holds, read at most once per interval and
//! sent only in the heartbeat that follows a successful read. A heartbeat
//! without it says nothing new about usage; it never means the buckets are
//! gone.

use std::{sync::Arc, time::Duration};

use sakala_agent_protocol::{ObjectStorageMetadata, Region};
use serde_json::{Value, json};

use super::periodic_read::{PeriodicRead, ReadMessages};
use crate::ports::{BUCKET_USAGE_BUSY, ObjectStorageAdmin};

/// How often bucket usage is read. One read asks the engine for every
/// bucket's quota and usage, and the control plane's usage sweep works in
/// minutes.
pub const BUCKET_USAGE_INTERVAL: Duration = Duration::from_secs(60);

/// Most entries of `metadata.buckets`, about 60 KiB of JSON, well inside the
/// API's 256 KiB heartbeat limit. `detail_counts.buckets` keeps the full count.
pub const MAX_REPORTED_BUCKETS: usize = 1_000;

/// `metadata.object_storage`.
const OBJECT_STORAGE_METADATA_KEY: &str = "object_storage";

/// `metadata.buckets` and `metadata.detail_counts.buckets`.
const BUCKETS_METADATA_KEY: &str = "buckets";

/// How long one usage read may take before the heartbeat goes without it.
fn read_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(100)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(20)
    }
}

/// What an object storage host reports about itself, from its configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectStorageIdentity {
    pub endpoint: String,
    pub region: Region,
    pub capacity_mb: u64,
}

/// Adds the object storage host's part of the heartbeat.
pub struct ObjectStorageHeartbeat {
    admin: Arc<dyn ObjectStorageAdmin>,
    identity: ObjectStorageIdentity,
    max_items: usize,
    reads: PeriodicRead,
}

impl ObjectStorageHeartbeat {
    #[must_use]
    pub fn new(admin: Arc<dyn ObjectStorageAdmin>, identity: ObjectStorageIdentity) -> Self {
        Self::with_bounds(admin, identity, BUCKET_USAGE_INTERVAL, MAX_REPORTED_BUCKETS)
    }

    #[must_use]
    pub fn with_bounds(
        admin: Arc<dyn ObjectStorageAdmin>,
        identity: ObjectStorageIdentity,
        interval: Duration,
        max_items: usize,
    ) -> Self {
        Self {
            admin,
            identity,
            max_items,
            reads: PeriodicRead::new(
                interval,
                read_timeout(),
                ReadMessages {
                    failing: "managed buckets could not be read; the node reports degraded until a read succeeds",
                    recovered: "managed buckets are read again",
                },
                Some(BUCKET_USAGE_BUSY),
            ),
        }
    }

    /// Adds `metadata.object_storage`, and `metadata.buckets` with its count
    /// when a read was due and succeeded. Returns whether the instance answered
    /// the last read, which keeps a node that cannot reach it `degraded`.
    pub(crate) async fn insert(&self, metadata: &mut Value) -> bool {
        let usage = self.reads.read_if_due(|| self.admin.bucket_usage()).await;
        let available = self.reads.available();
        let object_storage = ObjectStorageMetadata {
            endpoint: self.identity.endpoint.clone(),
            region: self.identity.region.clone(),
            capacity_mb: self.identity.capacity_mb,
            available,
        };
        metadata[OBJECT_STORAGE_METADATA_KEY] =
            serde_json::to_value(object_storage).unwrap_or(Value::Null);
        if let Some(mut usage) = usage {
            usage.sort_by(|left, right| left.name.cmp(&right.name));
            metadata[BUCKETS_METADATA_KEY] = Value::Array(
                usage
                    .iter()
                    .take(self.max_items)
                    .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
                    .collect(),
            );
            metadata["detail_counts"][BUCKETS_METADATA_KEY] = json!(usage.len());
        }
        available
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sakala_agent_protocol::ManagedBucketUsage;

    use super::*;
    use crate::ports::RuntimeExecutionError;

    struct UsageAdmin {
        reads: AtomicUsize,
        fail: AtomicBool,
        busy: AtomicBool,
        hang: bool,
        count: usize,
    }

    impl UsageAdmin {
        fn new(count: usize, fail: bool, hang: bool) -> Arc<Self> {
            Arc::new(Self {
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(fail),
                busy: AtomicBool::new(false),
                hang,
                count,
            })
        }
    }

    #[async_trait::async_trait]
    impl ObjectStorageAdmin for UsageAdmin {
        async fn bucket_usage(&self) -> Result<Vec<ManagedBucketUsage>, RuntimeExecutionError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if self.busy.load(Ordering::SeqCst) {
                return Err(RuntimeExecutionError::new(
                    BUCKET_USAGE_BUSY,
                    "a bucket command is running",
                ));
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(RuntimeExecutionError::new(
                    "bucket_admin_unavailable",
                    "the object storage container is not running",
                ));
            }
            // Reverse order, to prove the heartbeat sorts by name.
            Ok((0..self.count)
                .rev()
                .map(|index| ManagedBucketUsage {
                    name: format!("b-{index:016}"),
                    size_bytes: 1_048_576 + index as u64,
                })
                .collect())
        }
    }

    fn identity() -> ObjectStorageIdentity {
        ObjectStorageIdentity {
            endpoint: "https://s3.sakala.app".to_owned(),
            region: Region::parse("us-east-1").expect("region"),
            capacity_mb: 5_120,
        }
    }

    fn metadata() -> Value {
        json!({ "detail_counts": {} })
    }

    #[tokio::test]
    async fn usage_is_sent_once_per_read_and_the_identity_always() {
        let admin = UsageAdmin::new(2, false, false);
        let heartbeat = ObjectStorageHeartbeat::with_bounds(
            admin.clone(),
            identity(),
            Duration::from_secs(3_600),
            10,
        );

        let mut first = metadata();
        assert!(heartbeat.insert(&mut first).await);
        let mut second = metadata();
        assert!(heartbeat.insert(&mut second).await);

        assert_eq!(
            first["object_storage"],
            json!({
                "endpoint": "https://s3.sakala.app",
                "region": "us-east-1",
                "capacity_mb": 5120,
                "available": true,
            })
        );
        assert_eq!(
            first["buckets"],
            json!([
                { "name": "b-0000000000000000", "size_bytes": 1_048_576 },
                { "name": "b-0000000000000001", "size_bytes": 1_048_577 },
            ])
        );
        assert_eq!(first["detail_counts"]["buckets"], 2);
        assert_eq!(second["object_storage"], first["object_storage"]);
        assert!(second.get("buckets").is_none());
        assert!(second["detail_counts"].get("buckets").is_none());
        assert_eq!(admin.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_list_is_bounded_and_the_count_kept() {
        let heartbeat = ObjectStorageHeartbeat::with_bounds(
            UsageAdmin::new(7, false, false),
            identity(),
            Duration::ZERO,
            5,
        );
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["buckets"].as_array().expect("list").len(), 5);
        assert_eq!(sent["buckets"][4]["name"], "b-0000000000000004");
        assert_eq!(sent["detail_counts"]["buckets"], 7);
    }

    #[tokio::test]
    async fn an_instance_without_managed_buckets_sends_an_empty_list() {
        let heartbeat = ObjectStorageHeartbeat::with_bounds(
            UsageAdmin::new(0, false, false),
            identity(),
            Duration::ZERO,
            5,
        );
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["buckets"], json!([]));
        assert_eq!(sent["detail_counts"]["buckets"], 0);
    }

    #[tokio::test]
    async fn a_failed_or_hanging_read_omits_the_list_and_reports_unavailable_until_one_succeeds() {
        for admin in [
            UsageAdmin::new(1, true, false),
            UsageAdmin::new(1, false, true),
        ] {
            let heartbeat =
                ObjectStorageHeartbeat::with_bounds(admin.clone(), identity(), Duration::ZERO, 5);
            let mut sent = metadata();

            assert!(!heartbeat.insert(&mut sent).await);
            assert_eq!(sent["object_storage"]["available"], false);
            assert_eq!(sent["object_storage"]["capacity_mb"], 5120);
            assert!(sent.get("buckets").is_none());
            assert!(sent["detail_counts"].get("buckets").is_none());
        }

        let admin = UsageAdmin::new(1, true, false);
        let heartbeat =
            ObjectStorageHeartbeat::with_bounds(admin.clone(), identity(), Duration::ZERO, 5);
        assert!(!heartbeat.insert(&mut metadata()).await);
        admin.fail.store(false, Ordering::SeqCst);
        let mut recovered = metadata();
        assert!(heartbeat.insert(&mut recovered).await);
        assert_eq!(recovered["object_storage"]["available"], true);
        assert_eq!(recovered["detail_counts"]["buckets"], 1);
    }

    #[tokio::test]
    async fn a_failure_is_not_retried_before_the_interval() {
        let admin = UsageAdmin::new(1, true, false);
        let heartbeat = ObjectStorageHeartbeat::with_bounds(
            admin.clone(),
            identity(),
            Duration::from_secs(3_600),
            5,
        );
        assert!(!heartbeat.insert(&mut metadata()).await);
        assert!(!heartbeat.insert(&mut metadata()).await);
        assert_eq!(admin.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_read_skipped_while_a_command_runs_keeps_the_node_available_and_retries() {
        // A long DeleteBucket holds the instance, far past the 20 s read limit.
        let admin = UsageAdmin::new(1, false, false);
        admin.busy.store(true, Ordering::SeqCst);
        let heartbeat = ObjectStorageHeartbeat::with_bounds(
            admin.clone(),
            identity(),
            Duration::from_secs(3_600),
            5,
        );
        for _ in 0..3 {
            let mut sent = metadata();
            assert!(heartbeat.insert(&mut sent).await, "busy is not unavailable");
            assert_eq!(sent["object_storage"]["available"], true);
            assert!(sent.get("buckets").is_none());
        }
        // A skipped read is tried again at the next heartbeat, not after the
        // interval.
        assert_eq!(admin.reads.load(Ordering::SeqCst), 3);

        admin.busy.store(false, Ordering::SeqCst);
        let mut after = metadata();
        assert!(heartbeat.insert(&mut after).await);
        assert_eq!(after["detail_counts"]["buckets"], 1);
    }

    #[test]
    fn the_default_interval_is_sixty_seconds() {
        assert_eq!(BUCKET_USAGE_INTERVAL, Duration::from_secs(60));
        assert_eq!(MAX_REPORTED_BUCKETS, 1_000);
    }
}
