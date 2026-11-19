//! Managed PostgreSQL (ADR-020): the commands a database host runs and what its
//! heartbeat reports.
//!
//! A node takes these commands only when it advertises
//! [`MANAGED_POSTGRES_CAPABILITY`]. Every name the control plane sends is
//! checked here against [`MANAGED_NAME_PATTERN`] before any SQL is built, so
//! a database host never touches `postgres`, a template, or anything it did
//! not create.

use std::{fmt, net::IpAddr};

use serde::{Deserialize, Serialize};

use crate::placement::is_valid_hostname;

/// Capability advertised by a node whose agent manages a PostgreSQL instance.
pub const MANAGED_POSTGRES_CAPABILITY: &str = "managed-postgres";

/// Every managed database and login role starts with this prefix.
pub const MANAGED_NAME_PREFIX: &str = "p_";

/// Fewest characters after [`MANAGED_NAME_PREFIX`].
pub const MANAGED_NAME_MIN_SUFFIX_LENGTH: usize = 16;

/// Most characters after [`MANAGED_NAME_PREFIX`]. The whole name stays well
/// under PostgreSQL's 63-byte identifier limit.
pub const MANAGED_NAME_MAX_SUFFIX_LENGTH: usize = 40;

/// The rule [`ManagedName::parse`] implements, also used in SQL to find the
/// managed databases. A POSIX regular expression PostgreSQL's `~` accepts.
pub const MANAGED_NAME_PATTERN: &str = "^p_[a-z0-9]{16,40}$";

/// Fewest characters of a tenant password. The control plane generates long
/// random passwords; anything shorter is a bug on its side.
pub const DATABASE_PASSWORD_MIN_LENGTH: usize = 24;

/// Most characters of a tenant password.
pub const DATABASE_PASSWORD_MAX_LENGTH: usize = 128;

/// Failure codes of the database commands, sent as `error_code` on `fail`.
pub mod error_codes {
    /// The agent cannot reach its PostgreSQL instance as the admin user.
    pub const DATABASE_ADMIN_UNAVAILABLE: &str = "database_admin_unavailable";
    /// A `database` or `role` value does not match the managed name pattern.
    pub const DATABASE_NAME_INVALID: &str = "database_name_invalid";
    /// A password is too short, too long, or holds a character outside
    /// printable ASCII.
    pub const DATABASE_PASSWORD_INVALID: &str = "database_password_invalid";
    /// The extension is not on the allow-list.
    pub const DATABASE_EXTENSION_NOT_ALLOWED: &str = "database_extension_not_allowed";
    /// The extension is allowed but not installed on the database host.
    pub const DATABASE_EXTENSION_UNAVAILABLE: &str = "database_extension_unavailable";
    /// The database or role named by the command does not exist.
    pub const DATABASE_NOT_FOUND: &str = "database_not_found";
    /// The objects exist but belong together differently than the command
    /// says, for example a database owned by another role.
    pub const DATABASE_CONFLICT: &str = "database_conflict";
    /// PostgreSQL refused a statement for another reason.
    pub const DATABASE_OPERATION_FAILED: &str = "database_operation_failed";
}

/// The name of a managed database or login role, such as
/// `p_8k2m4q7x9z1c3v5b`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(into = "String")]
pub struct ManagedName(String);

impl ManagedName {
    /// Accepts exactly [`MANAGED_NAME_PATTERN`].
    ///
    /// # Errors
    ///
    /// Returns [`InvalidManagedName`] for any other value.
    pub fn parse(value: &str) -> Result<Self, InvalidManagedName> {
        let valid = value
            .strip_prefix(MANAGED_NAME_PREFIX)
            .is_some_and(|suffix| {
                (MANAGED_NAME_MIN_SUFFIX_LENGTH..=MANAGED_NAME_MAX_SUFFIX_LENGTH)
                    .contains(&suffix.len())
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidManagedName)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<ManagedName> for String {
    fn from(name: ManagedName) -> Self {
        name.0
    }
}

/// A name outside [`MANAGED_NAME_PATTERN`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidManagedName;

impl fmt::Display for InvalidManagedName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "must match {MANAGED_NAME_PATTERN}")
    }
}

impl std::error::Error for InvalidManagedName {}

/// A tenant's password. Never printed: `Debug` shows `[REDACTED]`, and the
/// type cannot be serialized.
#[derive(Clone, Eq, PartialEq)]
pub struct DatabasePassword(String);

