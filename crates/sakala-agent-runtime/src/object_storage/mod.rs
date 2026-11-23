//! The object storage host role (ADR-021): administers one local RustFS
//! instance for managed tenant buckets, through RustFS's own CLI `rc` inside
//! the instance's container.

mod rustfs;

use std::net::IpAddr;

pub use rustfs::{
    BUCKET_POLICY_ENV, BUCKET_SECRET_ENV, RC_ALIAS, RcObjectStorageAdmin, RustfsAdminConfig,
};
use url::Url;

/// The container `compose.object-storage.yaml` in sakala-deployment starts.
pub const DEFAULT_OBJECT_STORAGE_CONTAINER: &str = "sakala-object-storage-rustfs-1";

/// Where the deployment keeps the instance's data, read for free space.
pub const DEFAULT_OBJECT_STORAGE_DATA_DIR: &str = "/var/lib/sakala/object-storage";

/// The S3 signing region most SDKs default to, and RustFS's own default.
pub const DEFAULT_OBJECT_STORAGE_REGION: &str = "us-east-1";

/// Disk the instance leaves free even when every bucket is full, in MiB.
pub const DEFAULT_OBJECT_STORAGE_MIN_FREE_MB: u64 = 1_024;

/// Longest public endpoint, in bytes.
pub const PUBLIC_ENDPOINT_MAX_LENGTH: usize = 255;

/// Longest container name.
const CONTAINER_NAME_MAX_LENGTH: usize = 128;

const HTTPS_SCHEME: &str = "https";
const HTTP_SCHEME: &str = "http";
const LOOPBACK_HOST_NAME: &str = "localhost";

/// Checks `SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT` and returns it as the
/// origin the heartbeat reports, such as `https://s3.sakala.app`.
///
/// The value is an origin: `https://host[:port]` without path, query,
/// fragment, or credentials, at most [`PUBLIC_ENDPOINT_MAX_LENGTH`]
/// characters. `http://` is accepted only for a loopback host, for local
/// development, because tenants send their secret-signed requests here.
///
/// # Errors
///
/// Returns the rule the value breaks.
pub fn check_public_endpoint(value: &str) -> Result<String, &'static str> {
    if value.len() > PUBLIC_ENDPOINT_MAX_LENGTH {
        return Err("must be at most 255 characters");
    }
    let url = Url::parse(value).map_err(|_| "is not a URL")?;
    let loopback = match url.host_str() {
        None => return Err("must name a host"),
        Some(host) => {
            host == LOOPBACK_HOST_NAME
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        }
    };
    match url.scheme() {
        HTTPS_SCHEME => {}
        HTTP_SCHEME if loopback => {}
        HTTP_SCHEME => return Err("must use https; http is accepted only for a loopback host"),
        _ => return Err("must be an https URL"),
    }
    // `Url` turns an empty path into `/`, so a trailing slash is checked on the
    // raw value.
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || value.ends_with('/')
    {
        return Err("must be https://host[:port] without path, query, fragment, or credentials");
    }
    Ok(url.origin().ascii_serialization())
}

/// Checks `SAKALA_OBJECT_STORAGE_CONTAINER`, which becomes a `docker exec`
/// argument: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`, Docker's own rule for
/// container names, so it can never be read as an option.
///
/// # Errors
///
/// Returns the rule the value breaks.
pub fn check_container_name(value: &str) -> Result<(), &'static str> {
    let mut bytes = value.bytes();
    let valid = value.len() <= CONTAINER_NAME_MAX_LENGTH
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        Err("must match ^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_endpoints_are_origins() {
        for (value, origin) in [
            ("https://s3.sakala.app", "https://s3.sakala.app"),
            (
                "https://s3.staging.sakala.app",
                "https://s3.staging.sakala.app",
            ),
            (
                "https://S3.Example.AC.ID:8443",
                "https://s3.example.ac.id:8443",
            ),
            ("https://s3.sakala.app:443", "https://s3.sakala.app"),
            ("http://127.0.0.1:9000", "http://127.0.0.1:9000"),
            ("http://localhost:9000", "http://localhost:9000"),
            ("http://[::1]:9000", "http://[::1]:9000"),
        ] {
            assert_eq!(
                check_public_endpoint(value),
                Ok(origin.to_owned()),
                "{value}"
            );
        }
        for value in [
            "",
            "s3.sakala.app",
            "https://",
            "https://s3.sakala.app/",
            "https://s3.sakala.app/bucket",
            "https://s3.sakala.app?x=1",
            "https://s3.sakala.app#x",
            "https://user:secret@s3.sakala.app",
            "http://s3.sakala.app",
            "http://10.0.47.10:8190",
            "ftp://s3.sakala.app",
            &format!("https://{}.app", "a".repeat(250)),
        ] {
            assert!(check_public_endpoint(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn container_names_cannot_become_options() {
        for valid in ["sakala-object-storage-rustfs-1", "rustfs", "a", "x.y_z-1"] {
            assert!(check_container_name(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "-e",
            "--privileged",
            "_rustfs",
            "rust fs",
            "rustfs;id",
            "rustfs/1",
            &"a".repeat(129),
        ] {
            assert!(check_container_name(invalid).is_err(), "{invalid:?}");
        }
        assert!(check_container_name(&"a".repeat(128)).is_ok());
    }
}
