use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BucketKeyPayload, BucketPayloadError, CommandStatus, CreateBucketPayload,
    CreateDatabasePayload, DatabasePayloadError, DatabaseTargetPayload, DeployProjectPayload,
    DropDatabasePayload, InspectProjectPayload, InstallDatabaseExtensionPayload,
    RotateBucketKeyPayload, RotateDatabasePasswordPayload, RunJobPayload, SetBucketQuotaPayload,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredWorkloadState {
    Running,
    Stopped,
    Missing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileWorkloadAction {
    RestartLogFollower,
    CleanupFailedCandidate,
    RestoreRoute,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcileWorkloadPayload {
    pub desired_state: DesiredWorkloadState,
    #[serde(default)]
    pub actions: Vec<ReconcileWorkloadAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCleanupTarget {
    StaleWorkspaces,
    StaleImages,
    StaleRoutes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CleanupRuntimePayload {
    /// Explicit destructive-action gate supplied by the control plane.
    pub approved: bool,
    pub targets: Vec<RuntimeCleanupTarget>,
}

/// Optional payload of `StopProject`. An empty object keeps the images.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StopProjectPayload {
    /// Set by the control plane when the project was deleted: after its
    /// containers are gone, every image of the project is removed too.
    #[serde(default)]
    pub remove_images: bool,
}

/// The page a sleeping project's domain shows.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SleepingPage {
    /// "Proyek sedang dinyalakan": asks the control plane to wake the project
    /// and reloads until it is back. Sleep for idleness.
    #[default]
    Waking,
    /// "Proyek tidak tersedia": a static page that never asks to wake. For a
    /// project that will not wake on a visit, such as a suspended one.
    Unavailable,
}

/// Optional payload of `SleepProject`. An empty object shows the waking page.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SleepProjectPayload {
    #[serde(default)]
    pub page: SleepingPage,
}

/// Operation requested by the control plane for execution on a runtime node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum CommandType {
    InspectProject,
    DeployProject,
    RestartProject,
    StopProject,
    SleepProject,
    WakeProject,
    HealthCheck,
    RefreshRoute,
    ReconcileWorkload,
    CleanupRuntime,
    DrainNode,
    ResumeNode,
    /// Managed PostgreSQL, only on nodes that advertise `managed-postgres`.
    CreateDatabase,
    DropDatabase,
    RotateDatabasePassword,
    LockDatabase,
    UnlockDatabase,
    InstallDatabaseExtension,
    /// Managed object storage, only on nodes that advertise `managed-s3`.
    CreateBucket,
    DeleteBucket,
    RotateBucketKey,
    SetBucketQuota,
    LockBucket,
    UnlockBucket,
    /// One run of a project command in a container of the live image, only on
    /// nodes that advertise `scheduled-jobs`.
    RunJob,
}

/// Command record returned by the control-plane polling endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentCommand {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub command_type: CommandType,
    pub status: CommandStatus,
    pub project_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    #[serde(default)]
    pub payload: Value,
}

