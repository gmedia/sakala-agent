//! Shared protocol types between the Sakala control plane and runtime agent.

/// Wire-contract revision implemented by this agent build.
///
/// This is intentionally independent from the crate semantic version so the
/// control plane can reject an incompatible agent before assigning work.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod commands;
pub mod deployments;
pub mod events;
pub mod heartbeat;
pub mod inspections;
pub mod logs;
pub mod node;
pub mod repositories;
pub mod status;

pub use commands::{AgentCommand, CommandType, CompleteCommandPayload};
pub use deployments::{
    AppliedRuntimeResources, DeployProjectPayload, DeployProjectResult, DeploymentBuilder,
    LogBounds, RuntimeResourceLimits, RuntimeTimeoutLimits,
};
pub use events::{DeploymentEvent, DeploymentEventLevel};
pub use heartbeat::HeartbeatPayload;
pub use inspections::{InspectProjectPayload, ProjectInspection};
pub use logs::{DeploymentLog, LogStream};
pub use node::NodeInfo;
pub use repositories::RepositoryAccess;
pub use status::{CommandStatus, NodeStatus};
