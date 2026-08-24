use std::sync::Arc;

use async_trait::async_trait;
use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEvent, DeploymentLog, DesiredWorkloadState,
    InspectProjectPayload, LogBounds, ReconcileWorkloadAction, RuntimeCleanupTarget,
};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::RepositoryCredential;

#[derive(Clone, Debug)]
pub struct InspectProjectRequest {
    pub command_id: Uuid,
    pub payload: InspectProjectPayload,
    pub repository_credential: Option<RepositoryCredential>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct DeployProjectRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub payload: DeployProjectPayload,
    pub repository_credential: Option<RepositoryCredential>,
    pub cancellation: CancellationToken,
}

/// Lifecycle request identified exclusively by the command record.
#[derive(Clone, Debug)]
pub struct WorkloadLifecycleRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct ReconcileWorkloadRequest {
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub desired_state: DesiredWorkloadState,
    pub actions: Vec<ReconcileWorkloadAction>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct CleanupRuntimeRequest {
    pub command_id: Uuid,
    pub approved: bool,
    pub targets: Vec<RuntimeCleanupTarget>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandOutput {
    pub result: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeOrphan {
    pub container_id: String,
    pub project_id: Option<Uuid>,
    pub reason: String,
}

/// A Sakala-owned route file that no longer maps to any discovered workload.
/// This is a reconciliation finding, not authorization to delete the file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStaleRoute {
    pub path: String,
    pub project_id: Uuid,
    pub deployment_id: Option<Uuid>,
}

/// A dangling Sakala-managed image discovered before an approved/local GC pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStaleImage {
    pub image_id: String,
    pub project_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
}

/// Managed runtime object created by an older Agent whose labels are not
/// sufficient for current recovery/lifecycle behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCompatibilityIssue {
    pub container_id: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeWorkload {
    pub container_id: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub status: String,
}

/// Snapshot kesehatan workload yang dikumpulkan Agent dari runtime lokal.
///
/// Snapshot ini sengaja tidak memakai payload control-plane. Pelaporan ke API
/// akan ditambahkan setelah kontrak health lifecycle disepakati.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeHealthSnapshot {
    pub workload: RuntimeWorkload,
    pub ready: bool,
    pub reason: Option<String>,
}

/// Kapasitas workload yang diketahui runtime lokal saat snapshot diambil.
///
/// Nilai `None` berarti driver tidak dapat menentukan kapasitas dengan aman;
/// control plane tidak boleh menganggapnya sebagai kapasitas tanpa batas.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeCapacity {
    pub active_workloads: Option<usize>,
    pub stopped_workloads: Option<usize>,
    pub maximum_active_workloads: Option<usize>,
    pub active_builds: Option<usize>,
    pub maximum_concurrent_builds: Option<usize>,
}

/// Host/runtime telemetry gathered by the injected runtime adapter.
/// Core serializes this snapshot but never executes host commands directly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeTelemetry {
    pub hostname: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub cpu_total: Option<usize>,
    pub cpu_load_1m: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_available_bytes: Option<u64>,
    pub workspace_used_bytes: Option<u64>,
    pub runtime_operational: Option<bool>,
    pub runtime_dependencies: Value,
}

