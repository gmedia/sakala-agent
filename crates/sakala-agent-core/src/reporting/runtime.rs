use std::{
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, LogBounds};
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use crate::{
    api::ApiClient,
    logs::redactor::redact_line,
    ports::{CommandOutput, RuntimeExecutionError, RuntimeReporter, RuntimeReporterFactory},
};

/// Local ceiling for lines per log request; the control-plane policy
/// (`log_bounds.max_batch_lines`) can only lower it.
const DEFAULT_BATCH_LINES: usize = 100;
/// Local ceiling for message bytes per log request, comfortably below the
/// control-plane request body limit (1 MiB).
const MAX_BATCH_BYTES: usize = 512 * 1024;
/// How long a partially filled batch may wait before it is delivered.
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

pub struct ApiRuntimeReporterFactory {
    client: ApiClient,
}

impl ApiRuntimeReporterFactory {
    #[must_use]
    pub fn new(client: ApiClient) -> Self {
        Self { client }
    }
}

impl RuntimeReporterFactory for ApiRuntimeReporterFactory {
    fn reporter(&self, command_id: Uuid, log_bounds: LogBounds) -> Arc<dyn RuntimeReporter> {
        Arc::new(ApiRuntimeReporter::new(
            self.client.clone(),
            command_id,
            log_bounds,
        ))
    }
}

/// Reports events immediately and delivers logs in bounded batches.
///
/// Log lines are redacted and bounded when queued, then sent as
/// `{ "logs": [...] }` batches under one `Idempotency-Key` per request once
/// the batch is full or `FLUSH_INTERVAL` elapsed. A rejected or undeliverable
/// batch latches the reporter: later `log`/`flush` calls fail without
/// touching the control plane, which stops container log followers.
pub struct ApiRuntimeReporter {
    inner: Arc<ReporterInner>,
}

struct ReporterInner {
    client: ApiClient,
    command_id: Uuid,
    log_bounds: LogBounds,
    batch_max_lines: usize,
    flush_interval: Duration,
    state: Mutex<LogState>,
    flush_lock: Mutex<()>,
    flush_scheduled: AtomicBool,
    deployment_committed: AtomicBool,
    committed_output: StdMutex<Option<CommandOutput>>,
}

#[derive(Default)]
struct LogState {
    pending: Vec<DeploymentLog>,
    pending_bytes: usize,
    bytes_sent: u64,
    delivery_stopped: Option<String>,
}

impl ApiRuntimeReporter {
    #[must_use]
    pub fn new(client: ApiClient, command_id: Uuid, log_bounds: LogBounds) -> Self {
        Self::with_flush_interval(client, command_id, log_bounds, FLUSH_INTERVAL)
    }

    /// Overrides the batch flush interval. Primarily useful for deterministic
    /// integration tests.
    #[must_use]
    pub fn with_flush_interval(
        client: ApiClient,
        command_id: Uuid,
        log_bounds: LogBounds,
        flush_interval: Duration,
    ) -> Self {
        let batch_max_lines = log_bounds
            .max_batch_lines
            .and_then(|lines| usize::try_from(lines).ok())
            .map_or(DEFAULT_BATCH_LINES, |lines| lines.min(DEFAULT_BATCH_LINES))
            .max(1);
        Self {
            inner: Arc::new(ReporterInner {
                client,
                command_id,
                log_bounds,
                batch_max_lines,
                flush_interval,
                state: Mutex::new(LogState::default()),
                flush_lock: Mutex::new(()),
                flush_scheduled: AtomicBool::new(false),
                deployment_committed: AtomicBool::new(false),
                committed_output: StdMutex::new(None),
            }),
        }
    }
}

impl ReporterInner {
    fn stopped_error(reason: &str) -> RuntimeExecutionError {
        RuntimeExecutionError::reporting(format!("log delivery stopped: {reason}"))
    }

    /// Delivers pending lines in order, one bounded batch per request.
    async fn flush_pending(&self) -> Result<(), RuntimeExecutionError> {
        let _serialized = self.flush_lock.lock().await;
        loop {
            let batch = {
                let mut state = self.state.lock().await;
                if let Some(reason) = &state.delivery_stopped {
                    return Err(Self::stopped_error(reason));
                }
                if state.pending.is_empty() {
                    return Ok(());
                }
                let mut count = 0;
                let mut bytes = 0;
                for log in &state.pending {
                    if count > 0
                        && (count >= self.batch_max_lines
                            || bytes + log.message.len() > MAX_BATCH_BYTES)
                    {
                        break;
                    }
                    count += 1;
                    bytes += log.message.len();
                }
                state.pending_bytes = state.pending_bytes.saturating_sub(bytes);
                state.pending.drain(..count).collect::<Vec<_>>()
            };

            if let Err(error) = self.client.logs(self.command_id, &batch).await {
                let reason = error.to_string();
                let mut state = self.state.lock().await;
                let dropped = batch.len() + state.pending.len();
                state.pending.clear();
                state.pending_bytes = 0;
                state.delivery_stopped = Some(reason.clone());
                warn!(
                    command_id = %self.command_id,
                    dropped_lines = dropped,
                    %error,
                    "deployment log delivery stopped"
                );
                return Err(Self::stopped_error(&reason));
            }
        }
    }

    fn schedule_flush(self: &Arc<Self>) {
        if self.flush_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(inner.flush_interval).await;
            inner.flush_scheduled.store(false, Ordering::Release);
            // Failures latch `delivery_stopped`; the next `log` call surfaces them.
            let _ = inner.flush_pending().await;
        });
    }
}

