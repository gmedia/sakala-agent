use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::ports::{RuntimeExecutor, RuntimeHealthSnapshot};

/// Bounds one replica restore pass. Each `docker start` is bounded by the process
/// runner, so this only stops a pass that keeps going far too long.
const REPLICA_RESTORE_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Eq, PartialEq)]
struct HealthState {
    ready: bool,
    status: String,
    reason: Option<String>,
}

/// Memantau workload aktif dengan satu snapshot runtime per interval.
///
/// Satu panggilan batch menjaga pemeriksaan tetap bounded dan menghindari
/// stampede per-container. Jitter deterministik dari agent ID menyebarkan
/// pemeriksaan antar-node tanpa menyulitkan shutdown. Setiap interval juga
/// menyalakan lagi replika yang berhenti dari deployment yang masih melayani;
/// runtime yang memutuskan replika mana dan kapan.
pub async fn run(
    runtime: Arc<dyn RuntimeExecutor>,
    agent_id: String,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut known = HashMap::new();
    let initial_delay = jitter(&agent_id, interval);
    if !initial_delay.is_zero() {
        tokio::select! {
            () = sleep(initial_delay) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    info!("runtime health worker stopped before first check");
                    return;
                }
            }
        }
    }

    loop {
        let probe_deadline = interval.clamp(Duration::from_secs(1), Duration::from_secs(10));
        match tokio::time::timeout(probe_deadline, runtime.health_snapshot()).await {
            Ok(Ok(snapshots)) => observe(&mut known, snapshots),
            Ok(Err(error)) => warn!(%error, "runtime health snapshot failed"),
            Err(_) => warn!(?probe_deadline, "runtime health snapshot timed out"),
        }

        tokio::select! {
            outcome = tokio::time::timeout(REPLICA_RESTORE_DEADLINE, runtime.restore_replicas()) => {
                match outcome {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => warn!(%error, "replica restore pass failed"),
                    Err(_) => warn!(deadline = ?REPLICA_RESTORE_DEADLINE, "replica restore pass timed out"),
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }

        tokio::select! {
            () = sleep(interval) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    info!("runtime health worker stopped");
}

fn jitter(agent_id: &str, interval: Duration) -> Duration {
    let ceiling_ms = (interval.as_millis() / 5).min(5_000);
    if ceiling_ms == 0 {
        return Duration::ZERO;
    }
    let value = agent_id.bytes().fold(0_u128, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u128::from(byte))
    });
    Duration::from_millis((value % (ceiling_ms + 1)) as u64)
}

fn observe(known: &mut HashMap<String, HealthState>, snapshots: Vec<RuntimeHealthSnapshot>) {
    let mut current = HashMap::with_capacity(snapshots.len());
    for snapshot in snapshots {
        let container_id = snapshot.workload.container_id.clone();
        let next = HealthState {
            ready: snapshot.ready,
            status: snapshot.workload.status.clone(),
            reason: snapshot.reason.clone(),
        };
        match known.get(&container_id) {
            None => info!(
                container_id = %container_id,
                project_id = %snapshot.workload.project_id,
                deployment_id = %snapshot.workload.deployment_id,
                ready = next.ready,
                status = %next.status,
                reason = ?next.reason,
                "runtime workload health observed"
            ),
            Some(previous) if previous != &next => {
                if next.ready {
                    info!(
                        container_id = %container_id,
                        project_id = %snapshot.workload.project_id,
                        deployment_id = %snapshot.workload.deployment_id,
                        status = %next.status,
                        "runtime workload became ready"
                    );
                } else {
                    warn!(
                        container_id = %container_id,
                        project_id = %snapshot.workload.project_id,
                        deployment_id = %snapshot.workload.deployment_id,
                        status = %next.status,
                        reason = ?next.reason,
                        "runtime workload health degraded"
                    );
                }
            }
            Some(_) => {}
        }
        current.insert(container_id, next);
    }

    for (container_id, previous) in known.iter() {
        if !current.contains_key(container_id) {
            warn!(
                container_id = %container_id,
                ready = previous.ready,
                "runtime workload disappeared from active health snapshot"
            );
        }
    }
    *known = current;
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::ports::RuntimeWorkload;

    fn snapshot(container_id: &str, ready: bool) -> RuntimeHealthSnapshot {
        RuntimeHealthSnapshot {
            workload: RuntimeWorkload {
                container_id: container_id.to_owned(),
                project_id: Uuid::nil(),
                deployment_id: Uuid::nil(),
                status: if ready { "Up" } else { "Up (unhealthy)" }.to_owned(),
            },
            ready,
            reason: (!ready).then(|| "unhealthy".to_owned()),
            desired_replicas: std::num::NonZeroU32::MIN,
        }
    }

    #[test]
    fn state_is_replaced_only_by_latest_active_snapshot() {
        let mut known = HashMap::new();
        observe(&mut known, vec![snapshot("first", true)]);
        observe(
            &mut known,
            vec![snapshot("first", false), snapshot("second", true)],
        );

        assert_eq!(known.len(), 2);
        assert!(!known["first"].ready);

        observe(&mut known, vec![snapshot("second", true)]);
        assert_eq!(known.len(), 1);
        assert!(known.contains_key("second"));
    }

    #[derive(Default)]
    struct RestoreCountingRuntime {
        restores: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RuntimeExecutor for RestoreCountingRuntime {
        async fn restore_replicas(
            &self,
        ) -> Result<crate::ports::RuntimeReplicaRestoration, crate::ports::RuntimeExecutionError>
        {
            self.restores
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::ports::RuntimeReplicaRestoration::default())
        }
    }

    #[tokio::test]
    async fn every_interval_runs_one_replica_restore_pass() {
        let runtime = Arc::new(RestoreCountingRuntime::default());
        let (shutdown, receiver) = watch::channel(false);
        let worker = tokio::spawn(run(
            Arc::clone(&runtime) as Arc<dyn RuntimeExecutor>,
            String::new(),
            Duration::from_millis(10),
            receiver,
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            while runtime.restores.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("restore runs on every interval");
        shutdown.send(true).expect("worker listens");
        worker.await.expect("worker stops");
    }

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let interval = Duration::from_secs(10);
        assert_eq!(jitter("node-a", interval), jitter("node-a", interval));
        assert!(jitter("node-a", interval) <= Duration::from_secs(2));
    }
}
