use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::ProjectInspection;
use serde_json::Value;
use tokio::fs;

use crate::{
    ProcessRunner, RuntimeError, RuntimeReporter,
    process::run_checked,
    railpack::info_command,
    workspace::{DeploymentWorkspace, RepositorySource},
};

const MAX_RAILPACK_INFO_BYTES: u64 = 1024 * 1024;

#[async_trait]
pub trait ProjectInspector: Send + Sync {
    async fn inspect(
        &self,
        workspace: &DeploymentWorkspace,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError>;
}

pub struct RailpackProjectInspector {
    runner: Arc<dyn ProcessRunner>,
}

impl RailpackProjectInspector {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl ProjectInspector for RailpackProjectInspector {
    async fn inspect(
        &self,
        workspace: &DeploymentWorkspace,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError> {
        let info_path = workspace.root().join("railpack-info.json");
        run_checked(
            self.runner.as_ref(),
            &info_command(workspace.source(), &info_path),
            "railpack-info",
            reporter,
        )
        .await?;

        let metadata = fs::metadata(&info_path).await?;
        if metadata.len() > MAX_RAILPACK_INFO_BYTES {
            return Err(RuntimeError::Execution(format!(
                "Railpack info exceeded the {} byte safety limit",
                MAX_RAILPACK_INFO_BYTES
            )));
        }
        let railpack: Value =
            serde_json::from_slice(&fs::read(&info_path).await?).map_err(|error| {
                RuntimeError::Execution(format!("invalid Railpack info JSON: {error}"))
            })?;
        let manifests = detected_manifests(workspace.source()).await?;

        Ok(ProjectInspection {
            repository_url: source.repository_url.clone(),
            commit_sha: source.commit_sha.clone(),
            dockerfile_found: manifests.iter().any(|name| name == "Dockerfile"),
            env_example_found: manifests.iter().any(|name| name == ".env.example"),
            compose_found: manifests.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "compose.yml" | "compose.yaml" | "docker-compose.yml" | "docker-compose.yaml"
                )
            }),
            package_manager: detect_package_manager(&manifests).map(str::to_owned),
            manifests,
            railpack,
        })
    }
}

async fn detected_manifests(source: &Path) -> Result<Vec<String>, RuntimeError> {
    const CANDIDATES: &[&str] = &[
        ".env.example",
        "Cargo.toml",
        "Dockerfile",
        "README.md",
        "bun.lock",
        "bun.lockb",
        "composer.json",
        "composer.lock",
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
        "go.mod",
        "package-lock.json",
        "package.json",
        "pnpm-lock.yaml",
        "poetry.lock",
        "pyproject.toml",
        "requirements.txt",
        "uv.lock",
        "yarn.lock",
    ];

    let mut manifests = Vec::new();
    for candidate in CANDIDATES {
        if fs::try_exists(source.join(candidate)).await? {
            manifests.push((*candidate).to_owned());
        }
    }
    Ok(manifests)
}

fn detect_package_manager(manifests: &[String]) -> Option<&'static str> {
    for (manifest, package_manager) in [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
        ("package-lock.json", "npm"),
        ("composer.lock", "composer"),
        ("uv.lock", "uv"),
        ("poetry.lock", "poetry"),
        ("Cargo.toml", "cargo"),
        ("go.mod", "go"),
    ] {
        if manifests.iter().any(|item| item == manifest) {
            return Some(package_manager);
        }
    }
    None
}