impl DatabasePassword {
    /// Accepts [`DATABASE_PASSWORD_MIN_LENGTH`] to
    /// [`DATABASE_PASSWORD_MAX_LENGTH`] printable ASCII characters without
    /// spaces, which survive a connection URL, an env file, and a shell.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDatabasePassword`] for any other value. The error never
    /// holds the value.
    pub fn parse(value: &str) -> Result<Self, InvalidDatabasePassword> {
        let valid = (DATABASE_PASSWORD_MIN_LENGTH..=DATABASE_PASSWORD_MAX_LENGTH)
            .contains(&value.len())
            && value.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidDatabasePassword)
        }
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DatabasePassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// A password outside the accepted length or characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDatabasePassword;

impl fmt::Display for InvalidDatabasePassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "must be {DATABASE_PASSWORD_MIN_LENGTH} to {DATABASE_PASSWORD_MAX_LENGTH} printable ASCII characters without spaces"
        )
    }
}

impl std::error::Error for InvalidDatabasePassword {}

/// The extensions a tenant may ask for (ADR-020). All but `vector` are marked
/// trusted by PostgreSQL; the agent installs every one of them as the admin
/// user so a tenant never needs `CREATE` on the database.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseExtension {
    Pgcrypto,
    Citext,
    PgTrgm,
    Unaccent,
    Hstore,
    /// pgvector. Its PostgreSQL extension name is `vector`.
    Vector,
}

impl DatabaseExtension {
    pub const ALL: [Self; 6] = [
        Self::Pgcrypto,
        Self::Citext,
        Self::PgTrgm,
        Self::Unaccent,
        Self::Hstore,
        Self::Vector,
    ];

    /// The project name the control plane may send for [`Self::Vector`].
    pub const VECTOR_ALIAS: &str = "pgvector";

    /// The name `CREATE EXTENSION` takes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pgcrypto => "pgcrypto",
            Self::Citext => "citext",
            Self::PgTrgm => "pg_trgm",
            Self::Unaccent => "unaccent",
            Self::Hstore => "hstore",
            Self::Vector => "vector",
        }
    }

    /// Accepts an extension name from the allow-list, and `pgvector` for
    /// `vector`. Matching is exact: `PGCRYPTO` is not on the list.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionNotAllowed`] for any other value.
    pub fn parse(value: &str) -> Result<Self, ExtensionNotAllowed> {
        if value == Self::VECTOR_ALIAS {
            return Ok(Self::Vector);
        }
        Self::ALL
            .into_iter()
            .find(|extension| extension.as_str() == value)
            .ok_or(ExtensionNotAllowed)
    }
}

impl fmt::Display for DatabaseExtension {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An extension outside the allow-list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionNotAllowed;

impl fmt::Display for ExtensionNotAllowed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let allowed = DatabaseExtension::ALL.map(DatabaseExtension::as_str);
        write!(formatter, "must be one of {}", allowed.join(", "))
    }
}

impl std::error::Error for ExtensionNotAllowed {}

/// Payload of `CreateDatabase`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateDatabasePayload {
    pub database: ManagedName,
    pub role: ManagedName,
    pub password: DatabasePassword,
}

/// Payload of `DropDatabase`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropDatabasePayload {
    pub database: ManagedName,
    pub role: ManagedName,
}

/// Payload of `RotateDatabasePassword`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RotateDatabasePasswordPayload {
    pub role: ManagedName,
    pub password: DatabasePassword,
}

/// Payload of `LockDatabase` and `UnlockDatabase`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseTargetPayload {
    pub database: ManagedName,
}

/// Payload of `InstallDatabaseExtension`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallDatabaseExtensionPayload {
    pub database: ManagedName,
    pub extension: DatabaseExtension,
}

/// Why a database command payload was refused, each with its own failure code
/// so the control plane can tell a malformed record from a bad name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabasePayloadError {
    /// The payload is not an object with the expected string fields.
    Malformed(String),
    /// The named field is not a managed name.
    NameInvalid(&'static str),
    PasswordInvalid,
    /// The extension the payload asks for, which is not on the allow-list.
    ExtensionNotAllowed(String),
}

