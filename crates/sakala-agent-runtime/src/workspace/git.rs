use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::fs;
use uuid::Uuid;

use crate::{
    CommandSpec, ProcessRunner, RuntimeError, RuntimeReporter,
    process::run_checked,
    workspace::{DeploymentWorkspace, RepositorySource, WorkspaceManager},
};

pub struct GitWorkspaceManager {
    root: PathBuf,
    runner: Arc<dyn ProcessRunner>,
}

impl GitWorkspaceManager {
    #[must_use]
    pub fn new(root: PathBuf, runner: Arc<dyn ProcessRunner>) -> Self {
        Self { root, runner }
    }
}

#[async_trait]
impl WorkspaceManager for GitWorkspaceManager {
    async fn checkout(
        &self,
        command_id: Uuid,
        source: &RepositorySource,
        reporter: &dyn RuntimeReporter,
    ) -> Result<DeploymentWorkspace, RuntimeError> {
        let workspace = DeploymentWorkspace::new(self.root.join(command_id.to_string()));
        if fs::try_exists(workspace.root()).await? {
            fs::remove_dir_all(workspace.root()).await?;
        }
        fs::create_dir_all(workspace.root()).await?;

        let checkout_result = async {
            for (phase, command) in [
                (
                    "git-init",
                    CommandSpec::new("git")
                        .arg("init")
                        .arg("--initial-branch=sakala")
                        .arg(workspace.source().as_os_str()),
                ),
                (
                    "git-remote",
                    CommandSpec::new("git")
                        .arg("-C")
                        .arg(workspace.source().as_os_str())
                        .arg("remote")
                        .arg("add")
                        .arg("origin")
                        .arg(&source.repository_url),
                ),
                (
                    "git-fetch",
                    CommandSpec::new("git")
                        .arg("-C")
                        .arg(workspace.source().as_os_str())
                        .arg("fetch")
                        .arg("--depth=1")
                        .arg("origin")
                        .arg(&source.commit_sha),
                ),
                (
                    "git-checkout",
                    CommandSpec::new("git")
                        .arg("-C")
                        .arg(workspace.source().as_os_str())
                        .arg("checkout")
                        .arg("--detach")
                        .arg("FETCH_HEAD"),
                ),
            ] {
                run_checked(self.runner.as_ref(), &command, phase, reporter).await?;
            }

            Ok::<(), RuntimeError>(())
        }
        .await;

        if let Err(error) = checkout_result {
            let _ = self.cleanup(&workspace).await;
            return Err(error);
        }

        Ok(workspace)
    }

    async fn cleanup(&self, workspace: &DeploymentWorkspace) -> Result<(), RuntimeError> {
        match fs::remove_dir_all(workspace.root()).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
