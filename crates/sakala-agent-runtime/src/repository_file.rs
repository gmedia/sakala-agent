//! Bounded reads of files and directories inside a checked-out repository.
//!
//! Everything here is repository content read on the host, so the repository picks
//! where a symlink points. A path is only used after it is canonicalized and found
//! inside `root`, files must be regular files, every read stops one byte past its cap,
//! and directory listings stop after a fixed number of entries. Callers pass a
//! canonical `root`; anything unreadable is `None`, never an error with the content.

use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncReadExt};

/// Larger than any real manifest or config file.
pub(crate) const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// The canonical path of `path` when it is a regular file inside `root`.
pub(crate) async fn inside_file(root: &Path, path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).await.ok()?;
    (path.starts_with(root) && fs::metadata(&path).await.ok()?.is_file()).then_some(path)
}

pub(crate) async fn is_file_inside(root: &Path, path: &Path) -> bool {
    inside_file(root, path).await.is_some()
}

/// The canonical path of `path` when it is a directory inside `root`.
pub(crate) async fn inside_directory(root: &Path, path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).await.ok()?;
    (path.starts_with(root) && fs::metadata(&path).await.ok()?.is_dir()).then_some(path)
}

pub(crate) async fn is_directory_inside(root: &Path, path: &Path) -> bool {
    inside_directory(root, path).await.is_some()
}

/// The bytes of a regular file inside `root`, `None` when it is larger than `max_bytes`.
pub(crate) async fn read_bytes(root: &Path, path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let path = inside_file(root, path).await?;
    let mut contents = Vec::new();
    fs::File::open(&path)
        .await
        .ok()?
        .take(max_bytes + 1)
        .read_to_end(&mut contents)
        .await
        .ok()?;
    (contents.len() as u64 <= max_bytes).then_some(contents)
}

/// A UTF-8 file inside `root`, at most `max_bytes`.
pub(crate) async fn read_text(root: &Path, path: &Path, max_bytes: u64) -> Option<String> {
    String::from_utf8(read_bytes(root, path, max_bytes).await?).ok()
}

/// What a directory entry is, without following a symlink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryEntry {
    pub(crate) name: String,
    pub(crate) kind: EntryKind,
}

/// At most `max_entries` entries of a directory inside `root`, sorted by name so the
/// result does not depend on the file system's order. Names that are not UTF-8 are
/// skipped but still counted, so the work stays bounded.
pub(crate) async fn list_directory(
    root: &Path,
    path: &Path,
    max_entries: usize,
) -> Vec<DirectoryEntry> {
    let Some(directory) = inside_directory(root, path).await else {
        return Vec::new();
    };
    let Ok(mut reader) = fs::read_dir(&directory).await else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for _ in 0..max_entries {
        let Ok(Some(entry)) = reader.next_entry().await else {
            break;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let kind = match entry.file_type().await {
            Ok(kind) if kind.is_file() => EntryKind::File,
            Ok(kind) if kind.is_dir() => EntryKind::Directory,
            _ => EntryKind::Other,
        };
        entries.push(DirectoryEntry { name, kind });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    async fn canonical(temp: &TempDir) -> PathBuf {
        fs::canonicalize(temp.path()).await.expect("canonical")
    }

    #[tokio::test]
    async fn a_file_at_the_cap_is_read_and_one_byte_more_is_not() {
        let temp = TempDir::new().expect("temp");
        let root = canonical(&temp).await;
        std::fs::write(root.join("exact"), [b'a'; 8]).expect("file");
        std::fs::write(root.join("over"), [b'a'; 9]).expect("file");
        assert_eq!(
            read_bytes(&root, &root.join("exact"), 8).await,
            Some(vec![b'a'; 8])
        );
        assert_eq!(read_bytes(&root, &root.join("over"), 8).await, None);
    }

    #[tokio::test]
    async fn a_symlink_is_followed_only_while_it_stays_inside_the_root() {
        let outside = TempDir::new().expect("temp");
        std::fs::write(outside.path().join("secret"), "token").expect("file");
        let temp = TempDir::new().expect("temp");
        let root = canonical(&temp).await;
        std::fs::write(root.join("inside"), "ok").expect("file");
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("out"))
            .expect("symlink");
        std::os::unix::fs::symlink(root.join("inside"), root.join("in")).expect("symlink");
        std::os::unix::fs::symlink(outside.path(), root.join("outdir")).expect("symlink");
        assert_eq!(read_text(&root, &root.join("out"), 64).await, None);
        assert_eq!(
            read_text(&root, &root.join("in"), 64).await.as_deref(),
            Some("ok")
        );
        assert!(!is_directory_inside(&root, &root.join("outdir")).await);
        assert!(
            list_directory(&root, &root.join("outdir"), 10)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_listing_stops_after_its_cap() {
        let temp = TempDir::new().expect("temp");
        let root = canonical(&temp).await;
        for index in 0..5 {
            std::fs::write(root.join(format!("{index}.php")), "").expect("file");
        }
        std::fs::create_dir(root.join("sub")).expect("dir");
        assert_eq!(list_directory(&root, &root, 3).await.len(), 3);
        let all = list_directory(&root, &root, 100).await;
        assert_eq!(all.len(), 6);
        assert_eq!(
            all.last(),
            Some(&DirectoryEntry {
                name: "sub".to_owned(),
                kind: EntryKind::Directory
            })
        );
    }
}
