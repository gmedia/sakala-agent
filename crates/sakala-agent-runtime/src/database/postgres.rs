//! [`DatabaseAdmin`] over `tokio-postgres`, connected to the local instance as
//! its superuser.
//!
//! The admin DSN must point at a Unix socket or a loopback address: the
//! superuser credential never leaves the host and the connection carries no
//! TLS. One maintenance connection is kept and reopened when it closes; a
//! short-lived second connection is opened to a tenant database when a
//! statement must run inside it. Operations run one at a time.

use std::{net::IpAddr, str::FromStr, time::Duration};

use async_trait::async_trait;
use postgres_protocol::password::scram_sha_256;
use sakala_agent_core::ports::{DatabaseAdmin, RuntimeExecutionError, SecretString};
use sakala_agent_protocol::{
    CreateDatabasePayload, DatabaseTargetPayload, DropDatabasePayload,
    InstallDatabaseExtensionPayload, MANAGED_NAME_PATTERN, ManagedDatabaseSize, ManagedName,
    RotateDatabasePasswordPayload,
    databases::{
        CreateDatabaseResult, DatabaseLockResult, DropDatabaseResult,
        InstallDatabaseExtensionResult, RotateDatabasePasswordResult, error_codes,
    },
};
use tokio::sync::Mutex;
use tokio_postgres::{Client, Config, NoTls, config::Host, error::SqlState};
use tracing::debug;

use super::sql::{self, ObservedTenant};

/// The env var holding the admin DSN, named in configuration errors.
pub const DATABASE_ADMIN_DSN_ENV: &str = "SAKALA_DATABASE_ADMIN_DSN";

/// Oldest PostgreSQL major the role supports, as `server_version_num`. 15 made
/// schema `public` owned by the database owner and closed it to `PUBLIC` by
/// default; `DROP DATABASE ... WITH (FORCE)` needs 13.
pub const MIN_SERVER_VERSION_NUM: i32 = 150_000;

/// Used when the DSN sets no `connect_timeout`.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds every admin statement, so a statement stuck on a lock fails the
/// command instead of holding every other database command behind it.
const ADMIN_STATEMENT_TIMEOUT: &str = "120s";

const LOOPBACK_HOST_NAME: &str = "localhost";

/// The checked admin connection settings. `Debug` never shows the password.
#[derive(Clone)]
pub struct PostgresAdminConfig {
    config: Config,
}

impl PostgresAdminConfig {
    /// Parses a libpq-style DSN, URL or `key=value`, and refuses one whose
    /// hosts are not all Unix sockets or loopback addresses.
    ///
    /// # Errors
    ///
    /// Returns a message that never holds the DSN.
    pub fn parse(dsn: &SecretString) -> Result<Self, String> {
        let mut config = Config::from_str(dsn.expose())
            .map_err(|error| format!("{DATABASE_ADMIN_DSN_ENV} is not a valid DSN: {error}"))?;
        if config.get_hosts().is_empty() && config.get_hostaddrs().is_empty() {
            return Err(format!(
                "{DATABASE_ADMIN_DSN_ENV} must name a host: a Unix socket directory such as host=/var/run/postgresql, or a loopback address"
            ));
        }
        let local_hosts = config.get_hosts().iter().all(|host| match host {
            Host::Unix(_) => true,
            Host::Tcp(name) => {
                name == LOOPBACK_HOST_NAME
                    || name
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            }
        });
        let local_addresses = config
            .get_hostaddrs()
            .iter()
            .all(|address| address.is_loopback());
        if !local_hosts || !local_addresses {
            return Err(format!(
                "{DATABASE_ADMIN_DSN_ENV} must point at a Unix socket or a loopback address, because the superuser credential never leaves the database host"
            ));
        }
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        }
        Ok(Self { config })
    }
}

impl std::fmt::Debug for PostgresAdminConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresAdminConfig")
            .field("user", &self.config.get_user())
            .field("dbname", &self.config.get_dbname())
            .finish_non_exhaustive()
    }
}

pub struct PostgresDatabaseAdmin {
    config: Config,
    /// The maintenance connection, `None` until the first operation or after
    /// it closed. The lock also serializes every operation on the instance.
    admin: Mutex<Option<Client>>,
}

