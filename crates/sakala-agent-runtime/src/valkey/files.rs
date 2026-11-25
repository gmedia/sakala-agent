//! A store's files on the node: the agent's record of it, its admin password,
//! and the `valkey.conf` and `users.acl` its container reads.
//!
//! ```text
//! <data_dir>/                   0700
//!   <valkey_id>/                0700
//!     store.json                0600  the record: mode, size, lock, subnet, password hash
//!     admin.secret              0600  the sakala-admin password, never mounted
//!     conf/                     0755  mounted read-only at /etc/valkey
//!       valkey.conf             0644
//!       users.acl               0644  SHA-256 hashes only
//! ```
//!
//! The record is the agent's source of truth for what a store should be; the
//! container, network, and volume are rebuilt from it. `conf/` must be readable
//! by the image's uid 999, which on a rootless Docker or under a non-root agent
//! is "other", so it holds only hashes. Every file is written to a temporary
//! name and renamed, so the container never reads half a file, and `conf/` is
//! mounted as a directory so a rename reaches it.

use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use ipnet::Ipv4Net;
use sakala_agent_core::ports::SecretString;
use sakala_agent_protocol::{ValkeyMemoryMb, ValkeyMode, ValkeyPassword};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt};
use uuid::Uuid;

use super::{ADMIN_USER, VALKEY_CONTAINER_CONF_DIR, VALKEY_CONTAINER_DATA_DIR, VALKEY_MAXCLIENTS};

const RECORD_FILE: &str = "store.json";
const ADMIN_SECRET_FILE: &str = "admin.secret";
const CONF_DIR: &str = "conf";
const CONF_FILE: &str = "valkey.conf";
const ACL_FILE: &str = "users.acl";
const TEMPORARY_SUFFIX: &str = ".tmp";

/// Only the agent reads the record, the admin password, and the store
/// directories.
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
/// The store's uid reads these through the read-only mount.
const SHARED_DIR_MODE: u32 = 0o755;
const SHARED_FILE_MODE: u32 = 0o644;

/// Version of the record format, for a later agent that changes it.
const RECORD_VERSION: u32 = 1;

/// Valkey's eviction policy of each mode.
const CACHE_POLICY: &str = "allkeys-lru";
const PERSISTENT_POLICY: &str = "noeviction";

/// What the agent knows about one store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct StoreRecord {
    pub version: u32,
    pub valkey_id: Uuid,
    pub mode: ValkeyMode,
    pub maxmemory_mb: ValkeyMemoryMb,
    /// `default` is `off`: the tenant cannot log in, connections were closed.
    pub locked: bool,
    /// The store network's /27. The store is always at its `.2`.
    pub subnet: Ipv4Net,
    /// SHA-256 of the `default` user's password, lower-case hex, as
    /// `users.acl` holds it.
    pub password_sha256: String,
}

impl StoreRecord {
    pub fn new(
        valkey_id: Uuid,
        mode: ValkeyMode,
        maxmemory_mb: ValkeyMemoryMb,
        subnet: Ipv4Net,
        password: &ValkeyPassword,
    ) -> Self {
        Self {
            version: RECORD_VERSION,
            valkey_id,
            mode,
            maxmemory_mb,
            locked: false,
            subnet,
            password_sha256: sha256_hex(password.expose()),
        }
    }
}

/// Lower-case hex SHA-256, the form of a hashed password in an ACL file.
pub(super) fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `maxmemory` in bytes, as Valkey's `CONFIG SET` and `INFO` count it.
pub(super) fn maxmemory_bytes(maxmemory_mb: ValkeyMemoryMb) -> u64 {
    u64::from(maxmemory_mb.get()) * super::MIB
}

/// The store's configuration. Only settings that differ from Valkey 9's
/// defaults, or that a tenant must never change, are written.
pub(super) fn render_conf(record: &StoreRecord) -> String {
    let (policy, persistence) = match record.mode {
        ValkeyMode::Cache => (CACHE_POLICY, "appendonly no\n"),
        ValkeyMode::Persistent => (PERSISTENT_POLICY, "appendonly yes\nappendfsync everysec\n"),
    };
    format!(
        "# Written by the Sakala agent for store {id}; changes are overwritten.\n\
         port {port}\n\
         dir {data}\n\
         aclfile {conf}/{acl}\n\
         maxmemory {maxmemory}\n\
         maxmemory-policy {policy}\n\
         maxclients {maxclients}\n\
         save \"\"\n\
         {persistence}",
        id = record.valkey_id,
        port = sakala_agent_protocol::VALKEY_PORT,
        data = VALKEY_CONTAINER_DATA_DIR,
        conf = VALKEY_CONTAINER_CONF_DIR,
        acl = ACL_FILE,
        maxmemory = maxmemory_bytes(record.maxmemory_mb),
        maxclients = VALKEY_MAXCLIENTS,
    )
}

