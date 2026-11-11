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
    /// Inspects this directory instead of the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_directory: Option<String>,
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
    /// TCP ports that the final stage of `Dockerfile` declares with `EXPOSE`, sorted
    /// and without duplicates. Empty without a Dockerfile, without `EXPOSE`, or when no
    /// port could be resolved. Agents before this field omitted it.
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    pub railpack: Value,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ProjectInspection;

    fn result_without_ports() -> serde_json::Value {
        json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "dockerfile_found": true,
            "env_example_found": false,
            "compose_found": false,
            "manifests": ["Dockerfile"],
            "package_manager": null,
            "railpack": {}
        })
    }

    #[test]
    fn a_result_from_an_agent_before_exposed_ports_still_reads() {
        let inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert!(inspection.exposed_ports.is_empty());
    }

    #[test]
    fn exposed_ports_is_always_sent_so_absence_means_an_older_agent() {
        let mut inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["exposed_ports"],
            json!([])
        );
        inspection.exposed_ports = vec![80];
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["exposed_ports"],
            json!([80])
        );
    }
}
