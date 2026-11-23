//! Managed object storage commands (ADR-021). Payloads are validated here,
//! before the admin port sees them, and results are typed, so a secret access
//! key never reaches the command result.

use sakala_agent_protocol::{AgentCommand, BucketPayloadError, CommandType};
use serde::Serialize;

use crate::ports::{CommandOutput, ObjectStorageAdmin, RuntimeExecutionError};

/// Runs one bucket command. A node without an object storage admin refuses
/// every bucket command with `unsupported_runtime_command`.
pub async fn handle(
    command: &AgentCommand,
    admin: Option<&dyn ObjectStorageAdmin>,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let Some(admin) = admin else {
        return Err(RuntimeExecutionError::unsupported_command(format!(
            "{:?} needs a node with SAKALA_OBJECT_STORAGE_DRIVER=rustfs",
            command.command_type
        )));
    };
    match command.command_type {
        CommandType::CreateBucket => {
            let payload = command.create_bucket_payload().map_err(invalid(command))?;
            output(admin.create_bucket(&payload).await?)
        }
        CommandType::DeleteBucket => {
            let payload = command.bucket_key_payload().map_err(invalid(command))?;
            output(admin.delete_bucket(&payload).await?)
        }
        CommandType::RotateBucketKey => {
            let payload = command
                .rotate_bucket_key_payload()
                .map_err(invalid(command))?;
            output(admin.rotate_bucket_key(&payload).await?)
        }
        CommandType::SetBucketQuota => {
            let payload = command
                .set_bucket_quota_payload()
                .map_err(invalid(command))?;
            output(admin.set_bucket_quota(&payload).await?)
        }
        CommandType::LockBucket => {
            let payload = command.bucket_key_payload().map_err(invalid(command))?;
            output(admin.lock_bucket(&payload).await?)
        }
        CommandType::UnlockBucket => {
            let payload = command.bucket_key_payload().map_err(invalid(command))?;
            output(admin.unlock_bucket(&payload).await?)
        }
        other => Err(RuntimeExecutionError::invalid_command(format!(
            "{other:?} is not a bucket command"
        ))),
    }
}

fn invalid(command: &AgentCommand) -> impl Fn(BucketPayloadError) -> RuntimeExecutionError {
    let command_type = command.command_type;
    move |error| RuntimeExecutionError::new(error.code(), format!("{command_type:?} {error}"))
}

