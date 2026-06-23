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
    #[error("runtime build failed: {0}")]
    Build(String),
    #[error("runtime container operation failed: {0}")]
    Container(String),
    #[error("runtime health check failed: {0}")]
    Health(String),
    #[error("runtime routing failed: {0}")]
    Routing(String),
    #[error("runtime capacity exceeded: {0}")]
    Capacity(String),
    #[error("runtime operation {operation} exceeded its {seconds}s timeout")]
    Timeout { operation: String, seconds: u64 },
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
            Self::Build(_) => "runtime_build_failed",
            Self::Container(_) => "runtime_container_failed",
            Self::Health(_) => "runtime_health_check_failed",
            Self::Routing(_) => "runtime_routing_failed",
            Self::Capacity(_) => "runtime_capacity_exceeded",
            Self::Timeout { .. } => "runtime_timeout",
            Self::Reporting(_) => "runtime_reporting_failed",
            Self::Filesystem(_) => "runtime_filesystem_failed",
        }
    }

    #[must_use]
    pub fn failed_operation(phase: &str, status: Option<i32>) -> Self {
        let summary = format!("{phase} exited with status {status:?}");
        if phase.contains("build") || phase.starts_with("railpack-") {
            Self::Build(summary)
        } else if phase.starts_with("docker-") {
            Self::Container(summary)
        } else if phase.starts_with("caddy-") {
            Self::Routing(summary)
        } else {
            Self::Execution(summary)
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
