use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use tokio::time::{Instant, sleep, timeout};
use tracing::debug;

use crate::{
    CommandSpec, NullOutputSink, ProcessOutput, ProcessRunner, RuntimeError,
    health::{HealthChecker, ReadinessRequest},
};

const STATE_FORMAT: &str = "{{.State.Status}}\t{{.RestartCount}}\t{{.State.ExitCode}}\t{{.State.OOMKilled}}\t{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}\t{{.HostConfig.Memory}}";
const EXIT_HISTORY_FORMAT: &str = "{{.Action}}\t{{.Actor.Attributes.exitCode}}";
/// Exit code of a SIGKILL, which is what the kernel OOM killer sends.
const SIGKILL_EXIT_CODE: i32 = 137;
const FIRST_SERVER_ERROR: u16 = 500;
const BYTES_PER_MB: u64 = 1_048_576;
/// Slowest network read one probe attempt tolerates before trying again.
const MAX_PROBE_READ_SECONDS: u64 = 5;
/// Lets `wget` report its own timeout before the process is killed.
const PROBE_PROCESS_GRACE: Duration = Duration::from_secs(2);
/// Every attempt gets this long even at the deadline, so a zero window still
/// performs one complete check.
const MIN_ATTEMPT: Duration = Duration::from_secs(1);
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const EXIT_HISTORY_TIMEOUT: Duration = Duration::from_secs(3);

/// Decides readiness from Docker's state and an HTTP probe of the app.
///
/// The probe runs inside the router container because the host binary cannot
/// reach container addresses under rootless Docker, and because that is the
/// path the route will use.
pub struct DockerHealthChecker {
    runner: Arc<dyn ProcessRunner>,
    router_container: String,
    interval: Duration,
}

impl DockerHealthChecker {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        router_container: String,
        interval: Duration,
    ) -> Self {
        Self {
            runner,
            router_container,
            interval,
        }
    }

    async fn inspect(
        &self,
        container: &str,
        remaining: Duration,
    ) -> Result<ContainerState, RuntimeError> {
        let bound = remaining.max(MIN_ATTEMPT);
        let command = CommandSpec::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(STATE_FORMAT)
            .arg(container)
            .timeout(bound);
        let output = self.run_within(&command, bound).await?;
        if !output.success {
            return Err(RuntimeError::Container(format!(
                "docker inspect failed with {}",
                crate::error::describe_exit(output.code)
            )));
        }
        ContainerState::parse(&output.stdout).ok_or_else(|| {
            RuntimeError::Container("docker inspect returned an unknown state format".to_owned())
        })
    }

    async fn probe(&self, request: &ReadinessRequest, remaining: Duration) -> ProbeOutcome {
        let read_seconds = remaining
            .as_secs_f64()
            .ceil()
            .clamp(1.0, MAX_PROBE_READ_SECONDS as f64) as u64;
        let bound = Duration::from_secs(read_seconds) + PROBE_PROCESS_GRACE;
        let command = probe_command(
            &self.router_container,
            &request.container,
            request.port,
            read_seconds,
        )
        .timeout(bound);
        match self.run_within(&command, bound).await {
            Ok(output) => ProbeOutcome::classify(&output),
            Err(error) => {
                debug!(%error, "readiness probe process did not finish");
                ProbeOutcome::Inconclusive
            }
        }
    }

    async fn run_within(
        &self,
        command: &CommandSpec,
        bound: Duration,
    ) -> Result<ProcessOutput, RuntimeError> {
        timeout(bound, self.runner.run(command, &NullOutputSink))
            .await
            .unwrap_or_else(|_| {
                Err(RuntimeError::Timeout {
                    operation: command.program.clone(),
                    seconds: bound.as_secs(),
                })
            })
    }

    /// Docker clears the exit code and the OOM flag as soon as a restart
    /// succeeds, so a crash seen between restarts has to be read from history.
    async fn crash_error(
        &self,
        request: &ReadinessRequest,
        state: &ContainerState,
    ) -> RuntimeError {
        let mut exit_code = (state.status != "running").then_some(state.exit_code);
        let mut oom_killed = state.oom_killed;
        if exit_code.is_none() || (exit_code == Some(SIGKILL_EXIT_CODE) && !oom_killed) {
            let history = self.exit_history(request).await;
            exit_code = history.exit_code.or(exit_code);
            oom_killed |= history.oom_killed;
        }
        if oom_killed {
            RuntimeError::AppOutOfMemory {
                memory_limit_mb: state.memory_limit_mb(),
            }
        } else {
            RuntimeError::AppCrashed { exit_code }
        }
    }

    async fn exit_history(&self, request: &ReadinessRequest) -> ExitHistory {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let since = now.saturating_sub(request.started_at.elapsed().as_secs());
        let command = CommandSpec::new("docker")
            .arg("events")
            .arg("--since")
            .arg(since.to_string())
            // `--until` excludes the current second, and a later value only
            // holds the stream open until then.
            .arg("--until")
            .arg((now + 1).to_string())
            .arg("--filter")
            .arg(format!("container={}", request.container))
            .arg("--filter")
            .arg("event=die")
            .arg("--filter")
            .arg("event=oom")
            .arg("--format")
            .arg(EXIT_HISTORY_FORMAT)
            .timeout(EXIT_HISTORY_TIMEOUT);
        match self.run_within(&command, EXIT_HISTORY_TIMEOUT).await {
            Ok(output) if output.success => ExitHistory::parse(&output.stdout),
            Ok(output) => {
                debug!(status = ?output.code, "docker events exited without history");
                ExitHistory::default()
            }
            Err(error) => {
                debug!(%error, "docker events did not finish");
                ExitHistory::default()
            }
        }
    }
}

