use sakala_agent_protocol::{DeploymentLog, LogStream};
use time::OffsetDateTime;

use async_trait::async_trait;

use crate::{ProcessOutputSink, ProcessStream, RuntimeError, RuntimeReporter};

pub struct ReporterOutputSink<'a> {
    reporter: &'a dyn RuntimeReporter,
    phase: &'a str,
}

impl<'a> ReporterOutputSink<'a> {
    #[must_use]
    pub fn new(reporter: &'a dyn RuntimeReporter, phase: &'a str) -> Self {
        Self { reporter, phase }
    }
}

#[async_trait]
impl ProcessOutputSink for ReporterOutputSink<'_> {
    async fn line(&self, stream: ProcessStream, line: &str) -> Result<(), RuntimeError> {
        if line.trim().is_empty() {
            return Ok(());
        }

        self.reporter
            .log(DeploymentLog {
                stream: match stream {
                    ProcessStream::Stdout => LogStream::Stdout,
                    ProcessStream::Stderr => LogStream::Stderr,
                },
                message: format!("[{}] {line}", self.phase),
                recorded_at: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(Into::into)
    }
}
