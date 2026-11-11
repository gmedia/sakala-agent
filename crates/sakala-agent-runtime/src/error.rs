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
    /// The build could not pull an image because a registry or the network
    /// failed, which nothing in the repository can fix.
    #[error(
        "the build could not pull an image because the registry or the network did not answer: {0}"
    )]
    RegistryUnavailable(String),
    #[error("runtime container operation failed: {0}")]
    Container(String),
    #[error("runtime health check failed: {0}")]
    Health(String),
    #[error(
        "nothing answered on port {port} after waiting {waited_seconds} seconds; the app must listen on 0.0.0.0:$PORT, and Sakala sets PORT to {port}"
    )]
    AppNotListening { port: u16, waited_seconds: u64 },
    #[error(
        "the app on port {port} answered only with server errors for {waited_seconds} seconds; the last status was HTTP {status}"
    )]
    AppUnhealthy {
        port: u16,
        status: u16,
        waited_seconds: u64,
    },
    #[error("the app exited during start with {}", describe_exit_code(*.exit_code))]
    AppCrashed { exit_code: Option<i32> },
    /// The kernel refused to execute a program in the image with EPERM. In a
    /// Sakala container that means a file capability outside the workload
    /// allowlist, such as `cap_net_raw`.
    #[error(
        "the app could not start because Linux refused to run one of its programs (exit code {exit_code}, operation not permitted); the program asks for a Linux capability outside the small set Sakala containers keep, such as NET_RAW, so remove its file capabilities with setcap -r or use an image that does not need them"
    )]
    AppExecDenied { exit_code: i32 },
    #[error("the app ran out of memory during start; {}", describe_memory_limit(*.memory_limit_mb))]
    AppOutOfMemory { memory_limit_mb: Option<u64> },
    #[error("{}", describe_port_conflict(*.environment_port, *.container_port))]
    PortConflict {
        environment_port: Option<u16>,
        container_port: u16,
    },
    #[error(
        "root_directory {reason}; it must be a relative path to a directory inside the repository"
    )]
    RootDirectoryInvalid { reason: &'static str },
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
            Self::RegistryUnavailable(_) => "runtime_registry_unavailable",
            Self::Container(_) => "runtime_container_failed",
            Self::Health(_) => "runtime_health_check_failed",
            Self::AppNotListening { .. } => "runtime_app_not_listening",
            Self::AppUnhealthy { .. } => "runtime_app_unhealthy",
            Self::AppCrashed { .. } => "runtime_app_crashed",
            Self::AppExecDenied { .. } => "runtime_app_exec_denied",
            Self::AppOutOfMemory { .. } => "runtime_app_out_of_memory",
            Self::PortConflict { .. } => "runtime_port_conflict",
            Self::RootDirectoryInvalid { .. } => "runtime_root_directory_invalid",
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
        let summary = format!("{phase} failed with {}", describe_exit(status));
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
        if phase.ends_with("-build") && reports_registry_unavailable(stderr) {
            return Self::RegistryUnavailable(format!(
                "{phase} failed with {}",
                describe_exit(status)
            ));
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

impl RuntimeError {
    /// Turns a crash into [`RuntimeError::AppExecDenied`] when the app's last
    /// output shows a program refused with EPERM. A shell reports that with exit
    /// code 126, and runc with 255 when the image's own command is the program.
    /// The output is the app's own, so a matching line only changes the message
    /// this same deployment gets.
    #[must_use]
    pub fn refine_crash(self, output_reports_exec_denied: bool) -> Self {
        match self {
            Self::AppCrashed {
                exit_code: Some(exit_code @ (126 | 255)),
            } if output_reports_exec_denied => Self::AppExecDenied { exit_code },
            other => other,
        }
    }
}

/// Steps in which BuildKit talks to a registry. A step the repository runs, such
/// as `RUN curl`, never prints these, so its network failures stay build failures.
const REGISTRY_OPERATIONS: [&str; 5] = [
    "failed to resolve source metadata",
    "failed to do request",
    "failed to authorize",
    "failed to fetch",
    "failed to copy:",
];

/// Answers that mean the registry or the network failed, not the reference.
/// `not found`, `pull access denied`, and a bare `no such host` are left out:
/// a mistyped image or registry name produces them, and the user can fix that.
const TRANSPORT_FAILURES: [&str; 16] = [
    "server misbehaving",
    "temporary failure in name resolution",
    "i/o timeout",
    "tls handshake timeout",
    "connection reset by peer",
    "connection refused",
    "network is unreachable",
    "no route to host",
    "context deadline exceeded",
    "unexpected eof",
    "request canceled while waiting for connection",
    "toomanyrequests",
    "429 too many requests",
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
];

/// Whether a build failed while pulling an image because the registry or the
/// network failed. Railpack's own images are the platform's, so any registry
/// failure on them counts too.
#[must_use]
pub fn reports_registry_unavailable(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        REGISTRY_OPERATIONS
            .iter()
            .any(|operation| line.contains(operation))
            && (TRANSPORT_FAILURES
                .iter()
                .any(|failure| line.contains(failure))
                || line.contains("ghcr.io/railwayapp/railpack"))
    })
}

