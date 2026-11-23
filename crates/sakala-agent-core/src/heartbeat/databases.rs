//! `metadata.database` and `metadata.databases` of a database host (ADR-020).
//!
//! `metadata.database` goes into every heartbeat: the endpoint runtime nodes
//! use and whether the instance answered the last time the agent asked.
//! `metadata.databases` lists every managed database with its size, read at
//! most once per interval and sent only in the heartbeat that follows a
//! successful read, like `workloads.metrics`. A heartbeat without it says
//! nothing new about sizes; it never means the databases are gone.

use std::{sync::Arc, time::Duration};

use sakala_agent_protocol::DatabaseEndpoint;
use serde_json::{Value, json};

use super::periodic_read::{PeriodicRead, ReadMessages};
use crate::ports::DatabaseAdmin;

/// How often the sizes are read. `pg_database_size()` walks every file of every
/// managed database, and the control plane's quota watcher works in minutes.
pub const DATABASE_SIZE_INTERVAL: Duration = Duration::from_secs(60);

/// Most entries of `metadata.databases`, about 60 KiB of JSON, well inside the
/// API's 256 KiB heartbeat limit. ADR-020 sizes one instance for hundreds of
/// databases. `detail_counts.databases` keeps the full count.
pub const MAX_REPORTED_DATABASES: usize = 1_000;

/// `metadata.database`.
const DATABASE_METADATA_KEY: &str = "database";

/// `metadata.databases` and `metadata.detail_counts.databases`.
const DATABASES_METADATA_KEY: &str = "databases";

/// How long one size read may take before the heartbeat goes without it.
fn read_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(100)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(10)
    }
}

/// Adds the database host's part of the heartbeat.
pub struct DatabaseHeartbeat {
    admin: Arc<dyn DatabaseAdmin>,
    endpoint: DatabaseEndpoint,
    max_items: usize,
    reads: PeriodicRead,
}

impl DatabaseHeartbeat {
    #[must_use]
    pub fn new(admin: Arc<dyn DatabaseAdmin>, endpoint: DatabaseEndpoint) -> Self {
        Self::with_bounds(
            admin,
            endpoint,
            DATABASE_SIZE_INTERVAL,
            MAX_REPORTED_DATABASES,
        )
    }

    #[must_use]
    pub fn with_bounds(
        admin: Arc<dyn DatabaseAdmin>,
        endpoint: DatabaseEndpoint,
        interval: Duration,
        max_items: usize,
    ) -> Self {
        Self {
            admin,
            endpoint,
            max_items,
            reads: PeriodicRead::new(
                interval,
                read_timeout(),
                ReadMessages {
                    failing: "managed databases could not be read; the node reports degraded until a read succeeds",
                    recovered: "managed databases are read again",
                },
            ),
        }
    }

