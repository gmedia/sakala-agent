//! Core workers and control-plane API integration for the Sakala agent.

pub mod api;
pub mod commands;
pub mod config;
pub mod error;
pub mod health;
pub mod heartbeat;
pub mod logs;
pub mod node_state;
pub mod ports;
mod reporting;
pub mod repositories;
pub mod scheduler;
pub mod support;

pub use config::{AgentConfig, AgentMode};
pub use error::CoreError;
pub use node_state::{NodeLifecycle, NodeLifecycleState};