#[async_trait]
impl HealthChecker for DockerHealthChecker {
    async fn wait_until_ready(&self, request: &ReadinessRequest) -> Result<(), RuntimeError> {
        let deadline = request.started_at + request.timeout;
        let mut expected_restarts = request.expected_restarts;
        let mut progress = Progress::default();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.inspect(&request.container, remaining).await {
                Ok(state) => {
                    let baseline = *expected_restarts.get_or_insert(state.restart_count);
                    if state.has_exited() || state.restart_count > baseline {
                        return Err(self.crash_error(request, &state).await);
                    }
                    progress.oom_killed = state.oom_killed;
                    progress.memory_limit_mb = state.memory_limit_mb();
                    match (state.status.as_str(), state.health.as_str()) {
                        (_, "unhealthy") => {
                            return Err(RuntimeError::Health(
                                "the HEALTHCHECK declared by the image reports unhealthy"
                                    .to_owned(),
                            ));
                        }
                        (_, "starting") => progress.healthcheck_starting = true,
                        ("running", _) => {
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            match self.probe(request, remaining).await {
                                ProbeOutcome::Answered(Some(status))
                                    if status >= FIRST_SERVER_ERROR =>
                                {
                                    progress.last_server_error = Some(status);
                                }
                                ProbeOutcome::Answered(_) => return Ok(()),
                                ProbeOutcome::NoAnswer => progress.no_answer = true,
                                ProbeOutcome::Unresolved => progress.unresolved = true,
                                ProbeOutcome::Inconclusive => progress.inconclusive = true,
                                ProbeOutcome::RouterUnavailable(reason) => {
                                    return Err(RuntimeError::Dependency(format!(
                                        "router container {} cannot run the readiness probe: {reason}",
                                        self.router_container
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(error) => debug!(%error, "container state is not available yet"),
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(progress.verdict(request, &self.router_container));
            }
            sleep(self.interval.max(MIN_POLL_INTERVAL).min(remaining)).await;
        }
    }
}

/// `wget` always follows a redirect and BusyBox has no flag to stop it. In proxy
/// mode every request of the chain stays on the connection to `http_proxy`, so
/// pointing that at the app keeps a `Location` from reaching anything else: the
/// router must not fetch an address the app chose. The cost is an absolute-form
/// request line, which HTTP servers are required to accept.
fn probe_command(
    router_container: &str,
    container: &str,
    port: u16,
    read_seconds: u64,
) -> CommandSpec {
    let url = format!("http://{container}:{port}/");
    CommandSpec::new("docker")
        .arg("exec")
        .arg("--env")
        .arg(format!("http_proxy=http://{container}:{port}"))
        .arg(router_container)
        .arg("wget")
        .arg("-Y")
        .arg("on")
        .arg("--server-response")
        .arg("--spider")
        .arg("-T")
        .arg(read_seconds.to_string())
        .arg(url)
}

#[derive(Debug, Default)]
struct Progress {
    last_server_error: Option<u16>,
    no_answer: bool,
    unresolved: bool,
    inconclusive: bool,
    healthcheck_starting: bool,
    oom_killed: bool,
    memory_limit_mb: Option<u64>,
}

impl Progress {
    /// The failure at the deadline. Blames the app only for what was observed
    /// about the app: a probe that could not reach it says nothing about it.
    fn verdict(&self, request: &ReadinessRequest, router_container: &str) -> RuntimeError {
        let waited_seconds = request.timeout.as_secs();
        let port = request.port;
        if self.oom_killed {
            RuntimeError::AppOutOfMemory {
                memory_limit_mb: self.memory_limit_mb,
            }
        } else if let Some(status) = self.last_server_error {
            RuntimeError::AppUnhealthy {
                port,
                status,
                waited_seconds,
            }
        } else if self.no_answer {
            RuntimeError::AppNotListening {
                port,
                waited_seconds,
            }
        } else if self.unresolved {
            RuntimeError::Dependency(format!(
                "router container {router_container} cannot resolve the workload; check that it is attached to the runtime network"
            ))
        } else if self.inconclusive {
            RuntimeError::Dependency(format!(
                "router container {router_container} returned an unrecognised readiness probe result"
            ))
        } else if self.healthcheck_starting {
            RuntimeError::Health(format!(
                "the HEALTHCHECK declared by the image was still starting after {waited_seconds} seconds"
            ))
        } else {
            RuntimeError::Timeout {
                operation: request.operation.to_owned(),
                seconds: waited_seconds,
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ContainerState {
    status: String,
    restart_count: u32,
    exit_code: i32,
    oom_killed: bool,
    health: String,
    memory_bytes: u64,
}

impl ContainerState {
    fn parse(stdout: &str) -> Option<Self> {
        let mut fields = stdout.trim().split('\t');
        Some(Self {
            status: fields.next()?.to_owned(),
            restart_count: fields.next()?.parse().ok()?,
            exit_code: fields.next()?.parse().ok()?,
            oom_killed: fields.next()? == "true",
            health: fields.next()?.to_owned(),
            memory_bytes: fields.next()?.parse().ok()?,
        })
    }

    /// Under the `unless-stopped` policy a crash shows up as `restarting`.
    fn has_exited(&self) -> bool {
        matches!(self.status.as_str(), "restarting" | "exited" | "dead")
    }

    fn memory_limit_mb(&self) -> Option<u64> {
        (self.memory_bytes > 0).then_some(self.memory_bytes / BYTES_PER_MB)
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ExitHistory {
    exit_code: Option<i32>,
    oom_killed: bool,
}

impl ExitHistory {
    fn parse(stdout: &str) -> Self {
        let mut history = Self::default();
        for line in stdout.lines() {
            let (action, exit_code) = line.split_once('\t').unwrap_or((line, ""));
            match action.trim() {
                "oom" => history.oom_killed = true,
                "die" => history.exit_code = exit_code.trim().parse().ok(),
                _ => {}
            }
        }
        history
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ProbeOutcome {
    /// The app replied. `None` is a successful `wget` without a readable status.
    Answered(Option<u16>),
    /// Nothing usable came back from the app's port.
    NoAnswer,
    /// The workload's name did not resolve from the router.
    Unresolved,
    /// The probe ended in a way this code does not recognise.
    Inconclusive,
    /// The router container cannot run `wget` at all.
    RouterUnavailable(&'static str),
}

impl ProbeOutcome {
    /// BusyBox `wget` exits with 1 both for a 4xx and for a refused connection,
    /// so only its stderr tells them apart. The first status line is the app's
    /// reply to `/`, which is all that matters: the goal is "the app answers".
    /// Nothing from this output may reach a message, because the app wrote part
    /// of it.
    fn classify(output: &ProcessOutput) -> Self {
        if let Some(status) = first_http_status(&output.stderr) {
            return Self::Answered(Some(status));
        }
        if output.success {
            return Self::Answered(None);
        }
        let stderr = output.stderr.to_ascii_lowercase();
        if matches!(output.code, Some(126 | 127)) || stderr.contains("executable file not found") {
            return Self::RouterUnavailable("the wget command is missing from its image");
        }
        if stderr
            .lines()
            .any(|line| line.starts_with("error response from daemon"))
        {
            return Self::RouterUnavailable("the container is missing or not running");
        }
        if stderr.contains("wget: bad address") {
            return Self::Unresolved;
        }
        if [
            "wget: can't connect",
            "wget: download timed out",
            "wget: error getting response",
        ]
        .iter()
        .any(|failure| stderr.contains(failure))
        {
            return Self::NoAnswer;
        }
        Self::Inconclusive
    }
}

/// Reads the first `--server-response` status line, printed as `  HTTP/1.1 404
/// Not Found`. Header values come after it, so the app cannot spoof this line.
fn first_http_status(stderr: &str) -> Option<u16> {
    stderr.lines().find_map(|line| {
        let mut words = line.strip_prefix("  ")?.split_whitespace();
        words.next()?.strip_prefix("HTTP/")?;
        words
            .next()?
            .parse()
            .ok()
            .filter(|status| (100..=999).contains(status))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(stderr: &str) -> ProcessOutput {
        ProcessOutput {
            success: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    // The stderr samples below were captured from `wget` in
    // caddy:2.11.3-alpine (BusyBox v1.37.0) against real servers.
    #[test]
    fn a_client_error_is_an_answer_although_wget_exits_with_one() {
        let output = failed(
            "Connecting to app:3000 (172.25.0.4:3000)\n  HTTP/1.1 404 Not Found\nwget: server returned error: HTTP/1.1 404 Not Found\n",
        );

        assert_eq!(
            ProbeOutcome::classify(&output),
            ProbeOutcome::Answered(Some(404))
        );
    }

    #[test]
    fn a_server_error_keeps_its_status() {
        let output = failed(
            "Connecting to app:3000 (172.25.0.5:3000)\n  HTTP/1.1 500 Internal Server Error\nwget: server returned error: HTTP/1.1 500 Internal Server Error\n",
        );

        assert_eq!(
            ProbeOutcome::classify(&output),
            ProbeOutcome::Answered(Some(500))
        );
    }

    #[test]
    fn a_redirect_is_judged_by_the_first_response_only() {
        let output = failed(
            "Connecting to app:3000 (172.25.0.8:3000)\n  HTTP/1.1 302 Found\n  Content-Type: text/plain\n  Location: http://outside.invalid/\n  HTTP/1.1 302 Found\n  Location: http://outside.invalid/\nwget: too many redirections\n",
        );

        assert_eq!(
            ProbeOutcome::classify(&output),
            ProbeOutcome::Answered(Some(302))
        );
    }

    #[test]
    fn a_header_value_cannot_spoof_the_status() {
        let output = failed(
            "Connecting to app:3000 (172.25.0.8:3000)\n  HTTP/1.1 503 Service Unavailable\n  X-Spoof: HTTP/1.1 200 OK\nwget: server returned error: HTTP/1.1 503 Service Unavailable\n",
        );

        assert_eq!(
            ProbeOutcome::classify(&output),
            ProbeOutcome::Answered(Some(503))
        );
    }

    #[test]
    fn connection_failures_are_no_answer() {
        for stderr in [
            "Connecting to app:9999 (172.25.0.3:9999)\nwget: can't connect to remote host (172.25.0.3): Connection refused\n",
            "Connecting to app:3000 (172.25.0.13:3000)\nwget: download timed out\n",
            "Connecting to app:3000 (172.25.0.14:3000)\nwget: error getting response: Connection reset by peer\n",
            "Connecting to app:3000 (172.25.0.12:3000)\n  garbage\nwget: error getting response: Resource temporarily unavailable\n",
        ] {
            assert_eq!(
                ProbeOutcome::classify(&failed(stderr)),
                ProbeOutcome::NoAnswer,
                "{stderr}"
            );
        }
    }

    #[test]
    fn an_unresolvable_name_is_not_blamed_on_the_app() {
        let output = failed("wget: bad address 'app:3000'\n");

        assert_eq!(ProbeOutcome::classify(&output), ProbeOutcome::Unresolved);
    }

    #[test]
    fn a_broken_router_is_not_blamed_on_the_app() {
        let missing_wget = ProcessOutput {
            success: false,
            code: Some(127),
            stdout: String::new(),
            stderr: "OCI runtime exec failed: exec failed: unable to start container process: exec: \"wget\": executable file not found in $PATH\n".to_owned(),
        };
        assert!(matches!(
            ProbeOutcome::classify(&missing_wget),
            ProbeOutcome::RouterUnavailable(_)
        ));

        for stderr in [
            "Error response from daemon: No such container: router\n",
            "Error response from daemon: container a9b65f is not running\n",
        ] {
            assert!(matches!(
                ProbeOutcome::classify(&failed(stderr)),
                ProbeOutcome::RouterUnavailable(_)
            ));
        }
    }

    #[test]
    fn an_app_cannot_impersonate_a_docker_error() {
        let output = failed(
            "Connecting to app:3000 (172.25.0.12:3000)\n  Error response from daemon\nwget: error getting response: Resource temporarily unavailable\n",
        );

        assert_eq!(ProbeOutcome::classify(&output), ProbeOutcome::NoAnswer);
    }

    #[test]
    fn unknown_probe_output_stays_inconclusive() {
        assert_eq!(
            ProbeOutcome::classify(&failed("something unexpected\n")),
            ProbeOutcome::Inconclusive
        );
    }

    #[test]
    fn parses_the_state_docker_prints_for_a_crash_loop() {
        let state =
            ContainerState::parse("restarting\t3\t137\ttrue\tnone\t16777216\n").expect("state");

        assert!(state.has_exited());
        assert_eq!(state.exit_code, 137);
        assert!(state.oom_killed);
        assert_eq!(state.memory_limit_mb(), Some(16));
    }

    #[test]
    fn a_container_without_a_memory_limit_has_none_to_report() {
        let state = ContainerState::parse("running\t0\t0\tfalse\tnone\t0\n").expect("state");

        assert!(!state.has_exited());
        assert_eq!(state.memory_limit_mb(), None);
    }

    #[test]
    fn unknown_state_output_is_rejected() {
        assert_eq!(ContainerState::parse("running\n"), None);
    }

    #[test]
    fn exit_history_keeps_the_last_exit_and_any_oom() {
        assert_eq!(
            ExitHistory::parse("oom\t<no value>\ndie\t137\ndie\t3\n"),
            ExitHistory {
                exit_code: Some(3),
                oom_killed: true,
            }
        );
    }
}