    /// Adds `metadata.database`, and `metadata.databases` with its count when
    /// a read was due and succeeded. Returns whether the instance answered the
    /// last read, which keeps a node that cannot reach it `degraded`.
    pub(crate) async fn insert(&self, metadata: &mut Value) -> bool {
        let sizes = self.reads.read_if_due(|| self.admin.database_sizes()).await;
        let available = self.reads.available();
        metadata[DATABASE_METADATA_KEY] = json!({
            "endpoint": self.endpoint,
            "available": available,
        });
        if let Some(sizes) = sizes {
            metadata[DATABASES_METADATA_KEY] = Value::Array(
                sizes
                    .iter()
                    .take(self.max_items)
                    .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
                    .collect(),
            );
            metadata["detail_counts"][DATABASES_METADATA_KEY] = json!(sizes.len());
        }
        available
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sakala_agent_protocol::ManagedDatabaseSize;

    use super::*;
    use crate::ports::RuntimeExecutionError;

    struct SizedAdmin {
        reads: AtomicUsize,
        fail: AtomicBool,
        hang: bool,
        count: usize,
    }

    impl SizedAdmin {
        fn new(count: usize, fail: bool, hang: bool) -> Arc<Self> {
            Arc::new(Self {
                reads: AtomicUsize::new(0),
                fail: AtomicBool::new(fail),
                hang,
                count,
            })
        }
    }

    #[async_trait::async_trait]
    impl DatabaseAdmin for SizedAdmin {
        async fn database_sizes(&self) -> Result<Vec<ManagedDatabaseSize>, RuntimeExecutionError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(RuntimeExecutionError::new(
                    "database_admin_unavailable",
                    "connection refused",
                ));
            }
            Ok((0..self.count)
                .map(|index| ManagedDatabaseSize {
                    name: format!("p_{index:016}"),
                    size_bytes: 7_000_000 + index as u64,
                })
                .collect())
        }
    }

    fn endpoint() -> DatabaseEndpoint {
        DatabaseEndpoint::new("10.0.47.10", 5432).expect("endpoint")
    }

    fn metadata() -> Value {
        json!({ "detail_counts": {} })
    }

    #[tokio::test]
    async fn sizes_are_sent_once_per_read_and_the_endpoint_always() {
        let admin = SizedAdmin::new(2, false, false);
        let heartbeat = DatabaseHeartbeat::with_bounds(
            admin.clone(),
            endpoint(),
            Duration::from_secs(3_600),
            10,
        );

        let mut first = metadata();
        assert!(heartbeat.insert(&mut first).await);
        let mut second = metadata();
        assert!(heartbeat.insert(&mut second).await);

        assert_eq!(
            first["database"],
            json!({ "endpoint": { "host": "10.0.47.10", "port": 5432 }, "available": true })
        );
        assert_eq!(
            first["databases"],
            json!([
                { "name": "p_0000000000000000", "size_bytes": 7_000_000 },
                { "name": "p_0000000000000001", "size_bytes": 7_000_001 },
            ])
        );
        assert_eq!(first["detail_counts"]["databases"], 2);
        assert_eq!(second["database"], first["database"]);
        assert!(second.get("databases").is_none());
        assert!(second["detail_counts"].get("databases").is_none());
        assert_eq!(admin.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_list_is_bounded_and_the_count_kept() {
        let heartbeat = DatabaseHeartbeat::with_bounds(
            SizedAdmin::new(7, false, false),
            endpoint(),
            Duration::ZERO,
            5,
        );
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["databases"].as_array().expect("list").len(), 5);
        assert_eq!(sent["databases"][4]["name"], "p_0000000000000004");
        assert_eq!(sent["detail_counts"]["databases"], 7);
    }

    #[tokio::test]
    async fn an_empty_instance_sends_an_empty_list() {
        let heartbeat = DatabaseHeartbeat::with_bounds(
            SizedAdmin::new(0, false, false),
            endpoint(),
            Duration::ZERO,
            5,
        );
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["databases"], json!([]));
        assert_eq!(sent["detail_counts"]["databases"], 0);
    }

    #[tokio::test]
    async fn a_failed_or_hanging_read_omits_the_list_and_reports_unavailable_until_one_succeeds() {
        for admin in [
            SizedAdmin::new(1, true, false),
            SizedAdmin::new(1, false, true),
        ] {
            let heartbeat =
                DatabaseHeartbeat::with_bounds(admin.clone(), endpoint(), Duration::ZERO, 5);
            let mut sent = metadata();

            assert!(!heartbeat.insert(&mut sent).await);
            assert_eq!(sent["database"]["available"], false);
            assert!(sent.get("databases").is_none());
            assert!(sent["detail_counts"].get("databases").is_none());
        }

        let admin = SizedAdmin::new(1, true, false);
        let heartbeat =
            DatabaseHeartbeat::with_bounds(admin.clone(), endpoint(), Duration::ZERO, 5);
        assert!(!heartbeat.insert(&mut metadata()).await);
        admin.fail.store(false, Ordering::SeqCst);
        let mut recovered = metadata();
        assert!(heartbeat.insert(&mut recovered).await);
        assert_eq!(recovered["database"]["available"], true);
        assert_eq!(recovered["detail_counts"]["databases"], 1);
    }

    #[tokio::test]
    async fn a_failure_is_not_retried_before_the_interval() {
        let admin = SizedAdmin::new(1, true, false);
        let heartbeat = DatabaseHeartbeat::with_bounds(
            admin.clone(),
            endpoint(),
            Duration::from_secs(3_600),
            5,
        );
        assert!(!heartbeat.insert(&mut metadata()).await);
        assert!(!heartbeat.insert(&mut metadata()).await);
        assert_eq!(admin.reads.load(Ordering::SeqCst), 1);
    }
}
