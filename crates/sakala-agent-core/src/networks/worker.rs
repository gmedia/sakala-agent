use std::{sync::Arc, time::Duration};

use tokio::{sync::watch, time::sleep};
use tracing::{debug, info, warn};

use crate::{ports::RuntimeExecutor, scheduler::metrics::SchedulerMetrics};

/// One run is a few bounded Docker calls, so this only stops one that hangs.
const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// Keeps the router on every project network, once per `interval`, the first time
/// one interval after start, since startup recovery has just done the same.
///
/// A router container recreated by Compose keeps only the networks its Compose
/// file names, so without this every project route would fail until the Agent
/// restarts. Reattaching is harmless at any time. Removing unused networks waits
/// until no command is active, like image GC, and the runtime additionally keeps
/// it from overlapping a workload start.
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
        let remove_unused = scheduler_metrics.active_commands() == 0;
        tokio::select! {
            outcome = tokio::time::timeout(RUN_DEADLINE, runtime.reconcile_networks(remove_unused)) => {
                match outcome {
                    Ok(Ok(result)) => debug!(
                        removed = result.removed,
                        router_attached = result.router_attached,
                        remove_unused,
                        "project network reconciliation finished"
                    ),
                    Ok(Err(error)) => warn!(%error, "project network reconciliation failed"),
                    Err(_) => warn!(deadline = ?RUN_DEADLINE, "project network reconciliation timed out"),
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    info!("project network worker stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::ports::{RuntimeExecutionError, RuntimeNetworkReconciliation};

    #[derive(Default)]
    struct RecordingRuntime {
        runs: Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl RuntimeExecutor for RecordingRuntime {
        async fn reconcile_networks(
            &self,
            remove_unused: bool,
        ) -> Result<RuntimeNetworkReconciliation, RuntimeExecutionError> {
            self.runs.lock().expect("runs").push(remove_unused);
            Ok(RuntimeNetworkReconciliation::default())
        }
    }

    fn start(
        interval: Duration,
        metrics: Arc<SchedulerMetrics>,
    ) -> (
        Arc<RecordingRuntime>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let runtime = Arc::new(RecordingRuntime::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = tokio::spawn(run(
            Arc::clone(&runtime) as Arc<dyn RuntimeExecutor>,
            interval,
            metrics,
            shutdown_rx,
        ));
        (runtime, shutdown_tx, worker)
    }

    async fn wait_for_runs(runtime: &RecordingRuntime, count: usize) -> Vec<bool> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let runs = runtime.runs.lock().expect("runs").clone();
                if runs.len() >= count {
                    return runs;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("runs")
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
        assert!(runtime.runs.lock().expect("runs").is_empty());
    }

    #[tokio::test]
    async fn reconciles_after_every_interval_and_removes_unused_networks_when_idle() {
        let (runtime, shutdown, worker) = start(Duration::from_millis(20), Arc::default());
        let runs = wait_for_runs(&runtime, 3).await;
        shutdown.send(true).expect("shutdown");
        worker.await.expect("worker");
        assert!(runs.iter().all(|remove_unused| *remove_unused), "{runs:?}");
    }

    #[tokio::test]
    async fn keeps_reattaching_but_removes_nothing_while_a_command_is_active() {
        let metrics = Arc::new(SchedulerMetrics::default());
        metrics.command_started();
        let (runtime, shutdown, worker) = start(Duration::from_millis(10), metrics);
        let runs = wait_for_runs(&runtime, 3).await;
        shutdown.send(true).expect("shutdown");
        worker.await.expect("worker");
        assert!(runs.iter().all(|remove_unused| !remove_unused), "{runs:?}");
    }
}
