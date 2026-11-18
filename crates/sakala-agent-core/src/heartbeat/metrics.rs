//! `metadata.workloads.metrics`: CPU, memory, and restart count per running
//! workload container, sampled less often than heartbeats are sent.
//!
//! A heartbeat carries the list only when a fresh sample was taken for it, so
//! every entry the control plane receives is a new reading, and an absent list
//! means "no new reading" rather than zero usage. The control plane aggregates
//! per minute, and one `docker stats` sample takes about two seconds of Docker's
//! time, so sampling every heartbeat would cost more than it tells.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use sakala_agent_protocol::WorkloadMetrics;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::ports::RuntimeExecutor;

/// How often workload metrics are sampled, in seconds.
pub const METRICS_INTERVAL_ENV: &str = "SAKALA_METRICS_INTERVAL_SECONDS";

/// Default of [`METRICS_INTERVAL_ENV`]: one sample per minute, the control
/// plane's aggregation window.
pub const DEFAULT_METRICS_INTERVAL: Duration = Duration::from_secs(60);

/// Lowest accepted [`METRICS_INTERVAL_ENV`]. One sample keeps Docker busy for
/// about two seconds, so a shorter interval would mostly measure Docker itself.
pub const MIN_METRICS_INTERVAL: Duration = Duration::from_secs(10);

/// Highest accepted [`METRICS_INTERVAL_ENV`]. Beyond an hour the control
/// plane's per-minute charts would be mostly empty, which is more likely a
/// unit mistake than a choice.
pub const MAX_METRICS_INTERVAL: Duration = Duration::from_secs(3_600);

/// `metadata.workloads.metrics` and `metadata.detail_counts.metrics`.
const METRICS_METADATA_KEY: &str = "metrics";

/// How long one sample may take before the heartbeat goes without it. It covers
/// the runtime's three Docker calls, the slowest of which is `docker stats`.
fn sample_timeout() -> Duration {
    #[cfg(test)]
    {
        Duration::from_millis(100)
    }
    #[cfg(not(test))]
    {
        Duration::from_secs(12)
    }
}

/// Decides when a heartbeat takes a metrics sample and remembers whether the
/// last one failed, so a failure streak is logged once.
pub struct WorkloadMetricsSampler {
    interval: Duration,
    max_items: usize,
    state: Mutex<SamplerState>,
}

#[derive(Default)]
struct SamplerState {
    last_attempt: Option<Instant>,
    failing: bool,
}

impl WorkloadMetricsSampler {
    /// `max_items` bounds the list on the wire; `detail_counts.metrics` keeps
    /// the full count.
    #[must_use]
    pub fn new(interval: Duration, max_items: usize) -> Self {
        Self {
            interval,
            max_items,
            state: Mutex::new(SamplerState::default()),
        }
    }

