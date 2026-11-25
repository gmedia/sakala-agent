//! Managed Valkey commands (ADR-023). Payloads are validated here, before the
//! admin port sees them, and results are typed, so a tenant password never
//! reaches the command result.

use sakala_agent_protocol::{AgentCommand, CommandType, ValkeyPayloadError};
use serde::Serialize;

use crate::ports::{CommandOutput, RuntimeExecutionError, ValkeyAdmin};

/// Runs one Valkey command. A node without a Valkey admin refuses every Valkey
/// command with `unsupported_runtime_command`.
pub async fn handle(
    command: &AgentCommand,
    admin: Option<&dyn ValkeyAdmin>,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let Some(admin) = admin else {
        return Err(RuntimeExecutionError::unsupported_command(format!(
            "{:?} needs a node with SAKALA_VALKEY_DRIVER=docker",
            command.command_type
        )));
    };
    match command.command_type {
        CommandType::CreateValkey => {
            let payload = command.create_valkey_payload().map_err(invalid(command))?;
            output(admin.create_valkey(&payload).await?)
        }
        CommandType::DeleteValkey => {
            let payload = command.valkey_target_payload().map_err(invalid(command))?;
            output(admin.delete_valkey(&payload).await?)
        }
        CommandType::RotateValkeyPassword => {
            let payload = command
                .rotate_valkey_password_payload()
                .map_err(invalid(command))?;
            output(admin.rotate_valkey_password(&payload).await?)
        }
        CommandType::LockValkey => {
            let payload = command.valkey_target_payload().map_err(invalid(command))?;
            output(admin.lock_valkey(&payload).await?)
        }
        CommandType::UnlockValkey => {
            let payload = command.valkey_target_payload().map_err(invalid(command))?;
            output(admin.unlock_valkey(&payload).await?)
        }
        CommandType::ResizeValkey => {
            let payload = command.resize_valkey_payload().map_err(invalid(command))?;
            output(admin.resize_valkey(&payload).await?)
        }
        other => Err(RuntimeExecutionError::invalid_command(format!(
            "{other:?} is not a Valkey command"
        ))),
    }
}

fn invalid(command: &AgentCommand) -> impl Fn(ValkeyPayloadError) -> RuntimeExecutionError {
    let command_type = command.command_type;
    move |error| RuntimeExecutionError::new(error.code(), format!("{command_type:?} {error}"))
}

