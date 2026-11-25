//! `metadata.valkey` and `metadata.valkeys` of a Valkey host (ADR-023).
//!
//! `metadata.valkey` goes into every heartbeat: the node's memory budget for
//! stores and whether the last read of the stores succeeded. `metadata.valkeys`
//! lists every store with its container status and `INFO` readings, read at
//! most once per interval and sent only in the heartbeat that follows a
//! successful read. A heartbeat without it says nothing new; it never means the
//! stores are gone.

use std::{sync::Arc, time::Duration};

use sakala_agent_protocol::ValkeyHostMetadata;
use serde_json::{Value, json};

use super::periodic_read::{PeriodicRead, ReadMessages};
use crate::ports::{VALKEY_USAGE_BUSY, ValkeyAdmin};

/// How often the stores are read: one `docker inspect` and one `INFO` per
/// running store. ADR-023 asks for at most once a minute.
pub const VALKEY_USAGE_INTERVAL: Duration = Duration::from_secs(60);

/// Most entries of `metadata.valkeys`, about 300 KiB of JSON at most but far
/// above what a node's memory budget allows. `detail_counts.valkeys` keeps the
/// full count.
pub const MAX_REPORTED_VALKEYS: usize = 1_000;

/// `metadata.valkey`.
const VALKEY_METADATA_KEY: &str = "valkey";

/// `metadata.valkeys` and `metadata.detail_counts.valkeys`.
const VALKEYS_METADATA_KEY: &str = "valkeys";

/// How long one read may take before the heartbeat goes without it.
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

/// Adds the Valkey host's part of the heartbeat.
pub struct ValkeyHeartbeat {
    admin: Arc<dyn ValkeyAdmin>,
    capacity_mb: u64,
    max_items: usize,
    reads: PeriodicRead,
}

impl ValkeyHeartbeat {
    /// `capacity_mb` is `SAKALA_VALKEY_TOTAL_MEMORY_MB`.
    #[must_use]
    pub fn new(admin: Arc<dyn ValkeyAdmin>, capacity_mb: u64) -> Self {
        Self::with_bounds(
            admin,
            capacity_mb,
            VALKEY_USAGE_INTERVAL,
            MAX_REPORTED_VALKEYS,
        )
    }

    #[must_use]
    pub fn with_bounds(
        admin: Arc<dyn ValkeyAdmin>,
        capacity_mb: u64,
        interval: Duration,
        max_items: usize,
    ) -> Self {
        Self {
            admin,
            capacity_mb,
            max_items,
            reads: PeriodicRead::new(
                interval,
                read_timeout(),
                ReadMessages {
                    failing: "managed Valkey stores could not be read; the node reports degraded until a read succeeds",
                    recovered: "managed Valkey stores are read again",
                },
                Some(VALKEY_USAGE_BUSY),
            ),
        }
    }

