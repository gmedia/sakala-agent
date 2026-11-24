//! Shared protocol types between the Sakala control plane and runtime agent.

/// Wire-contract revision implemented by this agent build.
///
/// This is intentionally independent from the crate semantic version so the
/// control plane can reject an incompatible agent before assigning work.
pub const PROTOCOL_VERSION: u32 = 4;

pub mod buckets;
pub mod commands;
pub mod databases;
pub mod deployments;
pub mod events;
pub mod heartbeat;
pub mod inspections;
pub mod jobs;
pub mod logs;
pub mod node;
pub mod placement;
pub mod processes;
pub mod repositories;
pub mod status;

pub use buckets::{
    AccessKeyId, BUCKET_NAME_PATTERN, BucketKeyPayload, BucketName, BucketPayloadError,
    BucketQuotaMb, CreateBucketPayload, DeleteBucketPayload, MANAGED_S3_CAPABILITY,
    ManagedBucketUsage, ObjectStorageMetadata, RotateBucketKeyPayload, SecretAccessKey,
    SetBucketQuotaPayload,
};
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
    IMAGE_REUSED_EVENT, IMAGE_ROLLBACK_CAPABILITY, ImageReuseFallbackReason, LogBounds,
    RequestedImageReuse, RequestedReplicas, RuntimeResourceLimits, RuntimeTimeoutLimits,
};
pub use events::{DeploymentEvent, DeploymentEventLevel};
pub use heartbeat::{
    CrashedWorkloadDetail, HeartbeatPayload, WorkloadMetrics, WorkloadReplicas, WorkloadTraffic,
};
pub use inspections::{
    DatabaseEngine, DatabaseHint, DatabaseSignal, DatabaseSignalKind, EnvExample, EnvExampleEntry,
    EnvExampleFile, InspectProjectPayload, JsFramework, JsPackageManager, JsWorkspace,
    ProjectInspection, SuggestedReleaseCommand,
};
pub use jobs::{
    DEFAULT_JOB_TIMEOUT_SECONDS, JOB_FINISHED_EVENT, JOB_STARTED_EVENT, MAX_JOB_TIMEOUT_SECONDS,
    RunJobPayload, RunJobResult, SCHEDULED_JOBS_CAPABILITY,
};
pub use logs::{DeploymentLog, LogStream};
pub use node::{DesiredNodeLifecycleState, NodeInfo, NodeLifecyclePayload};
pub use placement::{
    EDGE_ADDRESS_MAX_LENGTH, EDGE_ADDRESS_MAX_PORT, EdgeAddress, InvalidEdgeAddress, InvalidRegion,
    NodePlacement, REGION_MAX_LENGTH, REGION_PATTERN, Region,
};
pub use processes::{
    JOB_PROCESS, MAX_PROCESS_NAME_LENGTH, MAX_WORKER_PROCESSES, PROCESSES_IGNORED_EVENT,
    RELEASE_PROCESS, RESERVED_PROCESS_NAMES, RequestedProcesses, WEB_PROCESS,
    WORKER_PROCESSES_CAPABILITY, WorkerProcess, is_valid_process_name,
};
pub use repositories::RepositoryAccess;
pub use status::{CommandStatus, NodeStatus};
