//! Managed object storage (ADR-021): the commands an object storage host runs
//! and what its heartbeat reports.
//!
//! A node takes these commands only when it advertises [`MANAGED_S3_CAPABILITY`].
//! Every bucket name, access key ID, and secret the control plane sends is
//! checked here before anything reaches the storage engine, so an object
//! storage host never touches a bucket or user it did not create. Names and keys
//! are later passed to `rc` as process arguments, so the patterns admit nothing
//! a shell or a URL would interpret.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::placement::Region;

/// Capability advertised by a node whose agent manages an S3-compatible
/// object storage instance. Tenants speak S3, not an engine-specific API.
pub const MANAGED_S3_CAPABILITY: &str = "managed-s3";

/// Every managed S3 bucket name starts with this prefix.
pub const BUCKET_NAME_PREFIX: &str = "b-";

/// Fewest characters after [`BUCKET_NAME_PREFIX`].
pub const BUCKET_NAME_MIN_SUFFIX_LENGTH: usize = 16;

/// Most characters after [`BUCKET_NAME_PREFIX`]. The whole name stays inside
/// S3's 63-character bucket name limit.
pub const BUCKET_NAME_MAX_SUFFIX_LENGTH: usize = 40;

/// The rule [`BucketName::parse`] implements.
pub const BUCKET_NAME_PATTERN: &str = "^b-[a-z0-9]{16,40}$";

/// Every access key ID starts with this prefix. The ID is also the name of the
/// storage engine's user that holds the key.
pub const ACCESS_KEY_ID_PREFIX: &str = "SK";

/// Fewest characters after [`ACCESS_KEY_ID_PREFIX`].
pub const ACCESS_KEY_ID_MIN_SUFFIX_LENGTH: usize = 16;

/// Most characters after [`ACCESS_KEY_ID_PREFIX`].
pub const ACCESS_KEY_ID_MAX_SUFFIX_LENGTH: usize = 38;

/// The rule [`AccessKeyId::parse`] implements.
pub const ACCESS_KEY_ID_PATTERN: &str = "^SK[A-Z0-9]{16,38}$";

/// Fewest characters of a secret access key. The control plane generates 40.
pub const SECRET_ACCESS_KEY_MIN_LENGTH: usize = 32;

/// Most characters of a secret access key.
pub const SECRET_ACCESS_KEY_MAX_LENGTH: usize = 128;

/// The rule [`SecretAccessKey::parse`] implements.
pub const SECRET_ACCESS_KEY_PATTERN: &str = "^[A-Za-z0-9]{32,128}$";

/// Smallest bucket quota, in MiB.
pub const BUCKET_QUOTA_MIN_MB: u32 = 1;

/// Largest bucket quota, in MiB (1 TiB).
pub const BUCKET_QUOTA_MAX_MB: u32 = 1_048_576;

/// Largest `size_bytes` a heartbeat reports for one bucket (1 PiB), the bound
/// the control plane validates.
pub const BUCKET_SIZE_MAX_BYTES: u64 = 1 << 50;

/// Failure codes of the bucket commands, sent as `error_code` on `fail`.
pub mod error_codes {
    /// `bucket` does not match [`super::BUCKET_NAME_PATTERN`].
    pub const BUCKET_NAME_INVALID: &str = "bucket_name_invalid";
    /// An access key ID or secret does not match its pattern.
    pub const BUCKET_KEY_INVALID: &str = "bucket_key_invalid";
    /// `quota_mb` is 0 or above [`super::BUCKET_QUOTA_MAX_MB`].
    pub const BUCKET_QUOTA_INVALID: &str = "bucket_quota_invalid";
    /// The node's total quota or its free-disk floor would be broken.
    pub const BUCKET_CAPACITY_EXCEEDED: &str = "bucket_capacity_exceeded";
    /// The engine's container cannot be reached, or its CLI cannot reach the
    /// engine.
    pub const BUCKET_ADMIN_UNAVAILABLE: &str = "bucket_admin_unavailable";
    /// The bucket or the key's user does not exist.
    pub const BUCKET_NOT_FOUND: &str = "bucket_not_found";
    /// The objects exist but belong together differently than the command
    /// says, for example a user that carries another policy. Nothing changed.
    pub const BUCKET_CONFLICT: &str = "bucket_conflict";
    /// The engine refused for another reason.
    pub const BUCKET_OPERATION_FAILED: &str = "bucket_operation_failed";
}

