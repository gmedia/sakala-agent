//! Managed Valkey (ADR-023): the commands a Valkey host runs, the field that
//! puts a project's containers on a store's network, and what the heartbeat
//! reports.
//!
//! A node takes these commands only when it advertises
//! [`MANAGED_VALKEY_CAPABILITY`]. Every store is one container on its own
//! internal network, named after the store's ID, so a command can only ever
//! reach Docker objects the agent derives from a UUID. Passwords are checked
//! here and never printed or serialized.

use std::{fmt, net::Ipv4Addr, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// Capability advertised by a node whose agent runs managed Valkey stores.
/// Requires the Docker runtime driver, because stores share bridge networks
/// with workloads.
pub const MANAGED_VALKEY_CAPABILITY: &str = "managed-valkey";

/// The port every store listens on, inside its own network.
pub const VALKEY_PORT: u16 = 6379;

/// Every store network, container, and volume is this prefix plus the store
/// ID, such as `sakala-valkey-4f1f21ef-730d-42d5-a46d-d965353cb993`.
pub const VALKEY_NAME_PREFIX: &str = "sakala-valkey-";

/// Smallest `maxmemory` of a store, in MiB.
pub const VALKEY_MEMORY_MIN_MB: u32 = 16;

/// Largest `maxmemory` of a store, in MiB. ADR-023 keeps stores above
/// 512 MB out of v1; the control plane offers 32, 64, and 128.
pub const VALKEY_MEMORY_MAX_MB: u32 = 512;

/// Fewest characters of a store password. The control plane generates 32.
pub const VALKEY_PASSWORD_MIN_LENGTH: usize = 32;

/// Most characters of a store password.
pub const VALKEY_PASSWORD_MAX_LENGTH: usize = 128;

/// The rule [`ValkeyPassword::parse`] implements.
pub const VALKEY_PASSWORD_PATTERN: &str = "^[A-Za-z0-9]{32,128}$";

/// Failure codes of the Valkey commands, and of a deployment or job whose
/// `valkey` field names a network it cannot join, sent as `error_code` on
/// `fail`.
pub mod error_codes {
    /// `mode` is not `cache` or `persistent`.
    pub const VALKEY_MODE_INVALID: &str = "valkey_mode_invalid";
    /// `maxmemory_mb` is outside [`super::VALKEY_MEMORY_MIN_MB`] to
    /// [`super::VALKEY_MEMORY_MAX_MB`].
    pub const VALKEY_MEMORY_INVALID: &str = "valkey_memory_invalid";
    /// `password` does not match [`super::VALKEY_PASSWORD_PATTERN`].
    pub const VALKEY_PASSWORD_INVALID: &str = "valkey_password_invalid";
    /// The node's `SAKALA_VALKEY_TOTAL_MEMORY_MB` budget or its
    /// `SAKALA_VALKEY_NETWORK_POOL` has no room for the store or the new size.
    pub const VALKEY_CAPACITY_EXCEEDED: &str = "valkey_capacity_exceeded";
    /// No store with this ID exists on the node.
    pub const VALKEY_NOT_FOUND: &str = "valkey_not_found";
    /// A store with this ID exists with another mode or size, or its Docker
    /// objects do not match its record. Nothing was changed.
    pub const VALKEY_CONFLICT: &str = "valkey_conflict";
    /// A persistent store holds more data than the requested `maxmemory`.
    /// Nothing was changed.
    pub const VALKEY_SIZE_BELOW_USAGE: &str = "valkey_size_below_usage";
    /// A running container other than the store is still on the store's
    /// network. Nothing was removed.
    pub const VALKEY_IN_USE: &str = "valkey_in_use";
    /// Docker or the store does not answer, or the store is not running. The
    /// command may have been partly applied; sending it again is safe.
    pub const VALKEY_UNAVAILABLE: &str = "valkey_unavailable";
    /// Docker or Valkey refused for another reason.
    pub const VALKEY_OPERATION_FAILED: &str = "valkey_operation_failed";
    /// A `DeployProject` or `RunJob` names a store network that is missing
    /// on this node, is not a store network of this agent, or whose store
    /// address is not `host`. Nothing of the deployment or job started.
    pub const VALKEY_NETWORK_UNAVAILABLE: &str = "valkey_network_unavailable";
}

/// Code of a payload that is not an object or misses a field, shared with every
/// other command.
const INVALID_COMMAND_CODE: &str = "invalid_runtime_command";

const VALKEY_ID_FIELD: &str = "valkey_id";
const MODE_FIELD: &str = "mode";
const MAXMEMORY_FIELD: &str = "maxmemory_mb";
const PASSWORD_FIELD: &str = "password";

/// The network, container, and volume name of a store.
#[must_use]
pub fn valkey_resource_name(valkey_id: Uuid) -> String {
    format!("{VALKEY_NAME_PREFIX}{}", valkey_id.hyphenated())
}

/// How a store keeps its data, chosen at creation and fixed in v1.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValkeyMode {
    /// No persistence, `allkeys-lru`: a full store evicts the least recently
    /// used keys, and a restart empties it.
    Cache,
    /// AOF with `appendfsync everysec`, `noeviction`: a full store refuses
    /// writes, and a restart keeps the data.
    Persistent,
}

