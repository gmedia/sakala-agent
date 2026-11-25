use async_trait::async_trait;
use sakala_agent_protocol::{
    CreateValkeyPayload, ManagedValkeyUsage, ResizeValkeyPayload, RotateValkeyPasswordPayload,
    ValkeyTargetPayload,
    valkey::{
        CreateValkeyResult, DeleteValkeyResult, ResizeValkeyResult, RotateValkeyPasswordResult,
        ValkeyLockResult,
    },
};

use super::RuntimeExecutionError;

/// Code of a [`ValkeyAdmin::valkey_usage`] error that means a command holds
/// the stores and the read was skipped, not that it failed. Only the heartbeat
/// sees it; it is never sent to the control plane.
pub const VALKEY_USAGE_BUSY: &str = "valkey_usage_busy";

fn unsupported(operation: &str) -> RuntimeExecutionError {
    RuntimeExecutionError::unsupported_command(format!("Valkey admin does not support {operation}"))
}

/// Runs the managed Valkey stores of a Valkey host (ADR-023): one container
/// per store, on its own internal network.
///
/// A separate port from [`super::RuntimeExecutor`] because it holds a
/// different privilege: each store's admin password, which never leaves the
/// node. Results are typed so a tenant password can never reach a command
/// result. Every operation is idempotent: running it again after it completed
/// changes nothing the tenant can see and succeeds. An operation an adapter
/// does not implement fails with `unsupported_runtime_command`.
#[async_trait]
pub trait ValkeyAdmin: Send + Sync {
    /// Checks what the role needs. Called once at startup for the log.
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        Err(unsupported("check"))
    }

    async fn create_valkey(
        &self,
        _payload: &CreateValkeyPayload,
    ) -> Result<CreateValkeyResult, RuntimeExecutionError> {
        Err(unsupported("create_valkey"))
    }

    async fn delete_valkey(
        &self,
        _payload: &ValkeyTargetPayload,
    ) -> Result<DeleteValkeyResult, RuntimeExecutionError> {
        Err(unsupported("delete_valkey"))
    }

    async fn rotate_valkey_password(
        &self,
        _payload: &RotateValkeyPasswordPayload,
    ) -> Result<RotateValkeyPasswordResult, RuntimeExecutionError> {
        Err(unsupported("rotate_valkey_password"))
    }

    async fn lock_valkey(
        &self,
        _payload: &ValkeyTargetPayload,
    ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
        Err(unsupported("lock_valkey"))
    }

    async fn unlock_valkey(
        &self,
        _payload: &ValkeyTargetPayload,
    ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
        Err(unsupported("unlock_valkey"))
    }

    async fn resize_valkey(
        &self,
        _payload: &ResizeValkeyPayload,
    ) -> Result<ResizeValkeyResult, RuntimeExecutionError> {
        Err(unsupported("resize_valkey"))
    }

    /// Every store of the node with its container and `INFO` readings. An
    /// error means the stores could not be listed, never that there are none;
    /// an error with code [`VALKEY_USAGE_BUSY`] means the read was skipped. A
    /// store that does not answer is listed with `null` readings.
    async fn valkey_usage(&self) -> Result<Vec<ManagedValkeyUsage>, RuntimeExecutionError> {
        Err(unsupported("valkey_usage"))
    }

    /// Removes this agent's store networks that no store record and no
    /// container use, and logs store containers and volumes without a record,
    /// which it never removes. Runs next to the project network reconcile.
    async fn reconcile(&self) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}
