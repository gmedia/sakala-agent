//! The SQL a database host runs, built without a connection so every statement
//! and every idempotency decision can be tested on its own.
//!
//! DDL takes no bind parameters, so names and the password verifier are written
//! into statements with libpq's escaping rules (`postgres_protocol::escape`).
//! Names are already restricted to `^p_[a-z0-9]{16,40}$`; escaping is the
//! second line of defence. Plaintext passwords never appear here: the role gets
//! a SCRAM-SHA-256 verifier computed by the agent, so the password reaches
//! neither the server nor its statement log.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use sakala_agent_core::ports::RuntimeExecutionError;
use sakala_agent_protocol::{DatabaseExtension, ManagedName, databases::error_codes};

/// `CONNECTION LIMIT` of every tenant role and database (ADR-020). Shared by
/// every replica of every project the database is attached to. A tenant can
/// raise its database's limit as owner, but not its role's.
pub const TENANT_CONNECTION_LIMIT: u32 = 5;

/// Role defaults a tenant may override with `SET`, for runaway queries and
/// forgotten transactions.
pub const TENANT_STATEMENT_TIMEOUT: &str = "60s";
pub const TENANT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT: &str = "60s";
/// Long enough for an application pool's idle connection to stay, short enough
/// that a crashed client does not keep one of the five slots for long.
pub const TENANT_IDLE_SESSION_TIMEOUT: &str = "30min";

/// `temp_file_limit` per session. Only a superuser can set this parameter, so
/// the tenant cannot raise it.
pub const TENANT_TEMP_FILE_LIMIT: &str = "1GB";

/// The role defaults set on every tenant role, in order.
pub const TENANT_ROLE_SETTINGS: [(&str, &str); 4] = [
    ("statement_timeout", TENANT_STATEMENT_TIMEOUT),
    (
        "idle_in_transaction_session_timeout",
        TENANT_IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
    ),
    ("idle_session_timeout", TENANT_IDLE_SESSION_TIMEOUT),
    ("temp_file_limit", TENANT_TEMP_FILE_LIMIT),
];

/// Every privileged attribute a tenant role must not have, spelled out so a
/// repeated create also removes one that was added by hand.
const TENANT_ROLE_ATTRIBUTES: &str =
    "NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS";

/// The schema tenants own and extensions are installed into.
const PUBLIC_SCHEMA: &str = "public";

/// Databases every tenant could connect to by default. Closed to `PUBLIC` so a
/// tenant connects to its own database only, and cannot reach another
/// database from which to alter its own.
pub const MAINTENANCE_DATABASES: [&str; 2] = ["postgres", "template1"];

/// Whether a role exists, who owns the database, and which other databases
/// the role owns. Parameters: `$1` database, `$2` role.
pub const OBSERVE_TENANT: &str = "\
SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $2) AS role_exists, \
(SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1) AS database_owner, \
ARRAY(SELECT d.datname::text FROM pg_database d JOIN pg_roles r ON r.oid = d.datdba \
WHERE r.rolname = $2 AND d.datname <> $1 ORDER BY 1) AS other_databases";

/// The owner of a database, no row when it does not exist. `$1` database.
pub const DATABASE_OWNER: &str =
    "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1";

/// One row when the role exists. `$1` role.
pub const ROLE_EXISTS: &str = "SELECT 1 FROM pg_roles WHERE rolname = $1";

/// Ends every session of a tenant role and counts them. `$1` role. Sessions of
/// other users, such as a backup running as the admin user, are left alone.
pub const TERMINATE_ROLE_SESSIONS: &str = "\
SELECT count(*) FILTER (WHERE pg_terminate_backend(pid)) FROM pg_stat_activity \
WHERE usename = $1 AND pid <> pg_backend_pid()";

/// One row when the extension is installed in the current database. `$1` name.
pub const EXTENSION_INSTALLED: &str = "SELECT 1 FROM pg_extension WHERE extname = $1";

/// One row when the host has the extension's files. `$1` name.
pub const EXTENSION_AVAILABLE: &str = "SELECT 1 FROM pg_available_extensions WHERE name = $1";

/// Whether the connected user is a superuser, and the server's version number.
pub const ADMIN_IDENTITY: &str = "\
SELECT rolsuper, current_setting('server_version_num')::int FROM pg_roles \
WHERE rolname = current_user";

/// Every managed database and its size, by name. `$1` the managed name pattern.
pub const DATABASE_SIZES: &str = "\
SELECT datname::text, pg_database_size(oid) FROM pg_database \
WHERE datname ~ $1 AND NOT datistemplate ORDER BY datname";