    /// Adds `metadata.valkey`, and `metadata.valkeys` with its count when a
    /// read was due and succeeded. Returns whether the stores could be read,
    /// which keeps a node that cannot read them `degraded`.
    pub(crate) async fn insert(&self, metadata: &mut Value) -> bool {
        let usage = self.reads.read_if_due(|| self.admin.valkey_usage()).await;
        let available = self.reads.available();
        metadata[VALKEY_METADATA_KEY] = serde_json::to_value(ValkeyHostMetadata {
            capacity_mb: self.capacity_mb,
            available,
        })
        .unwrap_or(Value::Null);
        if let Some(mut usage) = usage {
            usage.sort_by_key(|store| store.valkey_id);
            metadata[VALKEYS_METADATA_KEY] = Value::Array(
                usage
                    .iter()
                    .take(self.max_items)
                    .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
                    .collect(),
            );
            metadata["detail_counts"][VALKEYS_METADATA_KEY] = json!(usage.len());
        }
        available
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sakala_agent_protocol::{ManagedValkeyUsage, ValkeyMemoryMb, ValkeyMode};
    use uuid::Uuid;

    use super::*;
    use crate::ports::RuntimeExecutionError;

    struct UsageAdmin {
        reads: AtomicUsize,
        fail: AtomicBool,
        busy: AtomicBool,
        hang: bool,
        count: u128,
    }

    impl UsageAdmin {
        fn new(count: u128, fail: bool, hang: bool) -> Arc<Self> {
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
    impl ValkeyAdmin for UsageAdmin {
        async fn valkey_usage(&self) -> Result<Vec<ManagedValkeyUsage>, RuntimeExecutionError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if self.busy.load(Ordering::SeqCst) {
                return Err(RuntimeExecutionError::new(
                    VALKEY_USAGE_BUSY,
                    "a Valkey command is running",
                ));
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(RuntimeExecutionError::new(
                    "valkey_unavailable",
                    "Docker does not answer",
                ));
            }
            // Reverse order, to prove the heartbeat sorts by ID.
            Ok((0..self.count)
                .rev()
                .map(|index| ManagedValkeyUsage {
                    valkey_id: Uuid::from_u128(index + 1),
                    mode: ValkeyMode::Cache,
                    maxmemory_mb: ValkeyMemoryMb::new(64).expect("memory"),
                    locked: false,
                    status: "running".to_owned(),
                    restart_count: 0,
                    used_memory: Some(1_000_000),
                    maxmemory: Some(67_108_864),
                    evicted_keys: Some(0),
                    errorstat_oom: Some(0),
                    connected_clients: Some(1),
                    aof_last_write_status: None,
                })
                .collect())
        }
    }

    fn metadata() -> Value {
        json!({ "detail_counts": {} })
    }

    #[tokio::test]
    async fn stores_are_sent_once_per_read_and_the_budget_always() {
        let admin = UsageAdmin::new(2, false, false);
        let heartbeat =
            ValkeyHeartbeat::with_bounds(admin.clone(), 384, Duration::from_secs(3_600), 10);

        let mut first = metadata();
        assert!(heartbeat.insert(&mut first).await);
        let mut second = metadata();
        assert!(heartbeat.insert(&mut second).await);

        assert_eq!(
            first["valkey"],
            json!({ "capacity_mb": 384, "available": true })
        );
        let ids = first["valkeys"]
            .as_array()
            .expect("list")
            .iter()
            .map(|store| store["valkey_id"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                Uuid::from_u128(1).to_string(),
                Uuid::from_u128(2).to_string()
            ]
        );
        assert_eq!(first["valkeys"][0]["aof_last_write_status"], Value::Null);
        assert_eq!(first["detail_counts"]["valkeys"], 2);
        assert_eq!(second["valkey"], first["valkey"]);
        assert!(second.get("valkeys").is_none());
        assert_eq!(admin.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_list_is_bounded_and_the_count_kept() {
        let heartbeat =
            ValkeyHeartbeat::with_bounds(UsageAdmin::new(7, false, false), 384, Duration::ZERO, 5);
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["valkeys"].as_array().expect("list").len(), 5);
        assert_eq!(sent["detail_counts"]["valkeys"], 7);
    }

    #[tokio::test]
    async fn a_node_without_stores_sends_an_empty_list() {
        let heartbeat =
            ValkeyHeartbeat::with_bounds(UsageAdmin::new(0, false, false), 384, Duration::ZERO, 5);
        let mut sent = metadata();
        heartbeat.insert(&mut sent).await;

        assert_eq!(sent["valkeys"], json!([]));
        assert_eq!(sent["detail_counts"]["valkeys"], 0);
    }

    #[tokio::test]
    async fn a_failed_or_hanging_read_omits_the_list_and_reports_unavailable_until_one_succeeds() {
        for admin in [
            UsageAdmin::new(1, true, false),
            UsageAdmin::new(1, false, true),
        ] {
            let heartbeat = ValkeyHeartbeat::with_bounds(admin, 384, Duration::ZERO, 5);
            let mut sent = metadata();

            assert!(!heartbeat.insert(&mut sent).await);
            assert_eq!(sent["valkey"]["available"], false);
            assert_eq!(sent["valkey"]["capacity_mb"], 384);
            assert!(sent.get("valkeys").is_none());
        }

        let admin = UsageAdmin::new(1, true, false);
        let heartbeat = ValkeyHeartbeat::with_bounds(admin.clone(), 384, Duration::ZERO, 5);
        assert!(!heartbeat.insert(&mut metadata()).await);
        admin.fail.store(false, Ordering::SeqCst);
        let mut recovered = metadata();
        assert!(heartbeat.insert(&mut recovered).await);
        assert_eq!(recovered["valkey"]["available"], true);
        assert_eq!(recovered["detail_counts"]["valkeys"], 1);
    }

    #[tokio::test]
    async fn a_read_skipped_while_a_command_runs_keeps_the_node_available_and_retries() {
        let admin = UsageAdmin::new(1, false, false);
        admin.busy.store(true, Ordering::SeqCst);
        let heartbeat =
            ValkeyHeartbeat::with_bounds(admin.clone(), 384, Duration::from_secs(3_600), 5);
        for _ in 0..3 {
            let mut sent = metadata();
            assert!(heartbeat.insert(&mut sent).await, "busy is not unavailable");
            assert!(sent.get("valkeys").is_none());
        }
        assert_eq!(admin.reads.load(Ordering::SeqCst), 3);

        admin.busy.store(false, Ordering::SeqCst);
        let mut after = metadata();
        assert!(heartbeat.insert(&mut after).await);
        assert_eq!(after["detail_counts"]["valkeys"], 1);
    }

    #[test]
    fn the_default_interval_is_sixty_seconds() {
        assert_eq!(VALKEY_USAGE_INTERVAL, Duration::from_secs(60));
    }
}