/// Code of a payload that is not an object or misses a field, shared with every
/// other command.
const INVALID_COMMAND_CODE: &str = "invalid_runtime_command";

/// A managed S3 bucket name, such as `b-8k2m4q7x9z1c3v5b6n0d`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String")]
pub struct BucketName(String);

impl BucketName {
    /// Accepts exactly [`BUCKET_NAME_PATTERN`].
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBucketName`] for any other value.
    pub fn parse(value: &str) -> Result<Self, InvalidBucketName> {
        let valid = value
            .strip_prefix(BUCKET_NAME_PREFIX)
            .is_some_and(|suffix| {
                (BUCKET_NAME_MIN_SUFFIX_LENGTH..=BUCKET_NAME_MAX_SUFFIX_LENGTH)
                    .contains(&suffix.len())
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidBucketName)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BucketName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<BucketName> for String {
    fn from(name: BucketName) -> Self {
        name.0
    }
}

/// A name outside [`BUCKET_NAME_PATTERN`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBucketName;

impl fmt::Display for InvalidBucketName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "must match {BUCKET_NAME_PATTERN}")
    }
}

impl std::error::Error for InvalidBucketName {}

/// An access key ID such as `SK7Q2M9X4K1C8V5B3N0D`. Not secret.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String")]
pub struct AccessKeyId(String);

impl AccessKeyId {
    /// Accepts exactly [`ACCESS_KEY_ID_PATTERN`].
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBucketKey`] for any other value.
    pub fn parse(value: &str) -> Result<Self, InvalidBucketKey> {
        let valid = value
            .strip_prefix(ACCESS_KEY_ID_PREFIX)
            .is_some_and(|suffix| {
                (ACCESS_KEY_ID_MIN_SUFFIX_LENGTH..=ACCESS_KEY_ID_MAX_SUFFIX_LENGTH)
                    .contains(&suffix.len())
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidBucketKey)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccessKeyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<AccessKeyId> for String {
    fn from(key: AccessKeyId) -> Self {
        key.0
    }
}

/// A tenant's secret access key. Never printed: `Debug` shows `[REDACTED]`,
/// and the type cannot be serialized.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretAccessKey(String);

impl SecretAccessKey {
    /// Accepts exactly [`SECRET_ACCESS_KEY_PATTERN`]: letters and digits only,
    /// so nothing needs escaping in an env file or a URL.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidBucketKey`] for any other value. The error never holds
    /// the value.
    pub fn parse(value: &str) -> Result<Self, InvalidBucketKey> {
        let valid = (SECRET_ACCESS_KEY_MIN_LENGTH..=SECRET_ACCESS_KEY_MAX_LENGTH)
            .contains(&value.len())
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric());
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidBucketKey)
        }
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretAccessKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// An access key ID or secret outside its pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBucketKey;

impl fmt::Display for InvalidBucketKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("is not a valid access key")
    }
}

impl std::error::Error for InvalidBucketKey {}

/// A bucket's hard quota in MiB, from [`BUCKET_QUOTA_MIN_MB`] to
/// [`BUCKET_QUOTA_MAX_MB`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "u32")]
pub struct BucketQuotaMb(u32);

impl BucketQuotaMb {
    /// # Errors
    ///
    /// Returns [`InvalidBucketQuota`] outside the accepted range.
    pub fn new(value: u64) -> Result<Self, InvalidBucketQuota> {
        u32::try_from(value)
            .ok()
            .filter(|value| (BUCKET_QUOTA_MIN_MB..=BUCKET_QUOTA_MAX_MB).contains(value))
            .map(Self)
            .ok_or(InvalidBucketQuota)
    }

    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl From<BucketQuotaMb> for u32 {
    fn from(quota: BucketQuotaMb) -> Self {
        quota.0
    }
}

/// A quota outside [`BUCKET_QUOTA_MIN_MB`] to [`BUCKET_QUOTA_MAX_MB`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBucketQuota;

impl fmt::Display for InvalidBucketQuota {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "must be {BUCKET_QUOTA_MIN_MB} to {BUCKET_QUOTA_MAX_MB} MiB"
        )
    }
}