    /// Samples when no attempt was made within the interval. `None` when the
    /// interval has not passed yet or when the sample failed; a failed sample
    /// waits for the next interval too, so a hanging Docker costs one timeout
    /// per interval rather than one per heartbeat.
    pub(crate) async fn sample_if_due(
        &self,
        runtime: &dyn RuntimeExecutor,
    ) -> Option<Vec<WorkloadMetrics>> {
        {
            let mut state = self.state.lock().ok()?;
            if state
                .last_attempt
                .is_some_and(|last| last.elapsed() < self.interval)
            {
                return None;
            }
            state.last_attempt = Some(Instant::now());
        }
        let error = match tokio::time::timeout(sample_timeout(), runtime.workload_metrics()).await {
            Ok(Ok(metrics)) => {
                self.set_failing(false, None);
                return Some(metrics);
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => format!(
                "the sample did not finish within {} seconds",
                sample_timeout().as_secs_f64()
            ),
        };
        self.set_failing(true, Some(&error));
        None
    }

    fn set_failing(&self, failing: bool, error: Option<&str>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let was_failing = std::mem::replace(&mut state.failing, failing);
        match (was_failing, failing) {
            (false, true) => warn!(
                error = error.unwrap_or_default(),
                "workload metrics could not be sampled; heartbeats omit metrics until a sample succeeds"
            ),
            (true, false) => info!("workload metrics are sampled again"),
            _ => {}
        }
    }

    pub(crate) fn max_items(&self) -> usize {
        self.max_items
    }
}

/// Adds the list only for a fresh sample. Its first `bound` entries are sent,
/// in the runtime's order, and `detail_counts.metrics` keeps the full count.
pub(crate) fn insert_metrics(
    metadata: &mut Value,
    metrics: Option<&[WorkloadMetrics]>,
    bound: usize,
) {
    let Some(metrics) = metrics else { return };
    metadata["workloads"][METRICS_METADATA_KEY] = Value::Array(
        metrics
            .iter()
            .take(bound)
            .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
            .collect(),
    );
    metadata["detail_counts"][METRICS_METADATA_KEY] = json!(metrics.len());
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::ports::RuntimeExecutionError;

    struct CountingRuntime {
        calls: AtomicUsize,
        fail: bool,
        hang: bool,
    }

    impl CountingRuntime {
        fn new(fail: bool, hang: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                fail,
                hang,
            })
        }
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for CountingRuntime {
        async fn workload_metrics(&self) -> Result<Vec<WorkloadMetrics>, RuntimeExecutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if self.fail {
                return Err(RuntimeExecutionError::new(
                    "runtime_container_failed",
                    "docker stats failed",
                ));
            }
            Ok(vec![metric(0)])
        }
    }

    fn metric(replica: u32) -> WorkloadMetrics {
        WorkloadMetrics {
            project_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            replica,
            sampled_at: OffsetDateTime::UNIX_EPOCH,
            cpu_percent: 1.5,
            memory_bytes: 1_024,
            memory_limit_bytes: None,
            restart_count: 0,
        }
    }

    #[tokio::test]
    async fn a_sample_is_taken_once_per_interval() {
        let runtime = CountingRuntime::new(false, false);
        let sampler = WorkloadMetricsSampler::new(Duration::from_secs(3_600), 10);

        assert_eq!(
            sampler.sample_if_due(runtime.as_ref()).await,
            Some(vec![metric(0)])
        );
        assert_eq!(sampler.sample_if_due(runtime.as_ref()).await, None);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);

        let every_time = WorkloadMetricsSampler::new(Duration::ZERO, 10);
        assert!(every_time.sample_if_due(runtime.as_ref()).await.is_some());
        assert!(every_time.sample_if_due(runtime.as_ref()).await.is_some());
    }

    #[tokio::test]
    async fn a_failed_or_hanging_sample_is_omitted_and_waits_for_the_next_interval() {
        for runtime in [
            CountingRuntime::new(true, false),
            CountingRuntime::new(false, true),
        ] {
            let sampler = WorkloadMetricsSampler::new(Duration::from_secs(3_600), 10);

            assert_eq!(sampler.sample_if_due(runtime.as_ref()).await, None);
            assert!(sampler.state.lock().expect("state").failing);
            assert_eq!(sampler.sample_if_due(runtime.as_ref()).await, None);
            assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn a_successful_sample_ends_the_failure_streak() {
        let sampler = WorkloadMetricsSampler::new(Duration::ZERO, 10);

        assert_eq!(
            sampler
                .sample_if_due(CountingRuntime::new(true, false).as_ref())
                .await,
            None
        );
        assert!(sampler.state.lock().expect("state").failing);
        assert!(
            sampler
                .sample_if_due(CountingRuntime::new(false, false).as_ref())
                .await
                .is_some()
        );
        assert!(!sampler.state.lock().expect("state").failing);
    }

    #[test]
    fn the_list_is_bounded_and_the_count_kept() {
        let mut metadata = json!({ "workloads": {}, "detail_counts": {} });
        let metrics = (0..7).map(metric).collect::<Vec<_>>();

        insert_metrics(&mut metadata, Some(&metrics), 5);

        let sent = metadata["workloads"]["metrics"].as_array().expect("array");
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[4]["replica"], 4);
        assert_eq!(metadata["detail_counts"]["metrics"], 7);
    }

    #[test]
    fn no_sample_leaves_both_keys_out_and_an_empty_sample_sends_an_empty_list() {
        let mut absent = json!({ "workloads": {}, "detail_counts": {} });
        insert_metrics(&mut absent, None, 5);
        assert!(absent["workloads"].get("metrics").is_none());
        assert!(absent["detail_counts"].get("metrics").is_none());

        let mut empty = json!({ "workloads": {}, "detail_counts": {} });
        insert_metrics(&mut empty, Some(&[]), 5);
        assert_eq!(empty["workloads"]["metrics"], json!([]));
        assert_eq!(empty["detail_counts"]["metrics"], 0);
    }
}
