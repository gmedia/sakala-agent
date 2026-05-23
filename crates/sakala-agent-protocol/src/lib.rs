//! Shared protocol types between the Sakala dashboard and runtime agent.

pub mod commands;
pub mod deployments;
pub mod heartbeat;
pub mod logs;
pub mod node;

pub use commands::{AgentCommand, CommandStatus, CommandType};
pub use deployments::{DeploymentEvent, DeploymentEventLevel};
pub use heartbeat::HeartbeatPayload;
pub use logs::{DeploymentLog, LogStream};
pub use node::{NodeInfo, NodeStatus};
