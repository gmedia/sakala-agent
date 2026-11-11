mod dockerfile;

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use sakala_agent_protocol::ProjectInspection;
use serde_json::Value;
use tokio::{fs, io::AsyncReadExt};

use crate::{
    RuntimeError, RuntimeReporter, railpack::RailpackSandbox, workspace::RepositorySource,
};

#[async_trait]
pub trait ProjectInspector: Send + Sync {
    async fn inspect(
        &self,
        context: &Path,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError>;
}

pub struct RailpackProjectInspector {
    sandbox: Arc<RailpackSandbox>,
}

impl RailpackProjectInspector {
    #[must_use]
    pub fn new(sandbox: Arc<RailpackSandbox>) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl ProjectInspector for RailpackProjectInspector {
    async fn inspect(
        &self,
        context: &Path,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<ProjectInspection, RuntimeError> {
        let manifests = detected_manifests(context).await?;
        let dockerfile_found = manifests.iter().any(|name| name == "Dockerfile");
        let railpack: Value = match self.sandbox.info(context, reporter).await {
            // The parse error is not reported: the report is attacker-controlled.
            Ok(info) => serde_json::from_slice(&info).map_err(|_| {
                RuntimeError::Execution("Railpack produced an unreadable report".to_owned())
            })?,
            // Railpack fails on a repository with only a Dockerfile, but a Dockerfile
            // build never asks it. Its output is already in the command log.
            Err(RuntimeError::Build(_)) if dockerfile_found => Value::Null,
            Err(error) => return Err(error),
        };
        let exposed_ports = match read_dockerfile(context).await {
            Some(contents) => dockerfile::exposed_tcp_ports(&contents),
            None => Vec::new(),
        };

        Ok(ProjectInspection {
            repository_url: source.repository_url.clone(),
            commit_sha: source.commit_sha.clone(),
            dockerfile_found,
            env_example_found: manifests.iter().any(|name| name == ".env.example"),
            compose_found: manifests.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "compose.yml" | "compose.yaml" | "docker-compose.yml" | "docker-compose.yaml"
                )
            }),
            package_manager: detect_package_manager(&manifests).map(str::to_owned),
            manifests,
            exposed_ports,
            railpack,
        })
    }
}

/// Larger than any real Dockerfile; the file comes from the repository.
const MAX_DOCKERFILE_BYTES: u64 = 1024 * 1024;

/// The Dockerfile a build in `context` would use, or `None` when there is none to read.
/// A symlink is followed only while it stays inside the context, because the
/// repository chooses where it points.
async fn read_dockerfile(context: &Path) -> Option<String> {
    let root = fs::canonicalize(context).await.ok()?;
    let path = fs::canonicalize(context.join("Dockerfile")).await.ok()?;
    if !path.starts_with(&root) || !fs::metadata(&path).await.ok()?.is_file() {
        return None;
    }
    let mut contents = Vec::new();
    fs::File::open(&path)
        .await
        .ok()?
        .take(MAX_DOCKERFILE_BYTES + 1)
        .read_to_end(&mut contents)
        .await
        .ok()?;
    if contents.len() as u64 > MAX_DOCKERFILE_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&contents).into_owned())
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
        // Only the presence of a name is read here, so a symlink cannot leak.
        if fs::symlink_metadata(source.join(candidate)).await.is_ok() {
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
