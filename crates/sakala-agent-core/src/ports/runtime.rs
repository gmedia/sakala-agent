use async_trait::async_trait;
use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEvent, DeploymentLog, InspectProjectPayload,
};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectProjectRequest {
    pub command_id: Uuid,
    pub payload: InspectProjectPayload,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeployProjectRequest {
    pub command_id: Uuid,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub payload: DeployProjectPayload,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandOutput {
    pub result: Value,
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
}

#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn inspect_project(
        &self,
        _request: InspectProjectRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _ = reporter;
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project inspection",
        ))
    }

    async fn deploy_project(
        &self,
        _request: DeployProjectRequest,
        reporter: &dyn RuntimeReporter,
    ) -> Result<CommandOutput, RuntimeExecutionError> {
        let _ = reporter;
        Err(RuntimeExecutionError::unsupported_command(
            "runtime does not support project deployment",
        ))
    }
}
