use async_trait::async_trait;
use sakala_agent_protocol::{
    BucketKeyPayload, CreateBucketPayload, DeleteBucketPayload, ManagedBucketUsage,
    RotateBucketKeyPayload, SetBucketQuotaPayload,
    buckets::{
        BucketLockResult, CreateBucketResult, DeleteBucketResult, RotateBucketKeyResult,
        SetBucketQuotaResult,
    },
};

use super::RuntimeExecutionError;

/// Code of a [`ObjectStorageAdmin::bucket_usage`] error that means a command
/// holds the instance and the read was skipped, not that it failed. Only the
/// heartbeat sees it; it is never sent to the control plane.
pub const BUCKET_USAGE_BUSY: &str = "bucket_usage_busy";

fn unsupported(operation: &str) -> RuntimeExecutionError {
    RuntimeExecutionError::unsupported_command(format!(
        "object storage admin does not support {operation}"
    ))
}

/// Administers the S3-compatible object storage instance of an object storage
/// host (ADR-021).
///
/// A separate port from [`super::RuntimeExecutor`] and [`super::DatabaseAdmin`]
/// because it holds a different privilege: the instance's root key, which stays
/// inside the instance's container and never enters the agent. Results are
/// typed so a secret access key can never reach a command result. Every
/// operation is idempotent: running it again after it completed changes nothing
/// the tenant can see and succeeds. An operation an adapter does not implement
/// fails with `unsupported_runtime_command`.
#[async_trait]
pub trait ObjectStorageAdmin: Send + Sync {
    /// Asks the instance for its state. Called once at startup for the log.
    async fn check(&self) -> Result<(), RuntimeExecutionError> {
        Err(unsupported("check"))
    }

    async fn create_bucket(
        &self,
        _payload: &CreateBucketPayload,
    ) -> Result<CreateBucketResult, RuntimeExecutionError> {
        Err(unsupported("create_bucket"))
    }

    async fn delete_bucket(
        &self,
        _payload: &DeleteBucketPayload,
    ) -> Result<DeleteBucketResult, RuntimeExecutionError> {
        Err(unsupported("delete_bucket"))
    }

    async fn rotate_bucket_key(
        &self,
        _payload: &RotateBucketKeyPayload,
    ) -> Result<RotateBucketKeyResult, RuntimeExecutionError> {
        Err(unsupported("rotate_bucket_key"))
    }

    async fn set_bucket_quota(
        &self,
        _payload: &SetBucketQuotaPayload,
    ) -> Result<SetBucketQuotaResult, RuntimeExecutionError> {
        Err(unsupported("set_bucket_quota"))
    }

    async fn lock_bucket(
        &self,
        _payload: &BucketKeyPayload,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        Err(unsupported("lock_bucket"))
    }

    async fn unlock_bucket(
        &self,
        _payload: &BucketKeyPayload,
    ) -> Result<BucketLockResult, RuntimeExecutionError> {
        Err(unsupported("unlock_bucket"))
    }

    /// Every managed bucket, by name, with the bytes it holds. An error means
    /// the instance could not be read, never an empty instance; an error with
    /// code [`BUCKET_USAGE_BUSY`] means the read was skipped.
    async fn bucket_usage(&self) -> Result<Vec<ManagedBucketUsage>, RuntimeExecutionError> {
        Err(unsupported("bucket_usage"))
    }
}