impl DatabasePayloadError {
    /// The `error_code` the command fails with.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "invalid_runtime_command",
            Self::NameInvalid(_) => error_codes::DATABASE_NAME_INVALID,
            Self::PasswordInvalid => error_codes::DATABASE_PASSWORD_INVALID,
            Self::ExtensionNotAllowed(_) => error_codes::DATABASE_EXTENSION_NOT_ALLOWED,
        }
    }
}

impl fmt::Display for DatabasePayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(formatter, "payload is invalid: {reason}"),
            Self::NameInvalid(field) => write!(formatter, "{field} {InvalidManagedName}"),
            Self::PasswordInvalid => write!(formatter, "password {InvalidDatabasePassword}"),
            Self::ExtensionNotAllowed(extension) => {
                write!(formatter, "extension {extension:?} {ExtensionNotAllowed}")
            }
        }
    }
}

impl std::error::Error for DatabasePayloadError {}

/// The wire form of every database payload: plain strings, checked after
/// parsing so each rule has its own failure code. Unknown fields are ignored,
/// so the control plane can add one before the agent reads it.
#[derive(Deserialize)]
struct RawDatabasePayload {
    database: Option<String>,
    role: Option<String>,
    password: Option<String>,
    extension: Option<String>,
}

impl RawDatabasePayload {
    fn read(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        Self::deserialize(payload)
            .map_err(|error| DatabasePayloadError::Malformed(error.to_string()))
    }

    fn name(value: Option<&str>, field: &'static str) -> Result<ManagedName, DatabasePayloadError> {
        let value = value.ok_or_else(|| missing(field))?;
        ManagedName::parse(value).map_err(|_| DatabasePayloadError::NameInvalid(field))
    }

    fn database(&self) -> Result<ManagedName, DatabasePayloadError> {
        Self::name(self.database.as_deref(), "database")
    }

    fn role(&self) -> Result<ManagedName, DatabasePayloadError> {
        Self::name(self.role.as_deref(), "role")
    }

    fn password(&self) -> Result<DatabasePassword, DatabasePayloadError> {
        let value = self
            .password
            .as_deref()
            .ok_or_else(|| missing("password"))?;
        DatabasePassword::parse(value).map_err(|_| DatabasePayloadError::PasswordInvalid)
    }

    fn extension(&self) -> Result<DatabaseExtension, DatabasePayloadError> {
        let value = self
            .extension
            .as_deref()
            .ok_or_else(|| missing("extension"))?;
        DatabaseExtension::parse(value)
            .map_err(|_| DatabasePayloadError::ExtensionNotAllowed(value.to_owned()))
    }
}

fn missing(field: &str) -> DatabasePayloadError {
    DatabasePayloadError::Malformed(format!("missing field `{field}`"))
}

impl CreateDatabasePayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        let raw = RawDatabasePayload::read(payload)?;
        Ok(Self {
            database: raw.database()?,
            role: raw.role()?,
            password: raw.password()?,
        })
    }
}

impl DropDatabasePayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        let raw = RawDatabasePayload::read(payload)?;
        Ok(Self {
            database: raw.database()?,
            role: raw.role()?,
        })
    }
}

impl RotateDatabasePasswordPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        let raw = RawDatabasePayload::read(payload)?;
        Ok(Self {
            role: raw.role()?,
            password: raw.password()?,
        })
    }
}

impl DatabaseTargetPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        let raw = RawDatabasePayload::read(payload)?;
        Ok(Self {
            database: raw.database()?,
        })
    }
}

impl InstallDatabaseExtensionPayload {
    /// # Errors
    ///
    /// Returns the first rule the payload breaks.
    pub fn from_value(payload: &serde_json::Value) -> Result<Self, DatabasePayloadError> {
        let raw = RawDatabasePayload::read(payload)?;
        Ok(Self {
            database: raw.database()?,
            extension: raw.extension()?,
        })
    }
}

/// Result of `CreateDatabase`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CreateDatabaseResult {
    pub database: ManagedName,
    pub role: ManagedName,
    /// `false` when the database already existed, owned by `role`, as on a
    /// repeated command. Its role and privileges are set again either way.
    pub created: bool,
}

/// Result of `DropDatabase`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DropDatabaseResult {
    pub database: ManagedName,
    pub role: ManagedName,
    /// `false` when neither the database nor the role existed any more.
    pub dropped: bool,
}

/// Result of `RotateDatabasePassword`. Never holds the password.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RotateDatabasePasswordResult {
    pub role: ManagedName,
}

