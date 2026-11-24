//! Worker processes of a deployment (ADR-022).
//!
//! A project has one `web` process, the containers behind its route, and zero
//! or more worker processes: containers of the same image and variables that
//! run another command, serve nothing, and have no route.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::RuntimeResourceLimits;

/// Capability of a node that runs `DeployProject.processes`. The control plane
/// sends the field, and places a deployment with workers, only on such a node.
pub const WORKER_PROCESSES_CAPABILITY: &str = "worker-processes";

/// The process behind the project's route. Never a worker name.
pub const WEB_PROCESS: &str = "web";
/// The process of a deployment's release command. Never a worker name.
pub const RELEASE_PROCESS: &str = "release";
/// The process of a `RunJob` command. Never a worker name.
pub const JOB_PROCESS: &str = "job";
/// Names the platform uses for its own processes.
pub const RESERVED_PROCESS_NAMES: [&str; 3] = [WEB_PROCESS, RELEASE_PROCESS, JOB_PROCESS];

/// Most worker definitions one deployment may carry. A definition with zero
/// replicas still counts: it is configuration, not a quota.
pub const MAX_WORKER_PROCESSES: usize = 5;
/// Longest worker name, in characters.
pub const MAX_PROCESS_NAME_LENGTH: usize = 30;

/// Event of a deployment whose `processes` could not be read, which runs its
/// web process only. Level `warning`, metadata `{}`.
pub const PROCESSES_IGNORED_EVENT: &str = "deployment.processes.ignored";

/// One worker process of a deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerProcess {
    /// Lowercase letters, digits, and dashes, starting with a letter, at most
    /// [`MAX_PROCESS_NAME_LENGTH`] characters, and none of
    /// [`RESERVED_PROCESS_NAMES`].
    pub name: String,
    /// Runs as `sh -c <command>` in the container, like a release command.
    pub command: String,
    /// Containers of this process. Zero keeps the definition and runs nothing.
    #[serde(default = "default_worker_replicas")]
    pub replicas: u32,
    /// Limits of each container of this process. A field left out takes the
    /// value of the deployment's own `resources`.
    #[serde(default)]
    pub resources: RuntimeResourceLimits,
}

impl WorkerProcess {
    /// The limits of this process: its own values, and the deployment's where
    /// it has none.
    #[must_use]
    pub fn resources_within(&self, deployment: RuntimeResourceLimits) -> RuntimeResourceLimits {
        RuntimeResourceLimits {
            memory_mb: self.resources.memory_mb.or(deployment.memory_mb),
            cpu_millis: self.resources.cpu_millis.or(deployment.cpu_millis),
            pids_limit: self.resources.pids_limit.or(deployment.pids_limit),
        }
    }
}

const fn default_worker_replicas() -> u32 {
    1
}

/// Whether `name` may name a worker process.
#[must_use]
pub fn is_valid_process_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && name.len() <= MAX_PROCESS_NAME_LENGTH
        && !RESERVED_PROCESS_NAMES.contains(&name)
}

/// The `processes` field of a `DeployProject` payload.
///
/// Read leniently, like `replicas`: a value that is not a list of worker
/// definitions never fails the payload. The deployment then runs its web
/// process only and reports why. A list that reads but breaks a rule, such
/// as a reserved name, fails the deployment before checkout instead.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RequestedProcesses {
    /// The field is missing or `null`: the web process only, as before
    /// worker processes existed.
    #[default]
    Absent,
    /// The worker definitions, possibly none.
    Workers(Vec<WorkerProcess>),
    /// Present, but not a list of worker definitions.
    Unreadable,
}

impl RequestedProcesses {
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The worker definitions to run. Anything but [`Self::Workers`] is none.
    #[must_use]
    pub fn workers(&self) -> &[WorkerProcess] {
        match self {
            Self::Workers(workers) => workers,
            Self::Absent | Self::Unreadable => &[],
        }
    }
}

impl<'de> Deserialize<'de> for RequestedProcesses {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Null => Self::Absent,
            value => serde_json::from_value::<Vec<WorkerProcess>>(value)
                .map_or(Self::Unreadable, Self::Workers),
        })
    }
}

