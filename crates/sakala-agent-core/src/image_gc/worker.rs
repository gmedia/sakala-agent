use std::{sync::Arc, time::Duration};

use tokio::{sync::watch, time::sleep};
use tracing::{debug, info, warn};

use crate::{ports::RuntimeExecutor, scheduler::metrics::SchedulerMetrics};

/// One run lists, inspects, and removes images, each step bounded by the
/// process runner, so this only stops a run that keeps going far too long.
const RUN_DEADLINE: Duration = Duration::from_secs(600);

/// Removes stale Sakala images once per `interval`, the first time one interval
/// after start, so it never competes with startup recovery.
///
/// A run is skipped while a command is active. Selection protects every image a
/// container uses and every image younger than the retention ages, and Docker
/// refuses an image a container starts using after selection, but a deploy that
/// is between build and start is safer left alone altogether.
pub async fn run(
    runtime: Arc<dyn RuntimeExecutor>,
    interval: Duration,
    scheduler_metrics: Arc<SchedulerMetrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            () = sleep(interval) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
        let active = scheduler_metrics.active_commands();
        if active > 0 {
            debug!(
                active_commands = active,
                "image GC skipped while commands run"
            );
            continue;
        }
        tokio::select! {
            outcome = tokio::time::timeout(RUN_DEADLINE, runtime.collect_stale_images()) => {
                match outcome {
                    Ok(Ok(cleanup)) if cleanup.removed_images > 0 => info!(
                        removed_images = cleanup.removed_images,
                        reclaimed_bytes = cleanup.reclaimed_bytes,
                        "image GC removed stale Sakala images"
                    ),
                    Ok(Ok(_)) => debug!("image GC found nothing to remove"),
                    Ok(Err(error)) => warn!(%error, "image GC failed"),
                    Err(_) => warn!(deadline = ?RUN_DEADLINE, "image GC timed out"),
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    info!("image GC worker stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::ports::{RuntimeExecutionError, RuntimeImageCleanup};

    #[derive(Default)]
    struct CountingRuntime {
        runs: AtomicUsize,
    }

    #[async_trait]
    impl RuntimeExecutor for CountingRuntime {
        async fn collect_stale_images(&self) -> Result<RuntimeImageCleanup, RuntimeExecutionError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(RuntimeImageCleanup {
                removed_images: 1,
                reclaimed_bytes: 10,
            })
        }
    }

    fn start(
        interval: Duration,
        metrics: Arc<SchedulerMetrics>,
    ) -> (
        Arc<CountingRuntime>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let runtime = Arc::new(CountingRuntime::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = tokio::spawn(run(
            Arc::clone(&runtime) as Arc<dyn RuntimeExecutor>,
            interval,
            metrics,
            shutdown_rx,
        ));
        (runtime, shutdown_tx, worker)
    }

    #[tokio::test]
    async fn waits_one_interval_before_the_first_run_and_stops_on_shutdown() {
        let (runtime, shutdown, worker) = start(Duration::from_secs(3_600), Arc::default());
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker stops promptly")
            .expect("worker");
        assert_eq!(runtime.runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn runs_again_after_every_interval() {
        let (runtime, shutdown, worker) = start(Duration::from_millis(20), Arc::default());
        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.runs.load(Ordering::SeqCst) < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("three runs");
        shutdown.send(true).expect("shutdown");
        worker.await.expect("worker");
    }

    #[tokio::test]
    async fn skips_while_a_command_is_active() {
        let metrics = Arc::new(SchedulerMetrics::default());
        metrics.command_started();
        let (runtime, shutdown, worker) = start(Duration::from_millis(10), metrics);
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.send(true).expect("shutdown");
        worker.await.expect("worker");
        assert_eq!(runtime.runs.load(Ordering::SeqCst), 0);
    }
}