impl std::error::Error for InvalidBucketQuota {}

/// Payload of `CreateBucket`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateBucketPayload {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
    pub secret_access_key: SecretAccessKey,
    pub quota_mb: BucketQuotaMb,
}

/// Payload of `DeleteBucket`, `LockBucket`, and `UnlockBucket`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BucketKeyPayload {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
}

/// Payload of `DeleteBucket`. The same fields as [`BucketKeyPayload`].
pub type DeleteBucketPayload = BucketKeyPayload;

/// Payload of `RotateBucketKey`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RotateBucketKeyPayload {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
    pub secret_access_key: SecretAccessKey,
}

/// Payload of `SetBucketQuota`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetBucketQuotaPayload {
    pub bucket: BucketName,
    pub quota_mb: BucketQuotaMb,
}

/// Why a bucket command payload was refused, each with its own failure code
/// so the control plane can tell a malformed record from a bad value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BucketPayloadError {
    /// The payload is not an object, or a field is missing or of the wrong type.
    Malformed(String),
    NameInvalid,
    /// The named field is not a valid key. Never holds the value.
    KeyInvalid(&'static str),
    QuotaInvalid,
}

impl BucketPayloadError {
    /// The `error_code` the command fails with.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => INVALID_COMMAND_CODE,
            Self::NameInvalid => error_codes::BUCKET_NAME_INVALID,
            Self::KeyInvalid(_) => error_codes::BUCKET_KEY_INVALID,
            Self::QuotaInvalid => error_codes::BUCKET_QUOTA_INVALID,
        }
    }
}

impl fmt::Display for BucketPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(formatter, "payload is invalid: {reason}"),
            Self::NameInvalid => write!(formatter, "bucket {InvalidBucketName}"),
            Self::KeyInvalid(field) => write!(formatter, "{field} {InvalidBucketKey}"),
            Self::QuotaInvalid => write!(formatter, "quota_mb {InvalidBucketQuota}"),
        }
    }
}

impl std::error::Error for BucketPayloadError {}

const ACCESS_KEY_ID_FIELD: &str = "access_key_id";
const SECRET_ACCESS_KEY_FIELD: &str = "secret_access_key";

/// The wire form of every bucket payload: plain values, checked after parsing
/// so each rule has its own failure code. Unknown fields are ignored, so the
/// control plane can add one before the agent reads it.
#[derive(Deserialize)]
struct RawBucketPayload {
    bucket: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    /// A negative or fractional number is a wrong type; 0 and too large
    /// values are an invalid quota.
    quota_mb: Option<u64>,
}

impl RawBucketPayload {
    fn read(payload: &serde_json::Value) -> Result<Self, BucketPayloadError> {
        Self::deserialize(payload).map_err(|error| BucketPayloadError::Malformed(error.to_string()))
    }

    fn bucket(&self) -> Result<BucketName, BucketPayloadError> {
        let value = self.bucket.as_deref().ok_or_else(|| missing("bucket"))?;
        BucketName::parse(value).map_err(|_| BucketPayloadError::NameInvalid)
    }

    fn access_key_id(&self) -> Result<AccessKeyId, BucketPayloadError> {
        let value = self
            .access_key_id
            .as_deref()
            .ok_or_else(|| missing(ACCESS_KEY_ID_FIELD))?;
        AccessKeyId::parse(value).map_err(|_| BucketPayloadError::KeyInvalid(ACCESS_KEY_ID_FIELD))
    }

    fn secret_access_key(&self) -> Result<SecretAccessKey, BucketPayloadError> {
        let value = self
            .secret_access_key
            .as_deref()
            .ok_or_else(|| missing(SECRET_ACCESS_KEY_FIELD))?;
        SecretAccessKey::parse(value)
            .map_err(|_| BucketPayloadError::KeyInvalid(SECRET_ACCESS_KEY_FIELD))
    }