impl PostgresDatabaseAdmin {
    #[must_use]
    pub fn new(config: PostgresAdminConfig) -> Self {
        Self {
            config: config.config,
            admin: Mutex::new(None),
        }
    }

    async fn connect(
        &self,
        database: Option<&ManagedName>,
    ) -> Result<Client, RuntimeExecutionError> {
        let mut config = self.config.clone();
        if let Some(database) = database {
            config.dbname(database.as_str());
        }
        let (client, connection) = config.connect(NoTls).await.map_err(postgres_error)?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                debug!(%error, "PostgreSQL admin connection closed with an error");
            }
        });
        client
            .batch_execute(&format!(
                "SET statement_timeout = '{ADMIN_STATEMENT_TIMEOUT}'"
            ))
            .await
            .map_err(postgres_error)?;
        Ok(client)
    }

    /// Returns the open maintenance connection, or opens one, checks that it
    /// is a superuser on a supported version, and closes the maintenance
    /// databases to `PUBLIC`.
    async fn admin<'a>(
        &self,
        slot: &'a mut Option<Client>,
    ) -> Result<&'a mut Client, RuntimeExecutionError> {
        if slot.as_ref().is_none_or(Client::is_closed) {
            *slot = None;
            let client = self.connect(None).await?;
            let row = client
                .query_opt(sql::ADMIN_IDENTITY, &[])
                .await
                .map_err(postgres_error)?;
            let (superuser, version) = row
                .map(|row| (row.get::<_, bool>(0), row.get::<_, i32>(1)))
                .unwrap_or_default();
            if !superuser {
                return Err(unavailable(format!(
                    "the user of {DATABASE_ADMIN_DSN_ENV} is not a superuser"
                )));
            }
            if version < MIN_SERVER_VERSION_NUM {
                return Err(unavailable(format!(
                    "PostgreSQL server_version_num {version} is older than the supported {MIN_SERVER_VERSION_NUM}"
                )));
            }
            for database in sql::MAINTENANCE_DATABASES {
                client
                    .batch_execute(&sql::close_to_public(database))
                    .await
                    .map_err(postgres_error)?;
            }
            *slot = Some(client);
        }
        slot.as_mut()
            .ok_or_else(|| unavailable("the admin connection closed".to_owned()))
    }

    async fn observe(
        client: &Client,
        database: &ManagedName,
        role: &ManagedName,
    ) -> Result<ObservedTenant, RuntimeExecutionError> {
        let row = client
            .query_one(sql::OBSERVE_TENANT, &[&database.as_str(), &role.as_str()])
            .await
            .map_err(postgres_error)?;
        Ok(ObservedTenant {
            role_exists: row.get(0),
            database_owner: row.get(1),
            other_databases: row.get(2),
        })
    }

    /// The managed owner of a database the command names.
    async fn owner(
        client: &Client,
        database: &ManagedName,
    ) -> Result<ManagedName, RuntimeExecutionError> {
        let row = client
            .query_opt(sql::DATABASE_OWNER, &[&database.as_str()])
            .await
            .map_err(postgres_error)?
            .ok_or_else(|| not_found(format!("database {database} does not exist")))?;
        let owner: String = row.get(0);
        ManagedName::parse(&owner).map_err(|_| {
            sql::conflict(format!(
                "database {database} belongs to {owner}, which is not a managed role"
            ))
        })
    }

    async fn set_lock(
        &self,
        payload: &DatabaseTargetPayload,
        locked: bool,
    ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        let role = Self::owner(client, &payload.database).await?;
        client
            .batch_execute(&sql::login(&role, !locked))
            .await
            .map_err(postgres_error)?;
        let terminated_connections = if locked {
            let row = client
                .query_one(sql::TERMINATE_ROLE_SESSIONS, &[&role.as_str()])
                .await
                .map_err(postgres_error)?;
            u64::try_from(row.get::<_, i64>(0)).unwrap_or_default()
        } else {
            0
        };
        Ok(DatabaseLockResult {
            database: payload.database.clone(),
            role,
            locked,
            terminated_connections,
        })
    }
}

