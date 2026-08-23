use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use tokio::fs;
use tokio_util::sync::CancellationToken;
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
        cancellation: CancellationToken,
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
                        .arg(workspace.source().as_os_str())
                        .cancellation(cancellation.clone()),
                ),
                (
                    "git-remote",
                    CommandSpec::new("git")
                        .arg("-C")
                        .arg(workspace.source().as_os_str())
                        .arg("remote")
                        .arg("add")
                        .arg("origin")
                        .arg(&source.repository_url)
                        .cancellation(cancellation.clone()),
                ),
                (
                    "git-checkout",
                    CommandSpec::new("git")
                        .arg("-C")
                        .arg(workspace.source().as_os_str())
                        .arg("checkout")
                        .arg("--detach")
                        .arg("FETCH_HEAD")
                        .cancellation(cancellation.clone()),
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
                .arg(&source.commit_sha)
                .cancellation(cancellation.clone());
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

    async fn cleanup_stale(&self, minimum_age: Duration) -> Result<usize, RuntimeError> {
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let now = SystemTime::now();
        let mut cleaned = 0;

        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            if Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err() {
                continue;
            }
            let modified = entry.metadata().await?.modified()?;
            let age = now.duration_since(modified).unwrap_or_default();
            if age < minimum_age {
                continue;
            }

            fs::remove_dir_all(entry.path()).await?;
            cleaned += 1;
        }

        Ok(cleaned)
    }

    async fn available_disk_bytes(&self) -> Result<u64, RuntimeError> {
        let command = CommandSpec::new("df").arg("-Pk").arg(self.root.as_os_str());
        let output = self.runner.run(&command, &crate::NullOutputSink).await?;
        if !output.success {
            return Err(RuntimeError::Dependency(format!(
                "workspace disk inspection exited with status {:?}",
                output.code
            )));
        }
        parse_available_disk_bytes(&output.stdout)
    }
}

fn parse_available_disk_bytes(output: &str) -> Result<u64, RuntimeError> {
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            RuntimeError::Dependency("workspace disk inspection returned no data".to_owned())
        })?;
    let available_blocks = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| {
            RuntimeError::Dependency("workspace disk inspection format is invalid".to_owned())
        })?
        .parse::<u64>()
        .map_err(|_| {
            RuntimeError::Dependency("workspace disk availability is invalid".to_owned())
        })?;
    available_blocks.checked_mul(1_024).ok_or_else(|| {
        RuntimeError::Dependency("workspace disk availability is too large to represent".to_owned())
    })
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
    use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::{
        ProcessOutput, ProcessOutputSink, ProcessRunner, RuntimeError,
        workspace::{GitWorkspaceManager, WorkspaceManager},
    };

    use super::{parse_available_disk_bytes, restrict_workspace_permissions, write_askpass};

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

    #[tokio::test]
    async fn workspace_gc_only_removes_stale_uuid_directories() {
        let temp = TempDir::new().expect("temporary directory should be available");
        let stale = temp.path().join(Uuid::new_v4().to_string());
        let unrelated = temp.path().join("keep-me");
        tokio::fs::create_dir_all(&stale)
            .await
            .expect("stale workspace should be created");
        tokio::fs::create_dir_all(&unrelated)
            .await
            .expect("unrelated directory should be created");
        let manager = GitWorkspaceManager::new(temp.path().to_owned(), Arc::new(UnusedRunner));

        let cleaned = manager
            .cleanup_stale(Duration::ZERO)
            .await
            .expect("workspace GC should complete");

        assert_eq!(cleaned, 1);
        assert!(!stale.exists());
        assert!(unrelated.exists());
    }

    #[tokio::test]
    async fn workspace_gc_never_follows_uuid_named_symlinks() {
        let temp = TempDir::new().expect("temporary directory should be available");
        let target = temp.path().join("outside-workspace");
        let symlink = temp.path().join(Uuid::new_v4().to_string());
        tokio::fs::create_dir_all(&target)
            .await
            .expect("target directory should be created");
        std::os::unix::fs::symlink(&target, &symlink).expect("workspace symlink should be created");
        let manager = GitWorkspaceManager::new(temp.path().to_owned(), Arc::new(UnusedRunner));

        let cleaned = manager
            .cleanup_stale(Duration::ZERO)
            .await
            .expect("workspace GC should complete");

        assert_eq!(cleaned, 0);
        assert!(symlink.exists());
        assert!(target.exists());
    }

    #[test]
    fn parses_posix_df_available_blocks() {
        let available = parse_available_disk_bytes(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10000 2500 7500 25% /var/lib/sakala\n",
        )
        .expect("df output should parse");

        assert_eq!(available, 7_500 * 1_024);
    }

    struct UnusedRunner;

    #[async_trait]
    impl ProcessRunner for UnusedRunner {
        async fn run(
            &self,
            _spec: &crate::CommandSpec,
            _sink: &dyn ProcessOutputSink,
        ) -> Result<ProcessOutput, RuntimeError> {
            unreachable!("workspace GC does not run subprocesses")
        }
    }
}