/// Whether the output holds the line a shell or runc prints when `execve` fails
/// with EPERM, for example `exec /usr/bin/caddy: operation not permitted` or
/// `sh: exec: line 0: frankenphp: Operation not permitted`.
#[must_use]
pub fn reports_exec_denied(output: &str) -> bool {
    output.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.ends_with("operation not permitted")
            && (line.starts_with("exec ")
                || line.starts_with("sh: ")
                || line.starts_with("bash: ")
                || line.contains(": exec: ")
                || line.contains(": line "))
    })
}

/// Words for a process's end, because these messages reach the people deploying.
/// A process has no exit code when a signal stopped it.
pub(crate) fn describe_exit(code: Option<i32>) -> String {
    code.map_or_else(
        || "no exit code because a signal stopped it".to_owned(),
        |code| format!("exit code {code}"),
    )
}

fn describe_exit_code(exit_code: Option<i32>) -> String {
    exit_code.map_or_else(
        || "an exit code Docker no longer reports".to_owned(),
        |code| format!("exit code {code}"),
    )
}

fn describe_memory_limit(memory_limit_mb: Option<u64>) -> String {
    memory_limit_mb.map_or_else(
        || "Docker reports no memory limit for it".to_owned(),
        |limit| format!("its memory limit is {limit} MB"),
    )
}

