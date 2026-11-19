//! Shared protocol types between the Sakala control plane and runtime agent.

/// Wire-contract revision implemented by this agent build.
///
/// This is intentionally independent from the crate semantic version so the
/// control plane can reject an incompatible agent before assigning work.
pub const PROTOCOL_VERSION: u32 = 4;

pub mod commands;
pub mod databases;
pub mod deployments;
pub mod events;
pub mod heartbeat;
pub mod inspections;
pub mod logs;
pub mod node;
pub mod placement;
pub mod repositories;
pub mod status;

pub use commands::{
    AgentCommand, CleanupRuntimePayload, CommandPoll, CommandType, CompleteCommandPayload,
    DesiredWorkloadState, ReconcileWorkloadAction, ReconcileWorkloadPayload, RuntimeCleanupTarget,
    SleepProjectPayload, SleepingPage, StopProjectPayload,
};
pub use databases::{
    CreateDatabasePayload, DatabaseEndpoint, DatabaseExtension, DatabasePassword,
    DatabasePayloadError, DatabaseTargetPayload, DropDatabasePayload,
    InstallDatabaseExtensionPayload, MANAGED_NAME_PATTERN, MANAGED_POSTGRES_CAPABILITY,
    ManagedDatabaseSize, ManagedName, RotateDatabasePasswordPayload,
};
pub use deployments::{
    AppliedRuntimeResources, DeployProjectPayload, DeployProjectResult, DeploymentBuilder,
    FinalizationDeferredReason, IMAGE_REUSE_CAPABILITY, IMAGE_REUSE_SKIPPED_EVENT,
    IMAGE_REUSED_EVENT, ImageReuseFallbackReason, LogBounds, RequestedImageReuse,
    RequestedReplicas, RuntimeResourceLimits, RuntimeTimeoutLimits,
};
pub use events::{DeploymentEvent, DeploymentEventLevel};
pub use heartbeat::{
    CrashedWorkloadDetail, HeartbeatPayload, WorkloadMetrics, WorkloadReplicas, WorkloadTraffic,
};
pub use inspections::{InspectProjectPayload, JsPackageManager, JsWorkspace, ProjectInspection};
pub use logs::{DeploymentLog, LogStream};
pub use node::{DesiredNodeLifecycleState, NodeInfo, NodeLifecyclePayload};
pub use placement::{
    EDGE_ADDRESS_MAX_LENGTH, EDGE_ADDRESS_MAX_PORT, EdgeAddress, InvalidEdgeAddress, InvalidRegion,
    NodePlacement, REGION_MAX_LENGTH, REGION_PATTERN, Region,
};
pub use repositories::RepositoryAccess;
pub use status::{CommandStatus, NodeStatus};
