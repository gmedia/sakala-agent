use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::ports::{RuntimeExecutor, RuntimeHealthSnapshot};

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
/// pemeriksaan antar-node tanpa menyulitkan shutdown.
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
        match runtime.health_snapshot().await {
            Ok(snapshots) => observe(&mut known, snapshots),
            Err(error) => warn!(%error, "runtime health snapshot failed"),
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

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        let interval = Duration::from_secs(10);
        assert_eq!(jitter("node-a", interval), jitter("node-a", interval));
        assert!(jitter("node-a", interval) <= Duration::from_secs(2));
    }
}
