//! The database host role (ADR-020): administers one local PostgreSQL instance
//! for managed tenant databases.

mod postgres;
pub mod sql;

pub use postgres::{
    DATABASE_ADMIN_DSN_ENV, MIN_SERVER_VERSION_NUM, PostgresAdminConfig, PostgresDatabaseAdmin,
};
