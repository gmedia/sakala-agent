//! Runtime execution abstraction for Sakala.

pub mod caddy;
pub mod docker;
pub mod error;
pub mod executor;
pub mod health;
pub mod logs;
pub mod noop;
pub mod railpack;

pub use error::RuntimeError;
pub use executor::{ExecutionOutcome, RuntimeExecutor};
pub use noop::NoopRuntimeExecutor;
