//! Runtime execution abstraction for Sakala.

pub mod builders;
pub mod config;
pub mod containers;
pub mod database;
pub mod domains;
pub mod error;
pub mod executor;
pub mod health;
mod inspections;
pub mod js_workspace;
pub mod logs;
pub mod object_storage;
pub mod process;
pub mod railpack;
mod repository_file;
pub mod routing;
mod telemetry;
pub mod workspace;

pub use config::{
    DEFAULT_CRASH_STABLE_WINDOW, DEFAULT_MAX_REPLICAS_PER_PROJECT, DEFAULT_RELEASE_COMMAND_TIMEOUT,
    DockerRuntimeConfig, TimeoutSafetyConfig,
};
pub use containers::{MAX_REPLICAS_PER_PROJECT_CEILING, ResourceSafetyConfig};
pub use domains::{DEFAULT_PROJECT_DOMAIN_SUFFIXES, ProjectDomainSuffixes};
pub use error::RuntimeError;
pub use executor::{DockerRuntimeExecutor, NoopRuntimeExecutor};
pub use process::{
    CommandSpec, NullOutputSink, ProcessOutput, ProcessOutputSink, ProcessRunner, ProcessStream,
    TokioProcessRunner,
};

pub(crate) use sakala_agent_core::ports::RuntimeReporter;
