use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_core::ports::SecretString;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::RuntimeError;

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    secret_environment: BTreeMap<String, SecretString>,
    pub timeout: Option<Duration>,
    pub timeout_disabled: bool,
    pub cancellation: Option<CancellationToken>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
            secret_environment: BTreeMap::new(),
            timeout: None,
            timeout_disabled: false,
            cancellation: None,
        }
    }

    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        self.args.push(value.into());
        self
    }

    pub fn current_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.current_dir = Some(path.as_ref().to_owned());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn secret_env(mut self, key: impl Into<String>, value: SecretString) -> Self {
        self.secret_environment.insert(key.into(), value);
        self
    }

    /// Names of the secret environment variables. The values stay private.
    pub fn secret_environment_names(&self) -> impl Iterator<Item = &str> {
        self.secret_environment.keys().map(String::as_str)
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn without_timeout(mut self) -> Self {
        self.timeout_disabled = true;
        self
    }

    #[must_use]
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[async_trait]
pub trait ProcessOutputSink: Send + Sync {
    async fn line(&self, stream: ProcessStream, line: &str) -> Result<(), RuntimeError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NullOutputSink;

#[async_trait]
impl ProcessOutputSink for NullOutputSink {
    async fn line(&self, _stream: ProcessStream, _line: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError>;
}

#[derive(Clone, Copy, Debug)]
pub struct TokioProcessRunner {
    default_timeout: Duration,
}

impl TokioProcessRunner {
    #[must_use]
    pub fn new(default_timeout: Duration) -> Self {
        Self { default_timeout }
    }
}

impl Default for TokioProcessRunner {
    fn default() -> Self {
        Self::new(Duration::from_secs(120))
    }
}

#[async_trait]
impl ProcessRunner for TokioProcessRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        sink: &dyn ProcessOutputSink,
    ) -> Result<ProcessOutput, RuntimeError> {
        let mut command = Command::new(&spec.program);
        command
            .env_clear()
            .envs(
                std::env::vars_os()
                    .filter(|(name, _)| name.to_str().is_some_and(inherited_by_children)),
            )
            .args(&spec.args)
            .envs(&spec.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        for (key, value) in &spec.secret_environment {
            command.env(key, value.expose());
        }

        if let Some(current_dir) = &spec.current_dir {
            command.current_dir(current_dir);
        }

        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| {
            RuntimeError::Dependency(format!("could not start {}: {error}", spec.program))
        })?;
        let mut process_group = ProcessGroupGuard::new(child.id());
        let stdout = child.stdout.take().ok_or_else(|| {
            RuntimeError::Dependency(format!("could not capture {} stdout", spec.program))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            RuntimeError::Dependency(format!("could not capture {} stderr", spec.program))
        })?;
        let mut stdout = BoundedLines::new(BufReader::new(stdout));
        let mut stderr = BoundedLines::new(BufReader::new(stderr));
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut captured_stdout = String::new();
        let mut captured_stderr = String::new();
        let timeout =
            (!spec.timeout_disabled).then(|| spec.timeout.unwrap_or(self.default_timeout));
        let cancellation = spec.cancellation.clone();
        let deadline = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(deadline);

        while !stdout_done || !stderr_done {
            tokio::select! {
                _ = cancelled(&cancellation) => {
                    process_group.kill();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(RuntimeError::Cancelled);
                }
                () = &mut deadline => {
                    process_group.kill();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(RuntimeError::Timeout {
                        operation: spec.program.clone(),
                        seconds: timeout.map_or(0, |timeout| timeout.as_secs()),
                    });
                }
                line = stdout.next_line(), if !stdout_done => {
                    match line.map_err(RuntimeError::Filesystem)? {
                        Some(line) => {
                            sink.line(ProcessStream::Stdout, &line).await?;
                            append_bounded(&mut captured_stdout, &line);
                        }
                        None => stdout_done = true,
                    }
                }
                line = stderr.next_line(), if !stderr_done => {
                    match line.map_err(RuntimeError::Filesystem)? {
                        Some(line) => {
                            sink.line(ProcessStream::Stderr, &line).await?;
                            append_bounded(&mut captured_stderr, &line);
                        }
                        None => stderr_done = true,
                    }
                }
            }
        }

        let status = tokio::select! {
            _ = cancelled(&cancellation) => {
                process_group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(RuntimeError::Cancelled);
            }
            () = &mut deadline => {
                process_group.kill();
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(RuntimeError::Timeout {
                    operation: spec.program.clone(),
                    seconds: timeout.map_or(0, |timeout| timeout.as_secs()),
                });
            }
            status = child.wait() => status.map_err(|error| {
                RuntimeError::Dependency(format!("could not wait for {}: {error}", spec.program))
            })?,
        };
        process_group.disarm();

        Ok(ProcessOutput {
            success: status.success(),
            code: status.code(),
            stdout: captured_stdout,
            stderr: captured_stderr,
        })
    }
}

async fn cancelled(cancellation: &Option<CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take().and_then(|pid| i32::try_from(pid).ok()) {
            // The child is its own process-group leader, so this also terminates descendants.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }

        #[cfg(not(unix))]
        {
            self.pid = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Longest line kept from a process. The reporter cuts further to the command's
/// `max_line_length`; this ceiling only protects the agent's own memory, because a
/// tenant decides what a followed container prints, newlines included.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Splits output into lines without ever holding more than `MAX_LINE_BYTES` of one.
/// A longer line is delivered cut at the limit, and its remainder is thrown away.
///
/// Output is whatever a process prints. A byte that is not UTF-8 must not fail the
/// command or lose the lines after it, so it becomes a replacement character.
///
/// `next_line` is safe to cancel in a `select!`: everything it has taken from the
/// reader is already in `self`.
struct BoundedLines<R> {
    reader: R,
    line: Vec<u8>,
    discarding: bool,
}

impl<R: AsyncBufRead + Unpin> BoundedLines<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
            discarding: false,
        }
    }

    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok((!self.line.is_empty()).then(|| self.take_line()));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let content = &available[..newline.unwrap_or(available.len())];
            let consumed = newline.map_or(available.len(), |position| position + 1);
            if !self.discarding {
                let room = MAX_LINE_BYTES - self.line.len();
                self.line
                    .extend_from_slice(&content[..content.len().min(room)]);
            }
            self.reader.consume(consumed);

            if self.discarding {
                self.discarding = newline.is_none();
            } else if newline.is_some() {
                return Ok(Some(self.take_line()));
            } else if self.line.len() >= MAX_LINE_BYTES {
                self.discarding = true;
                return Ok(Some(self.take_line()));
            }
        }
    }

    fn take_line(&mut self) -> String {
        let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
        let line = String::from_utf8_lossy(line).into_owned();
        self.line.clear();
        line
    }
}