/// Result of `LockDatabase` and `UnlockDatabase`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DatabaseLockResult {
    pub database: ManagedName,
    /// The owner role, whose login the lock turns off.
    pub role: ManagedName,
    pub locked: bool,
    /// Sessions of the database ended by a lock, `0` on unlock.
    pub terminated_connections: u64,
}

/// Result of `InstallDatabaseExtension`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InstallDatabaseExtensionResult {
    pub database: ManagedName,
    pub extension: DatabaseExtension,
    /// `false` when the extension was already installed in the database.
    pub created: bool,
}

/// `metadata.database.endpoint`: where runtime nodes reach the instance over
/// the private network, from `SAKALA_DATABASE_PUBLIC_HOST` and
/// `SAKALA_DATABASE_PUBLIC_PORT`. The control plane puts it into `DB_HOST`,
/// `DB_PORT`, and `DATABASE_URL` of attached projects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DatabaseEndpoint {
    /// An IPv4 address, an IPv6 address without brackets, or a DNS name.
    pub host: String,
    pub port: u16,
}

impl DatabaseEndpoint {
    /// # Errors
    ///
    /// Returns [`InvalidDatabaseEndpoint`] when the host is not an IP address
    /// or a DNS name, or the port is 0.
    pub fn new(host: &str, port: u16) -> Result<Self, InvalidDatabaseEndpoint> {
        if port == 0 {
            return Err(InvalidDatabaseEndpoint::Port);
        }
        if host.parse::<IpAddr>().is_ok() || is_valid_hostname(host) {
            Ok(Self {
                host: host.to_owned(),
                port,
            })
        } else {
            Err(InvalidDatabaseEndpoint::Host)
        }
    }
}

/// The part of a database endpoint that is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidDatabaseEndpoint {
    Host,
    Port,
}

impl fmt::Display for InvalidDatabaseEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => formatter.write_str(
                "must be an IPv4 address, an IPv6 address without brackets, or a DNS name",
            ),
            Self::Port => formatter.write_str("must be a port from 1 to 65535"),
        }
    }
}

impl std::error::Error for InvalidDatabaseEndpoint {}

