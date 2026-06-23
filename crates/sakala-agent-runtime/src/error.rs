use thiserror::Error;

use sakala_agent_core::ports::RuntimeExecutionError;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid runtime configuration: {0}")]
    Configuration(String),
    #[error("invalid runtime command: {0}")]
    InvalidCommand(String),
    #[error("runtime dependency failed: {0}")]
    Dependency(String),
    #[error("runtime operation failed: {0}")]
    Execution(String),
    #[error("runtime reporting failed: {0}")]
    Reporting(String),
    #[error("runtime filesystem operation failed: {0}")]
    Filesystem(#[from] std::io::Error),
}

impl RuntimeError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "invalid_runtime_configuration",
            Self::InvalidCommand(_) => "invalid_runtime_command",
            Self::Dependency(_) => "runtime_dependency_failed",
            Self::Execution(_) => "runtime_execution_failed",
            Self::Reporting(_) => "runtime_reporting_failed",
            Self::Filesystem(_) => "runtime_filesystem_failed",
        }
    }
}

impl From<RuntimeError> for RuntimeExecutionError {
    fn from(error: RuntimeError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

impl From<RuntimeExecutionError> for RuntimeError {
    fn from(error: RuntimeExecutionError) -> Self {
        Self::Reporting(error.to_string())
    }
}
