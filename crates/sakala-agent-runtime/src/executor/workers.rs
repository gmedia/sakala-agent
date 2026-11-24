//! The worker processes a deployment runs next to its web replicas (ADR-022).

use std::{collections::HashSet, num::NonZeroU32};

use sakala_agent_protocol::{
    AppliedRuntimeResources, DeployProjectPayload, MAX_PROCESS_NAME_LENGTH, MAX_WORKER_PROCESSES,
    RESERVED_PROCESS_NAMES, RuntimeResourceLimits, is_valid_process_name,
};
use uuid::Uuid;

use super::shell_command::required_shell_command;
use crate::{
    RuntimeError,
    containers::{ReplicaSlot, worker_container_name},
};

/// One worker process of a deployment, checked against the node's limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkerPlan {
    pub process: String,
    /// Trimmed; runs as `sh -c` in each container.
    pub command: String,
    pub replicas: u32,
    pub resources: AppliedRuntimeResources,
}

/// One container a deployment starts for a worker process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkerCandidate {
    pub process: String,
    pub command: String,
    pub slot: ReplicaSlot,
    pub name: String,
    pub resources: AppliedRuntimeResources,
}

/// Checks the payload's worker definitions in order and resolves their
/// limits. The control plane checks the same rules first, so a failure here
/// is a safety net: the deployment fails before checkout instead of running
/// a worker the API would not allow.
pub(super) fn resolve_workers(
    payload: &DeployProjectPayload,
    max_replicas_per_project: u32,
    resolve_resources: impl Fn(RuntimeResourceLimits) -> Result<AppliedRuntimeResources, RuntimeError>,
) -> Result<Vec<WorkerPlan>, RuntimeError> {
    let workers = payload.processes.workers();
    if workers.len() > MAX_WORKER_PROCESSES {
        return Err(RuntimeError::InvalidCommand(format!(
            "processes holds {} worker definitions; a deployment may have at most {MAX_WORKER_PROCESSES}",
            workers.len()
        )));
    }
    let mut names = HashSet::new();
    workers
        .iter()
        .map(|worker| {
            if !is_valid_process_name(&worker.name) {
                return Err(RuntimeError::InvalidCommand(format!(
                    "process name {:?} must be lowercase letters, digits, and dashes, start with a letter, have at most {MAX_PROCESS_NAME_LENGTH} characters, and not be one of {}",
                    worker.name,
                    RESERVED_PROCESS_NAMES.join(", ")
                )));
            }
            if !names.insert(worker.name.as_str()) {
                return Err(RuntimeError::InvalidCommand(format!(
                    "process name {:?} is used twice",
                    worker.name
                )));
            }
            let command = required_shell_command(
                &format!("command of process {}", worker.name),
                &worker.command,
            )?;
            if worker.replicas > max_replicas_per_project {
                return Err(RuntimeError::ReplicasExceeded {
                    requested: worker.replicas,
                    maximum: max_replicas_per_project,
                });
            }
            Ok(WorkerPlan {
                process: worker.name.clone(),
                command,
                replicas: worker.replicas,
                resources: resolve_resources(worker.resources_within(payload.resources))?,
            })
        })
        .collect()
}

/// Every container the workers of a deployment start, process by process.
pub(super) fn worker_candidates(
    workers: &[WorkerPlan],
    project_id: Uuid,
    deployment_id: Uuid,
) -> Vec<WorkerCandidate> {
    workers
        .iter()
        .filter_map(|worker| Some((worker, NonZeroU32::new(worker.replicas)?)))
        .flat_map(|(worker, replicas)| {
            ReplicaSlot::all(replicas).map(move |slot| WorkerCandidate {
                process: worker.process.clone(),
                command: worker.command.clone(),
                slot,
                name: worker_container_name(project_id, deployment_id, &worker.process, slot),
                resources: worker.resources,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const MAX_REPLICAS: u32 = 4;

    fn payload(processes: Value) -> DeployProjectPayload {
        serde_json::from_value(json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
            "resources": { "memory_mb": 256, "cpu_millis": 500 },
            "processes": processes,
        }))
        .expect("payload")
    }

    fn applied(limits: RuntimeResourceLimits) -> Result<AppliedRuntimeResources, RuntimeError> {
        let memory_mb = limits.memory_mb.unwrap_or(256);
        if memory_mb > 512 {
            return Err(RuntimeError::InvalidCommand("memory too high".to_owned()));
        }
        Ok(AppliedRuntimeResources {
            memory_mb,
            cpu_millis: limits.cpu_millis.unwrap_or(500),
            pids_limit: limits.pids_limit.unwrap_or(128),
        })
    }

    fn resolve(processes: Value) -> Result<Vec<WorkerPlan>, RuntimeError> {
        resolve_workers(&payload(processes), MAX_REPLICAS, applied)
    }

    #[test]
    fn no_processes_means_no_workers() {
        for processes in [Value::Null, json!([]), json!("not a list")] {
            assert!(
                resolve(processes.clone()).expect("no workers").is_empty(),
                "{processes}"
            );
        }
    }

    #[test]
    fn workers_take_the_deployment_limits_they_do_not_set() {
        let workers = resolve(json!([
            { "name": "queue", "command": "  php artisan queue:work  ", "replicas": 2,
              "resources": { "memory_mb": 384 } },
            { "name": "idle", "command": "npm run idle", "replicas": 0 },
        ]))
        .expect("workers");
        assert_eq!(workers[0].command, "php artisan queue:work");
        assert_eq!(workers[0].resources.memory_mb, 384);
        assert_eq!(workers[0].resources.cpu_millis, 500);
        assert_eq!(workers[1].replicas, 0);

        let candidates = worker_candidates(
            &workers,
            Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("UUID"),
            Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID"),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            [
                "sakala-worker-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-queue-r0",
                "sakala-worker-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-queue-r1",
            ],
            "a process with zero replicas starts nothing"
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.slot.count.get() == 2)
        );
    }

    #[test]
    fn a_definition_that_breaks_a_rule_fails_before_checkout() {
        for (processes, expected) in [
            (
                json!([{ "name": "web", "command": "x" }]),
                "process name \"web\"",
            ),
            (
                json!([{ "name": "Queue", "command": "x" }]),
                "process name \"Queue\"",
            ),
            (
                json!([{ "name": "queue", "command": "x" }, { "name": "queue", "command": "y" }]),
                "used twice",
            ),
            (
                json!([{ "name": "queue", "command": "  " }]),
                "must not be empty",
            ),
            (json!([{ "name": "queue", "command": "a\nb" }]), "one line"),
            (
                json!([{ "name": "queue", "command": "x", "resources": { "memory_mb": 4096 } }]),
                "memory too high",
            ),
            (
                json!(
                    (0..6)
                        .map(|index| json!({ "name": format!("w{index}"), "command": "x" }))
                        .collect::<Vec<_>>()
                ),
                "at most 5",
            ),
        ] {
            let error = resolve(processes.clone()).expect_err(&processes.to_string());
            assert_eq!(error.code(), "invalid_runtime_command", "{processes}");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn more_replicas_than_the_node_allows_fail_like_web_replicas() {
        let error = resolve(json!([{ "name": "queue", "command": "x", "replicas": 5 }]))
            .expect_err("above the maximum");
        assert_eq!(error.code(), "runtime_replicas_exceeded");
    }
}
