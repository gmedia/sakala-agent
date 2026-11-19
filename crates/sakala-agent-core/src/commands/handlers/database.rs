//! Managed PostgreSQL commands. Payloads are validated here, before the admin
//! port sees them, and results are typed, so a password never reaches the
//! command result.

use sakala_agent_protocol::{AgentCommand, CommandType, DatabasePayloadError};
use serde::Serialize;

use crate::ports::{CommandOutput, DatabaseAdmin, RuntimeExecutionError};

/// Runs one database command. A node without a database admin refuses every
/// database command with `unsupported_runtime_command`.
pub async fn handle(
    command: &AgentCommand,
    admin: Option<&dyn DatabaseAdmin>,
) -> Result<CommandOutput, RuntimeExecutionError> {
    let Some(admin) = admin else {
        return Err(RuntimeExecutionError::unsupported_command(format!(
            "{:?} needs a node with SAKALA_DATABASE_DRIVER=postgres",
            command.command_type
        )));
    };
    match command.command_type {
        CommandType::CreateDatabase => {
            let payload = command
                .create_database_payload()
                .map_err(invalid(command))?;
            output(admin.create_database(&payload).await?)
        }
        CommandType::DropDatabase => {
            let payload = command.drop_database_payload().map_err(invalid(command))?;
            output(admin.drop_database(&payload).await?)
        }
        CommandType::RotateDatabasePassword => {
            let payload = command
                .rotate_database_password_payload()
                .map_err(invalid(command))?;
            output(admin.rotate_password(&payload).await?)
        }
        CommandType::LockDatabase => {
            let payload = command
                .database_target_payload()
                .map_err(invalid(command))?;
            output(admin.lock_database(&payload).await?)
        }
        CommandType::UnlockDatabase => {
            let payload = command
                .database_target_payload()
                .map_err(invalid(command))?;
            output(admin.unlock_database(&payload).await?)
        }
        CommandType::InstallDatabaseExtension => {
            let payload = command
                .install_database_extension_payload()
                .map_err(invalid(command))?;
            output(admin.install_extension(&payload).await?)
        }
        other => Err(RuntimeExecutionError::invalid_command(format!(
            "{other:?} is not a database command"
        ))),
    }
}

fn invalid(command: &AgentCommand) -> impl Fn(DatabasePayloadError) -> RuntimeExecutionError {
    let command_type = command.command_type;
    move |error| RuntimeExecutionError::new(error.code(), format!("{command_type:?} {error}"))
}