/// What the instance holds for one tenant before a create or a drop.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ObservedTenant {
    pub role_exists: bool,
    /// The owner of the database, `None` when the database does not exist.
    pub database_owner: Option<String>,
    /// Databases other than this one owned by the role.
    pub other_databases: Vec<String>,
}

/// The statements of one `CreateDatabase`, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePlan {
    /// Role statements, run in one transaction.
    pub role: Vec<String>,
    /// Database statements, run outside a transaction because `CREATE
    /// DATABASE` cannot run inside one.
    pub database: Vec<String>,
    /// Run as the admin user while connected to the new database.
    pub inside_database: Vec<String>,
    pub creates_database: bool,
}

/// The statements of one `DropDatabase`, in order, none in a transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DropPlan {
    pub statements: Vec<String>,
    /// Whether the database or the role existed.
    pub drops_anything: bool,
}

#[must_use]
pub fn identifier(name: &ManagedName) -> String {
    escape_identifier(name.as_str())
}

/// Plans a create that converges: a missing role or database is created, an
/// existing one gets its limits, defaults, and privileges set again, and a
/// completed create run again changes nothing a tenant could notice. An
/// existing role keeps its `LOGIN` state, so a repeated create never unlocks a
/// locked database.
///
/// # Errors
///
/// `database_conflict` when the database belongs to another role, or the role
/// already owns another database.
pub fn plan_create(
    database: &ManagedName,
    role: &ManagedName,
    password_verifier: &str,
    observed: &ObservedTenant,
) -> Result<CreatePlan, RuntimeExecutionError> {
    check_ownership(database, role, observed)?;
    let role_name = identifier(role);
    let database_name = identifier(database);
    let password = escape_literal(password_verifier);
    let mut role_statements = vec![if observed.role_exists {
        format!(
            "ALTER ROLE {role_name} WITH {TENANT_ROLE_ATTRIBUTES} CONNECTION LIMIT {TENANT_CONNECTION_LIMIT} PASSWORD {password}"
        )
    } else {
        format!(
            "CREATE ROLE {role_name} WITH LOGIN {TENANT_ROLE_ATTRIBUTES} CONNECTION LIMIT {TENANT_CONNECTION_LIMIT} PASSWORD {password}"
        )
    }];
    role_statements.extend(TENANT_ROLE_SETTINGS.iter().map(|(parameter, value)| {
        format!(
            "ALTER ROLE {role_name} SET {parameter} = {}",
            escape_literal(value)
        )
    }));
    let creates_database = observed.database_owner.is_none();
    let database_statements = vec![
        if creates_database {
            format!(
                "CREATE DATABASE {database_name} WITH OWNER {role_name} CONNECTION LIMIT {TENANT_CONNECTION_LIMIT}"
            )
        } else {
            format!(
                "ALTER DATABASE {database_name} WITH CONNECTION LIMIT {TENANT_CONNECTION_LIMIT}"
            )
        },
        format!("REVOKE ALL ON DATABASE {database_name} FROM PUBLIC"),
    ];
    Ok(CreatePlan {
        role: role_statements,
        database: database_statements,
        inside_database: vec![
            format!("REVOKE CREATE ON SCHEMA {PUBLIC_SCHEMA} FROM PUBLIC"),
            format!("GRANT ALL ON SCHEMA {PUBLIC_SCHEMA} TO {role_name}"),
        ],
        creates_database,
    })
}

/// Plans a drop that converges: the role's login goes first so nothing
/// reconnects, the database is dropped with its sessions, then the role.
/// Whatever is already gone is skipped.
///
/// # Errors
///
/// `database_conflict` when the database belongs to another role, or the role
/// still owns another database and could not be dropped.
pub fn plan_drop(
    database: &ManagedName,
    role: &ManagedName,
    observed: &ObservedTenant,
) -> Result<DropPlan, RuntimeExecutionError> {
    check_ownership(database, role, observed)?;
    let role_name = identifier(role);
    let mut statements = Vec::new();
    if observed.role_exists {
        statements.push(login(role, false));
    }
    if observed.database_owner.is_some() {
        statements.push(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            identifier(database)
        ));
    }
    if observed.role_exists {
        statements.push(format!("DROP ROLE IF EXISTS {role_name}"));
    }
    Ok(DropPlan {
        drops_anything: !statements.is_empty(),
        statements,
    })
}

fn check_ownership(
    database: &ManagedName,
    role: &ManagedName,
    observed: &ObservedTenant,
) -> Result<(), RuntimeExecutionError> {
    if let Some(owner) = observed
        .database_owner
        .as_deref()
        .filter(|owner| *owner != role.as_str())
    {
        return Err(conflict(format!(
            "database {database} belongs to role {owner}, not {role}"
        )));
    }
    if !observed.other_databases.is_empty() {
        return Err(conflict(format!(
            "role {role} already owns {}",
            observed.other_databases.join(", ")
        )));
    }
    Ok(())
}