#[async_trait]
impl DatabaseAdmin for PostgresDatabaseAdmin {
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        self.admin(&mut slot).await.map(|_| ())
    }

    async fn create_database(
        &self,
        payload: &CreateDatabasePayload,
    ) -> Result<CreateDatabaseResult, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        let observed = Self::observe(client, &payload.database, &payload.role).await?;
        let verifier = scram_sha_256(payload.password.expose().as_bytes());
        let plan = sql::plan_create(&payload.database, &payload.role, &verifier, &observed)?;
        let transaction = client.transaction().await.map_err(postgres_error)?;
        for statement in &plan.role {
            transaction
                .batch_execute(statement)
                .await
                .map_err(postgres_error)?;
        }
        transaction.commit().await.map_err(postgres_error)?;
        for statement in &plan.database {
            client
                .batch_execute(statement)
                .await
                .map_err(postgres_error)?;
        }
        let tenant = self.connect(Some(&payload.database)).await?;
        for statement in &plan.inside_database {
            tenant
                .batch_execute(statement)
                .await
                .map_err(postgres_error)?;
        }
        Ok(CreateDatabaseResult {
            database: payload.database.clone(),
            role: payload.role.clone(),
            created: plan.creates_database,
        })
    }

    async fn drop_database(
        &self,
        payload: &DropDatabasePayload,
    ) -> Result<DropDatabaseResult, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        let observed = Self::observe(client, &payload.database, &payload.role).await?;
        let plan = sql::plan_drop(&payload.database, &payload.role, &observed)?;
        for statement in &plan.statements {
            client
                .batch_execute(statement)
                .await
                .map_err(|error| match error.code() {
                    Some(&SqlState::DEPENDENT_OBJECTS_STILL_EXIST) => sql::conflict(format!(
                        "role {} still owns objects outside its database",
                        payload.role
                    )),
                    _ => postgres_error(error),
                })?;
        }
        Ok(DropDatabaseResult {
            database: payload.database.clone(),
            role: payload.role.clone(),
            dropped: plan.drops_anything,
        })
    }

    async fn rotate_password(
        &self,
        payload: &RotateDatabasePasswordPayload,
    ) -> Result<RotateDatabasePasswordResult, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        client
            .query_opt(sql::ROLE_EXISTS, &[&payload.role.as_str()])
            .await
            .map_err(postgres_error)?
            .ok_or_else(|| not_found(format!("role {} does not exist", payload.role)))?;
        let verifier = scram_sha_256(payload.password.expose().as_bytes());
        client
            .batch_execute(&sql::set_scram_verifier(&payload.role, &verifier))
            .await
            .map_err(postgres_error)?;
        Ok(RotateDatabasePasswordResult {
            role: payload.role.clone(),
        })
    }

    async fn lock_database(
        &self,
        payload: &DatabaseTargetPayload,
    ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
        self.set_lock(payload, true).await
    }

    async fn unlock_database(
        &self,
        payload: &DatabaseTargetPayload,
    ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
        self.set_lock(payload, false).await
    }

    async fn install_extension(
        &self,
        payload: &InstallDatabaseExtensionPayload,
    ) -> Result<InstallDatabaseExtensionResult, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        Self::owner(client, &payload.database).await?;
        let name = payload.extension.as_str();
        client
            .query_opt(sql::EXTENSION_AVAILABLE, &[&name])
            .await
            .map_err(postgres_error)?
            .ok_or_else(|| {
                RuntimeExecutionError::new(
                    error_codes::DATABASE_EXTENSION_UNAVAILABLE,
                    format!("extension {name} is not installed on the database host"),
                )
            })?;
        let tenant = self.connect(Some(&payload.database)).await?;
        let installed = tenant
            .query_opt(sql::EXTENSION_INSTALLED, &[&name])
            .await
            .map_err(postgres_error)?
            .is_some();
        tenant
            .batch_execute(&sql::create_extension(payload.extension))
            .await
            .map_err(postgres_error)?;
        Ok(InstallDatabaseExtensionResult {
            database: payload.database.clone(),
            extension: payload.extension,
            created: !installed,
        })
    }

    async fn database_sizes(&self) -> Result<Vec<ManagedDatabaseSize>, RuntimeExecutionError> {
        let mut slot = self.admin.lock().await;
        let client = self.admin(&mut slot).await?;
        let rows = client
            .query(sql::DATABASE_SIZES, &[&MANAGED_NAME_PATTERN])
            .await
            .map_err(postgres_error)?;
        Ok(rows
            .iter()
            .map(|row| ManagedDatabaseSize {
                name: row.get(0),
                size_bytes: u64::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
            })
            .collect())
    }
}

