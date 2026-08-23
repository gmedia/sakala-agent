//! Runtime execution abstraction for Sakala.

pub mod builders;
pub mod config;
pub mod containers;
pub mod error;
pub mod executor;
pub mod health;
mod inspections;
pub mod logs;
pub mod process;
mod railpack;
pub mod routing;
pub mod workspace;

pub use config::{DockerRuntimeConfig, TimeoutSafetyConfig};
pub use containers::ResourceSafetyConfig;
pub use error::RuntimeError;
pub use executor::{DockerRuntimeExecutor, NoopRuntimeExecutor};
pub use process::{
    CommandSpec, NullOutputSink, ProcessOutput, ProcessOutputSink, ProcessRunner, ProcessStream,
    TokioProcessRunner,
};

pub(crate) use sakala_agent_core::ports::RuntimeReporter;
