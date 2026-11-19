//! Proves the database host role against a real PostgreSQL: commands go through
//! the same dispatcher the agent uses, and tenants connect over TCP with the
//! passwords the control plane would inject.
//!
//! Opt-in, because it needs a Docker daemon and `postgres:18-alpine` locally:
//!
//! ```bash
//! cargo test -p sakala-agent-runtime --test database_smoke_test -- --ignored --nocapture
//! ```
//!
//! `SAKALA_SMOKE_POSTGRES_IMAGE` replaces the image, for example with
//! `pgvector/pgvector:pg18` to see `vector` installed instead of reported
//! unavailable.
//!
//! It starts one container named `sakala-smdb-postgres`, published on a random
//! loopback port, with `log_statement=all` so the server log can be checked for
//! the plaintext password, and removes it at the end.

use std::{
    process::Command,
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::{
    commands::CommandDispatcher,
    ports::{CommandOutput, DatabaseAdmin, RuntimeExecutionError, RuntimeReporter, SecretString},
};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CommandType, DeploymentEvent, DeploymentLog,
    databases::error_codes,
};
use sakala_agent_runtime::{
    NoopRuntimeExecutor,
    database::{PostgresAdminConfig, PostgresDatabaseAdmin},
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls, error::SqlState};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CONTAINER: &str = "sakala-smdb-postgres";
const DEFAULT_IMAGE: &str = "postgres:18-alpine";
const IMAGE_ENV: &str = "SAKALA_SMOKE_POSTGRES_IMAGE";
const READY_ATTEMPTS: usize = 60;

const DATABASE_A: &str = "p_smokeaaaaaaaaaaaa";
const ROLE_A: &str = "p_smokeroleaaaaaaaa";
const DATABASE_B: &str = "p_smokebbbbbbbbbbbb";
const ROLE_B: &str = "p_smokerolebbbbbbbb";
const OUTSIDER: &str = "smoke_outsider";

// Passwords are random per run, so nothing secret-looking is committed and a
// leftover container from an earlier run cannot be reused by accident.
static ADMIN_PASSWORD: LazyLock<String> = LazyLock::new(random_password);
static PASSWORD_A: LazyLock<String> = LazyLock::new(random_password);
static PASSWORD_A_ROTATED: LazyLock<String> = LazyLock::new(random_password);
static PASSWORD_B: LazyLock<String> = LazyLock::new(random_password);
static OUTSIDER_PASSWORD: LazyLock<String> = LazyLock::new(random_password);

/// 32 hexadecimal characters, inside the agent's accepted password length.
fn random_password() -> String {
    Uuid::new_v4().simple().to_string()
}

struct SilentReporter;

#[async_trait]
impl RuntimeReporter for SilentReporter {
    async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }
}

struct Container;

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", CONTAINER]);
    }
}