/// `LOGIN` on unlock, `NOLOGIN` on lock and before a drop. A lock turns off
/// the role's login rather than the database's `ALLOW_CONNECTIONS`: the
/// tenant cannot turn it back on without `CREATEROLE`, and the admin user can
/// still connect for backups and size reads.
#[must_use]
pub fn login(role: &ManagedName, allowed: bool) -> String {
    let attribute = if allowed { "LOGIN" } else { "NOLOGIN" };
    format!("ALTER ROLE {} WITH {attribute}", identifier(role))
}

/// Rotation: the role's password becomes a SCRAM-SHA-256 verifier computed by
/// the agent, never the plaintext.
#[must_use]
pub fn set_scram_verifier(role: &ManagedName, password_verifier: &str) -> String {
    format!(
        "ALTER ROLE {} WITH PASSWORD {}",
        identifier(role),
        escape_literal(password_verifier)
    )
}

/// Installs into `public`, the schema the tenant owns, so the tenant uses the
/// extension without qualifying names.
#[must_use]
pub fn create_extension(extension: DatabaseExtension) -> String {
    format!(
        "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {PUBLIC_SCHEMA}",
        escape_identifier(extension.as_str())
    )
}

/// Closes one maintenance database to `PUBLIC`.
#[must_use]
pub fn close_to_public(database: &str) -> String {
    format!(
        "REVOKE ALL ON DATABASE {} FROM PUBLIC",
        escape_identifier(database)
    )
}

