use async_trait::async_trait;
use sakala_agent_protocol::{
    CreateDatabasePayload, DatabaseTargetPayload, DropDatabasePayload,
    InstallDatabaseExtensionPayload, ManagedDatabaseSize, RotateDatabasePasswordPayload,
    databases::{
        CreateDatabaseResult, DatabaseLockResult, DropDatabaseResult,
        InstallDatabaseExtensionResult, RotateDatabasePasswordResult,
    },
};

use super::RuntimeExecutionError;

fn unsupported(operation: &str) -> RuntimeExecutionError {
    RuntimeExecutionError::unsupported_command(format!(
        "database admin does not support {operation}"
    ))
}

/// Administers the managed PostgreSQL instance of a database host (ADR-020).
///
/// A separate port from [`super::RuntimeExecutor`] because it holds a different
/// privilege, the instance's superuser credential, which never leaves the
/// adapter. Results are typed so a password can never reach a command result.
/// Every operation is idempotent: running it again after it completed changes
/// nothing and succeeds. Like the runtime port, an operation an adapter does not
/// implement fails with `unsupported_runtime_command`.
#[async_trait]
pub trait DatabaseAdmin: Send + Sync {
    /// Connects, checks that the admin user is a superuser, and closes the
    /// maintenance databases to `PUBLIC`. Called once at startup for the log;
    /// every operation does the same on its own when it has to reconnect.
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        Err(unsupported("check"))
    }

    async fn create_database(
        &self,
        _payload: &CreateDatabasePayload,
    ) -> Result<CreateDatabaseResult, RuntimeExecutionError> {
        Err(unsupported("create_database"))
    }

    async fn drop_database(
        &self,
        _payload: &DropDatabasePayload,
    ) -> Result<DropDatabaseResult, RuntimeExecutionError> {
        Err(unsupported("drop_database"))
    }

    async fn rotate_password(
        &self,
        _payload: &RotateDatabasePasswordPayload,
    ) -> Result<RotateDatabasePasswordResult, RuntimeExecutionError> {
        Err(unsupported("rotate_password"))
    }

    async fn lock_database(
        &self,
        _payload: &DatabaseTargetPayload,
    ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
        Err(unsupported("lock_database"))
    }

    async fn unlock_database(
        &self,
        _payload: &DatabaseTargetPayload,
    ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
        Err(unsupported("unlock_database"))
    }

    async fn install_extension(
        &self,
        _payload: &InstallDatabaseExtensionPayload,
    ) -> Result<InstallDatabaseExtensionResult, RuntimeExecutionError> {
        Err(unsupported("install_extension"))
    }

    /// Every managed database, by name, with its size. An error means the
    /// instance could not be read, never an empty instance.
    async fn database_sizes(&self) -> Result<Vec<ManagedDatabaseSize>, RuntimeExecutionError> {
        Err(unsupported("database_sizes"))
    }
}