impl AgentCommand {
    pub fn deploy_payload(&self) -> Result<DeployProjectPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn run_job_payload(&self) -> Result<RunJobPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn inspect_payload(&self) -> Result<InspectProjectPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn reconcile_workload_payload(
        &self,
    ) -> Result<ReconcileWorkloadPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    pub fn cleanup_runtime_payload(&self) -> Result<CleanupRuntimePayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }

    /// A missing or `null` payload reads as an empty object.
    pub fn stop_project_payload(&self) -> Result<StopProjectPayload, serde_json::Error> {
        if self.payload.is_null() {
            return Ok(StopProjectPayload::default());
        }
        serde_json::from_value(self.payload.clone())
    }

    pub fn create_database_payload(&self) -> Result<CreateDatabasePayload, DatabasePayloadError> {
        CreateDatabasePayload::from_value(&self.payload)
    }

    pub fn drop_database_payload(&self) -> Result<DropDatabasePayload, DatabasePayloadError> {
        DropDatabasePayload::from_value(&self.payload)
    }

    pub fn rotate_database_password_payload(
        &self,
    ) -> Result<RotateDatabasePasswordPayload, DatabasePayloadError> {
        RotateDatabasePasswordPayload::from_value(&self.payload)
    }

    /// The payload of `LockDatabase` and `UnlockDatabase`.
    pub fn database_target_payload(&self) -> Result<DatabaseTargetPayload, DatabasePayloadError> {
        DatabaseTargetPayload::from_value(&self.payload)
    }

    pub fn install_database_extension_payload(
        &self,
    ) -> Result<InstallDatabaseExtensionPayload, DatabasePayloadError> {
        InstallDatabaseExtensionPayload::from_value(&self.payload)
    }

    pub fn create_bucket_payload(&self) -> Result<CreateBucketPayload, BucketPayloadError> {
        CreateBucketPayload::from_value(&self.payload)
    }

    /// The payload of `DeleteBucket`, `LockBucket`, and `UnlockBucket`.
    pub fn bucket_key_payload(&self) -> Result<BucketKeyPayload, BucketPayloadError> {
        BucketKeyPayload::from_value(&self.payload)
    }

    pub fn rotate_bucket_key_payload(&self) -> Result<RotateBucketKeyPayload, BucketPayloadError> {
        RotateBucketKeyPayload::from_value(&self.payload)
    }

    pub fn set_bucket_quota_payload(&self) -> Result<SetBucketQuotaPayload, BucketPayloadError> {
        SetBucketQuotaPayload::from_value(&self.payload)
    }

    /// A missing or `null` payload reads as an empty object. An unknown `page`
    /// is an error here; the dispatcher falls back to the waking page instead
    /// of refusing the sleep.
    pub fn sleep_project_payload(&self) -> Result<SleepProjectPayload, serde_json::Error> {
        if self.payload.is_null() {
            return Ok(SleepProjectPayload::default());
        }
        serde_json::from_value(self.payload.clone())
    }
}

/// Response of the control-plane polling endpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommandPoll {
    pub data: Vec<AgentCommand>,
    /// Commands of this node the owner asked to cancel. Repeated on every poll
    /// until they are terminal, and absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cancel_requested: Vec<Uuid>,
}

/// Body sent when an agent completes a command successfully.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CompleteCommandPayload {
    #[serde(default)]
    pub result: Value,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stop(payload: Value) -> AgentCommand {
        command(CommandType::StopProject, payload)
    }

    fn command(command_type: CommandType, payload: Value) -> AgentCommand {
        AgentCommand {
            id: Uuid::nil(),
            command_type,
            status: CommandStatus::Pending,
            project_id: Some(Uuid::nil()),
            deployment_id: Some(Uuid::nil()),
            payload,
        }
    }

    #[test]
    fn stop_keeps_images_unless_the_control_plane_asks_otherwise() {
        for payload in [json!({}), Value::Null, json!({ "remove_images": false })] {
            assert!(
                !stop(payload)
                    .stop_project_payload()
                    .expect("payload")
                    .remove_images
            );
        }
        assert!(
            stop(json!({ "remove_images": true }))
                .stop_project_payload()
                .expect("payload")
                .remove_images
        );
        assert!(
            stop(json!({ "remove_images": "yes" }))
                .stop_project_payload()
                .is_err()
        );
    }

    #[test]
    fn sleep_shows_the_waking_page_unless_the_control_plane_asks_otherwise() {
        let page = |payload: Value| {
            command(CommandType::SleepProject, payload)
                .sleep_project_payload()
                .map(|payload| payload.page)
        };
        for payload in [
            Value::Null,
            json!({}),
            json!({ "page": "waking" }),
            json!({ "remove_images": true }),
        ] {
            assert_eq!(page(payload).expect("payload"), SleepingPage::Waking);
        }
        assert_eq!(
            page(json!({ "page": "unavailable" })).expect("payload"),
            SleepingPage::Unavailable
        );
        for payload in [
            json!({ "page": "suspended" }),
            json!({ "page": "Unavailable" }),
            json!({ "page": 1 }),
            json!({ "page": null }),
            json!("unavailable"),
        ] {
            assert!(page(payload.clone()).is_err(), "{payload}");
        }
    }

    #[test]
    fn the_sleeping_page_has_a_snake_case_wire_name() {
        assert_eq!(
            serde_json::to_value(SleepProjectPayload {
                page: SleepingPage::Unavailable
            })
            .expect("serialize"),
            json!({ "page": "unavailable" })
        );
    }
}
