//! The files Edge owns in its state directory.
//!
//! - `routes.caddy`: the Caddyfile fragment the host Caddyfile imports.
//! - `custom-domains.caddy`: one site per custom domain (ADR-024), imported at
//!   the top level of the host Caddyfile. Its path can be set apart from the
//!   state directory.
//! - `routes.json`: the last table Caddy accepted, in the API's format, used to
//!   serve again after a restart while the API is unreachable.
//! - `status.json`: the sync state for monitoring.
//!
//! Every file is replaced by writing a temporary file next to it and renaming
//! it into place, so Caddy, a restart, or a monitoring probe never reads half
//! of one.

use std::{
    io,
    path::{Path, PathBuf},
};

use tokio::fs;

pub const ROUTES_FILE: &str = "routes.caddy";
pub const CUSTOM_DOMAINS_FILE: &str = "custom-domains.caddy";
pub const SNAPSHOT_FILE: &str = "routes.json";
pub const STATUS_FILE: &str = "status.json";
/// Suffix of the temporary file each write goes through.
const TEMPORARY_SUFFIX: &str = ".tmp";

/// What a Caddyfile fragment held before a replacement, to put it back when
/// Caddy refuses the new one.
#[derive(Debug)]
pub struct RoutesBackup {
    path: PathBuf,
    previous: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct EdgeFiles {
    dir: PathBuf,
    custom_domains_path: PathBuf,
}

impl EdgeFiles {
    /// Every file in `dir`, the custom domains file included.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            custom_domains_path: dir.join(CUSTOM_DOMAINS_FILE),
            dir,
        }
    }

    /// Writes the custom domains file to `path` instead.
    #[must_use]
    pub fn with_custom_domains_path(mut self, path: PathBuf) -> Self {
        self.custom_domains_path = path;
        self
    }

    #[must_use]
    pub fn custom_domains_path(&self) -> &Path {
        &self.custom_domains_path
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn routes_path(&self) -> PathBuf {
        self.dir.join(ROUTES_FILE)
    }

    #[must_use]
    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_FILE)
    }

    #[must_use]
    pub fn status_path(&self) -> PathBuf {
        self.dir.join(STATUS_FILE)
    }

    pub async fn ensure_dir(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir).await
    }

    /// Replaces the routes file with `content`. `None` when it already holds
    /// exactly that, so nothing was written and Caddy needs no reload.
    pub async fn replace_routes(&self, content: &str) -> io::Result<Option<RoutesBackup>> {
        replace(self.routes_path(), content).await
    }

    /// [`Self::replace_routes`] for the custom domains file.
    pub async fn replace_custom_domains(&self, content: &str) -> io::Result<Option<RoutesBackup>> {
        replace(self.custom_domains_path.clone(), content).await
    }

    /// Puts back what a file held before [`Self::replace_routes`] or
    /// [`Self::replace_custom_domains`]. A file that did not exist is removed
    /// again.
    pub async fn restore_routes(&self, backup: RoutesBackup) -> io::Result<()> {
        match backup.previous {
            Some(content) => write_atomic(&backup.path, &content).await,
            None => match fs::remove_file(&backup.path).await {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            },
        }
    }

    pub async fn read_snapshot(&self) -> io::Result<Option<Vec<u8>>> {
        read_optional(&self.snapshot_path()).await
    }

    pub async fn write_snapshot(&self, content: &[u8]) -> io::Result<()> {
        write_atomic(&self.snapshot_path(), content).await
    }

    pub async fn write_status(&self, content: &[u8]) -> io::Result<()> {
        write_atomic(&self.status_path(), content).await
    }
}

async fn replace(path: PathBuf, content: &str) -> io::Result<Option<RoutesBackup>> {
    let previous = read_optional(&path).await?;
    if previous.as_deref() == Some(content.as_bytes()) {
        return Ok(None);
    }
    write_atomic(&path, content.as_bytes()).await?;
    Ok(Some(RoutesBackup { path, previous }))
}

async fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path).await {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes `<path>.tmp`, flushes it to disk, and renames it over `path`, so a
/// crash leaves either the old file or the new one, never a mix.
async fn write_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(TEMPORARY_SUFFIX);
    let temporary = PathBuf::from(temporary);
    let file = fs::File::create(&temporary).await?;
    let mut writer = tokio::io::BufWriter::new(file);
    tokio::io::AsyncWriteExt::write_all(&mut writer, content).await?;
    tokio::io::AsyncWriteExt::flush(&mut writer).await?;
    writer.into_inner().sync_all().await?;
    fs::rename(&temporary, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replacing_identical_routes_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = EdgeFiles::new(dir.path().to_path_buf());

        assert!(files.replace_routes("a").await.expect("write").is_some());
        assert!(files.replace_routes("a").await.expect("write").is_none());
        assert!(!dir.path().join("routes.caddy.tmp").exists());
    }

    #[tokio::test]
    async fn restoring_puts_back_the_previous_routes_or_removes_new_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = EdgeFiles::new(dir.path().to_path_buf());

        let first = files
            .replace_routes("old")
            .await
            .expect("write")
            .expect("changed");
        let second = files
            .replace_routes("new")
            .await
            .expect("write")
            .expect("changed");
        files.restore_routes(second).await.expect("restore");
        assert_eq!(
            fs::read_to_string(files.routes_path()).await.expect("read"),
            "old"
        );

        files.restore_routes(first).await.expect("restore");
        assert!(!files.routes_path().exists());
    }

    #[tokio::test]
    async fn the_custom_domains_file_has_its_own_path_and_backup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let path = elsewhere.path().join("custom.caddy");
        let files = EdgeFiles::new(dir.path().to_path_buf()).with_custom_domains_path(path.clone());
        assert_eq!(
            EdgeFiles::new(dir.path().to_path_buf()).custom_domains_path(),
            dir.path().join("custom-domains.caddy")
        );

        files.replace_routes("routes").await.expect("write");
        let backup = files
            .replace_custom_domains("custom")
            .await
            .expect("write")
            .expect("changed");
        assert_eq!(fs::read_to_string(&path).await.expect("read"), "custom");
        files.restore_routes(backup).await.expect("restore");
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(files.routes_path()).await.expect("read"),
            "routes"
        );
    }
}
