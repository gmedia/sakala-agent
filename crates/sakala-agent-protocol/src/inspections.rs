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
    /// The JavaScript workspace the root directory belongs to, when it is a package
    /// of one at a higher directory. Always sent, `null` when there is none, so a
    /// missing field means an Agent before this field.
    #[serde(default)]
    pub workspace: Option<JsWorkspace>,
    pub railpack: Value,
}

/// The package manager that installs a JavaScript workspace, as Railpack picks it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JsPackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl JsPackageManager {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }
}

/// A root directory that is one package of a JavaScript workspace (monorepo). Its
/// lockfile and package manager live at the workspace root, so a Railpack build
/// runs there and the commands below target the package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JsWorkspace {
    pub manager: JsPackageManager,
    /// The workspace root, relative to the repository root, `.` for the repository
    /// root itself. The build context.
    pub root_directory: String,
    /// The package, relative to the workspace root.
    pub package_directory: String,
    /// `name` from the package's `package.json`, `null` when it has none.
    pub package_name: Option<String>,
    /// The build command that runs at the workspace root, `null` when none is set
    /// and the package has no `build` script.
    pub build_command: Option<String>,
    /// The start command, `null` when the image serves `static_output_directory`
    /// or when neither a command nor a `start` script exists.
    pub start_command: Option<String>,
    /// The directory of a static build (a Vite single-page app), relative to the
    /// workspace root, which the image serves with Railpack's own web server.
    pub static_output_directory: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{JsPackageManager, JsWorkspace, ProjectInspection};

    fn result_without_ports() -> serde_json::Value {
        // A result from an Agent before `exposed_ports` and `workspace`.
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

    #[test]
    fn workspace_is_null_without_one_and_absent_only_from_an_older_agent() {
        let mut inspection: ProjectInspection =
            serde_json::from_value(result_without_ports()).expect("older result");
        assert_eq!(inspection.workspace, None);
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["workspace"],
            serde_json::Value::Null
        );
        inspection.workspace = Some(JsWorkspace {
            manager: JsPackageManager::Pnpm,
            root_directory: ".".to_owned(),
            package_directory: "apps/web".to_owned(),
            package_name: Some("@example/web".to_owned()),
            build_command: Some("pnpm --filter '@example/web...' run build".to_owned()),
            start_command: None,
            static_output_directory: Some("apps/web/dist".to_owned()),
        });
        assert_eq!(
            serde_json::to_value(&inspection).expect("json")["workspace"],
            json!({
                "manager": "pnpm",
                "root_directory": ".",
                "package_directory": "apps/web",
                "package_name": "@example/web",
                "build_command": "pnpm --filter '@example/web...' run build",
                "start_command": null,
                "static_output_directory": "apps/web/dist"
            })
        );
    }
}