    fn quota_mb(&self) -> Result<BucketQuotaMb, BucketPayloadError> {
        let value = self.quota_mb.ok_or_else(|| missing("quota_mb"))?;
        BucketQuotaMb::new(value).map_err(|_| BucketPayloadError::QuotaInvalid)
    }
}

fn missing(field: &str) -> BucketPayloadError {
    BucketPayloadError::Malformed(format!("missing field `{field}`"))
}

impl CreateBucketPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, BucketPayloadError> {
        let raw = RawBucketPayload::read(payload)?;
        Ok(Self {
            bucket: raw.bucket()?,
            access_key_id: raw.access_key_id()?,
            secret_access_key: raw.secret_access_key()?,
            quota_mb: raw.quota_mb()?,
        })
    }
}

impl BucketKeyPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, BucketPayloadError> {
        let raw = RawBucketPayload::read(payload)?;
        Ok(Self {
            bucket: raw.bucket()?,
            access_key_id: raw.access_key_id()?,
        })
    }
}

impl RotateBucketKeyPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, BucketPayloadError> {
        let raw = RawBucketPayload::read(payload)?;
        Ok(Self {
            bucket: raw.bucket()?,
            access_key_id: raw.access_key_id()?,
            secret_access_key: raw.secret_access_key()?,
        })
    }
}

impl SetBucketQuotaPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, BucketPayloadError> {
        let raw = RawBucketPayload::read(payload)?;
        Ok(Self {
            bucket: raw.bucket()?,
            quota_mb: raw.quota_mb()?,
        })
    }
}

/// Result of `CreateBucket`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CreateBucketResult {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
    pub quota_mb: BucketQuotaMb,
    /// `false` when the bucket already existed, as on a repeated command. Its
    /// quota, policy, and secret are set again either way.
    pub created: bool,
}

/// Result of `DeleteBucket`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeleteBucketResult {
    pub bucket: BucketName,
    /// `false` when neither the bucket, nor its user, nor its policy existed.
    pub deleted: bool,
}

/// Result of `RotateBucketKey`. Never holds the secret.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RotateBucketKeyResult {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
}

/// Result of `SetBucketQuota`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SetBucketQuotaResult {
    pub bucket: BucketName,
    pub quota_mb: BucketQuotaMb,
}

/// Result of `LockBucket` and `UnlockBucket`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BucketLockResult {
    pub bucket: BucketName,
    pub access_key_id: AccessKeyId,
    pub locked: bool,
}

/// `metadata.object_storage`: how tenants reach the instance and how much the
/// node may allocate. Sent in every heartbeat of an object storage host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObjectStorageMetadata {
    /// `SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT`, such as `https://s3.sakala.app`:
    /// an origin without path, query, or credentials.
    pub endpoint: String,
    /// The S3 signing region, `SAKALA_OBJECT_STORAGE_REGION`.
    pub region: Region,
    /// `SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB`: the most all bucket quotas of
    /// the node may add up to.
    pub capacity_mb: u64,
    /// Whether the last usage read succeeded. `false` makes the node
    /// `degraded`.
    pub available: bool,
}

