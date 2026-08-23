use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{RuntimeError, RuntimeReporter};
use sakala_agent_core::ports::RepositoryCredential;

mod git;

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
}