/// The store's users: `default` for the tenant, with every command except
/// `@admin` (configuration, persistence, replication, monitoring, ACL
/// changes), and the agent's own admin user.
pub(super) fn render_acl(record: &StoreRecord, admin_sha256: &str) -> String {
    let state = if record.locked { "off" } else { "on" };
    format!(
        "user default {state} #{tenant} ~* &* +@all -@admin\n\
         user {ADMIN_USER} on #{admin_sha256} ~* &* +@all\n",
        tenant = record.password_sha256,
    )
}

/// The store directories under the role's data directory.
#[derive(Clone, Debug)]
pub(super) struct StoreFiles {
    root: PathBuf,
}

impl StoreFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, valkey_id: Uuid) -> PathBuf {
        self.root.join(valkey_id.hyphenated().to_string())
    }

    /// Bind-mounted at [`VALKEY_CONTAINER_CONF_DIR`].
    pub fn conf_dir(&self, valkey_id: Uuid) -> PathBuf {
        self.dir(valkey_id).join(CONF_DIR)
    }

    /// Creates the data directory, readable by the agent only.
    pub async fn ensure_root(&self) -> io::Result<()> {
        ensure_dir(&self.root, PRIVATE_DIR_MODE).await
    }

    pub async fn read_record(&self, valkey_id: Uuid) -> io::Result<Option<StoreRecord>> {
        let path = self.dir(valkey_id).join(RECORD_FILE);
        match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Every store with a record, in no particular order. A directory that is
    /// not named after a UUID, or has no record, is not a store.
    pub async fn records(&self) -> io::Result<Vec<StoreRecord>> {
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let Some(valkey_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            if let Some(record) = self.read_record(valkey_id).await? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Whether anything of the store is on disk.
    pub async fn exists(&self, valkey_id: Uuid) -> io::Result<bool> {
        fs::try_exists(self.dir(valkey_id)).await
    }

    /// The admin password, `None` when the store has none yet.
    pub async fn admin_password(&self, valkey_id: Uuid) -> io::Result<Option<SecretString>> {
        match fs::read_to_string(self.dir(valkey_id).join(ADMIN_SECRET_FILE)).await {
            Ok(password) => Ok(Some(SecretString::new(password.trim().to_owned()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Writes the record, `valkey.conf`, and `users.acl`, and an admin
    /// password when the store has none, so that a repeated create keeps the
    /// password the running store already has.
    pub async fn write(&self, record: &StoreRecord) -> io::Result<()> {
        self.ensure_root().await?;
        let dir = self.dir(record.valkey_id);
        ensure_dir(&dir, PRIVATE_DIR_MODE).await?;
        let conf = self.conf_dir(record.valkey_id);
        ensure_dir(&conf, SHARED_DIR_MODE).await?;

        let admin = match self.admin_password(record.valkey_id).await? {
            Some(admin) => admin,
            None => {
                let admin = SecretString::new(new_admin_password());
                write_atomic(
                    &dir.join(ADMIN_SECRET_FILE),
                    admin.expose().as_bytes(),
                    PRIVATE_FILE_MODE,
                )
                .await?;
                admin
            }
        };
        let record_json = serde_json::to_vec_pretty(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(&dir.join(RECORD_FILE), &record_json, PRIVATE_FILE_MODE).await?;
        write_atomic(
            &conf.join(CONF_FILE),
            render_conf(record).as_bytes(),
            SHARED_FILE_MODE,
        )
        .await?;
        write_atomic(
            &conf.join(ACL_FILE),
            render_acl(record, &sha256_hex(admin.expose())).as_bytes(),
            SHARED_FILE_MODE,
        )
        .await
    }

    /// Removes everything of the store. Nothing there is not an error.
    pub async fn remove(&self, valkey_id: Uuid) -> io::Result<()> {
        match fs::remove_dir_all(self.dir(valkey_id)).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// 64 hex characters from two random UUIDs, 244 random bits: the agent's own
/// login, never sent anywhere.
fn new_admin_password() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Creates `path` when missing, and gives it `mode` either way. The mode is set
/// after creating, because the process umask (`UMask=0077` in a hardened
/// unit) would otherwise take the store's read access to `conf/` away.
async fn ensure_dir(path: &Path, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path).await {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
}

async fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(TEMPORARY_SUFFIX);
    let temporary = PathBuf::from(temporary);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(mode)
        .open(&temporary)
        .await?;
    file.write_all(contents).await?;
    file.sync_all().await?;
    drop(file);
    fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode)).await?;
    fs::rename(&temporary, path).await
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3Jh";

    fn record(mode: ValkeyMode) -> StoreRecord {
        StoreRecord::new(
            Uuid::from_u128(7),
            mode,
            ValkeyMemoryMb::new(64).expect("memory"),
            "10.213.0.32/27".parse().expect("subnet"),
            &ValkeyPassword::parse(PASSWORD).expect("password"),
        )
    }

    #[test]
    fn the_password_is_stored_as_the_hash_an_acl_file_takes() {
        // printf %s "$PASSWORD" | sha256sum
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let record = record(ValkeyMode::Cache);
        assert_eq!(record.password_sha256, sha256_hex(PASSWORD));
        assert!(
            !serde_json::to_string(&record)
                .expect("json")
                .contains(PASSWORD)
        );
    }

    #[test]
    fn each_mode_gets_its_policy_and_persistence() {
        let cache = render_conf(&record(ValkeyMode::Cache));
        assert!(cache.contains("maxmemory 67108864\n"));
        assert!(cache.contains("maxmemory-policy allkeys-lru\n"));
        assert!(cache.contains("appendonly no\n"));
        assert!(cache.contains("save \"\"\n"));
        assert!(cache.contains("aclfile /etc/valkey/users.acl\n"));
        assert!(cache.contains("dir /data\n"));
        assert!(cache.contains("maxclients 64\n"));
        assert!(!cache.contains("appendfsync"));

        let persistent = render_conf(&record(ValkeyMode::Persistent));
        assert!(persistent.contains("maxmemory-policy noeviction\n"));
        assert!(persistent.contains("appendonly yes\nappendfsync everysec\n"));
        assert!(persistent.contains("save \"\"\n"));
    }

    #[test]
    fn the_tenant_gets_everything_but_admin_and_a_lock_turns_it_off() {
        let mut record = record(ValkeyMode::Persistent);
        let admin = sha256_hex("admin");
        assert_eq!(
            render_acl(&record, &admin),
            format!(
                "user default on #{} ~* &* +@all -@admin\nuser sakala-admin on #{admin} ~* &* +@all\n",
                sha256_hex(PASSWORD)
            )
        );
        record.locked = true;
        assert!(
            render_acl(&record, &admin)
                .starts_with(&format!("user default off #{} ", sha256_hex(PASSWORD)))
        );
    }

    #[tokio::test]
    async fn files_are_written_with_their_modes_and_the_admin_password_kept() {
        let temp = tempfile::TempDir::new().expect("temp");
        let files = StoreFiles::new(temp.path().join("valkey"));
        let mut record = record(ValkeyMode::Persistent);

        files.write(&record).await.expect("write");
        let mode = |path: PathBuf| std::fs::metadata(path).expect("metadata").mode() & 0o777;
        // Whatever the umask, the store's uid can read its configuration
        // after the first write.
        assert_eq!(mode(files.conf_dir(record.valkey_id)), 0o755);
        assert_eq!(
            mode(files.conf_dir(record.valkey_id).join(CONF_FILE)),
            0o644
        );
        let admin = files
            .admin_password(record.valkey_id)
            .await
            .expect("read")
            .expect("admin");
        assert_eq!(admin.expose().len(), 64);
        record.locked = true;
        files.write(&record).await.expect("write again");
        assert_eq!(
            files
                .admin_password(record.valkey_id)
                .await
                .expect("read")
                .expect("admin")
                .expose(),
            admin.expose(),
            "a second write keeps the admin password"
        );

        let dir = files.dir(record.valkey_id);
        assert_eq!(mode(files.root().to_owned()), 0o700);
        assert_eq!(mode(dir.clone()), 0o700);
        assert_eq!(mode(dir.join(RECORD_FILE)), 0o600);
        assert_eq!(mode(dir.join(ADMIN_SECRET_FILE)), 0o600);
        assert_eq!(mode(files.conf_dir(record.valkey_id)), 0o755);
        assert_eq!(
            mode(files.conf_dir(record.valkey_id).join(CONF_FILE)),
            0o644
        );
        assert_eq!(mode(files.conf_dir(record.valkey_id).join(ACL_FILE)), 0o644);
        let acl =
            std::fs::read_to_string(files.conf_dir(record.valkey_id).join(ACL_FILE)).expect("acl");
        assert!(acl.starts_with("user default off"));
        assert!(
            !acl.contains(admin.expose()),
            "the ACL file holds hashes only"
        );

        assert_eq!(
            files.read_record(record.valkey_id).await.expect("read"),
            Some(record.clone())
        );
        std::fs::create_dir_all(files.root().join("not-a-store")).expect("dir");
        assert_eq!(
            files.records().await.expect("records"),
            vec![record.clone()]
        );

        files.remove(record.valkey_id).await.expect("remove");
        files.remove(record.valkey_id).await.expect("remove again");
        assert!(!files.exists(record.valkey_id).await.expect("exists"));
        assert_eq!(
            files.read_record(record.valkey_id).await.expect("read"),
            None
        );
    }

    #[tokio::test]
    async fn a_missing_data_directory_holds_no_stores() {
        let temp = tempfile::TempDir::new().expect("temp");
        let files = StoreFiles::new(temp.path().join("absent"));
        assert!(files.records().await.expect("records").is_empty());
    }
}