/// One entry of `metadata.buckets`: a managed bucket and the bytes it holds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedBucketUsage {
    pub name: String,
    /// Completed objects, in bytes, as the engine counts them for the quota.
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const BUCKET: &str = "b-8k2m4q7x9z1c3v5b6n0d";
    const KEY: &str = "SK7Q2M9X4K1C8V5B3N0D";
    const SECRET: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2Fy";

    #[test]
    fn bucket_names_follow_the_documented_pattern() {
        for valid in [
            "b-0123456789abcdef",
            "b-abcdefghijklmnopqrstuvwxyz0123456789abcd",
            BUCKET,
        ] {
            assert!(BucketName::parse(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "b-",
            "b-0123456789abcde",
            "b-abcdefghijklmnopqrstuvwxyz0123456789abcde",
            "B-0123456789abcdef",
            "b-0123456789ABCDEF",
            "w-01a0d73b-uploads",
            "b_0123456789abcdef",
            "b-0123456789abcdef\n",
            "b-0123456789abcd-f",
            "b-0123456789abcd.f",
            "b-0123456789abcd/f",
            "b-0123456789abcd f",
            "b-0123456789abcdéf",
            "../b-0123456789abcdef",
        ] {
            assert!(BucketName::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn access_key_ids_follow_the_documented_pattern() {
        for valid in [
            "SK0123456789ABCDEF",
            "SKABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789AB",
            KEY,
        ] {
            assert!(AccessKeyId::parse(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "SK",
            "SK0123456789ABCDE",
            "SKABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABC",
            "sk0123456789ABCDEF",
            "SK0123456789abcdef",
            "AK0123456789ABCDEF",
            "SK0123456789ABCDE-",
            "SK0123456789ABCDEF ",
            "rustfsadmin",
        ] {
            assert!(AccessKeyId::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn secrets_are_long_alphanumeric_and_never_printed() {
        assert!(SecretAccessKey::parse(&"a".repeat(SECRET_ACCESS_KEY_MIN_LENGTH)).is_ok());
        assert!(SecretAccessKey::parse(&"Z9".repeat(SECRET_ACCESS_KEY_MAX_LENGTH / 2)).is_ok());
        for invalid in [
            "a".repeat(SECRET_ACCESS_KEY_MIN_LENGTH - 1),
            "a".repeat(SECRET_ACCESS_KEY_MAX_LENGTH + 1),
            format!("{SECRET}/"),
            format!("{SECRET}+"),
            format!("{SECRET}="),
            format!("{SECRET}\n"),
            format!("{SECRET} "),
            format!("{SECRET}é"),
            format!("{SECRET}'"),
        ] {
            assert!(SecretAccessKey::parse(&invalid).is_err(), "{invalid:?}");
        }

        let secret = SecretAccessKey::parse(SECRET).expect("secret");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        let payload = CreateBucketPayload::from_value(&json!({
            "bucket": BUCKET,
            "access_key_id": KEY,
            "secret_access_key": SECRET,
            "quota_mb": 256,
        }))
        .expect("payload");
        assert!(!format!("{payload:?}").contains(SECRET));

        let short = "shortbutstillsecretvalue";
        let error = RotateBucketKeyPayload::from_value(&json!({
            "bucket": BUCKET,
            "access_key_id": KEY,
            "secret_access_key": short,
        }))
        .expect_err("short secret");
        assert_eq!(
            error,
            BucketPayloadError::KeyInvalid(SECRET_ACCESS_KEY_FIELD)
        );
        assert!(!error.to_string().contains(short));
    }

    #[test]
    fn quotas_are_between_one_mib_and_one_tib() {
        assert_eq!(BucketQuotaMb::new(1).map(BucketQuotaMb::get), Ok(1));
        assert_eq!(
            BucketQuotaMb::new(u64::from(BUCKET_QUOTA_MAX_MB)).map(BucketQuotaMb::get),
            Ok(BUCKET_QUOTA_MAX_MB)
        );
        for invalid in [
            0,
            u64::from(BUCKET_QUOTA_MAX_MB) + 1,
            u64::from(u32::MAX) + 1,
        ] {
            assert_eq!(BucketQuotaMb::new(invalid), Err(InvalidBucketQuota));
        }
    }

    #[test]
    fn each_payload_rule_has_its_own_failure_code() {
        let code = |payload: serde_json::Value| {
            CreateBucketPayload::from_value(&payload)
                .expect_err("invalid")
                .code()
        };
        let valid = || {
            json!({
                "bucket": BUCKET,
                "access_key_id": KEY,
                "secret_access_key": SECRET,
                "quota_mb": 256,
            })
        };
        let with = |field: &str, value: serde_json::Value| {
            let mut payload = valid();
            payload[field] = value;
            payload
        };
        let without = |field: &str| {
            let mut payload = valid();
            payload.as_object_mut().expect("object").remove(field);
            payload
        };

        assert!(CreateBucketPayload::from_value(&valid()).is_ok());
        assert_eq!(code(json!(BUCKET)), INVALID_COMMAND_CODE);
        assert_eq!(code(serde_json::Value::Null), INVALID_COMMAND_CODE);
        for field in ["bucket", "access_key_id", "secret_access_key", "quota_mb"] {
            assert_eq!(code(without(field)), INVALID_COMMAND_CODE, "{field}");
        }
        assert_eq!(code(with("bucket", json!(7))), INVALID_COMMAND_CODE);
        assert_eq!(code(with("quota_mb", json!("256"))), INVALID_COMMAND_CODE);
        assert_eq!(code(with("quota_mb", json!(-1))), INVALID_COMMAND_CODE);
        assert_eq!(code(with("quota_mb", json!(1.5))), INVALID_COMMAND_CODE);
        assert_eq!(
            code(with("bucket", json!("w-01a0d73b-uploads"))),
            error_codes::BUCKET_NAME_INVALID
        );
        assert_eq!(
            code(with("access_key_id", json!("rustfsadmin"))),
            error_codes::BUCKET_KEY_INVALID
        );
        assert_eq!(
            code(with(
                "secret_access_key",
                json!("a$(id)bcdefghijklmnopqrstuvwxyz0123")
            )),
            error_codes::BUCKET_KEY_INVALID
        );
        assert_eq!(
            code(with("quota_mb", json!(0))),
            error_codes::BUCKET_QUOTA_INVALID
        );
        assert_eq!(
            code(with("quota_mb", json!(1_048_577))),
            error_codes::BUCKET_QUOTA_INVALID
        );
    }

    #[test]
    fn payloads_read_only_their_fields_and_ignore_unknown_ones() {
        let key = BucketKeyPayload::from_value(&json!({
            "bucket": BUCKET,
            "access_key_id": KEY,
            "locked": true,
        }))
        .expect("payload");
        assert_eq!(key.bucket.as_str(), BUCKET);
        assert_eq!(key.access_key_id.as_str(), KEY);

        let quota =
            SetBucketQuotaPayload::from_value(&json!({ "bucket": BUCKET, "quota_mb": 512 }))
                .expect("payload");
        assert_eq!(quota.quota_mb.get(), 512);
        assert!(
            SetBucketQuotaPayload::from_value(&json!({ "bucket": BUCKET })).is_err(),
            "quota_mb is required"
        );
        assert!(
            BucketKeyPayload::from_value(&json!({ "bucket": BUCKET })).is_err(),
            "access_key_id is required"
        );
    }

    #[test]
    fn results_serialize_to_the_documented_shapes() {
        let bucket = BucketName::parse(BUCKET).expect("bucket");
        let key = AccessKeyId::parse(KEY).expect("key");
        let quota = BucketQuotaMb::new(256).expect("quota");
        assert_eq!(
            serde_json::to_value(CreateBucketResult {
                bucket: bucket.clone(),
                access_key_id: key.clone(),
                quota_mb: quota,
                created: true,
            })
            .expect("serialize"),
            json!({ "bucket": BUCKET, "access_key_id": KEY, "quota_mb": 256, "created": true })
        );
        assert_eq!(
            serde_json::to_value(DeleteBucketResult {
                bucket: bucket.clone(),
                deleted: false,
            })
            .expect("serialize"),
            json!({ "bucket": BUCKET, "deleted": false })
        );
        assert_eq!(
            serde_json::to_value(RotateBucketKeyResult {
                bucket: bucket.clone(),
                access_key_id: key.clone(),
            })
            .expect("serialize"),
            json!({ "bucket": BUCKET, "access_key_id": KEY })
        );
        assert_eq!(
            serde_json::to_value(SetBucketQuotaResult {
                bucket: bucket.clone(),
                quota_mb: quota,
            })
            .expect("serialize"),
            json!({ "bucket": BUCKET, "quota_mb": 256 })
        );
        assert_eq!(
            serde_json::to_value(BucketLockResult {
                bucket,
                access_key_id: key,
                locked: true,
            })
            .expect("serialize"),
            json!({ "bucket": BUCKET, "access_key_id": KEY, "locked": true })
        );
        assert_eq!(
            serde_json::to_value(ObjectStorageMetadata {
                endpoint: "https://s3.sakala.app".to_owned(),
                region: Region::parse("us-east-1").expect("region"),
                capacity_mb: 5_120,
                available: true,
            })
            .expect("serialize"),
            json!({
                "endpoint": "https://s3.sakala.app",
                "region": "us-east-1",
                "capacity_mb": 5120,
                "available": true,
            })
        );
    }
}