impl ValkeyMode {
    /// The wire name, the same one serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Persistent => "persistent",
        }
    }
}

impl fmt::Display for ValkeyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ValkeyMode {
    type Err = InvalidValkeyMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cache" => Ok(Self::Cache),
            "persistent" => Ok(Self::Persistent),
            _ => Err(InvalidValkeyMode),
        }
    }
}

/// A mode other than `cache` or `persistent`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidValkeyMode;

impl fmt::Display for InvalidValkeyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("must be cache or persistent")
    }
}

impl std::error::Error for InvalidValkeyMode {}

/// A store's `maxmemory` in MiB, from [`VALKEY_MEMORY_MIN_MB`] to
/// [`VALKEY_MEMORY_MAX_MB`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "u32")]
pub struct ValkeyMemoryMb(u32);

impl ValkeyMemoryMb {
    /// # Errors
    ///
    /// Returns [`InvalidValkeyMemory`] outside the accepted range.
    pub fn new(value: u64) -> Result<Self, InvalidValkeyMemory> {
        u32::try_from(value)
            .ok()
            .filter(|value| (VALKEY_MEMORY_MIN_MB..=VALKEY_MEMORY_MAX_MB).contains(value))
            .map(Self)
            .ok_or(InvalidValkeyMemory)
    }

    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl From<ValkeyMemoryMb> for u32 {
    fn from(memory: ValkeyMemoryMb) -> Self {
        memory.0
    }
}

impl<'de> Deserialize<'de> for ValkeyMemoryMb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A size outside [`VALKEY_MEMORY_MIN_MB`] to [`VALKEY_MEMORY_MAX_MB`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidValkeyMemory;

impl fmt::Display for InvalidValkeyMemory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "must be {VALKEY_MEMORY_MIN_MB} to {VALKEY_MEMORY_MAX_MB} MiB"
        )
    }
}

impl std::error::Error for InvalidValkeyMemory {}

/// The password of a store's `default` user. Never printed: `Debug` shows
/// `[REDACTED]`, and the type cannot be serialized.
#[derive(Clone, Eq, PartialEq)]
pub struct ValkeyPassword(String);

impl ValkeyPassword {
    /// Accepts exactly [`VALKEY_PASSWORD_PATTERN`]: letters and digits only,
    /// so it needs no escaping in `REDIS_URL` or an env file.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValkeyPassword`] for any other value. The error never
    /// holds the value.
    pub fn parse(value: &str) -> Result<Self, InvalidValkeyPassword> {
        let valid = (VALKEY_PASSWORD_MIN_LENGTH..=VALKEY_PASSWORD_MAX_LENGTH)
            .contains(&value.len())
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric());
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidValkeyPassword)
        }
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ValkeyPassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// A password outside [`VALKEY_PASSWORD_PATTERN`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidValkeyPassword;

impl fmt::Display for InvalidValkeyPassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "must match {VALKEY_PASSWORD_PATTERN}")
    }
}

impl std::error::Error for InvalidValkeyPassword {}

/// Payload of `CreateValkey`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateValkeyPayload {
    pub valkey_id: Uuid,
    pub mode: ValkeyMode,
    pub maxmemory_mb: ValkeyMemoryMb,
    pub password: ValkeyPassword,
}

/// Payload of `DeleteValkey`, `LockValkey`, and `UnlockValkey`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValkeyTargetPayload {
    pub valkey_id: Uuid,
}

/// Payload of `RotateValkeyPassword`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RotateValkeyPasswordPayload {
    pub valkey_id: Uuid,
    pub password: ValkeyPassword,
}

/// Payload of `ResizeValkey`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResizeValkeyPayload {
    pub valkey_id: Uuid,
    pub maxmemory_mb: ValkeyMemoryMb,
}