impl RuntimeCapacity {
    #[must_use]
    pub fn available_workload_slots(&self) -> Option<usize> {
        self.maximum_active_workloads
            .zip(self.active_workloads)
            .map(|(maximum, active)| maximum.saturating_sub(active))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeReconciliationReport {
    pub inspected_containers: usize,
    pub cleaned_workspaces: usize,
    pub reclaimed_image_bytes: u64,
    pub workloads: Vec<RuntimeWorkload>,
    pub orphans: Vec<RuntimeOrphan>,
    pub stale_routes: Vec<RuntimeStaleRoute>,
    pub stale_images: Vec<RuntimeStaleImage>,
    pub reattached_log_followers: usize,
    pub recovered_execution_records: usize,
    pub compatibility_issues: Vec<RuntimeCompatibilityIssue>,
}

/// Result of a local runtime dependency check performed before command polling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimePreflightCheck {
    pub name: String,
    pub fatal: bool,
    pub ready: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimePreflightReport {
    pub checks: Vec<RuntimePreflightCheck>,
}

impl RuntimePreflightReport {
    #[must_use]
    pub fn has_fatal_failure(&self) -> bool {
        self.checks.iter().any(|check| check.fatal && !check.ready)
    }
}

impl CommandOutput {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_result(result: Value) -> Self {
        Self { result }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct RuntimeExecutionError {
    code: String,
    message: String,
}

impl RuntimeExecutionError {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn reporting(message: impl Into<String>) -> Self {
        Self::new("runtime_reporting_failed", message)
    }

    #[must_use]
    pub fn invalid_command(message: impl Into<String>) -> Self {
        Self::new("invalid_runtime_command", message)
    }

    #[must_use]
    pub fn unsupported_command(message: impl Into<String>) -> Self {
        Self::new("unsupported_runtime_command", message)
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

#[async_trait]
pub trait RuntimeReporter: Send + Sync {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError>;
    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError>;

    /// Marks the irreversible runtime cutover. Core uses this signal to avoid
    /// turning an already-live deployment into a timeout failure.
    fn mark_deployment_committed(&self, _output: CommandOutput) {}

    #[must_use]
    fn deployment_committed(&self) -> bool {
        false
    }

    /// Returns the terminal output captured at cutover when finalization must
    /// be deferred to an explicit control-plane repair after its bounded grace.
    #[must_use]
    fn committed_output(&self) -> Option<CommandOutput> {
        None
    }
}

/// Creates a bounded reporter for runtime work recovered after an Agent restart.
///
/// The runtime only receives opaque reporters; API credentials and transport
/// remain owned by core.
pub trait RuntimeReporterFactory: Send + Sync {
    fn reporter(&self, command_id: Uuid, log_bounds: LogBounds) -> Arc<dyn RuntimeReporter>;
}

#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn preflight(&self) -> Result<RuntimePreflightReport, RuntimeExecutionError> {
        Ok(RuntimePreflightReport::default())
    }

    async fn reconcile(&self) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        Ok(RuntimeReconciliationReport::default())
    }

    /// Rebuilds runtime-owned background state after an Agent restart.
    async fn recover(
        &self,
        _reporter_factory: Option<Arc<dyn RuntimeReporterFactory>>,
    ) -> Result<RuntimeReconciliationReport, RuntimeExecutionError> {
        self.reconcile().await
    }

    /// Mengambil kapasitas deployment lokal tanpa mengubah runtime.
    async fn capacity(&self) -> Result<RuntimeCapacity, RuntimeExecutionError> {
        Ok(RuntimeCapacity::default())
    }

    /// Mengambil kesehatan workload aktif tanpa melakukan mutasi runtime.
    async fn health_snapshot(&self) -> Result<Vec<RuntimeHealthSnapshot>, RuntimeExecutionError> {
        Ok(Vec::new())
    }

    async fn node_telemetry(&self) -> Result<NodeTelemetry, RuntimeExecutionError> {
        Ok(NodeTelemetry::default())
    }

    async fn shutdown(&self) -> Result<(), RuntimeExecutionError> {
        Ok(())
    }

    async fn inspect_project(
        &self,
        _request: InspectProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _ = reporter;
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project inspection",
        ))
    }

    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _ = reporter;
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project deployment",
        ))
    }

    async fn restart_project(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project restart",
        ))
    }

    async fn stop_project(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project stop",
        ))
    }

    async fn sleep_project(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project sleep",
        ))
    }

    async fn wake_project(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project wake",
        ))
    }

    async fn health_check(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support explicit health checks",
        ))
    }

    async fn refresh_route(
        &self,
        _request: WorkloadLifecycleRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support route refresh",
        ))
    }

    async fn reconcile_workload(
        &self,
        _request: ReconcileWorkloadRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support workload reconciliation",
        ))
    }

    async fn cleanup_runtime(
        &self,
        _request: CleanupRuntimeRequest,
        _reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support approved cleanup",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeCapacity;

    #[test]
    fn capacity_never_reports_negative_available_slots() {
        assert_eq!(
            RuntimeCapacity {
                active_workloads: Some(4),
                stopped_workloads: None,
                maximum_active_workloads: Some(2),
                active_builds: None,
                maximum_concurrent_builds: None,
            }
            .available_workload_slots(),
            Some(0)
        );
    }
}