fn output(result: impl Serialize) -> Result<CommandOutput, RuntimeExecutionError> {
    serde_json::to_value(result)
        .map(CommandOutput::with_result)
        .map_err(|error| {
            RuntimeExecutionError::new(
                "runtime_execution_failed",
                format!("bucket command result could not be serialized: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sakala_agent_protocol::{
        BucketKeyPayload, CommandStatus, CreateBucketPayload, ManagedBucketUsage,
        RotateBucketKeyPayload, SetBucketQuotaPayload,
        buckets::{
            BucketLockResult, CreateBucketResult, DeleteBucketResult, RotateBucketKeyResult,
            SetBucketQuotaResult, error_codes,
        },
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const BUCKET: &str = "b-8k2m4q7x9z1c3v5b6n0d";
    const KEY: &str = "SK7Q2M9X4K1C8V5B3N0D";
    const SECRET: &str = "c2VjcmV0YWNjZXNza2V5dmFsdWUxMjM0NTY3ODkw";

    /// Records the calls it received and answers like a real instance would.
    #[derive(Default)]
    struct RecordingAdmin {
        calls: Mutex<Vec<String>>,
        existing: Mutex<bool>,
    }

    impl RecordingAdmin {
        fn record(&self, call: String) {
            self.calls.lock().expect("lock").push(call);
        }
    }

    #[async_trait]
    impl ObjectStorageAdmin for RecordingAdmin {
        async fn create_bucket(
            &self,
            payload: &CreateBucketPayload,
        ) -> Result<CreateBucketResult, RuntimeExecutionError> {
            self.record(format!(
                "create {} {} {:?} {}",
                payload.bucket,
                payload.access_key_id,
                payload.secret_access_key,
                payload.quota_mb.get()
            ));
            let created = !std::mem::replace(&mut *self.existing.lock().expect("lock"), true);
            Ok(CreateBucketResult {
                bucket: payload.bucket.clone(),
                access_key_id: payload.access_key_id.clone(),
                quota_mb: payload.quota_mb,
                created,
            })
        }

        async fn delete_bucket(
            &self,
            payload: &BucketKeyPayload,
        ) -> Result<DeleteBucketResult, RuntimeExecutionError> {
            self.record(format!(
                "delete {} {}",
                payload.bucket, payload.access_key_id
            ));
            let deleted = std::mem::replace(&mut *self.existing.lock().expect("lock"), false);
            Ok(DeleteBucketResult {
                bucket: payload.bucket.clone(),
                deleted,
            })
        }

        async fn rotate_bucket_key(
            &self,
            payload: &RotateBucketKeyPayload,
        ) -> Result<RotateBucketKeyResult, RuntimeExecutionError> {
            self.record(format!(
                "rotate {} {:?}",
                payload.access_key_id, payload.secret_access_key
            ));
            Ok(RotateBucketKeyResult {
                bucket: payload.bucket.clone(),
                access_key_id: payload.access_key_id.clone(),
            })
        }

        async fn set_bucket_quota(
            &self,
            payload: &SetBucketQuotaPayload,
        ) -> Result<SetBucketQuotaResult, RuntimeExecutionError> {
            self.record(format!(
                "quota {} {}",
                payload.bucket,
                payload.quota_mb.get()
            ));
            Ok(SetBucketQuotaResult {
                bucket: payload.bucket.clone(),
                quota_mb: payload.quota_mb,
            })
        }

        async fn lock_bucket(
            &self,
            payload: &BucketKeyPayload,
        ) -> Result<BucketLockResult, RuntimeExecutionError> {
            self.record(format!("lock {}", payload.access_key_id));
            Ok(BucketLockResult {
                bucket: payload.bucket.clone(),
                access_key_id: payload.access_key_id.clone(),
                locked: true,
            })
        }

        async fn unlock_bucket(
            &self,
            payload: &BucketKeyPayload,
        ) -> Result<BucketLockResult, RuntimeExecutionError> {
            self.record(format!("unlock {}", payload.access_key_id));
            Ok(BucketLockResult {
                bucket: payload.bucket.clone(),
                access_key_id: payload.access_key_id.clone(),
                locked: false,
            })
        }

        async fn bucket_usage(&self) -> Result<Vec<ManagedBucketUsage>, RuntimeExecutionError> {
            Ok(Vec::new())
        }
    }

    fn command(command_type: CommandType, payload: Value) -> AgentCommand {
        AgentCommand {
            id: Uuid::new_v4(),
            command_type,
            status: CommandStatus::Claimed,
            project_id: None,
            deployment_id: None,
            payload,
        }
    }

    async fn run(
        admin: &RecordingAdmin,
        command_type: CommandType,
        payload: Value,
    ) -> Result<Value, RuntimeExecutionError> {
        handle(&command(command_type, payload), Some(admin))
            .await
            .map(|output| output.result)
    }

    #[tokio::test]
    async fn every_bucket_command_reaches_the_admin_and_returns_a_typed_result() {
        let admin = RecordingAdmin::default();
        let create = json!({
            "bucket": BUCKET,
            "access_key_id": KEY,
            "secret_access_key": SECRET,
            "quota_mb": 256,
        });
        let key = json!({ "bucket": BUCKET, "access_key_id": KEY });

        let first = run(&admin, CommandType::CreateBucket, create.clone())
            .await
            .expect("create");
        let again = run(&admin, CommandType::CreateBucket, create)
            .await
            .expect("create again");
        let rotate = run(
            &admin,
            CommandType::RotateBucketKey,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "secret_access_key": SECRET }),
        )
        .await
        .expect("rotate");
        let quota = run(
            &admin,
            CommandType::SetBucketQuota,
            json!({ "bucket": BUCKET, "quota_mb": 512 }),
        )
        .await
        .expect("quota");
        let lock = run(&admin, CommandType::LockBucket, key.clone())
            .await
            .expect("lock");
        let unlock = run(&admin, CommandType::UnlockBucket, key.clone())
            .await
            .expect("unlock");
        let delete = run(&admin, CommandType::DeleteBucket, key.clone())
            .await
            .expect("delete");
        let delete_again = run(&admin, CommandType::DeleteBucket, key)
            .await
            .expect("delete again");

        assert_eq!(
            first,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "quota_mb": 256, "created": true })
        );
        assert_eq!(
            again,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "quota_mb": 256, "created": false })
        );
        assert_eq!(rotate, json!({ "bucket": BUCKET, "access_key_id": KEY }));
        assert_eq!(quota, json!({ "bucket": BUCKET, "quota_mb": 512 }));
        assert_eq!(
            lock,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "locked": true })
        );
        assert_eq!(
            unlock,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "locked": false })
        );
        assert_eq!(delete, json!({ "bucket": BUCKET, "deleted": true }));
        assert_eq!(delete_again, json!({ "bucket": BUCKET, "deleted": false }));

        let calls = admin.calls.lock().expect("lock").join("\n");
        assert!(calls.contains("[REDACTED]"));
        assert!(!calls.contains(SECRET), "the secret reached Debug output");
        for result in [first, again, rotate] {
            assert!(!result.to_string().contains(SECRET));
        }
    }

    #[tokio::test]
    async fn invalid_payloads_fail_before_the_admin_with_their_own_code() {
        let admin = RecordingAdmin::default();
        let cases = [
            (
                CommandType::CreateBucket,
                json!({ "bucket": "w-01a0d73b-uploads", "access_key_id": KEY, "secret_access_key": SECRET, "quota_mb": 1 }),
                error_codes::BUCKET_NAME_INVALID,
            ),
            (
                CommandType::DeleteBucket,
                json!({ "bucket": BUCKET, "access_key_id": "rustfsadmin" }),
                error_codes::BUCKET_KEY_INVALID,
            ),
            (
                CommandType::RotateBucketKey,
                json!({ "bucket": BUCKET, "access_key_id": KEY, "secret_access_key": "too-short" }),
                error_codes::BUCKET_KEY_INVALID,
            ),
            (
                CommandType::SetBucketQuota,
                json!({ "bucket": BUCKET, "quota_mb": 0 }),
                error_codes::BUCKET_QUOTA_INVALID,
            ),
            (
                CommandType::LockBucket,
                Value::Null,
                "invalid_runtime_command",
            ),
            (
                CommandType::UnlockBucket,
                json!({ "bucket": BUCKET }),
                "invalid_runtime_command",
            ),
        ];
        for (command_type, payload, code) in cases {
            let error = run(&admin, command_type, payload)
                .await
                .expect_err("invalid");
            assert_eq!(error.code(), code, "{command_type:?}");
        }
        assert!(admin.calls.lock().expect("lock").is_empty());

        let error = run(
            &admin,
            CommandType::RotateBucketKey,
            json!({ "bucket": BUCKET, "access_key_id": KEY, "secret_access_key": "short-but-secret" }),
        )
        .await
        .expect_err("invalid");
        assert!(!error.to_string().contains("short-but-secret"));
    }

    #[tokio::test]
    async fn a_node_without_an_object_storage_admin_refuses_bucket_commands() {
        let error = handle(
            &command(
                CommandType::LockBucket,
                json!({ "bucket": BUCKET, "access_key_id": KEY }),
            ),
            None,
        )
        .await
        .expect_err("unsupported");
        assert_eq!(error.code(), "unsupported_runtime_command");
    }

    #[tokio::test]
    async fn the_failure_fixture_holds_what_the_handler_sends() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../examples/failures/bucket-failures.json"
        ))
        .expect("fixture");
        let message = |code: &str| {
            fixture
                .as_array()
                .expect("list")
                .iter()
                .find(|entry| entry["body"]["error_code"] == code)
                .and_then(|entry| entry["body"]["error_message"].as_str())
                .unwrap_or_else(|| panic!("no {code} in the fixture"))
                .to_owned()
        };
        let admin = RecordingAdmin::default();
        let cases = [
            (
                CommandType::CreateBucket,
                json!({ "bucket": "w-01a0d73b-uploads", "access_key_id": KEY, "secret_access_key": SECRET, "quota_mb": 1 }),
            ),
            (
                CommandType::RotateBucketKey,
                json!({ "bucket": BUCKET, "access_key_id": KEY, "secret_access_key": "too-short" }),
            ),
            (
                CommandType::SetBucketQuota,
                json!({ "bucket": BUCKET, "quota_mb": 0 }),
            ),
        ];
        for (command_type, payload) in cases {
            let error = run(&admin, command_type, payload)
                .await
                .expect_err("invalid");
            assert_eq!(error.to_string(), message(error.code()), "{command_type:?}");
        }
        let unsupported = handle(
            &command(
                CommandType::LockBucket,
                json!({ "bucket": BUCKET, "access_key_id": KEY }),
            ),
            None,
        )
        .await
        .expect_err("unsupported");
        assert_eq!(unsupported.to_string(), message(unsupported.code()));
    }
}
