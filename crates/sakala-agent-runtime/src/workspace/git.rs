use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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
        restrict_workspace_permissions(workspace.root()).await?;

        let askpass = match &source.credential {
            Some(_) => Some(write_askpass(workspace.root()).await?),
            None => None,
        };

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

            let mut fetch = CommandSpec::new("git")
                .arg("-C")
                .arg(workspace.source().as_os_str())
                .arg("fetch")
                .arg("--depth=1")
                .arg("origin")
                .arg(&source.commit_sha);
            if let (Some(credential), Some(askpass)) = (&source.credential, &askpass) {
                fetch = fetch
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("GIT_ASKPASS", askpass.display().to_string())
                    .env("SAKALA_GIT_ASKPASS_USERNAME", &credential.username)
                    .secret_env("SAKALA_GIT_ASKPASS_TOKEN", credential.token.clone());
            }
            run_checked(self.runner.as_ref(), &fetch, "git-fetch", reporter).await?;

            Ok::<(), RuntimeError>(())
        }
        .await;

        let askpass_cleanup = match askpass {
            Some(askpass) => fs::remove_file(&askpass).await.map_err(RuntimeError::from),
            None => Ok(()),
        };

        if let Err(error) = checkout_result {
            let _ = self.cleanup(&workspace).await;
            return Err(error);
        }

        if let Err(error) = askpass_cleanup {
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

async fn restrict_workspace_permissions(root: &Path) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

const ASKPASS_SCRIPT: &str = "#!/bin/sh\ncase \"$1\" in\n  *Username*) printf '%s\\n' \"$SAKALA_GIT_ASKPASS_USERNAME\" ;;\n  *) printf '%s\\n' \"$SAKALA_GIT_ASKPASS_TOKEN\" ;;\nesac\n";

async fn write_askpass(root: &Path) -> Result<PathBuf, RuntimeError> {
    let path = root.join(".sakala-git-askpass");
    fs::write(&path, ASKPASS_SCRIPT).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{restrict_workspace_permissions, write_askpass};

    #[tokio::test]
    async fn temporary_checkout_files_are_owner_only() {
        let temp = TempDir::new().expect("temporary directory should be available");
        let workspace = temp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace should be created");

        restrict_workspace_permissions(&workspace)
            .await
            .expect("workspace permission should be restricted");
        let askpass = write_askpass(&workspace)
            .await
            .expect("askpass script should be written");

        assert_eq!(
            std::fs::metadata(&workspace)
                .expect("workspace metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(askpass)
                .expect("askpass metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