fn output(result: impl Serialize) -> Result<CommandOutput, RuntimeExecutionError> {
    serde_json::to_value(result)
        .map(CommandOutput::with_result)
        .map_err(|error| {
            RuntimeExecutionError::new(
                "runtime_execution_failed",
                format!("Valkey command result could not be serialized: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, sync::Mutex};

    use async_trait::async_trait;
    use sakala_agent_protocol::{
        CommandStatus, CreateValkeyPayload, ResizeValkeyPayload, RotateValkeyPasswordPayload,
        VALKEY_PORT, ValkeyEndpoint, ValkeyTargetPayload,
        valkey::{
            CreateValkeyResult, DeleteValkeyResult, ResizeValkeyResult, RotateValkeyPasswordResult,
            ValkeyLockResult, error_codes,
        },
        valkey_resource_name,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const ID: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";
    const PASSWORD: &str = "c2VjcmV0dmFsa2V5cGFzc3dvcmQxMjM0";

    /// Records the calls it received and answers like a real node would.
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
    impl ValkeyAdmin for RecordingAdmin {
        async fn create_valkey(
            &self,
            payload: &CreateValkeyPayload,
        ) -> Result<CreateValkeyResult, RuntimeExecutionError> {
            self.record(format!(
                "create {} {} {} {:?}",
                payload.valkey_id,
                payload.mode,
                payload.maxmemory_mb.get(),
                payload.password
            ));
            let created = !std::mem::replace(&mut *self.existing.lock().expect("lock"), true);
            Ok(CreateValkeyResult {
                valkey_id: payload.valkey_id,
                mode: payload.mode,
                maxmemory_mb: payload.maxmemory_mb,
                memory_limit_mb: 192,
                network: valkey_resource_name(payload.valkey_id),
                endpoint: ValkeyEndpoint {
                    host: Ipv4Addr::new(10, 213, 0, 2),
                    port: VALKEY_PORT,
                },
                created,
                locked: false,
            })
        }

        async fn delete_valkey(
            &self,
            payload: &ValkeyTargetPayload,
        ) -> Result<DeleteValkeyResult, RuntimeExecutionError> {
            self.record(format!("delete {}", payload.valkey_id));
            let deleted = std::mem::replace(&mut *self.existing.lock().expect("lock"), false);
            Ok(DeleteValkeyResult {
                valkey_id: payload.valkey_id,
                deleted,
            })
        }

        async fn rotate_valkey_password(
            &self,
            payload: &RotateValkeyPasswordPayload,
        ) -> Result<RotateValkeyPasswordResult, RuntimeExecutionError> {
            self.record(format!(
                "rotate {} {:?}",
                payload.valkey_id, payload.password
            ));
            Ok(RotateValkeyPasswordResult {
                valkey_id: payload.valkey_id,
                disconnected_clients: 2,
            })
        }

        async fn lock_valkey(
            &self,
            payload: &ValkeyTargetPayload,
        ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
            self.record(format!("lock {}", payload.valkey_id));
            Ok(ValkeyLockResult {
                valkey_id: payload.valkey_id,
                locked: true,
                disconnected_clients: 1,
            })
        }

        async fn unlock_valkey(
            &self,
            payload: &ValkeyTargetPayload,
        ) -> Result<ValkeyLockResult, RuntimeExecutionError> {
            self.record(format!("unlock {}", payload.valkey_id));
            Ok(ValkeyLockResult {
                valkey_id: payload.valkey_id,
                locked: false,
                disconnected_clients: 0,
            })
        }

        async fn resize_valkey(
            &self,
            payload: &ResizeValkeyPayload,
        ) -> Result<ResizeValkeyResult, RuntimeExecutionError> {
            self.record(format!(
                "resize {} {}",
                payload.valkey_id,
                payload.maxmemory_mb.get()
            ));
            Ok(ResizeValkeyResult {
                valkey_id: payload.valkey_id,
                maxmemory_mb: payload.maxmemory_mb,
                memory_limit_mb: 320,
                restarted: false,
            })
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
    async fn every_valkey_command_reaches_the_admin_and_returns_a_typed_result() {
        let admin = RecordingAdmin::default();
        let create = json!({
            "valkey_id": ID,
            "mode": "persistent",
            "maxmemory_mb": 64,
            "password": PASSWORD,
        });
        let target = json!({ "valkey_id": ID });

        let first = run(&admin, CommandType::CreateValkey, create.clone())
            .await
            .expect("create");
        let again = run(&admin, CommandType::CreateValkey, create)
            .await
            .expect("create again");
        let rotate = run(
            &admin,
            CommandType::RotateValkeyPassword,
            json!({ "valkey_id": ID, "password": PASSWORD }),
        )
        .await
        .expect("rotate");
        let lock = run(&admin, CommandType::LockValkey, target.clone())
            .await
            .expect("lock");
        let unlock = run(&admin, CommandType::UnlockValkey, target.clone())
            .await
            .expect("unlock");
        let resize = run(
            &admin,
            CommandType::ResizeValkey,
            json!({ "valkey_id": ID, "maxmemory_mb": 128 }),
        )
        .await
        .expect("resize");
        let delete = run(&admin, CommandType::DeleteValkey, target.clone())
            .await
            .expect("delete");
        let delete_again = run(&admin, CommandType::DeleteValkey, target)
            .await
            .expect("delete again");

        assert_eq!(first["created"], true);
        assert_eq!(again["created"], false);
        assert_eq!(first["network"], format!("sakala-valkey-{ID}"));
        assert_eq!(
            first["endpoint"],
            json!({ "host": "10.213.0.2", "port": 6379 })
        );
        assert_eq!(
            rotate,
            json!({ "valkey_id": ID, "disconnected_clients": 2 })
        );
        assert_eq!(
            lock,
            json!({ "valkey_id": ID, "locked": true, "disconnected_clients": 1 })
        );
        assert_eq!(
            unlock,
            json!({ "valkey_id": ID, "locked": false, "disconnected_clients": 0 })
        );
        assert_eq!(
            resize,
            json!({ "valkey_id": ID, "maxmemory_mb": 128, "memory_limit_mb": 320, "restarted": false })
        );
        assert_eq!(delete, json!({ "valkey_id": ID, "deleted": true }));
        assert_eq!(delete_again, json!({ "valkey_id": ID, "deleted": false }));

        let calls = admin.calls.lock().expect("lock").join("\n");
        assert!(calls.contains("[REDACTED]"));
        assert!(
            !calls.contains(PASSWORD),
            "the password reached Debug output"
        );
        for result in [first, again, rotate] {
            assert!(!result.to_string().contains(PASSWORD));
        }
    }

    #[tokio::test]
    async fn invalid_payloads_fail_before_the_admin_with_their_own_code() {
        let admin = RecordingAdmin::default();
        let cases = [
            (
                CommandType::CreateValkey,
                json!({ "valkey_id": ID, "mode": "volatile", "maxmemory_mb": 64, "password": PASSWORD }),
                error_codes::VALKEY_MODE_INVALID,
            ),
            (
                CommandType::CreateValkey,
                json!({ "valkey_id": ID, "mode": "cache", "maxmemory_mb": 1024, "password": PASSWORD }),
                error_codes::VALKEY_MEMORY_INVALID,
            ),
            (
                CommandType::RotateValkeyPassword,
                json!({ "valkey_id": ID, "password": "too-short" }),
                error_codes::VALKEY_PASSWORD_INVALID,
            ),
            (
                CommandType::ResizeValkey,
                json!({ "valkey_id": ID, "maxmemory_mb": 8 }),
                error_codes::VALKEY_MEMORY_INVALID,
            ),
            (
                CommandType::LockValkey,
                Value::Null,
                "invalid_runtime_command",
            ),
            (
                CommandType::DeleteValkey,
                json!({ "valkey_id": "store-1" }),
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
            CommandType::RotateValkeyPassword,
            json!({ "valkey_id": ID, "password": "short-but-secret" }),
        )
        .await
        .expect_err("invalid");
        assert!(!error.to_string().contains("short-but-secret"));
    }

    #[tokio::test]
    async fn a_node_without_a_valkey_admin_refuses_valkey_commands() {
        let error = handle(
            &command(CommandType::LockValkey, json!({ "valkey_id": ID })),
            None,
        )
        .await
        .expect_err("unsupported");
        assert_eq!(error.code(), "unsupported_runtime_command");
    }

    #[tokio::test]
    async fn the_failure_fixture_holds_what_the_handler_sends() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../examples/failures/valkey-failures.json"
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
                CommandType::CreateValkey,
                json!({ "valkey_id": ID, "mode": "volatile", "maxmemory_mb": 64, "password": PASSWORD }),
            ),
            (
                CommandType::ResizeValkey,
                json!({ "valkey_id": ID, "maxmemory_mb": 1024 }),
            ),
            (
                CommandType::RotateValkeyPassword,
                json!({ "valkey_id": ID, "password": "too-short" }),
            ),
        ];
        for (command_type, payload) in cases {
            let error = run(&admin, command_type, payload)
                .await
                .expect_err("invalid");
            assert_eq!(error.to_string(), message(error.code()), "{command_type:?}");
        }
        let unsupported = handle(
            &command(CommandType::LockValkey, json!({ "valkey_id": ID })),
            None,
        )
        .await
        .expect_err("unsupported");
        assert_eq!(unsupported.to_string(), message(unsupported.code()));
    }
}