/// Why a Valkey command payload was refused, each with its own failure code
/// so the control plane can tell a malformed record from a bad value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValkeyPayloadError {
    /// The payload is not an object, or a field is missing or of the wrong
    /// type, `valkey_id` not being a UUID included.
    Malformed(String),
    ModeInvalid,
    MemoryInvalid,
    /// Never holds the value.
    PasswordInvalid,
}

impl ValkeyPayloadError {
    /// The `error_code` the command fails with.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => INVALID_COMMAND_CODE,
            Self::ModeInvalid => error_codes::VALKEY_MODE_INVALID,
            Self::MemoryInvalid => error_codes::VALKEY_MEMORY_INVALID,
            Self::PasswordInvalid => error_codes::VALKEY_PASSWORD_INVALID,
        }
    }
}

impl fmt::Display for ValkeyPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(formatter, "payload is invalid: {reason}"),
            Self::ModeInvalid => write!(formatter, "{MODE_FIELD} {InvalidValkeyMode}"),
            Self::MemoryInvalid => write!(formatter, "{MAXMEMORY_FIELD} {InvalidValkeyMemory}"),
            Self::PasswordInvalid => write!(formatter, "{PASSWORD_FIELD} {InvalidValkeyPassword}"),
        }
    }
}

impl std::error::Error for ValkeyPayloadError {}

/// The wire form of every Valkey payload: plain values, checked after parsing
/// so each rule has its own failure code. Unknown fields are ignored, so the
/// control plane can add one before the agent reads it.
#[derive(Deserialize)]
struct RawValkeyPayload {
    valkey_id: Option<Uuid>,
    mode: Option<String>,
    /// A negative or fractional number is a wrong type; 0 and too large
    /// values are an invalid size.
    maxmemory_mb: Option<u64>,
    password: Option<String>,
}

impl RawValkeyPayload {
    fn read(payload: &serde_json::Value) -> Result<Self, ValkeyPayloadError> {
        Self::deserialize(payload).map_err(|error| ValkeyPayloadError::Malformed(error.to_string()))
    }

    fn valkey_id(&self) -> Result<Uuid, ValkeyPayloadError> {
        self.valkey_id.ok_or_else(|| missing(VALKEY_ID_FIELD))
    }

    fn mode(&self) -> Result<ValkeyMode, ValkeyPayloadError> {
        self.mode
            .as_deref()
            .ok_or_else(|| missing(MODE_FIELD))?
            .parse()
            .map_err(|_| ValkeyPayloadError::ModeInvalid)
    }

    fn maxmemory_mb(&self) -> Result<ValkeyMemoryMb, ValkeyPayloadError> {
        let value = self.maxmemory_mb.ok_or_else(|| missing(MAXMEMORY_FIELD))?;
        ValkeyMemoryMb::new(value).map_err(|_| ValkeyPayloadError::MemoryInvalid)
    }

    fn password(&self) -> Result<ValkeyPassword, ValkeyPayloadError> {
        let value = self
            .password
            .as_deref()
            .ok_or_else(|| missing(PASSWORD_FIELD))?;
        ValkeyPassword::parse(value).map_err(|_| ValkeyPayloadError::PasswordInvalid)
    }
}

fn missing(field: &str) -> ValkeyPayloadError {
    ValkeyPayloadError::Malformed(format!("missing field `{field}`"))
}

impl CreateValkeyPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, ValkeyPayloadError> {
        let raw = RawValkeyPayload::read(payload)?;
        Ok(Self {
            valkey_id: raw.valkey_id()?,
            mode: raw.mode()?,
            maxmemory_mb: raw.maxmemory_mb()?,
            password: raw.password()?,
        })
    }
}

impl ValkeyTargetPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, ValkeyPayloadError> {
        let raw = RawValkeyPayload::read(payload)?;
        Ok(Self {
            valkey_id: raw.valkey_id()?,
        })
    }
}

impl RotateValkeyPasswordPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, ValkeyPayloadError> {
        let raw = RawValkeyPayload::read(payload)?;
        Ok(Self {
            valkey_id: raw.valkey_id()?,
            password: raw.password()?,
        })
    }
}

impl ResizeValkeyPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, ValkeyPayloadError> {
        let raw = RawValkeyPayload::read(payload)?;
        Ok(Self {
            valkey_id: raw.valkey_id()?,
            maxmemory_mb: raw.maxmemory_mb()?,
        })
    }
}

