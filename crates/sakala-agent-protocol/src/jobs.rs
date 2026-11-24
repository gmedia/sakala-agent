//! One-off job runs from a project's live image (ADR-022).
//!
//! The control plane is the only scheduler: for each due slot of a schedule,
//! and for "Run now", it creates one `RunJob` command pinned to the node that
//! serves the project. The Agent runs the command once, in a container of the
//! live deployment's image, and reports how it ended.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    LogBounds, RequestedImageReuse, RuntimeResourceLimits, deployments::default_container_port,
};

/// Capability of a node that runs `RunJob`. An older Agent fails to read a
/// poll response that holds an unknown command type, so the control plane
/// assigns `RunJob` only to a node that advertises it.
pub const SCHEDULED_JOBS_CAPABILITY: &str = "scheduled-jobs";

/// How long a job may run when its payload names no timeout.
pub const DEFAULT_JOB_TIMEOUT_SECONDS: u64 = 300;
/// The longest timeout a job may ask for, the release command's node deadline.
pub const MAX_JOB_TIMEOUT_SECONDS: u64 = 600;

/// Event sent once the job container started. Metadata: `container`,
/// `image`, `timeout_seconds`.
pub const JOB_STARTED_EVENT: &str = "job.started";
/// Event sent once the job container ended, whatever the outcome, before the
/// terminal report. Metadata: `exit_code` (`null` after a timeout),
/// `duration_ms`, `oom_killed`, `timed_out`.
pub const JOB_FINISHED_EVENT: &str = "job.finished";

/// Payload of `RunJob`. The image identity, variables, resources, port, and
/// log bounds are copied from the live deployment's `DeployProject` payload,
/// so a run uses exactly what the live version runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunJobPayload {
    /// Runs as `sh -c <command>` in the job container. 1 to 1000 characters
    /// without control characters, the rule of the release command.
    pub command: String,
    /// Seconds the job may run before it is stopped. Absent or `null` is
    /// [`DEFAULT_JOB_TIMEOUT_SECONDS`]; at most [`MAX_JOB_TIMEOUT_SECONDS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// The commit of the live deployment.
    pub commit_sha: String,
    /// The deployment that built the live image, when the live deployment
    /// reused it. Absent means the live deployment, the command's
    /// `deployment_id`, built it.
    #[serde(default, skip_serializing_if = "RequestedImageReuse::is_absent")]
    pub reuse_image_from_deployment_id: RequestedImageReuse,
    /// Exported as `PORT`, as for the web process, so a job reading the same
    /// configuration as the app sees the same value. Nothing listens on it.
    #[serde(default = "default_container_port")]
    pub container_port: u16,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: RuntimeResourceLimits,
    #[serde(default)]
    pub log_bounds: LogBounds,
}

impl RunJobPayload {
    /// The requested timeout, or the default when none was requested. Not
    /// checked against [`MAX_JOB_TIMEOUT_SECONDS`]: the runtime refuses it.
    #[must_use]
    pub fn timeout_seconds_or_default(&self) -> u64 {
        self.timeout_seconds.unwrap_or(DEFAULT_JOB_TIMEOUT_SECONDS)
    }
}

/// Completion result of a `RunJob` whose command exited with code 0. Any
/// other ending is a failure with its own `error_code`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunJobResult {
    pub exit_code: i32,
    /// From the start of the job container until it exited.
    pub duration_ms: u64,
    pub oom_killed: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn a_minimal_payload_takes_the_defaults() {
        let payload: RunJobPayload = serde_json::from_value(json!({
            "command": "php artisan report:daily",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
        }))
        .expect("payload");
        assert_eq!(
            payload.timeout_seconds_or_default(),
            DEFAULT_JOB_TIMEOUT_SECONDS
        );
        assert_eq!(payload.container_port, 3000);
        assert_eq!(
            payload.reuse_image_from_deployment_id,
            RequestedImageReuse::Absent
        );
        assert!(payload.environment.is_empty());
        assert_eq!(payload.resources, RuntimeResourceLimits::default());
    }

    #[test]
    fn the_image_of_an_earlier_deployment_is_read_like_a_deploy_payload() {
        let payload: RunJobPayload = serde_json::from_value(json!({
            "command": "npm run cleanup",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "timeout_seconds": 120,
            "reuse_image_from_deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
        }))
        .expect("payload");
        assert_eq!(payload.timeout_seconds_or_default(), 120);
        assert_eq!(
            payload.reuse_image_from_deployment_id,
            RequestedImageReuse::Deployment(
                Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID")
            )
        );
    }

    #[test]
    fn a_payload_without_a_command_or_commit_is_refused() {
        for payload in [
            json!({ "commit_sha": "0123456789abcdef0123456789abcdef01234567" }),
            json!({ "command": "npm run cleanup" }),
            json!({ "command": ["npm", "run"], "commit_sha": "0123456789abcdef0123456789abcdef01234567" }),
        ] {
            assert!(
                serde_json::from_value::<RunJobPayload>(payload.clone()).is_err(),
                "{payload}"
            );
        }
    }

    #[test]
    fn the_result_has_its_wire_names() {
        assert_eq!(
            serde_json::to_value(RunJobResult {
                exit_code: 0,
                duration_ms: 1_250,
                oom_killed: false,
            })
            .expect("serialize"),
            json!({ "exit_code": 0, "duration_ms": 1_250, "oom_killed": false })
        );
    }
}