/// One entry of `metadata.databases`: a managed database and its size.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedDatabaseSize {
    pub name: String,
    /// `pg_database_size()`: every file of the database on disk, in bytes.
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eGNvcmdl";

    #[test]
    fn managed_names_follow_the_documented_pattern() {
        for valid in [
            "p_0123456789abcdef",
            "p_abcdefghijklmnopqrstuvwxyz0123456789abcd",
            "p_01hzy3k9q8w7e6r5t4",
        ] {
            assert!(ManagedName::parse(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "p_",
            "p_0123456789abcde",
            "p_abcdefghijklmnopqrstuvwxyz0123456789abcde",
            "P_0123456789abcdef",
            "p_0123456789ABCDEF",
            "q_0123456789abcdef",
            "postgres",
            "template1",
            "p_0123456789abcdef\n",
            "p_0123456789abcde\"",
            "p_0123456789abcd-f",
            "p_0123456789abcd f",
            "p_0123456789abcd;f",
            "p_0123456789abcdéf",
        ] {
            assert!(ManagedName::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_password_never_reaches_debug_output_or_errors() {
        let password = DatabasePassword::parse(PASSWORD).expect("password");
        assert_eq!(format!("{password:?}"), "[REDACTED]");
        let payload = CreateDatabasePayload::from_value(&json!({
            "database": "p_0123456789abcdef",
            "role": "p_0123456789abcdef",
            "password": PASSWORD,
        }))
        .expect("payload");
        assert!(!format!("{payload:?}").contains(PASSWORD));

        let short = "short-secret-value";
        let error = CreateDatabasePayload::from_value(&json!({
            "database": "p_0123456789abcdef",
            "role": "p_0123456789abcdef",
            "password": short,
        }))
        .expect_err("short password");
        assert_eq!(error, DatabasePayloadError::PasswordInvalid);
        assert!(!error.to_string().contains(short));
    }

    #[test]
    fn passwords_are_long_printable_ascii() {
        assert!(DatabasePassword::parse(&"a".repeat(DATABASE_PASSWORD_MIN_LENGTH)).is_ok());
        assert!(DatabasePassword::parse(&"a".repeat(DATABASE_PASSWORD_MAX_LENGTH)).is_ok());
        assert!(DatabasePassword::parse("!#$%&'()*+,-./:;<=>?@[]^_`{|}~\"\\abc").is_ok());
        for invalid in [
            "a".repeat(DATABASE_PASSWORD_MIN_LENGTH - 1),
            "a".repeat(DATABASE_PASSWORD_MAX_LENGTH + 1),
            format!("{PASSWORD} space"),
            format!("{PASSWORD}\n"),
            format!("{PASSWORD}\0"),
            format!("{PASSWORD}é"),
        ] {
            assert!(DatabasePassword::parse(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn each_payload_rule_has_its_own_failure_code() {
        let code = |payload: serde_json::Value| {
            CreateDatabasePayload::from_value(&payload)
                .expect_err("invalid")
                .code()
        };
        assert_eq!(code(json!("p_0123456789abcdef")), "invalid_runtime_command");
        assert_eq!(code(json!({})), "invalid_runtime_command");
        assert_eq!(
            code(json!({ "database": 7, "role": "p_0123456789abcdef", "password": PASSWORD })),
            "invalid_runtime_command"
        );
        assert_eq!(
            code(
                json!({ "database": "postgres", "role": "p_0123456789abcdef", "password": PASSWORD })
            ),
            error_codes::DATABASE_NAME_INVALID
        );
        assert_eq!(
            code(
                json!({ "database": "p_0123456789abcdef", "role": "x\"; DROP ROLE postgres; --", "password": PASSWORD })
            ),
            error_codes::DATABASE_NAME_INVALID
        );
        assert_eq!(
            code(
                json!({ "database": "p_0123456789abcdef", "role": "p_0123456789abcdef", "password": "short" })
            ),
            error_codes::DATABASE_PASSWORD_INVALID
        );
        let extension = InstallDatabaseExtensionPayload::from_value(&json!({
            "database": "p_0123456789abcdef",
            "extension": "plpython3u",
        }))
        .expect_err("not allowed");
        assert_eq!(
            extension.code(),
            error_codes::DATABASE_EXTENSION_NOT_ALLOWED
        );
    }

    #[test]
    fn unknown_payload_fields_are_ignored() {
        let payload = DatabaseTargetPayload::from_value(&json!({
            "database": "p_0123456789abcdef",
            "reason": "quota",
        }))
        .expect("payload");
        assert_eq!(payload.database.as_str(), "p_0123456789abcdef");
    }

    #[test]
    fn the_extension_allow_list_is_exact_and_knows_pgvector() {
        for extension in DatabaseExtension::ALL {
            assert_eq!(DatabaseExtension::parse(extension.as_str()), Ok(extension));
        }
        assert_eq!(
            DatabaseExtension::parse("pgvector"),
            Ok(DatabaseExtension::Vector)
        );
        for refused in [
            "plpython3u",
            "dblink",
            "postgres_fdw",
            "adminpack",
            "file_fdw",
            "PGCRYPTO",
            " pgcrypto",
            "pgcrypto;",
            "",
        ] {
            assert!(DatabaseExtension::parse(refused).is_err(), "{refused:?}");
        }
        assert_eq!(
            serde_json::to_value(DatabaseExtension::PgTrgm).expect("serialize"),
            json!("pg_trgm")
        );
    }

    #[test]
    fn endpoints_take_an_address_or_a_name_and_a_real_port() {
        for host in ["10.0.47.10", "fd00::10", "db-01.internal", "localhost"] {
            assert!(DatabaseEndpoint::new(host, 5432).is_ok(), "{host}");
        }
        for host in [
            "",
            "[fd00::10]",
            "db 01",
            "db_01.internal",
            "postgres://db",
            "10.0.0.1:5432",
        ] {
            assert_eq!(
                DatabaseEndpoint::new(host, 5432),
                Err(InvalidDatabaseEndpoint::Host),
                "{host}"
            );
        }
        assert_eq!(
            DatabaseEndpoint::new("10.0.47.10", 0),
            Err(InvalidDatabaseEndpoint::Port)
        );
        assert_eq!(
            serde_json::to_value(DatabaseEndpoint::new("10.0.47.10", 5432).expect("endpoint"))
                .expect("serialize"),
            json!({ "host": "10.0.47.10", "port": 5432 })
        );
    }
}