impl Serialize for RequestedProcesses {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Workers(workers) => workers.serialize(serializer),
            Self::Absent | Self::Unreadable => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::DeployProjectPayload;

    use super::*;

    fn processes(value: Option<Value>) -> RequestedProcesses {
        let mut payload = json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        });
        if let Some(value) = value {
            payload["processes"] = value;
        }
        serde_json::from_value::<DeployProjectPayload>(payload)
            .expect("a processes value never fails the payload")
            .processes
    }

    #[test]
    fn processes_are_absent_when_missing_or_null() {
        for value in [None, Some(Value::Null)] {
            let requested = processes(value);
            assert_eq!(requested, RequestedProcesses::Absent);
            assert!(requested.workers().is_empty());
        }
    }

    #[test]
    fn a_list_of_definitions_is_read_with_defaults() {
        let requested = processes(Some(json!([
            { "name": "queue", "command": "php artisan queue:work", "replicas": 2,
              "resources": { "memory_mb": 384 } },
            { "name": "mailer", "command": "npm run mailer" },
        ])));
        let workers = requested.workers();
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].replicas, 2);
        assert_eq!(workers[0].resources.memory_mb, Some(384));
        assert_eq!(workers[1].replicas, 1, "one replica when left out");
        assert_eq!(workers[1].resources, RuntimeResourceLimits::default());
        assert_eq!(
            processes(Some(json!([]))),
            RequestedProcesses::Workers(Vec::new())
        );
    }

    #[test]
    fn anything_else_runs_the_web_process_only_instead_of_refusing_the_deployment() {
        for value in [
            json!("queue"),
            json!(3),
            json!({ "name": "queue", "command": "x" }),
            json!([{ "name": "queue" }]),
            json!([{ "command": "x" }]),
            json!([{ "name": "queue", "command": "x", "replicas": -1 }]),
            json!([{ "name": "queue", "command": "x", "replicas": "2" }]),
            json!([{ "name": 7, "command": "x" }]),
            json!(["queue"]),
        ] {
            let requested = processes(Some(value.clone()));
            assert_eq!(requested, RequestedProcesses::Unreadable, "{value}");
            assert!(requested.workers().is_empty(), "{value}");
        }
    }

    #[test]
    fn only_a_list_is_written_back() {
        assert_eq!(
            serde_json::to_value(RequestedProcesses::Unreadable).expect("serialize"),
            Value::Null
        );
        let workers = RequestedProcesses::Workers(vec![WorkerProcess {
            name: "queue".to_owned(),
            command: "php artisan queue:work".to_owned(),
            replicas: 1,
            resources: RuntimeResourceLimits::default(),
        }]);
        assert_eq!(
            serde_json::to_value(workers).expect("serialize"),
            json!([{
                "name": "queue",
                "command": "php artisan queue:work",
                "replicas": 1,
                "resources": { "memory_mb": null, "cpu_millis": null, "pids_limit": null },
            }])
        );
    }

    #[test]
    fn a_worker_takes_the_deployment_limits_it_does_not_set() {
        let worker = WorkerProcess {
            name: "queue".to_owned(),
            command: "x".to_owned(),
            replicas: 1,
            resources: RuntimeResourceLimits {
                memory_mb: Some(128),
                cpu_millis: None,
                pids_limit: None,
            },
        };
        let deployment = RuntimeResourceLimits {
            memory_mb: Some(512),
            cpu_millis: Some(250),
            pids_limit: None,
        };
        assert_eq!(
            worker.resources_within(deployment),
            RuntimeResourceLimits {
                memory_mb: Some(128),
                cpu_millis: Some(250),
                pids_limit: None,
            }
        );
    }

    #[test]
    fn process_names_follow_the_rule_of_the_control_plane() {
        for name in ["queue", "q", "queue-high", "worker2", &"a".repeat(30), "a-"] {
            assert!(is_valid_process_name(name), "{name}");
        }
        for name in [
            "",
            "web",
            "release",
            "job",
            "Queue",
            "2queue",
            "-queue",
            "queue_high",
            "queue high",
            "queué",
            &"a".repeat(31),
        ] {
            assert!(!is_valid_process_name(name), "{name}");
        }
    }
}
