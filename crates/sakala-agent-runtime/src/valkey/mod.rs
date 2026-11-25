//! The Valkey host role (ADR-023): one Valkey container per managed store,
//! under the node's workload runtime, on an internal network of its own with a
//! fixed address, administered through `valkey-cli` inside the container.
//!
//! The agent writes each store's `valkey.conf` and `users.acl` on the node and
//! mounts them read-only; the data lives in a Docker volume per store, which
//! Docker gives to the image's own uid. A project's containers reach the store
//! by joining its network when they are created (see
//! [`crate::containers`]); nothing listens on the host.

mod admin;
mod files;
mod info;
mod network;

use std::path::Path;

pub use admin::{DockerValkeyAdmin, ValkeyAdminConfig};
pub(crate) use network::check_store_network;
pub use network::{VALKEY_SUBNET_PREFIX, ValkeyNetworkPool};
use sakala_agent_protocol::ValkeyMemoryMb;

/// Valkey 9.2.0 on Alpine, pinned by the digest of its multi-platform index.
/// `valkey/valkey:9-alpine` resolved to the same digest on 2026-11-25.
pub const DEFAULT_VALKEY_IMAGE: &str = "valkey/valkey:9.2.0-alpine@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882";

/// Where the stores' records and configuration live.
pub const DEFAULT_VALKEY_DATA_DIR: &str = "/var/lib/sakala/valkey";

/// What every store counts against `SAKALA_VALKEY_TOTAL_MEMORY_MB` on top of
/// its `maxmemory`: Σ(`maxmemory_mb` + 64), as ADR-023 defines the budget.
pub const VALKEY_BUDGET_OVERHEAD_MB: u64 = 64;

/// The container's memory limit is `2 × maxmemory + 64` MiB, in both modes.
///
/// Measured under gVisor on staging (spike S1, 2026-11-24): a persistent store
/// with `maxmemory 64mb` filled until it refused writes used 155 MiB, and an
/// AOF rewrite of 26 MB of data peaked at 103 MiB. 2 × 64 + 64 = 192 MiB
/// leaves 37 MiB above the full store and room for the rewrite's
/// copy-on-write. ADR-023's `maxmemory + 64` for a cache store would be
/// 128 MiB, below those 155 MiB, so a full cache store, which never forks but
/// fragments under constant eviction, gets the same formula. The limit is a
/// ceiling, not a reservation: the node budget stays Σ(`maxmemory` + 64).
pub const VALKEY_MEMORY_LIMIT_FACTOR: u64 = 2;

/// The fixed part of the memory limit: the gVisor sandbox (about 43 MiB in
/// ADR-020) and Valkey's own baseline.
pub const VALKEY_MEMORY_LIMIT_OVERHEAD_MB: u64 = 64;

/// CPU of one store, so a Lua loop of one tenant cannot take the node.
pub const VALKEY_CPU_MILLIS: u32 = 250;

/// Processes and threads of one store; Valkey 9 runs about ten.
pub const VALKEY_PIDS_LIMIT: u32 = 64;

/// Connections one store accepts. ADR-023's estimate is about four at once.
pub const VALKEY_MAXCLIENTS: u32 = 64;

/// The image's `valkey` user. A numeric non-root user keeps the image's
/// entrypoint from switching users, which fails under `--cap-drop ALL`.
pub const VALKEY_CONTAINER_USER: &str = "999:1000";

/// The agent's own user in every store, for commands and backups.
pub const ADMIN_USER: &str = "sakala-admin";

/// Where the store's container finds its configuration and keeps its data.
pub(crate) const VALKEY_CONTAINER_CONF_DIR: &str = "/etc/valkey";
pub(crate) const VALKEY_CONTAINER_DATA_DIR: &str = "/data";

/// Labels of a store's container, network, and volume.
pub(crate) const VALKEY_ID_LABEL: &str = "dev.sakala.valkey-id";

const MIB: u64 = 1_024 * 1_024;

/// The memory limit of a store's container, in MiB.
#[must_use]
pub fn memory_limit_mb(maxmemory_mb: ValkeyMemoryMb) -> u64 {
    VALKEY_MEMORY_LIMIT_FACTOR * u64::from(maxmemory_mb.get()) + VALKEY_MEMORY_LIMIT_OVERHEAD_MB
}

/// What a store counts against the node's budget, in MiB.
#[must_use]
pub fn budget_mb(maxmemory_mb: ValkeyMemoryMb) -> u64 {
    u64::from(maxmemory_mb.get()) + VALKEY_BUDGET_OVERHEAD_MB
}

const IMAGE_DIGEST_SEPARATOR: &str = "@sha256:";
const IMAGE_DIGEST_LENGTH: usize = 64;

/// Checks `SAKALA_VALKEY_IMAGE`: a reference pinned by digest,
/// `<name>[:tag]@sha256:<64 hex>`, which becomes a `docker run` argument, so
/// it can never be read as an option.
///
/// # Errors
///
/// Returns the rule the value breaks.
pub fn check_image_reference(value: &str) -> Result<(), &'static str> {
    let Some((name, digest)) = value.split_once(IMAGE_DIGEST_SEPARATOR) else {
        return Err("must be pinned by digest, such as valkey/valkey:9.2.0-alpine@sha256:<64 hex>");
    };
    let name_valid = name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        });
    let digest_valid = digest.len() == IMAGE_DIGEST_LENGTH
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if name_valid && digest_valid {
        Ok(())
    } else {
        Err("must be <name>[:tag]@sha256:<64 lowercase hex>")
    }
}

/// Checks `SAKALA_VALKEY_DATA_DIR`, which becomes part of a `--mount` value.
///
/// # Errors
///
/// Returns the rule the value breaks.
pub fn check_data_dir(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("must be an absolute path");
    }
    if path
        .to_str()
        .is_none_or(|path| path.contains([',', '=', '"', '\n']))
    {
        return Err("must be UTF-8 without commas, equals signs, quotes, or line breaks");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_limit_and_budget_follow_their_formulas() {
        let size = |mb| ValkeyMemoryMb::new(mb).expect("size");
        assert_eq!(memory_limit_mb(size(32)), 128);
        assert_eq!(memory_limit_mb(size(64)), 192);
        assert_eq!(memory_limit_mb(size(128)), 320);
        assert_eq!(budget_mb(size(64)), 128);
        // The production budget of 384 MiB holds three 64 MiB stores.
        assert_eq!(3 * budget_mb(size(64)), 384);
    }

    #[test]
    fn the_image_is_pinned_by_digest() {
        assert!(check_image_reference(DEFAULT_VALKEY_IMAGE).is_ok());
        assert!(
            check_image_reference(
                "registry.example.ac.id:5000/mirror/valkey@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882"
            )
            .is_ok()
        );
        for invalid in [
            "",
            "valkey/valkey:9-alpine",
            "valkey/valkey@sha256:short",
            "-valkey@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882",
            "Valkey@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882",
            "valkey@sha256:C8E2D17498838AF5D91E12EB7C11220B3434A7B7131A333E3AF52AD5E0B80882",
            "valkey @sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882",
        ] {
            assert!(check_image_reference(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn the_data_directory_fits_a_mount_option() {
        assert!(check_data_dir(Path::new(DEFAULT_VALKEY_DATA_DIR)).is_ok());
        for invalid in ["valkey", "/var/lib/a,b", "/var/lib/a=b", "/var/lib/\"a"] {
            assert!(check_data_dir(Path::new(invalid)).is_err(), "{invalid:?}");
        }
    }
}
