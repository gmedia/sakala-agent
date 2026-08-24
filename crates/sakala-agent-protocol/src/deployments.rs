use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::RepositoryAccess;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentBuilder {
    #[default]
    Auto,
    Dockerfile,
    Railpack,
}

/// Product-level resource request resolved by the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeResourceLimits {
    pub memory_mb: Option<u64>,
    pub cpu_millis: Option<u32>,
    pub pids_limit: Option<u32>,
}

/// Product-level phase deadlines resolved by the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeTimeoutLimits {
    pub build_timeout_seconds: Option<u64>,
    pub start_timeout_seconds: Option<u64>,
    pub command_timeout_seconds: Option<u64>,
}

/// Product-level bounds for deployment logs sent to the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogBounds {
    pub max_line_length: Option<u64>,
    pub max_batch_lines: Option<u64>,
    pub max_total_bytes: Option<u64>,
}

/// Concrete resource limits enforced by the runtime node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppliedRuntimeResources {
    pub memory_mb: u64,
    pub cpu_millis: u32,
    pub pids_limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployProjectResult {
    pub requested_resources: RuntimeResourceLimits,
    pub applied_resources: AppliedRuntimeResources,
    #[serde(default, skip_serializing_if = "is_false")]
    pub finalization_deferred: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalization_deferred_reason: Option<FinalizationDeferredReason>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationDeferredReason {
    GraceElapsed,
    RuntimeError,
}

/// Immutable input required to deploy one HTTP application.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployProjectPayload {
    pub repository_url: String,
    pub commit_sha: String,
    #[serde(default)]
    pub repository_access: RepositoryAccess,
    pub domain: String,
    #[serde(default = "default_container_port")]
    pub container_port: u16,
    #[serde(default)]
    pub builder: DeploymentBuilder,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: RuntimeResourceLimits,
    #[serde(default)]
    pub timeouts: RuntimeTimeoutLimits,
    #[serde(default)]
    pub log_bounds: LogBounds,
}

const fn default_container_port() -> u16 {
    3000
}

const fn is_false(value: &bool) -> bool {
    !*value
}