/// Where a project's containers reach a store: an IPv4 address on the store's
/// own network.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValkeyEndpoint {
    pub host: Ipv4Addr,
    pub port: u16,
}

/// Result of `CreateValkey`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreateValkeyResult {
    pub valkey_id: Uuid,
    pub mode: ValkeyMode,
    pub maxmemory_mb: ValkeyMemoryMb,
    /// The container's memory limit, from the node's formula.
    pub memory_limit_mb: u64,
    /// `sakala-valkey-<valkey_id>`, the `network` of the `valkey` field of
    /// `DeployProject` and `RunJob`.
    pub network: String,
    /// `endpoint.host` is the `host` of that field and `REDIS_HOST`.
    pub endpoint: ValkeyEndpoint,
    /// `false` when the store already existed, as on a repeated command. Its
    /// password is set again either way; a lock stays.
    pub created: bool,
    pub locked: bool,
}

/// Result of `DeleteValkey`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeleteValkeyResult {
    pub valkey_id: Uuid,
    /// `false` when nothing of the store existed on the node.
    pub deleted: bool,
}

/// Result of `RotateValkeyPassword`. Never holds the password.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RotateValkeyPasswordResult {
    pub valkey_id: Uuid,
    /// Connections of the `default` user that were closed, because a
    /// connection stays logged in after its password changes.
    pub disconnected_clients: u64,
}

/// Result of `LockValkey` and `UnlockValkey`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValkeyLockResult {
    pub valkey_id: Uuid,
    pub locked: bool,
    /// Connections of the `default` user that were closed; always 0 for
    /// `UnlockValkey`.
    pub disconnected_clients: u64,
}

/// Result of `ResizeValkey`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResizeValkeyResult {
    pub valkey_id: Uuid,
    pub maxmemory_mb: ValkeyMemoryMb,
    pub memory_limit_mb: u64,
    /// `true` when the container had to be created again with the new memory
    /// limit, because the runtime could not change it in place. A cache store
    /// is then empty; a persistent store reloaded its AOF.
    pub restarted: bool,
}

/// The `valkey` field of `DeployProject` and `RunJob`: the store network every
/// container of the deployment or job joins when it is created, so the
/// application reaches its store at `host`. Absent or `null` means no store.
///
/// Checked when parsed: `network` must be `sakala-valkey-<uuid>`, so a payload
/// can never put a workload on `host`, `bridge`, or another project's network,
/// and `host` must be an IPv4 address.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawValkeyAttachment")]
pub struct ValkeyAttachment {
    pub network: String,
    pub host: Ipv4Addr,
}

impl ValkeyAttachment {
    /// The store the network belongs to; `None` only for a value built by
    /// hand around the parsing rule.
    #[must_use]
    pub fn valkey_id(&self) -> Option<Uuid> {
        self.network
            .strip_prefix(VALKEY_NAME_PREFIX)
            .and_then(|id| Uuid::parse_str(id).ok())
    }
}

#[derive(Deserialize)]
struct RawValkeyAttachment {
    network: String,
    host: Ipv4Addr,
}

impl TryFrom<RawValkeyAttachment> for ValkeyAttachment {
    type Error = String;

    fn try_from(raw: RawValkeyAttachment) -> Result<Self, Self::Error> {
        let valid = raw
            .network
            .strip_prefix(VALKEY_NAME_PREFIX)
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_some_and(|id| valkey_resource_name(id) == raw.network);
        if !valid {
            return Err(format!(
                "valkey.network must be {VALKEY_NAME_PREFIX}<lowercase UUID>"
            ));
        }
        Ok(Self {
            network: raw.network,
            host: raw.host,
        })
    }
}

/// `metadata.valkey`: the node's budget for stores. Sent in every heartbeat of
/// a Valkey host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValkeyHostMetadata {
    /// `SAKALA_VALKEY_TOTAL_MEMORY_MB`: the most Σ(`maxmemory_mb` + 64) of all
    /// stores of the node may reach.
    pub capacity_mb: u64,
    /// Whether the last read of the stores succeeded. `false` makes the node
    /// `degraded`.
    pub available: bool,
}