/// Maps a client error to a failure code. Errors without a server response,
/// and server errors that mean the instance cannot serve right now, are
/// `database_admin_unavailable`; the message is the server's own, which never
/// holds a statement or a password.
fn postgres_error(error: tokio_postgres::Error) -> RuntimeExecutionError {
    let Some(database_error) = error.as_db_error() else {
        return unavailable(format!(
            "the PostgreSQL instance cannot be reached: {error}"
        ));
    };
    let code = database_error.code().code();
    let message = format!(
        "PostgreSQL refused the statement: {} (SQLSTATE {code})",
        database_error.message()
    );
    if is_unavailable_state(code) {
        unavailable(message)
    } else {
        RuntimeExecutionError::new(error_codes::DATABASE_OPERATION_FAILED, message)
    }
}

/// Connection exceptions (class 08), insufficient resources (53), and the
/// instance shutting down or starting up (57P01 to 57P05).
fn is_unavailable_state(code: &str) -> bool {
    code.starts_with("08") || code.starts_with("53") || code.starts_with("57P")
}

fn unavailable(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::DATABASE_ADMIN_UNAVAILABLE, message)
}

fn not_found(message: String) -> RuntimeExecutionError {
    RuntimeExecutionError::new(error_codes::DATABASE_NOT_FOUND, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(dsn: &str) -> Result<PostgresAdminConfig, String> {
        PostgresAdminConfig::parse(&SecretString::new(dsn))
    }

    #[test]
    fn the_admin_dsn_must_stay_on_the_host() {
        for local in [
            "host=/var/run/postgresql user=postgres dbname=postgres",
            "postgresql://postgres:secret-admin-password@localhost:5432/postgres",
            "postgresql://postgres:secret-admin-password@127.0.0.1:5433/postgres",
            "host=::1 user=postgres password=secret-admin-password",
            "host=/run/postgresql,127.0.0.1 user=postgres",
            "hostaddr=127.0.0.1 user=postgres",
        ] {
            assert!(parse(local).is_ok(), "{local}");
        }
        for remote in [
            "postgresql://postgres:secret-admin-password@10.0.47.10:5432/postgres",
            "postgresql://postgres:secret-admin-password@db.internal/postgres",
            "host=127.0.0.1,10.0.0.5 user=postgres password=secret-admin-password",
            "host=localhost hostaddr=10.0.0.5 user=postgres password=secret-admin-password",
            "user=postgres password=secret-admin-password",
        ] {
            let error = parse(remote).expect_err(remote);
            assert!(error.contains(DATABASE_ADMIN_DSN_ENV));
            assert!(!error.contains("secret-admin-password"), "{error}");
        }
        let error = parse("host=/run/postgresql port=x password=secret-admin-password")
            .expect_err("invalid");
        assert!(!error.contains("secret-admin-password"), "{error}");
    }

    #[test]
    fn the_admin_config_never_prints_its_password() {
        let config = parse("postgresql://postgres:secret-admin-password@localhost/postgres")
            .expect("config");
        let printed = format!("{config:?}");
        assert!(!printed.contains("secret-admin-password"), "{printed}");
        assert!(printed.contains("postgres"));
        assert_eq!(
            config.config.get_connect_timeout(),
            Some(&DEFAULT_CONNECT_TIMEOUT)
        );
    }

    #[test]
    fn instance_states_that_cannot_serve_count_as_unavailable() {
        for code in ["08006", "53300", "57P01", "57P03"] {
            assert!(is_unavailable_state(code), "{code}");
        }
        for code in ["42501", "42P04", "2BP01", "0A000"] {
            assert!(!is_unavailable_state(code), "{code}");
        }
    }
}
