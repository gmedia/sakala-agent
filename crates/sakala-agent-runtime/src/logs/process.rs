use sakala_agent_protocol::{DeploymentLog, LogStream};
use time::OffsetDateTime;

use async_trait::async_trait;

use tracing::warn;

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
        let stream = match stream {
            ProcessStream::Stdout => LogStream::Stdout,
            ProcessStream::Stderr => LogStream::Stderr,
        };

        // Build tooling redraws progress with carriage returns, so one line of
        // process output can hold several frames. The control plane requires
        // one message per line and rejects a batch that breaks it, so each
        // frame is reported on its own rather than flattened or dropped.
        for frame in line.split('\r') {
            let frame = frame.trim_end();
            if frame.trim().is_empty() {
                continue;
            }

            // Logs are telemetry. Delivery can stop for reasons that say
            // nothing about the work in progress — an exhausted log budget, a
            // rejected batch — and a deployment that already builds and runs
            // must not be failed because its output could not be shipped. The
            // stop is recorded once per line here and stays visible in the
            // agent's own log.
            if let Err(error) = self
                .reporter
                .log(DeploymentLog {
                    stream,
                    message: format!("[{}] {frame}", self.phase),
                    recorded_at: OffsetDateTime::now_utc(),
                })
                .await
            {
                warn!(phase = self.phase, %error, "deployment log line not delivered");
                return Ok(());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sakala_agent_core::ports::{CommandOutput, RuntimeExecutionError};
    use sakala_agent_protocol::DeploymentEvent;

    use super::*;

    #[derive(Default)]
    struct RecordingReporter {
        lines: Mutex<Vec<String>>,
        fail: bool,
    }

    #[async_trait]
    impl RuntimeReporter for RecordingReporter {
        async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
            if self.fail {
                return Err(RuntimeExecutionError::reporting(
                    "log delivery stopped: control-plane rejected report with HTTP 422".to_owned(),
                ));
            }
            self.lines.lock().expect("lock").push(log.message.clone());
            Ok(())
        }

        async fn flush(&self) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        fn mark_deployment_committed(&self, _output: CommandOutput) {}
    }

    #[tokio::test]
    async fn reports_each_carriage_return_frame_as_its_own_line() {
        // Build tooling redraws progress in place, so one line of output can
        // hold several frames. The control plane requires one message per
        // line and rejects the whole batch otherwise.
        let reporter = RecordingReporter::default();
        let sink = ReporterOutputSink::new(&reporter, "build");

        sink.line(ProcessStream::Stdout, "#5 0.1 downloading\r#5 1.0 done")
            .await
            .expect("progress output is reportable");

        let lines = reporter.lines.lock().expect("lock").clone();
        assert_eq!(lines, ["[build] #5 0.1 downloading", "[build] #5 1.0 done"]);
        assert!(lines.iter().all(|line| !line.contains('\r')));
    }

    #[tokio::test]
    async fn a_failed_delivery_does_not_abort_the_command() {
        // A staging deployment whose build had already succeeded was failed
        // because one log batch was rejected. Telemetry must not decide
        // whether the work itself succeeded.
        let reporter = RecordingReporter {
            fail: true,
            ..RecordingReporter::default()
        };
        let sink = ReporterOutputSink::new(&reporter, "build");

        sink.line(ProcessStream::Stdout, "still building")
            .await
            .expect("a rejected log line must not fail the command");
    }
}
