use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};
use sakala_agent_core::ports::RepositoryCredential;

mod git;

const MAX_ROOT_DIRECTORY_BYTES: usize = 255;

/// Reduces an optional `root_directory` to the directory it names. Empty and `.`
/// both mean the repository root.
#[must_use]
pub fn requested_root_directory(root_directory: Option<&str>) -> Option<&str> {
    root_directory
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != ".")
}

/// Rejects what can be judged from the text alone, before any process runs. The
/// value comes from the control plane, but it is user input and picks a host path,
/// so the check does not rely on the API's own validation.
pub fn validate_root_directory(root_directory: &str) -> Result<(), RuntimeError> {
    let invalid = |reason| Err(RuntimeError::RootDirectoryInvalid { reason });
    if root_directory.len() > MAX_ROOT_DIRECTORY_BYTES {
        return invalid("is too long");
    }
    if root_directory
        .chars()
        .any(|character| character.is_control() || character == '\\')
    {
        return invalid("holds characters a path cannot use");
    }
    let only_descends = Path::new(root_directory)
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if !only_descends {
        return invalid("must not be absolute or contain ..");
    }
    Ok(())
}

pub use git::GitWorkspaceManager;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySource {
    pub repository_url: String,
    pub commit_sha: String,
    pub credential: Option<RepositoryCredential>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentWorkspace {
    root: PathBuf,
    source: PathBuf,
}

impl DeploymentWorkspace {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let source = root.join("source");
        Self { root, source }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Resolves the directory a build works in. Only the checkout can show where
    /// a symlink leads, so this runs after it, and the resolved path is what gets
    /// used: nothing follows the link again later.
    pub async fn build_context(
        &self,
        root_directory: Option<&str>,
    ) -> Result<PathBuf, RuntimeError> {
        let Some(root_directory) = requested_root_directory(root_directory) else {
            return Ok(self.source.clone());
        };
        validate_root_directory(root_directory)?;
        let invalid = |reason| RuntimeError::RootDirectoryInvalid { reason };
        let source = tokio::fs::canonicalize(&self.source).await?;
        let context = tokio::fs::canonicalize(self.source.join(root_directory))
            .await
            .map_err(|_| invalid("does not exist in the repository"))?;
        if !context.starts_with(&source) {
            return Err(invalid("leads outside the repository through a symlink"));
        }
        if !tokio::fs::metadata(&context).await?.is_dir() {
            return Err(invalid("is not a directory"));
        }
        Ok(context)
    }
}

impl RepositorySource {
    #[must_use]
    pub fn without_credential(&self) -> Self {
        Self {
            repository_url: self.repository_url.clone(),
            commit_sha: self.commit_sha.clone(),
            credential: None,
        }
    }
}

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    async fn checkout(
        &self,
        command_id: Uuid,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
        cancellation: CancellationToken,
    ) -> Result<DeploymentWorkspace, RuntimeError>;

    async fn cleanup(&self, workspace: &DeploymentWorkspace) -> Result<(), RuntimeError>;

    async fn cleanup_stale(&self, minimum_age: Duration) -> Result<usize, RuntimeError>;

    /// Free bytes available on the filesystem that owns the workspace root.
    async fn available_disk_bytes(&self) -> Result<u64, RuntimeError>;
}
