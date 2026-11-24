//! Worker processes of a deployment (ADR-022).
//!
//! A worker container is a managed workload like a web replica: same image,
//! variables, network, runtime, security options, and restart policy, with
//! the deployment's identity labels. Two labels tell it apart:
//! `dev.sakala.workload-kind=worker` (a web replica carries `web`) and
//! `dev.sakala.process=<name>`. Its replica labels count the containers of
//! its own process. Everything that routes, probes, or restores web replicas
//! reads the process and leaves workers out.

use uuid::Uuid;

use crate::containers::replicas::ReplicaSlot;

/// Key of the label that says what a managed container serves.
pub(crate) const WORKLOAD_KIND_LABEL_KEY: &str = "dev.sakala.workload-kind";
/// Value of [`WORKLOAD_KIND_LABEL_KEY`] on the containers behind the route.
pub(crate) const WEB_WORKLOAD_KIND: &str = "web";
/// Value of [`WORKLOAD_KIND_LABEL_KEY`] on a worker process's containers.
pub(crate) const WORKER_WORKLOAD_KIND: &str = "worker";
/// The worker process a container runs.
pub(crate) const PROCESS_LABEL: &str = "dev.sakala.process";

/// The labels that say which process a container runs: `None` is the web
/// process.
pub(crate) fn process_labels(process: Option<&str>) -> Vec<String> {
    match process {
        None => vec![format!("{WORKLOAD_KIND_LABEL_KEY}={WEB_WORKLOAD_KIND}")],
        Some(process) => vec![
            format!("{WORKLOAD_KIND_LABEL_KEY}={WORKER_WORKLOAD_KIND}"),
            format!("{PROCESS_LABEL}={process}"),
        ],
    }
}

/// Reads the process back from a container's two labels. Only a container
/// marked as a worker that names its process is one; anything else, including
/// a container of an Agent from before workers, is the web process.
pub(crate) fn process_from_labels(kind: Option<&str>, process: Option<&str>) -> Option<String> {
    let process = process
        .map(str::trim)
        .filter(|process| !process.is_empty())?;
    (kind.map(str::trim) == Some(WORKER_WORKLOAD_KIND)).then(|| process.to_owned())
}

/// Names one container of a worker process. Nothing resolves it, because a
/// worker serves no route, so unlike a web replica it need not fit a DNS
/// label; it keeps the full deployment ID so names never collide.
#[must_use]
pub fn worker_container_name(
    project_id: Uuid,
    deployment_id: Uuid,
    process: &str,
    slot: ReplicaSlot,
) -> String {
    let project = project_id.to_string();
    format!(
        "sakala-worker-{}-{deployment_id}-{process}-r{}",
        &project[..8],
        slot.index
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    #[test]
    fn labels_round_trip_and_anything_else_is_the_web_process() {
        assert_eq!(process_labels(None), ["dev.sakala.workload-kind=web"]);
        assert_eq!(
            process_labels(Some("queue")),
            [
                "dev.sakala.workload-kind=worker",
                "dev.sakala.process=queue"
            ]
        );
        assert_eq!(
            process_from_labels(Some("worker"), Some("queue")).as_deref(),
            Some("queue")
        );
        for (kind, process) in [
            (Some("web"), None),
            (None, None),
            (Some(""), Some("")),
            (Some("web"), Some("queue")),
            (Some("worker"), Some("")),
            (Some("worker"), None),
            (None, Some("queue")),
        ] {
            assert_eq!(
                process_from_labels(kind, process),
                None,
                "{kind:?} {process:?}"
            );
        }
    }

    #[test]
    fn a_worker_name_keeps_the_deployment_and_its_index() {
        let slot = ReplicaSlot {
            index: 1,
            count: NonZeroU32::new(2).expect("non-zero"),
        };
        assert_eq!(
            worker_container_name(
                Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("UUID"),
                Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID"),
                "queue",
                slot,
            ),
            "sakala-worker-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-queue-r1"
        );
    }
}
