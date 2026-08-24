use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RepositoryAccess;

/// Immutable repository input used for the lightweight project-preview workflow.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InspectProjectPayload {
    pub repository_url: String,
    pub commit_sha: String,
    #[serde(default)]
    pub repository_access: RepositoryAccess,
}

/// Stable Sakala metadata plus the raw Railpack result for forward compatibility.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectInspection {
    pub repository_url: String,
    pub commit_sha: String,
    pub dockerfile_found: bool,
    pub env_example_found: bool,
    pub compose_found: bool,
    pub manifests: Vec<String>,
    pub package_manager: Option<String>,
    pub railpack: Value,
}