/// One entry of `metadata.valkeys`: a store, its container, and what Valkey's
/// `INFO` says about it. The `INFO` fields are `null` when the store is not
/// running or did not answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedValkeyUsage {
    pub valkey_id: Uuid,
    pub mode: ValkeyMode,
    pub maxmemory_mb: ValkeyMemoryMb,
    pub locked: bool,
    /// Docker's `State.Status` (`running`, `restarting`, `exited`, ...), or
    /// `missing` when the store's container does not exist.
    pub status: String,
    /// Docker's restart count of the container; 0 when it is missing.
    pub restart_count: u64,
    /// `used_memory`, in bytes.
    pub used_memory: Option<u64>,
    /// `maxmemory`, in bytes.
    pub maxmemory: Option<u64>,
    /// `evicted_keys` since the store started: keys a full cache store dropped.
    pub evicted_keys: Option<u64>,
    /// `errorstat_OOM` count since the store started: writes a full
    /// persistent store refused.
    pub errorstat_oom: Option<u64>,
    pub connected_clients: Option<u64>,
    /// `aof_last_write_status`, `ok` or `err`; always `null` for a cache
    /// store, which has no AOF.
    pub aof_last_write_status: Option<String>,
}

/// The status of a store whose container does not exist.
pub const VALKEY_STATUS_MISSING: &str = "missing";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";
    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3Jh";

    fn valid() -> serde_json::Value {
        json!({
            "valkey_id": ID,
            "mode": "persistent",
            "maxmemory_mb": 64,
            "password": PASSWORD,
        })
    }

    #[test]
    fn a_create_payload_reads_every_field() {
        let payload = CreateValkeyPayload::from_value(&valid()).expect("payload");
        assert_eq!(payload.valkey_id.to_string(), ID);
        assert_eq!(payload.mode, ValkeyMode::Persistent);
        assert_eq!(payload.maxmemory_mb.get(), 64);
        assert_eq!(payload.password.expose(), PASSWORD);
        assert!(!format!("{payload:?}").contains(PASSWORD));
    }

    #[test]
    fn each_payload_rule_has_its_own_failure_code() {
        let code = |payload: serde_json::Value| {
            CreateValkeyPayload::from_value(&payload)
                .expect_err("invalid")
                .code()
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

        assert_eq!(code(json!(ID)), INVALID_COMMAND_CODE);
        assert_eq!(code(serde_json::Value::Null), INVALID_COMMAND_CODE);
        for field in ["valkey_id", "mode", "maxmemory_mb", "password"] {
            assert_eq!(code(without(field)), INVALID_COMMAND_CODE, "{field}");
        }
        assert_eq!(
            code(with("valkey_id", json!("not-a-uuid"))),
            INVALID_COMMAND_CODE
        );
        assert_eq!(
            code(with("maxmemory_mb", json!("64"))),
            INVALID_COMMAND_CODE
        );
        assert_eq!(code(with("maxmemory_mb", json!(-1))), INVALID_COMMAND_CODE);
        assert_eq!(
            code(with("mode", json!("volatile"))),
            error_codes::VALKEY_MODE_INVALID
        );
        assert_eq!(
            code(with("mode", json!("Cache"))),
            error_codes::VALKEY_MODE_INVALID
        );
        for size in [0, 15, 513, u64::from(u32::MAX) + 1] {
            assert_eq!(
                code(with("maxmemory_mb", json!(size))),
                error_codes::VALKEY_MEMORY_INVALID,
                "{size}"
            );
        }
        for password in [
            "short",
            &"a".repeat(VALKEY_PASSWORD_MAX_LENGTH + 1),
            &format!("{PASSWORD}/"),
            &format!("{PASSWORD}@"),
            &format!("{PASSWORD}\n"),
            &format!("{PASSWORD} "),
        ] {
            let error = CreateValkeyPayload::from_value(&with("password", json!(password)))
                .expect_err("invalid");
            assert_eq!(error.code(), error_codes::VALKEY_PASSWORD_INVALID);
            assert!(!error.to_string().contains(password));
        }
    }

    #[test]
    fn sizes_are_between_sixteen_and_five_hundred_twelve_mib() {
        assert_eq!(ValkeyMemoryMb::new(16).map(ValkeyMemoryMb::get), Ok(16));
        assert_eq!(ValkeyMemoryMb::new(512).map(ValkeyMemoryMb::get), Ok(512));
        assert_eq!(ValkeyMemoryMb::new(15), Err(InvalidValkeyMemory));
        assert_eq!(ValkeyMemoryMb::new(513), Err(InvalidValkeyMemory));
    }

    #[test]
    fn the_other_payloads_read_only_their_fields() {
        let target = ValkeyTargetPayload::from_value(&json!({ "valkey_id": ID, "locked": true }))
            .expect("target");
        assert_eq!(target.valkey_id.to_string(), ID);
        let rotate = RotateValkeyPasswordPayload::from_value(&json!({
            "valkey_id": ID,
            "password": PASSWORD,
        }))
        .expect("rotate");
        assert_eq!(rotate.password.expose(), PASSWORD);
        let resize =
            ResizeValkeyPayload::from_value(&json!({ "valkey_id": ID, "maxmemory_mb": 128 }))
                .expect("resize");
        assert_eq!(resize.maxmemory_mb.get(), 128);
        assert!(ResizeValkeyPayload::from_value(&json!({ "valkey_id": ID })).is_err());
        assert!(RotateValkeyPasswordPayload::from_value(&json!({ "valkey_id": ID })).is_err());
    }

    #[test]
    fn a_password_never_prints() {
        let password = ValkeyPassword::parse(PASSWORD).expect("password");
        assert_eq!(format!("{password:?}"), "[REDACTED]");
    }

    #[test]
    fn an_attachment_names_only_a_store_network() {
        let attachment: ValkeyAttachment = serde_json::from_value(json!({
            "network": format!("sakala-valkey-{ID}"),
            "host": "10.213.0.2",
        }))
        .expect("attachment");
        assert_eq!(
            attachment.valkey_id().map(|id| id.to_string()).as_deref(),
            Some(ID)
        );
        assert_eq!(attachment.host, Ipv4Addr::new(10, 213, 0, 2));
        assert_eq!(
            serde_json::to_value(&attachment).expect("serialize"),
            json!({ "network": format!("sakala-valkey-{ID}"), "host": "10.213.0.2" })
        );

        for network in [
            "host".to_owned(),
            "bridge".to_owned(),
            format!("sakala-project-{ID}"),
            format!("sakala-valkey-{}", ID.to_uppercase()),
            format!("sakala-valkey-{}", ID.replace('-', "")),
            format!("sakala-valkey-{ID}x"),
            "sakala-valkey-".to_owned(),
        ] {
            let error = serde_json::from_value::<ValkeyAttachment>(json!({
                "network": network,
                "host": "10.213.0.2",
            }))
            .expect_err("refused");
            assert!(error.to_string().contains("valkey.network"), "{network}");
        }
        for host in [
            json!("10.213.0.256"),
            json!("::1"),
            json!("redis"),
            json!(7),
        ] {
            assert!(
                serde_json::from_value::<ValkeyAttachment>(json!({
                    "network": format!("sakala-valkey-{ID}"),
                    "host": host,
                }))
                .is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn results_and_heartbeat_entries_serialize_to_the_documented_shapes() {
        let id = Uuid::parse_str(ID).expect("uuid");
        let memory = ValkeyMemoryMb::new(64).expect("memory");
        assert_eq!(
            serde_json::to_value(CreateValkeyResult {
                valkey_id: id,
                mode: ValkeyMode::Cache,
                maxmemory_mb: memory,
                memory_limit_mb: 192,
                network: valkey_resource_name(id),
                endpoint: ValkeyEndpoint {
                    host: Ipv4Addr::new(10, 213, 0, 2),
                    port: VALKEY_PORT,
                },
                created: true,
                locked: false,
            })
            .expect("serialize"),
            json!({
                "valkey_id": ID,
                "mode": "cache",
                "maxmemory_mb": 64,
                "memory_limit_mb": 192,
                "network": format!("sakala-valkey-{ID}"),
                "endpoint": { "host": "10.213.0.2", "port": 6379 },
                "created": true,
                "locked": false,
            })
        );
        assert_eq!(
            serde_json::to_value(ManagedValkeyUsage {
                valkey_id: id,
                mode: ValkeyMode::Persistent,
                maxmemory_mb: memory,
                locked: false,
                status: "running".to_owned(),
                restart_count: 0,
                used_memory: Some(1_048_576),
                maxmemory: Some(67_108_864),
                evicted_keys: Some(0),
                errorstat_oom: Some(2),
                connected_clients: Some(3),
                aof_last_write_status: Some("ok".to_owned()),
            })
            .expect("serialize"),
            json!({
                "valkey_id": ID,
                "mode": "persistent",
                "maxmemory_mb": 64,
                "locked": false,
                "status": "running",
                "restart_count": 0,
                "used_memory": 1_048_576,
                "maxmemory": 67_108_864,
                "evicted_keys": 0,
                "errorstat_oom": 2,
                "connected_clients": 3,
                "aof_last_write_status": "ok",
            })
        );
    }
}