fn output(result: impl Serialize) -> Result<CommandOutput, RuntimeExecutionError> {
    serde_json::to_value(result)
        .map(CommandOutput::with_result)
        .map_err(|error| {
            RuntimeExecutionError::new(
                "runtime_execution_failed",
                format!("database command result could not be serialized: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sakala_agent_protocol::{
        CommandStatus, CreateDatabasePayload, DatabaseTargetPayload, DropDatabasePayload,
        InstallDatabaseExtensionPayload, ManagedDatabaseSize, ManagedName,
        RotateDatabasePasswordPayload,
        databases::{
            CreateDatabaseResult, DatabaseLockResult, DropDatabaseResult,
            InstallDatabaseExtensionResult, RotateDatabasePasswordResult, error_codes,
        },
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const DATABASE: &str = "p_0123456789abcdef";
    const ROLE: &str = "p_fedcba9876543210";
    const PASSWORD: &str = "c2VjcmV0LXBhc3N3b3JkLXZhbHVlLTEyMzQ1";

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

    fn name(value: &str) -> ManagedName {
        ManagedName::parse(value).expect("name")
    }

    #[async_trait]
    impl DatabaseAdmin for RecordingAdmin {
        async fn check(&self) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        async fn create_database(
            &self,
            payload: &CreateDatabasePayload,
        ) -> Result<CreateDatabaseResult, RuntimeExecutionError> {
            self.record(format!(
                "create {} {} {:?}",
                payload.database, payload.role, payload.password
            ));
            let mut existing = self.existing.lock().expect("lock");
            let created = !*existing;
            *existing = true;
            Ok(CreateDatabaseResult {
                database: payload.database.clone(),
                role: payload.role.clone(),
                created,
            })
        }

        async fn drop_database(
            &self,
            payload: &DropDatabasePayload,
        ) -> Result<DropDatabaseResult, RuntimeExecutionError> {
            self.record(format!("drop {} {}", payload.database, payload.role));
            let dropped = std::mem::replace(&mut *self.existing.lock().expect("lock"), false);
            Ok(DropDatabaseResult {
                database: payload.database.clone(),
                role: payload.role.clone(),
                dropped,
            })
        }

        async fn rotate_password(
            &self,
            payload: &RotateDatabasePasswordPayload,
        ) -> Result<RotateDatabasePasswordResult, RuntimeExecutionError> {
            self.record(format!("rotate {}", payload.role));
            Ok(RotateDatabasePasswordResult {
                role: payload.role.clone(),
            })
        }

        async fn lock_database(
            &self,
            payload: &DatabaseTargetPayload,
        ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
            self.record(format!("lock {}", payload.database));
            Ok(DatabaseLockResult {
                database: payload.database.clone(),
                role: name(ROLE),
                locked: true,
                terminated_connections: 2,
            })
        }

        async fn unlock_database(
            &self,
            payload: &DatabaseTargetPayload,
        ) -> Result<DatabaseLockResult, RuntimeExecutionError> {
            self.record(format!("unlock {}", payload.database));
            Ok(DatabaseLockResult {
                database: payload.database.clone(),
                role: name(ROLE),
                locked: false,
                terminated_connections: 0,
            })
        }

        async fn install_extension(
            &self,
            payload: &InstallDatabaseExtensionPayload,
        ) -> Result<InstallDatabaseExtensionResult, RuntimeExecutionError> {
            self.record(format!(
                "extension {} {}",
                payload.database, payload.extension
            ));
            Ok(InstallDatabaseExtensionResult {
                database: payload.database.clone(),
                extension: payload.extension,
                created: true,
            })
        }

        async fn database_sizes(&self) -> Result<Vec<ManagedDatabaseSize>, RuntimeExecutionError> {
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
    async fn every_database_command_reaches_the_admin_and_returns_a_typed_result() {
        let admin = RecordingAdmin::default();
        let create = json!({ "database": DATABASE, "role": ROLE, "password": PASSWORD });

        let first = run(&admin, CommandType::CreateDatabase, create.clone())
            .await
            .expect("create");
        let again = run(&admin, CommandType::CreateDatabase, create)
            .await
            .expect("create again");
        let rotate = run(
            &admin,
            CommandType::RotateDatabasePassword,
            json!({ "role": ROLE, "password": PASSWORD }),
        )
        .await
        .expect("rotate");
        let lock = run(
            &admin,
            CommandType::LockDatabase,
            json!({ "database": DATABASE }),
        )
        .await
        .expect("lock");
        let unlock = run(
            &admin,
            CommandType::UnlockDatabase,
            json!({ "database": DATABASE }),
        )
        .await
        .expect("unlock");
        let extension = run(
            &admin,
            CommandType::InstallDatabaseExtension,
            json!({ "database": DATABASE, "extension": "pgvector" }),
        )
        .await
        .expect("extension");
        let drop = run(
            &admin,
            CommandType::DropDatabase,
            json!({ "database": DATABASE, "role": ROLE }),
        )
        .await
        .expect("drop");
        let drop_again = run(
            &admin,
            CommandType::DropDatabase,
            json!({ "database": DATABASE, "role": ROLE }),
        )
        .await
        .expect("drop again");

        assert_eq!(
            first,
            json!({ "database": DATABASE, "role": ROLE, "created": true })
        );
        assert_eq!(
            again,
            json!({ "database": DATABASE, "role": ROLE, "created": false })
        );
        assert_eq!(rotate, json!({ "role": ROLE }));
        assert_eq!(
            lock,
            json!({ "database": DATABASE, "role": ROLE, "locked": true, "terminated_connections": 2 })
        );
        assert_eq!(
            unlock,
            json!({ "database": DATABASE, "role": ROLE, "locked": false, "terminated_connections": 0 })
        );
        assert_eq!(
            extension,
            json!({ "database": DATABASE, "extension": "vector", "created": true })
        );
        assert_eq!(
            drop,
            json!({ "database": DATABASE, "role": ROLE, "dropped": true })
        );
        assert_eq!(
            drop_again,
            json!({ "database": DATABASE, "role": ROLE, "dropped": false })
        );

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
                CommandType::CreateDatabase,
                json!({ "database": "postgres", "role": ROLE, "password": PASSWORD }),
                error_codes::DATABASE_NAME_INVALID,
            ),
            (
                CommandType::DropDatabase,
                json!({ "database": DATABASE, "role": "p_x\"; DROP ROLE postgres; --" }),
                error_codes::DATABASE_NAME_INVALID,
            ),
            (
                CommandType::RotateDatabasePassword,
                json!({ "role": ROLE, "password": "too-short" }),
                error_codes::DATABASE_PASSWORD_INVALID,
            ),
            (
                CommandType::InstallDatabaseExtension,
                json!({ "database": DATABASE, "extension": "plpython3u" }),
                error_codes::DATABASE_EXTENSION_NOT_ALLOWED,
            ),
            (
                CommandType::LockDatabase,
                Value::Null,
                "invalid_runtime_command",
            ),
            (
                CommandType::UnlockDatabase,
                json!({}),
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
            CommandType::RotateDatabasePassword,
            json!({ "role": ROLE, "password": "short-but-secret" }),
        )
        .await
        .expect_err("invalid");
        assert!(!error.to_string().contains("short-but-secret"));
    }

    #[tokio::test]
    async fn a_node_without_a_database_admin_refuses_database_commands() {
        let error = handle(
            &command(CommandType::LockDatabase, json!({ "database": DATABASE })),
            None,
        )
        .await
        .expect_err("unsupported");
        assert_eq!(error.code(), "unsupported_runtime_command");
    }
}