/// Whether a child process may inherit a variable from the agent. This is an
/// allowlist because the agent's environment holds its control-plane token, and
/// nothing a child runs should be able to read that. `SAKALA_*` never passes, so a
/// new setting cannot leak by matching a prefix below. Anything a command needs
/// beyond this is passed explicitly through `CommandSpec`.
fn inherited_by_children(name: &str) -> bool {
    const NAMES: [&str; 18] = [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
        "XDG_RUNTIME_DIR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
    ];
    const PREFIXES: [&str; 3] = ["DOCKER_", "BUILDX_", "BUILDKIT_"];
    !name.starts_with("SAKALA_")
        && (NAMES.contains(&name.to_ascii_uppercase().as_str())
            || PREFIXES.iter().any(|prefix| name.starts_with(prefix)))
}

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

fn append_bounded(output: &mut String, line: &str) {
    if output.len() >= MAX_CAPTURE_BYTES {
        return;
    }

    let remaining = MAX_CAPTURE_BYTES - output.len();
    let line = if line.len() <= remaining {
        line
    } else {
        let mut boundary = remaining;
        while boundary > 0 && !line.is_char_boundary(boundary) {
            boundary -= 1;
        }
        &line[..boundary]
    };
    output.push_str(line);
    if output.len() < MAX_CAPTURE_BYTES {
        output.push('\n');
    }
}
