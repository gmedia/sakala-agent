use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
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
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            environment: BTreeMap::new(),
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

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

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

        let mut child = command.spawn().map_err(|error| {
            RuntimeError::Dependency(format!("could not start {}: {error}", spec.program))
        })?;
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

        while !stdout_done || !stderr_done {
            tokio::select! {
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

        let status = child.wait().await.map_err(|error| {
            RuntimeError::Dependency(format!("could not wait for {}: {error}", spec.program))
        })?;

        Ok(ProcessOutput {
            success: status.success(),
            code: status.code(),
            stdout: captured_stdout,
            stderr: captured_stderr,
        })
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