/// Names `PORT` only when it is a real port number. A value that is not one is
/// arbitrary user input, and an environment value must not reach a failure
/// message, so it is described rather than repeated.
fn describe_port_conflict(environment_port: Option<u16>, container_port: u16) -> String {
    match environment_port {
        Some(port) => format!(
            "environment PORT={port} must match container_port {container_port}; remove PORT or set it to {container_port}"
        ),
        None => format!(
            "environment PORT is not a valid port number and must match container_port {container_port}; remove PORT or set it to {container_port}"
        ),
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

    fn assert_reads_as_words(message: &str) {
        for debug_form in ["Some(", "None", "Ok(", "Err(", "{:?}"] {
            assert!(!message.contains(debug_form), "{message}");
        }
    }

    #[test]
    fn a_failed_process_is_described_in_words() {
        for phase in [
            "git-fetch",
            "docker-build",
            "railpack-prepare",
            "docker-run",
            "caddy-validate",
            "runtime",
        ] {
            let failed = RuntimeError::failed_operation(phase, Some(1)).to_string();
            assert!(failed.contains("exit code 1"), "{failed}");
            assert_reads_as_words(&failed);

            let stopped = RuntimeError::failed_operation(phase, None).to_string();
            assert!(stopped.contains("no exit code"), "{stopped}");
            assert_reads_as_words(&stopped);
        }
    }

    #[test]
    fn app_failures_read_as_words_when_a_fact_is_missing() {
        for error in [
            RuntimeError::AppCrashed { exit_code: None },
            RuntimeError::AppOutOfMemory {
                memory_limit_mb: None,
            },
            RuntimeError::PortConflict {
                environment_port: None,
                container_port: 3000,
            },
        ] {
            assert_reads_as_words(&error.to_string());
        }
    }

    /// Every error built from a process status goes through one helper.
    #[test]
    fn no_runtime_source_formats_a_process_status_for_debugging() {
        for source in [
            include_str!("containers/docker.rs"),
            include_str!("executor/docker.rs"),
            include_str!("health/docker.rs"),
            include_str!("workspace/git.rs"),
            include_str!("error.rs"),
        ] {
            let needle = concat!("status {", ":?}");
            assert!(!source.contains(needle));
            let named = concat!("status {status", ":?}");
            assert!(!source.contains(named));
        }
    }

    /// The lines are what Docker 29 logged for an image binary marked
    /// `cap_net_bind_service=ep`, run with `--cap-drop ALL`.
    #[test]
    fn a_program_refused_by_the_kernel_is_not_blamed_on_the_app_code() {
        for (line, exit_code) in [
            ("exec /usr/bin/caddy: operation not permitted", 255),
            ("sh: exec: line 0: caddy: Operation not permitted", 126),
            ("sh: caddy: Operation not permitted", 126),
            (
                "/start-container.sh: line 23: /usr/local/bin/frankenphp: Operation not permitted",
                126,
            ),
            (
                "/usr/local/bin/docker-php-entrypoint: 9: exec: frankenphp: Operation not permitted",
                126,
            ),
        ] {
            let output = format!("Starting Laravel server ...\n{line}\n");
            assert!(super::reports_exec_denied(&output), "{line}");
            let error = RuntimeError::AppCrashed {
                exit_code: Some(exit_code),
            }
            .refine_crash(true);
            assert_eq!(error.code(), "runtime_app_exec_denied");
            let message = error.to_string();
            assert!(message.contains("setcap -r"), "{message}");
            assert!(!message.contains("only NET_BIND_SERVICE"), "{message}");
            assert!(!message.contains("environment"), "{message}");
            assert_reads_as_words(&message);
        }
    }

    #[test]
    fn an_ordinary_crash_stays_a_crash() {
        for output in [
            "Error: connect ECONNREFUSED 127.0.0.1:5432",
            "chown: /app/storage: Operation not permitted",
            "rm: can't remove '/etc/hosts': Operation not permitted",
            "flush: Operation not permitted",
        ] {
            assert!(!super::reports_exec_denied(output), "{output}");
        }
        for (exit_code, denied) in [(Some(1), true), (None, true), (Some(126), false)] {
            let error = RuntimeError::AppCrashed { exit_code }.refine_crash(denied);
            assert_eq!(error.code(), "runtime_app_crashed");
        }
        let unrelated = RuntimeError::Health("x".to_owned()).refine_crash(true);
        assert_eq!(unrelated.code(), "runtime_health_check_failed");
    }

    /// The first line is what Docker 29.9.3 printed for a builder with no
    /// network; the not-found and denied lines too, for images that do not exist.
    #[test]
    fn a_registry_that_does_not_answer_is_not_blamed_on_the_repository() {
        for stderr in [
            r#"ERROR: failed to build: failed to solve: alpine:3.21.0: failed to resolve source metadata for docker.io/library/alpine:3.21.0: failed to do request: Head "https://registry-1.docker.io/v2/library/alpine/manifests/3.21.0": dial tcp: lookup registry-1.docker.io on 127.0.0.11:53: server misbehaving"#,
            r#"ERROR: failed to solve: node:22: failed to resolve source metadata for docker.io/library/node:22: failed to do request: Head "https://registry-1.docker.io/v2/library/node/manifests/22": dial tcp 54.236.113.205:443: i/o timeout"#,
            r#"ERROR: failed to solve: failed to fetch anonymous token: unexpected status from GET request to https://auth.docker.io/token: 429 Too Many Requests"#,
            r#"#5 ERROR: failed to copy: httpReadSeeker: failed open: failed to do request: Get "https://production.cloudflare.docker.com/registry-v2/x": net/http: TLS handshake timeout"#,
            r#"ERROR: failed to solve: ghcr.io/railwayapp/railpack-frontend:v0.23.0: failed to resolve source metadata for ghcr.io/railwayapp/railpack-frontend:v0.23.0: unexpected status from HEAD request: 401 Unauthorized"#,
        ] {
            for phase in ["docker-build", "railpack-build"] {
                let error = RuntimeError::failed_process(phase, Some(1), stderr);
                assert_eq!(error.code(), "runtime_registry_unavailable", "{stderr}");
                let message = error.to_string();
                assert!(message.contains("exit code 1"), "{message}");
                assert!(!message.contains("docker.io"), "{message}");
                assert_reads_as_words(&message);
            }
        }
    }

    #[test]
    fn an_image_reference_the_user_can_fix_stays_a_build_failure() {
        for stderr in [
            "ERROR: failed to build: failed to solve: alpine:9.99.99-doesnotexist: failed to resolve source metadata for docker.io/library/alpine:9.99.99-doesnotexist: docker.io/library/alpine:9.99.99-doesnotexist: not found",
            "ERROR: failed to build: failed to solve: sakala-agentfix-private/nothing-here:1: failed to resolve source metadata for docker.io/sakala-agentfix-private/nothing-here:1: pull access denied, repository does not exist or may require authorization: server message: insufficient_scope: authorization failed",
            r#"ERROR: failed to solve: registry.example.invalid/app:1: failed to resolve source metadata for registry.example.invalid/app:1: failed to do request: Head "https://registry.example.invalid/v2/app/manifests/1": dial tcp: lookup registry.example.invalid: no such host"#,
            "#7 12.3 curl: (28) Failed to connect to example.com port 443: Connection refused",
            "npm ERR! network request to https://registry.npmjs.org/x failed, reason: connect ETIMEDOUT",
        ] {
            let error = RuntimeError::failed_process("docker-build", Some(1), stderr);
            assert_eq!(error.code(), "runtime_build_failed", "{stderr}");
        }
        let elsewhere = RuntimeError::failed_process(
            "docker-run",
            Some(125),
            "failed to resolve source metadata: i/o timeout",
        );
        assert_eq!(elsewhere.code(), "runtime_container_failed");
    }

    #[test]
    fn git_stderr_is_classified_without_becoming_the_reported_message() {
        for (stderr, code) in [
            ("remote: Repository not found.", "repository_not_found"),
            (
                "fatal: Authentication failed for token ghs_invalid_credential",
                "repository_auth_failed",
            ),
            ("fatal: token has expired", "repository_credential_expired"),
            (
                "fatal: GitHub App installation token has been revoked",
                "repository_credential_expired",
            ),
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
            assert!(!error.to_string().contains("ghs_invalid_credential"));
        }
    }
}
