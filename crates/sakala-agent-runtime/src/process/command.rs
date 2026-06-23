use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::RuntimeError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub timeout_disabled: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
            timeout: None,
            timeout_disabled: false,
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
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn without_timeout(mut self) -> Self {
        self.timeout_disabled = true;
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
            .args(&spec.args)
            .envs(&spec.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

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
        let mut stdout = BufReader::new(stdout).lines();
        let mut stderr = BufReader::new(stderr).lines();
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut captured_stdout = String::new();
        let mut captured_stderr = String::new();
        let timeout =
            (!spec.timeout_disabled).then(|| spec.timeout.unwrap_or(self.default_timeout));
        let deadline = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(deadline);

        while !stdout_done || !stderr_done {
            tokio::select! {
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