#[must_use]
pub fn conflict(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::DATABASE_CONFLICT, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIER: &str = "SCRAM-SHA-256$4096:c2FsdA==$c3RvcmVk:c2VydmVy";

    fn name(value: &str) -> ManagedName {
        ManagedName::parse(value).expect("name")
    }

    fn database() -> ManagedName {
        name("p_0123456789abcdef")
    }

    fn role() -> ManagedName {
        name("p_fedcba9876543210")
    }

    #[test]
    fn identifiers_and_literals_are_quoted_by_libpq_rules() {
        assert_eq!(identifier(&database()), "\"p_0123456789abcdef\"");
        assert_eq!(escape_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(escape_literal("it's"), "'it''s'");
        assert_eq!(escape_literal("back\\slash"), " E'back\\\\slash'");
    }

    #[test]
    fn a_first_create_makes_a_hardened_role_and_database() {
        let plan =
            plan_create(&database(), &role(), VERIFIER, &ObservedTenant::default()).expect("plan");

        assert_eq!(
            plan.role,
            [
                format!(
                    "CREATE ROLE \"p_fedcba9876543210\" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 5 PASSWORD '{VERIFIER}'"
                ),
                "ALTER ROLE \"p_fedcba9876543210\" SET statement_timeout = '60s'".to_owned(),
                "ALTER ROLE \"p_fedcba9876543210\" SET idle_in_transaction_session_timeout = '60s'"
                    .to_owned(),
                "ALTER ROLE \"p_fedcba9876543210\" SET idle_session_timeout = '30min'".to_owned(),
                "ALTER ROLE \"p_fedcba9876543210\" SET temp_file_limit = '1GB'".to_owned(),
            ]
        );
        assert_eq!(
            plan.database,
            [
                "CREATE DATABASE \"p_0123456789abcdef\" WITH OWNER \"p_fedcba9876543210\" CONNECTION LIMIT 5",
                "REVOKE ALL ON DATABASE \"p_0123456789abcdef\" FROM PUBLIC",
            ]
        );
        assert_eq!(
            plan.inside_database,
            [
                "REVOKE CREATE ON SCHEMA public FROM PUBLIC",
                "GRANT ALL ON SCHEMA public TO \"p_fedcba9876543210\"",
            ]
        );
        assert!(plan.creates_database);
    }

    #[test]
    fn a_repeated_create_converges_without_creating_or_unlocking() {
        let completed = ObservedTenant {
            role_exists: true,
            database_owner: Some(role().to_string()),
            other_databases: Vec::new(),
        };
        let plan = plan_create(&database(), &role(), VERIFIER, &completed).expect("plan");

        assert!(!plan.creates_database);
        assert!(plan.role[0].starts_with("ALTER ROLE \"p_fedcba9876543210\" WITH NOSUPERUSER"));
        assert!(!plan.role[0].contains("LOGIN "), "{}", plan.role[0]);
        assert!(plan.role[0].contains("CONNECTION LIMIT 5"));
        assert_eq!(
            plan.database[0],
            "ALTER DATABASE \"p_0123456789abcdef\" WITH CONNECTION LIMIT 5"
        );
        let every = [plan.role, plan.database, plan.inside_database].concat();
        assert!(
            every
                .iter()
                .all(|statement| !statement.starts_with("CREATE"))
        );
    }

    #[test]
    fn a_create_interrupted_after_the_role_creates_only_the_database() {
        let interrupted = ObservedTenant {
            role_exists: true,
            ..ObservedTenant::default()
        };
        let plan = plan_create(&database(), &role(), VERIFIER, &interrupted).expect("plan");

        assert!(plan.role[0].starts_with("ALTER ROLE"));
        assert!(plan.database[0].starts_with("CREATE DATABASE"));
        assert!(plan.creates_database);
    }

    #[test]
    fn a_create_never_takes_over_another_database_or_gives_a_role_a_second_one() {
        let foreign = ObservedTenant {
            role_exists: true,
            database_owner: Some("postgres".to_owned()),
            other_databases: Vec::new(),
        };
        let error = plan_create(&database(), &role(), VERIFIER, &foreign).expect_err("conflict");
        assert_eq!(error.code(), error_codes::DATABASE_CONFLICT);
        assert!(error.to_string().contains("belongs to role postgres"));

        let second = ObservedTenant {
            role_exists: true,
            database_owner: None,
            other_databases: vec!["p_aaaaaaaaaaaaaaaa".to_owned()],
        };
        let error = plan_create(&database(), &role(), VERIFIER, &second).expect_err("conflict");
        assert_eq!(error.code(), error_codes::DATABASE_CONFLICT);
        assert!(!error.to_string().contains(VERIFIER));
    }

    #[test]
    fn a_drop_closes_logins_first_and_skips_what_is_gone() {
        let full = ObservedTenant {
            role_exists: true,
            database_owner: Some(role().to_string()),
            other_databases: Vec::new(),
        };
        let plan = plan_drop(&database(), &role(), &full).expect("plan");
        assert_eq!(
            plan.statements,
            [
                "ALTER ROLE \"p_fedcba9876543210\" WITH NOLOGIN",
                "DROP DATABASE IF EXISTS \"p_0123456789abcdef\" WITH (FORCE)",
                "DROP ROLE IF EXISTS \"p_fedcba9876543210\"",
            ]
        );
        assert!(plan.drops_anything);

        let role_only = ObservedTenant {
            role_exists: true,
            ..ObservedTenant::default()
        };
        let plan = plan_drop(&database(), &role(), &role_only).expect("plan");
        assert_eq!(plan.statements.len(), 2);
        assert!(plan.statements[1].starts_with("DROP ROLE"));

        let gone = plan_drop(&database(), &role(), &ObservedTenant::default()).expect("plan");
        assert!(gone.statements.is_empty());
        assert!(!gone.drops_anything);
    }

    #[test]
    fn a_drop_never_removes_a_database_of_another_role() {
        let foreign = ObservedTenant {
            role_exists: true,
            database_owner: Some("p_aaaaaaaaaaaaaaaa".to_owned()),
            other_databases: Vec::new(),
        };
        assert_eq!(
            plan_drop(&database(), &role(), &foreign)
                .expect_err("conflict")
                .code(),
            error_codes::DATABASE_CONFLICT
        );
    }

    #[test]
    fn lock_password_extension_and_maintenance_statements() {
        assert_eq!(
            login(&role(), false),
            "ALTER ROLE \"p_fedcba9876543210\" WITH NOLOGIN"
        );
        assert_eq!(
            login(&role(), true),
            "ALTER ROLE \"p_fedcba9876543210\" WITH LOGIN"
        );
        assert_eq!(
            set_scram_verifier(&role(), VERIFIER),
            format!("ALTER ROLE \"p_fedcba9876543210\" WITH PASSWORD '{VERIFIER}'")
        );
        assert_eq!(
            create_extension(DatabaseExtension::Vector),
            "CREATE EXTENSION IF NOT EXISTS \"vector\" WITH SCHEMA public"
        );
        assert_eq!(
            create_extension(DatabaseExtension::PgTrgm),
            "CREATE EXTENSION IF NOT EXISTS \"pg_trgm\" WITH SCHEMA public"
        );
        assert_eq!(
            MAINTENANCE_DATABASES.map(close_to_public),
            [
                "REVOKE ALL ON DATABASE \"postgres\" FROM PUBLIC",
                "REVOKE ALL ON DATABASE \"template1\" FROM PUBLIC",
            ]
        );
    }
}