#[async_trait]
impl RuntimeReporter for ApiRuntimeReporter {
    async fn event(&self, event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        self.inner
            .client
            .event(self.inner.command_id, &event)
            .await
            .map(|_| ())
            .map_err(|error| RuntimeExecutionError::reporting(error.to_string()))
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        let flush_now = {
            let mut state = self.inner.state.lock().await;
            if let Some(reason) = &state.delivery_stopped {
                return Err(ReporterInner::stopped_error(reason));
            }
            let Some(log) = bounded_log(log, self.inner.log_bounds, state.bytes_sent) else {
                return Ok(());
            };
            // The budget is reserved when a line is accepted so the local
            // accounting matches what the control plane will receive.
            state.bytes_sent += u64::try_from(log.message.len()).unwrap_or(u64::MAX);
            state.pending_bytes += log.message.len();
            state.pending.push(log);
            state.pending.len() >= self.inner.batch_max_lines
                || state.pending_bytes >= MAX_BATCH_BYTES
        };

        if flush_now {
            self.inner.flush_pending().await
        } else {
            self.inner.schedule_flush();
            Ok(())
        }
    }

    async fn flush(&self) -> Result<(), RuntimeExecutionError> {
        self.inner.flush_pending().await
    }

    fn mark_deployment_committed(&self, output: CommandOutput) {
        *self
            .inner
            .committed_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(output);
        self.inner
            .deployment_committed
            .store(true, Ordering::Release);
    }

    fn deployment_committed(&self) -> bool {
        self.inner.deployment_committed.load(Ordering::Acquire)
    }

    fn committed_output(&self) -> Option<CommandOutput> {
        if !self.deployment_committed() {
            return None;
        }
        self.inner
            .committed_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn bounded_log(mut log: DeploymentLog, bounds: LogBounds, sent: u64) -> Option<DeploymentLog> {
    // A zero-line batch policy means no log may be emitted.
    if bounds.max_batch_lines == Some(0) {
        return None;
    }
    log.message = sanitise_line(&redact_line(&log.message));
    truncate_utf8(&mut log.message, bounds.max_line_length);
    if let Some(maximum) = bounds.max_total_bytes {
        if sent >= maximum {
            return None;
        }
        truncate_utf8(&mut log.message, Some(maximum - sent));
    }
    (!log.message.is_empty()).then_some(log)
}

/// Collapses the control characters a log message may not contain.
///
/// The control plane requires one message to be one line and rejects the whole
/// batch otherwise. Build tooling redraws progress with carriage returns, so a
/// single line of process output can carry several of them; a rejected batch
/// used to abort the deployment that produced it. Callers that can split a
/// line do so first (see the runtime's process sink); this is the last guard
/// for the paths that cannot.
fn sanitise_line(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' => ' ',
            other => other,
        })
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn truncate_utf8(value: &mut String, maximum: Option<u64>) {
    let Some(maximum) = maximum else { return };
    let maximum = usize::try_from(maximum).unwrap_or(usize::MAX);
    if value.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use sakala_agent_protocol::{DeploymentLog, LogStream};
    use time::OffsetDateTime;

    use super::{LogBounds, bounded_log};

    fn log(message: &str) -> DeploymentLog {
        DeploymentLog {
            stream: LogStream::Stdout,
            message: message.to_owned(),
            recorded_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn redacts_before_truncating_and_preserves_utf8_boundaries() {
        let secret = bounded_log(
            log("TOKEN=super-secret-value"),
            LogBounds {
                max_line_length: Some(16),
                ..LogBounds::default()
            },
            0,
        )
        .expect("redacted log should fit");
        assert_eq!(secret.message, "TOKEN=[REDACTED]");

        let unicode = bounded_log(
            log("ééé"),
            LogBounds {
                max_line_length: Some(5),
                ..LogBounds::default()
            },
            0,
        )
        .expect("unicode log should be retained");
        assert_eq!(unicode.message, "éé");
        assert_eq!(unicode.message.len(), 4);
    }

    #[test]
    fn total_byte_budget_is_shared_across_log_lines() {
        let bounds = LogBounds {
            max_total_bytes: Some(6),
            ..LogBounds::default()
        };
        let first = bounded_log(log("abcd"), bounds, 0).expect("first line should fit");
        let second = bounded_log(log("efgh"), bounds, first.message.len() as u64)
            .expect("remaining budget should be used");
        let third = bounded_log(
            log("ignored"),
            bounds,
            (first.message.len() + second.message.len()) as u64,
        );

        assert_eq!(first.message, "abcd");
        assert_eq!(second.message, "ef");
        assert!(third.is_none());
    }

    #[test]
    fn collapses_carriage_returns_the_control_plane_rejects() {
        // Build progress redraws with carriage returns, and a message holding
        // one was rejected with HTTP 422, which used to fail the deployment.
        let bounded = bounded_log(
            log("#5 0.1 downloading\r#5 1.0 done"),
            LogBounds::default(),
            0,
        )
        .expect("a progress line is still worth reporting");

        assert!(!bounded.message.contains('\r'));
        assert!(!bounded.message.contains('\n'));
        assert_eq!(bounded.message, "#5 0.1 downloading #5 1.0 done");
    }

    #[test]
    fn drops_a_line_that_is_only_control_characters() {
        assert!(bounded_log(log("\r\n"), LogBounds::default(), 0).is_none());
    }
}
