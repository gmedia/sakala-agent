//! Core workers and dashboard integration for the Sakala agent.

pub mod commands;
pub mod config;
pub mod dashboard;
pub mod error;
pub mod heartbeat;
pub mod logs;
pub mod scheduler;
pub mod support;

pub use config::{AgentConfig, AgentMode};
pub use error::CoreError;