fn docker(arguments: &[&str]) -> String {
    let output = Command::new("docker")
        .args(arguments)
        .output()
        .expect("docker runs");
    assert!(
        output.status.success(),
        "docker {}: {}",
        arguments.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn start_postgres() -> (Container, u16) {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
    let password = format!("POSTGRES_PASSWORD={}", ADMIN_PASSWORD.as_str());
    let image = std::env::var(IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    docker(&[
        "run",
        "-d",
        "--name",
        CONTAINER,
        "-e",
        &password,
        "-p",
        "127.0.0.1::5432",
        &image,
        "-c",
        "log_statement=all",
    ]);
    let published = docker(&["port", CONTAINER, "5432/tcp"]);
    let port = published
        .lines()
        .next()
        .and_then(|line| line.rsplit(':').next())
        .and_then(|port| port.parse().ok())
        .expect("published port");
    (Container, port)
}

async fn connect(
    port: u16,
    database: &str,
    user: &str,
    password: &str,
) -> Result<Client, tokio_postgres::Error> {
    let mut config = tokio_postgres::Config::new();
    config
        .host("127.0.0.1")
        .port(port)
        .dbname(database)
        .user(user)
        .password(password)
        .connect_timeout(Duration::from_secs(5));
    let (client, connection) = config.connect(NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn wait_until_ready(port: u16) {
    for _ in 0..READY_ATTEMPTS {
        if connect(port, "postgres", "postgres", ADMIN_PASSWORD.as_str())
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("PostgreSQL did not accept TCP connections in time");
}

async fn dispatch(
    dispatcher: &CommandDispatcher,
    command_type: CommandType,
    payload: Value,
) -> Result<Value, RuntimeExecutionError> {
    dispatcher
        .dispatch(
            &AgentCommand {
                id: Uuid::new_v4(),
                command_type,
                status: CommandStatus::Claimed,
                project_id: None,
                deployment_id: None,
                payload,
            },
            Arc::new(SilentReporter),
            CancellationToken::new(),
        )
        .await
        .map(|CommandOutput { result }| result)
}

fn sql_state(error: &tokio_postgres::Error) -> String {
    error.code().map_or_else(
        || format!("no SQLSTATE ({error})"),
        |code| code.code().to_owned(),
    )
}

async fn show(client: &Client, parameter: &str) -> String {
    client
        .query_one(&format!("SHOW {parameter}"), &[])
        .await
        .expect("show")
        .get(0)
}

#[tokio::test]
#[ignore = "needs Docker and postgres:18-alpine"]
async fn the_database_role_isolates_tenants_on_a_real_postgres() {
    let (_container, port) = start_postgres();
    wait_until_ready(port).await;
    let version: String = connect(port, "postgres", "postgres", ADMIN_PASSWORD.as_str())
        .await
        .expect("admin")
        .query_one("SHOW server_version", &[])
        .await
        .expect("version")
        .get(0);
    println!("PostgreSQL {version} on 127.0.0.1:{port}");

    let admin_config = PostgresAdminConfig::parse(&SecretString::new(format!(
        "postgresql://postgres:{}@127.0.0.1:{port}/postgres",
        ADMIN_PASSWORD.as_str()
    )))
    .expect("admin config");
    let admin = Arc::new(PostgresDatabaseAdmin::new(admin_config));
    admin.check().await.expect("admin check");
    let dispatcher = CommandDispatcher::new(Arc::new(NoopRuntimeExecutor))
        .with_database_admin(Arc::clone(&admin) as Arc<dyn DatabaseAdmin>);

    // Create, then create again: the second run changes nothing.
    let create_a =
        json!({ "database": DATABASE_A, "role": ROLE_A, "password": PASSWORD_A.as_str() });
    let first = dispatch(&dispatcher, CommandType::CreateDatabase, create_a.clone())
        .await
        .expect("create A");
    let again = dispatch(&dispatcher, CommandType::CreateDatabase, create_a)
        .await
        .expect("create A again");
    assert_eq!(
        first,
        json!({ "database": DATABASE_A, "role": ROLE_A, "created": true })
    );
    assert_eq!(
        again,
        json!({ "database": DATABASE_A, "role": ROLE_A, "created": false })
    );
    assert!(!first.to_string().contains(PASSWORD_A.as_str()));
    dispatch(
        &dispatcher,
        CommandType::CreateDatabase,
        json!({ "database": DATABASE_B, "role": ROLE_B, "password": PASSWORD_B.as_str() }),
    )
    .await
    .expect("create B");
    println!("create: first {first}, repeated {again}");

    // The tenant connects and works in its own database.
    let tenant = connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
        .await
        .expect("tenant A connects");
    tenant
        .batch_execute(
            "CREATE TABLE notes (id serial PRIMARY KEY, body text NOT NULL); \
             INSERT INTO notes (body) VALUES ('hello');",
        )
        .await
        .expect("tenant creates a table");
    let notes: i64 = tenant
        .query_one("SELECT count(*) FROM notes", &[])
        .await
        .expect("count")
        .get(0);
    assert_eq!(notes, 1);
    println!("tenant A created a table and read {notes} row back");

    // Role defaults, and the one the tenant cannot raise.
    assert_eq!(show(&tenant, "statement_timeout").await, "1min");
    assert_eq!(
        show(&tenant, "idle_in_transaction_session_timeout").await,
        "1min"
    );
    assert_eq!(show(&tenant, "idle_session_timeout").await, "30min");
    assert_eq!(show(&tenant, "temp_file_limit").await, "1GB");
    let raise = tenant
        .batch_execute("SET temp_file_limit = '10GB'")
        .await
        .expect_err("temp_file_limit is superuser-only");
    assert_eq!(sql_state(&raise), SqlState::INSUFFICIENT_PRIVILEGE.code());
    let timeout = tenant
        .batch_execute("SET statement_timeout = '100ms'; SELECT pg_sleep(1)")
        .await
        .expect_err("statement_timeout applies");
    assert_eq!(sql_state(&timeout), SqlState::QUERY_CANCELED.code());
    tenant
        .batch_execute("RESET statement_timeout")
        .await
        .expect("reset");
    let alter_role = tenant
        .batch_execute(&format!("ALTER ROLE {ROLE_A} CONNECTION LIMIT 50"))
        .await
        .expect_err("tenant cannot raise its role limit");
    assert_eq!(
        sql_state(&alter_role),
        SqlState::INSUFFICIENT_PRIVILEGE.code()
    );
    println!(
        "defaults: statement_timeout 1min, idle_in_transaction 1min, idle_session 30min, temp_file_limit 1GB; raising temp_file_limit and the role limit refused (42501); a 100 ms statement_timeout cancelled pg_sleep (57014)"
    );

    // Isolation: no other tenant database, no maintenance database.
    for database in [DATABASE_B, "postgres", "template1"] {
        let error = connect(port, database, ROLE_A, PASSWORD_A.as_str())
            .await
            .expect_err("tenant A cannot connect elsewhere");
        assert_eq!(
            sql_state(&error),
            SqlState::INSUFFICIENT_PRIVILEGE.code(),
            "{database}"
        );
    }
    println!("tenant A refused on {DATABASE_B}, postgres, template1 (42501)");

    // No CREATE on schema public for anyone but the owner, even a role that
    // was given CONNECT by hand.
    let superuser = connect(port, DATABASE_A, "postgres", ADMIN_PASSWORD.as_str())
        .await
        .expect("admin in A");
    superuser
        .batch_execute(&format!(
            "CREATE ROLE {OUTSIDER} LOGIN PASSWORD '{}'; \
             GRANT CONNECT ON DATABASE {DATABASE_A} TO {OUTSIDER};",
            OUTSIDER_PASSWORD.as_str()
        ))
        .await
        .expect("outsider");
    let outsider = connect(port, DATABASE_A, OUTSIDER, &OUTSIDER_PASSWORD)
        .await
        .expect("outsider connects");
    let create = outsider
        .batch_execute("CREATE TABLE intruder (id int)")
        .await
        .expect_err("no CREATE on public");
    assert_eq!(sql_state(&create), SqlState::INSUFFICIENT_PRIVILEGE.code());
    let read = outsider
        .batch_execute("SELECT * FROM notes")
        .await
        .expect_err("no access to tenant tables");
    assert_eq!(sql_state(&read), SqlState::INSUFFICIENT_PRIVILEGE.code());
    drop(outsider);
    superuser
        .batch_execute(&format!(
            "REVOKE CONNECT ON DATABASE {DATABASE_A} FROM {OUTSIDER}; DROP ROLE {OUTSIDER};"
        ))
        .await
        .expect("drop outsider");
    drop(superuser);
    println!(
        "a role with CONNECT on A could neither CREATE in public nor read the tenant's table (42501)"
    );

    // Connection limit 5. Every session in the database counts, the admin's
    // too, so wait until the closed admin and outsider sessions are gone.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.push(
            connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
                .await
                .expect("session"),
        );
    }
    let sixth = connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
        .await
        .expect_err("sixth session");
    assert_eq!(sql_state(&sixth), SqlState::TOO_MANY_CONNECTIONS.code());
    drop(sessions);
    println!("connection limit: 5 sessions open, the sixth refused (53300)");

    // Sizes from the heartbeat query.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sizes = admin.database_sizes().await.expect("sizes");
    let names: Vec<_> = sizes.iter().map(|size| size.name.as_str()).collect();
    assert_eq!(names, [DATABASE_A, DATABASE_B]);
    assert!(sizes.iter().all(|size| size.size_bytes > 0));
    println!(
        "sizes: {}",
        sizes
            .iter()
            .map(|size| format!("{} {} bytes", size.name, size.size_bytes))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Lock: open sessions end, new ones are refused, the admin still connects.
    let lock = dispatch(
        &dispatcher,
        CommandType::LockDatabase,
        json!({ "database": DATABASE_A }),
    )
    .await
    .expect("lock");
    assert_eq!(lock["locked"], true);
    assert_eq!(lock["role"], ROLE_A);
    assert!(lock["terminated_connections"].as_u64().expect("count") >= 1);
    assert!(
        tenant.batch_execute("SELECT 1").await.is_err(),
        "the open session ended"
    );
    let refused = connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
        .await
        .expect_err("locked");
    assert_eq!(
        sql_state(&refused),
        SqlState::INVALID_AUTHORIZATION_SPECIFICATION.code()
    );
    connect(port, DATABASE_A, "postgres", ADMIN_PASSWORD.as_str())
        .await
        .expect("admin connects to a locked database for backups");
    assert_eq!(admin.database_sizes().await.expect("sizes").len(), 2);
    dispatch(
        &dispatcher,
        CommandType::LockDatabase,
        json!({ "database": DATABASE_A }),
    )
    .await
    .expect("lock again");
    let repeat = dispatch(
        &dispatcher,
        CommandType::CreateDatabase,
        json!({ "database": DATABASE_A, "role": ROLE_A, "password": PASSWORD_A.as_str() }),
    )
    .await
    .expect("create on a locked database");
    assert_eq!(repeat["created"], false);
    assert!(
        connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
            .await
            .is_err(),
        "a repeated create does not unlock"
    );
    println!(
        "lock: {lock}; new sessions refused (28000), admin still connects, a repeated create keeps it locked"
    );

    let unlock = dispatch(
        &dispatcher,
        CommandType::UnlockDatabase,
        json!({ "database": DATABASE_A }),
    )
    .await
    .expect("unlock");
    assert_eq!(unlock["locked"], false);
    connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
        .await
        .expect("tenant connects after unlock");
    println!("unlock: {unlock}; tenant connects again");

    // Rotation: the old password stops working, the new one works.
    let rotate = dispatch(
        &dispatcher,
        CommandType::RotateDatabasePassword,
        json!({ "role": ROLE_A, "password": PASSWORD_A_ROTATED.as_str() }),
    )
    .await
    .expect("rotate");
    assert_eq!(rotate, json!({ "role": ROLE_A }));
    assert!(
        connect(port, DATABASE_A, ROLE_A, PASSWORD_A.as_str())
            .await
            .is_err()
    );
    let tenant = connect(port, DATABASE_A, ROLE_A, PASSWORD_A_ROTATED.as_str())
        .await
        .expect("new password");
    println!("rotate: old password refused, new password accepted");

    // Extensions: allowed and present, allowed but absent, not allowed.
    let pgcrypto = dispatch(
        &dispatcher,
        CommandType::InstallDatabaseExtension,
        json!({ "database": DATABASE_A, "extension": "pgcrypto" }),
    )
    .await
    .expect("pgcrypto");
    assert_eq!(pgcrypto["created"], true);
    let pgcrypto_again = dispatch(
        &dispatcher,
        CommandType::InstallDatabaseExtension,
        json!({ "database": DATABASE_A, "extension": "pgcrypto" }),
    )
    .await
    .expect("pgcrypto again");
    assert_eq!(pgcrypto_again["created"], false);
    let digest: String = tenant
        .query_one("SELECT encode(digest('sakala', 'sha256'), 'hex')", &[])
        .await
        .expect("tenant uses pgcrypto")
        .get(0);
    assert_eq!(digest.len(), 64);
    let vector = dispatch(
        &dispatcher,
        CommandType::InstallDatabaseExtension,
        json!({ "database": DATABASE_A, "extension": "pgvector" }),
    )
    .await;
    let vector_outcome = match &vector {
        Ok(result) => format!("installed {result}"),
        Err(error) => {
            assert_eq!(error.code(), error_codes::DATABASE_EXTENSION_UNAVAILABLE);
            format!("{} ({})", error.code(), error)
        }
    };
    let refused = dispatch(
        &dispatcher,
        CommandType::InstallDatabaseExtension,
        json!({ "database": DATABASE_A, "extension": "plpython3u" }),
    )
    .await
    .expect_err("not on the allow-list");
    assert_eq!(refused.code(), error_codes::DATABASE_EXTENSION_NOT_ALLOWED);
    println!(
        "extensions: pgcrypto created then already present, tenant ran digest(); pgvector: {vector_outcome}; plpython3u: {}",
        refused.code()
    );

    // Names outside the pattern never reach SQL.
    let invalid = dispatch(
        &dispatcher,
        CommandType::DropDatabase,
        json!({ "database": "postgres", "role": ROLE_A }),
    )
    .await
    .expect_err("invalid name");
    assert_eq!(invalid.code(), error_codes::DATABASE_NAME_INVALID);
    let mismatched = dispatch(
        &dispatcher,
        CommandType::DropDatabase,
        json!({ "database": DATABASE_A, "role": ROLE_B }),
    )
    .await
    .expect_err("database of another role");
    assert_eq!(mismatched.code(), error_codes::DATABASE_CONFLICT);
    let missing = dispatch(
        &dispatcher,
        CommandType::LockDatabase,
        json!({ "database": "p_smokemissingmissing" }),
    )
    .await
    .expect_err("missing");
    assert_eq!(missing.code(), error_codes::DATABASE_NOT_FOUND);
    println!(
        "refused: name postgres ({}), A with role B ({}), missing database ({})",
        invalid.code(),
        mismatched.code(),
        missing.code()
    );

    // Drop with a session open, then drop again.
    let drop_a = json!({ "database": DATABASE_A, "role": ROLE_A });
    let dropped = dispatch(&dispatcher, CommandType::DropDatabase, drop_a.clone())
        .await
        .expect("drop A");
    let dropped_again = dispatch(&dispatcher, CommandType::DropDatabase, drop_a)
        .await
        .expect("drop A again");
    assert_eq!(dropped["dropped"], true);
    assert_eq!(dropped_again["dropped"], false);
    let leftovers: i64 = connect(port, "postgres", "postgres", ADMIN_PASSWORD.as_str())
        .await
        .expect("admin")
        .query_one(
            "SELECT (SELECT count(*) FROM pg_database WHERE datname = $1) + \
             (SELECT count(*) FROM pg_roles WHERE rolname = $2)",
            &[&DATABASE_A, &ROLE_A],
        )
        .await
        .expect("leftovers")
        .get(0);
    assert_eq!(leftovers, 0);
    assert!(
        connect(port, DATABASE_A, ROLE_A, PASSWORD_A_ROTATED.as_str())
            .await
            .is_err()
    );
    let remaining = admin.database_sizes().await.expect("sizes");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, DATABASE_B);
    println!("drop: {dropped}, repeated {dropped_again}; database and role gone, B untouched");

    // With log_statement=all, the server log holds the verifier, never a password.
    let log = Command::new("docker")
        .args(["logs", CONTAINER])
        .output()
        .expect("docker logs");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&log.stdout),
        String::from_utf8_lossy(&log.stderr)
    );
    for password in [
        PASSWORD_A.as_str(),
        PASSWORD_A_ROTATED.as_str(),
        PASSWORD_B.as_str(),
    ] {
        assert!(
            !log.contains(password),
            "a tenant password reached the server log"
        );
    }
    assert!(log.contains("SCRAM-SHA-256$4096:"));
    println!("server log with log_statement=all: no tenant password, only SCRAM verifiers");
}
