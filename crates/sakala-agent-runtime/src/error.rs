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
    #[error("runtime repository operation failed: {0}")]
    Repository(String),
    #[error("repository was not found")]
    RepositoryNotFound,
    #[error("repository access was denied")]
    RepositoryAccessDenied,
    #[error("repository authentication failed")]
    RepositoryAuthFailed,
    #[error("repository credential has expired or is no longer valid")]
    RepositoryCredentialExpired,
    #[error("requested repository commit was not found")]
    RepositoryCommitNotFound,
    #[error("repository checkout failed")]
    RepositoryCheckoutFailed,
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
    #[error("runtime disk pressure: {0}")]
    DiskPressure(String),
    #[error("managed workload was not found")]
    WorkloadNotFound,
    #[error("managed workload is not running")]
    WorkloadNotRunning,
    #[error("runtime operation was cancelled")]
    Cancelled,
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
            Self::Repository(_) => "runtime_repository_failed",
            Self::RepositoryNotFound => "repository_not_found",
            Self::RepositoryAccessDenied => "repository_access_denied",
            Self::RepositoryAuthFailed => "repository_auth_failed",
            Self::RepositoryCredentialExpired => "repository_credential_expired",
            Self::RepositoryCommitNotFound => "repository_commit_not_found",
            Self::RepositoryCheckoutFailed => "repository_checkout_failed",
            Self::Execution(_) => "runtime_execution_failed",
            Self::Build(_) => "runtime_build_failed",
            Self::Container(_) => "runtime_container_failed",
            Self::Health(_) => "runtime_health_check_failed",
            Self::Routing(_) => "runtime_routing_failed",
            Self::Capacity(_) => "runtime_capacity_exceeded",
            Self::DiskPressure(_) => "runtime_disk_pressure",
            Self::WorkloadNotFound => "runtime_workload_not_found",
            Self::WorkloadNotRunning => "runtime_workload_not_running",
            Self::Cancelled => "runtime_cancelled",
            Self::Timeout { .. } => "runtime_timeout",
            Self::Reporting(_) => "runtime_reporting_failed",
            Self::Filesystem(_) => "runtime_filesystem_failed",
        }
    }

    #[must_use]
    pub fn failed_operation(phase: &str, status: Option<i32>) -> Self {
        let summary = format!("{phase} exited with status {status:?}");
        if phase.starts_with("git-") {
            Self::Repository(summary)
        } else if phase.contains("build") || phase.starts_with("railpack-") {
            Self::Build(summary)
        } else if phase.starts_with("docker-") {
            Self::Container(summary)
        } else if phase.starts_with("caddy-") {
            Self::Routing(summary)
        } else {
            Self::Execution(summary)
        }
    }

    #[must_use]
    pub fn failed_process(phase: &str, status: Option<i32>, stderr: &str) -> Self {
        if phase.starts_with("git-") {
            return Self::classify_repository_failure(stderr);
        }
        Self::failed_operation(phase, status)
    }

    fn classify_repository_failure(stderr: &str) -> Self {
        let detail = stderr.to_ascii_lowercase();
        if detail.contains("repository not found") {
            Self::RepositoryNotFound
        } else if detail.contains("authentication failed")
            || detail.contains("http basic: access denied")
            || detail.contains("invalid username or token")
        {
            Self::RepositoryAuthFailed
        } else if detail.contains("expired")
            || detail.contains("token has been revoked")
            || detail.contains("token is not valid")
        {
            Self::RepositoryCredentialExpired
        } else if detail.contains("permission denied")
            || detail.contains("could not read from remote repository")
            || detail.contains("access denied")
        {
            Self::RepositoryAccessDenied
        } else if detail.contains("couldn't find remote ref")
            || detail.contains("not our ref")
            || detail.contains("reference is not a tree")
            || detail.contains("unknown revision")
        {
            Self::RepositoryCommitNotFound
        } else {
            Self::RepositoryCheckoutFailed
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

#[cfg(test)]
mod tests {
    use super::RuntimeError;

    #[test]
    fn git_stderr_is_classified_without_becoming_the_reported_message() {
        for (stderr, code) in [
            ("remote: Repository not found.", "repository_not_found"),
            ("fatal: Authentication failed", "repository_auth_failed"),
            ("fatal: token has expired", "repository_credential_expired"),
            (
                "fatal: Could not read from remote repository.",
                "repository_access_denied",
            ),
            (
                "fatal: couldn't find remote ref deadbeef",
                "repository_commit_not_found",
            ),
        ] {
            let error = RuntimeError::failed_process("git-fetch", Some(128), stderr);
            assert_eq!(error.code(), code);
            assert!(!error.to_string().contains(stderr));
        }
    }
}
