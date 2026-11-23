use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use backon::{BackoffBuilder, ExponentialBuilder};
use sakala_agent_protocol::{DeploymentEvent, DeploymentLog, LogBounds, LogStream};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use super::events::{EventDelivery, EventDeliveryPolicy};
use crate::{
    api::{ApiClient, ReportAttemptError},
    logs::redactor::ValueRedactor,
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
/// Most message bytes one command keeps queued while the control plane is
/// unreachable, on top of the one batch in flight. Past it the oldest queued
/// lines are dropped.
const MAX_BUFFERED_BYTES: usize = 2 * 1024 * 1024;
/// Most lines one command keeps queued, for output made of many short lines.
const MAX_BUFFERED_LINES: usize = 10_000;
/// First retry delay after a transient failure. Each later step doubles up to
/// `RETRY_MAX_DELAY`, and jitter adds up to the same amount again.
pub(super) const RETRY_MIN_DELAY: Duration = Duration::from_secs(1);
pub(super) const RETRY_MAX_DELAY: Duration = Duration::from_secs(15);
/// A server's `Retry-After` is honoured up to this, so one odd header cannot
/// park delivery for hours.
pub(super) const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// How long [`RuntimeReporter::flush`] waits for the queue to drain while the
/// control plane is unreachable before it gives the queued lines up as lost.
const FLUSH_GRACE: Duration = Duration::from_secs(120);

/// Buffering and retry limits of one [`ApiRuntimeReporter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogDeliveryPolicy {
    pub flush_interval: Duration,
    pub retry_min_delay: Duration,
    pub retry_max_delay: Duration,
    pub flush_grace: Duration,
    pub max_buffered_lines: usize,
    pub max_buffered_bytes: usize,
}

impl Default for LogDeliveryPolicy {
    fn default() -> Self {
        Self {
            flush_interval: FLUSH_INTERVAL,
            retry_min_delay: RETRY_MIN_DELAY,
            retry_max_delay: RETRY_MAX_DELAY,
            flush_grace: FLUSH_GRACE,
            max_buffered_lines: MAX_BUFFERED_LINES,
            max_buffered_bytes: MAX_BUFFERED_BYTES,
        }
    }
}

impl LogDeliveryPolicy {
    /// Unbounded exponential backoff with jitter; how long delivery may keep
    /// retrying is decided by `flush` and by a rejection, not by a count.
    fn backoff(&self) -> impl Iterator<Item = Duration> + use<> {
        ExponentialBuilder::new()
            .with_min_delay(self.retry_min_delay)
            .with_max_delay(self.retry_max_delay)
            .with_factor(2.0)
            .with_jitter()
            .without_max_times()
            .build()
    }
}

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
    fn reporter(
        &self,
        command_id: Uuid,
        log_bounds: LogBounds,
        redactor: ValueRedactor,
    ) -> Arc<dyn RuntimeReporter> {
        Arc::new(
            ApiRuntimeReporter::new(self.client.clone(), command_id, log_bounds)
                .with_redactor(redactor),
        )
    }
}

/// Reports events immediately and delivers logs in bounded, ordered batches.
///
/// Events are sent one request each, retried within a time budget, and given
/// up with a warning when the control plane stays unreachable; see
/// [`EventDeliveryPolicy`]. Only a `409` fails `event`.
///
/// `log` redacts and bounds a line and queues it; it never waits for the
/// network, so a slow or unreachable control plane cannot stall the build or
/// container output that produces the lines. One background worker per
/// reporter sends the queue as `{ "logs": [...] }` batches, strictly one after
/// another, once a batch is full or `flush_interval` elapsed.
///
/// Each batch keeps one `Idempotency-Key` and the same content until it is
/// acknowledged, so a retry after a lost response is answered with the
/// original sequence range instead of storing the lines twice. Transient
/// failures (connection, timeout, unreadable body, `408`, `429`, `5xx`) are
/// retried with exponential backoff and jitter, honouring `Retry-After`, for
/// as long as the command runs; `flush` bounds the wait at the end.
///
/// The queue holds at most `max_buffered_lines` lines and
/// `max_buffered_bytes` message bytes besides the batch in flight. When it is
/// full the oldest queued lines are dropped, because the end of the output
/// (the failing step, the release command) explains a deployment best. The
/// timeline then carries a system line saying how many lines are missing at
/// that point.
///
/// A rejection (`409`, `413`, `422`, another `4xx`, or a `200` without a valid
/// acknowledgement) latches the reporter: queued lines are dropped and later
/// `log`/`flush` calls fail without touching the control plane.
pub struct ApiRuntimeReporter {
    inner: Arc<ReporterInner>,
    redactor: ValueRedactor,
    events: Arc<EventDelivery>,
}

struct ReporterInner {
    client: ApiClient,
    command_id: Uuid,
    log_bounds: LogBounds,
    batch_max_lines: usize,
    policy: LogDeliveryPolicy,
    state: StdMutex<LogState>,
    /// Wakes the worker before `flush_interval` when a batch is full or a
    /// flush is waiting.
    batch_ready: Notify,
    /// Wakes `flush` callers whenever a batch finished, delivered or not.
    batch_done: Notify,
    deployment_committed: AtomicBool,
    committed_output: StdMutex<Option<CommandOutput>>,
}

#[derive(Default)]
struct LogState {
    queue: VecDeque<DeploymentLog>,
    queued_bytes: usize,
    /// Lines of the batch the worker is delivering.
    in_flight: usize,
    /// Message bytes counted against `max_total_bytes`: everything delivered,
    /// in flight, or queued.
    budget_used: u64,
    worker_running: bool,
    flush_requested: bool,
    /// Lines dropped from the queue since the last missing-lines notice.
    gap_lines: u64,
    /// Lines that will never reach the control plane, for the final warning.
    lost_lines: u64,
    /// Cancelled by `flush` to give up the batch in flight after its grace.
    abandon: CancellationToken,
    delivery_stopped: Option<String>,
}

enum Delivery {
    Delivered,
    Rejected(String),
    Abandoned,
}

impl ApiRuntimeReporter {
    #[must_use]
    pub fn new(client: ApiClient, command_id: Uuid, log_bounds: LogBounds) -> Self {
        Self::with_policy(client, command_id, log_bounds, LogDeliveryPolicy::default())
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
        Self::with_policy(
            client,
            command_id,
            log_bounds,
            LogDeliveryPolicy {
                flush_interval,
                ..LogDeliveryPolicy::default()
            },
        )
    }

    /// Overrides every buffering and retry limit. Primarily useful for
    /// deterministic integration tests.
    #[must_use]
    pub fn with_policy(
        client: ApiClient,
        command_id: Uuid,
        log_bounds: LogBounds,
        policy: LogDeliveryPolicy,
    ) -> Self {
        let batch_max_lines = log_bounds
            .max_batch_lines
            .and_then(|lines| usize::try_from(lines).ok())
            .map_or(DEFAULT_BATCH_LINES, |lines| lines.min(DEFAULT_BATCH_LINES))
            .max(1);
        let events = Arc::new(EventDelivery::new(
            client.clone(),
            command_id,
            EventDeliveryPolicy::default(),
        ));
        Self {
            inner: Arc::new(ReporterInner {
                client,
                command_id,
                log_bounds,
                batch_max_lines,
                policy,
                state: StdMutex::new(LogState::default()),
                batch_ready: Notify::new(),
                batch_done: Notify::new(),
                deployment_committed: AtomicBool::new(false),
                committed_output: StdMutex::new(None),
            }),
            redactor: ValueRedactor::default(),
            events,
        }
    }

    /// Overrides the event retry limits. Primarily useful for deterministic
    /// integration tests.
    #[must_use]
    pub fn with_event_policy(self, policy: EventDeliveryPolicy) -> Self {
        let events = Arc::new(EventDelivery::new(
            self.inner.client.clone(),
            self.inner.command_id,
            policy,
        ));
        self.with_event_delivery(events)
    }

    /// Shares event delivery with reports sent before this reporter existed,
    /// so the undelivered count and the outage state cover the whole command.
    #[must_use]
    pub(crate) fn with_event_delivery(mut self, events: Arc<EventDelivery>) -> Self {
        self.events = events;
        self
    }

    /// Number of events of this command that were given up.
    #[must_use]
    pub fn undelivered_events(&self) -> u64 {
        self.events.undelivered()
    }

    /// Adds the command's environment values to what this reporter hides.
    #[must_use]
    pub fn with_redactor(mut self, redactor: ValueRedactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Applies this command's redaction to text reported outside this reporter,
    /// such as the terminal failure message.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.redactor.redact(text)
    }

    /// Waits up to `grace` for every queued line to be delivered.
    ///
    /// Lines still queued or in flight when `grace` runs out are given up and
    /// counted as lost; the reporter keeps accepting new lines afterwards,
    /// because a deployment's container output is still followed under the
    /// same command. Returns an error naming the number of lost lines when
    /// any line of this command was lost, or the reason delivery stopped.
    pub async fn flush_within(&self, grace: Duration) -> Result<(), RuntimeExecutionError> {
        let spawn = {
            let mut state = self.inner.lock_state();
            if state.delivery_stopped.is_some() || state.is_idle() {
                return state.delivery_result();
            }
            state.flush_requested = true;
            !std::mem::replace(&mut state.worker_running, true)
        };
        if spawn {
            self.inner.spawn_worker();
        }
        self.inner.batch_ready.notify_one();
        if tokio::time::timeout(grace, self.inner.wait_until_idle())
            .await
            .is_err()
        {
            self.inner.abandon_pending(grace);
        }
        self.inner.lock_state().delivery_result()
    }
}

impl LogState {
    fn is_idle(&self) -> bool {
        self.queue.is_empty() && self.in_flight == 0
    }

    fn delivery_result(&self) -> Result<(), RuntimeExecutionError> {
        if let Some(reason) = &self.delivery_stopped {
            return Err(ReporterInner::stopped_error(reason));
        }
        if self.lost_lines > 0 {
            return Err(RuntimeExecutionError::reporting(format!(
                "{} log lines were not delivered",
                self.lost_lines
            )));
        }
        Ok(())
    }

    fn pop_oldest(&mut self) -> Option<DeploymentLog> {
        let log = self.queue.pop_front()?;
        let bytes = log.message.len();
        self.queued_bytes = self.queued_bytes.saturating_sub(bytes);
        // A line that never left the agent did not use the command's budget.
        self.budget_used = self
            .budget_used
            .saturating_sub(u64::try_from(bytes).unwrap_or(u64::MAX));
        Some(log)
    }
}

impl ReporterInner {
    fn stopped_error(reason: &str) -> RuntimeExecutionError {
        RuntimeExecutionError::reporting(format!("log delivery stopped: {reason}"))
    }

    fn lock_state(&self) -> MutexGuard<'_, LogState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn spawn_worker(self: &Arc<Self>) {
        tokio::spawn(Arc::clone(self).run_worker());
    }

    /// Drops the oldest queued lines until `incoming` more bytes fit.
    fn make_room(&self, state: &mut LogState, incoming: usize) {
        let mut dropped = 0_u64;
        while !state.queue.is_empty()
            && (state.queue.len() >= self.policy.max_buffered_lines
                || state.queued_bytes + incoming > self.policy.max_buffered_bytes)
        {
            state.pop_oldest();
            dropped += 1;
        }
        if dropped == 0 {
            return;
        }
        if state.gap_lines == 0 {
            warn!(
                command_id = %self.command_id,
                max_buffered_lines = self.policy.max_buffered_lines,
                max_buffered_bytes = self.policy.max_buffered_bytes,
                "deployment log buffer is full; dropping the oldest queued lines"
            );
        }
        state.gap_lines += dropped;
        state.lost_lines += dropped;
    }

    /// Takes the next batch off the queue, led by a notice when lines are
    /// missing just before it.
    fn take_batch(&self, state: &mut LogState) -> Vec<DeploymentLog> {
        let mut batch = Vec::new();
        let mut bytes = 0;
        if let Some(notice) = self.missing_lines_notice(state) {
            bytes += notice.message.len();
            batch.push(notice);
        }
        while let Some(log) = state.queue.front() {
            if !batch.is_empty()
                && (batch.len() >= self.batch_max_lines
                    || bytes + log.message.len() > MAX_BATCH_BYTES)
            {
                break;
            }
            bytes += log.message.len();
            state.queued_bytes = state.queued_bytes.saturating_sub(log.message.len());
            batch.extend(state.queue.pop_front());
        }
        batch
    }

    fn missing_lines_notice(&self, state: &mut LogState) -> Option<DeploymentLog> {
        let missing = std::mem::take(&mut state.gap_lines);
        if missing == 0 {
            return None;
        }
        let notice = bounded_log(
            DeploymentLog {
                stream: LogStream::System,
                message: format!(
                    "{missing} log lines are missing here: the Sakala Agent could not reach the control plane in time and dropped them."
                ),
                recorded_at: OffsetDateTime::now_utc(),
            },
            self.log_bounds,
            state.budget_used,
            &ValueRedactor::default(),
        )?;
        state.budget_used += u64::try_from(notice.message.len()).unwrap_or(u64::MAX);
        Some(notice)
    }

    async fn run_worker(self: Arc<Self>) {
        loop {
            let ready = {
                let state = self.lock_state();
                state.flush_requested
                    || state.queue.len() >= self.batch_max_lines
                    || state.queued_bytes >= MAX_BATCH_BYTES
            };
            if !ready {
                // Lets a partial batch fill up; a full batch or a flush ends
                // the wait early.
                let _ =
                    tokio::time::timeout(self.policy.flush_interval, self.batch_ready.notified())
                        .await;
            }
            let (batch, abandon) = {
                let mut state = self.lock_state();
                if state.delivery_stopped.is_some() || state.queue.is_empty() {
                    state.worker_running = false;
                    state.flush_requested = false;
                    drop(state);
                    self.batch_done.notify_waiters();
                    return;
                }
                let batch = self.take_batch(&mut state);
                if state.queue.is_empty() {
                    state.flush_requested = false;
                }
                state.in_flight = batch.len();
                (batch, state.abandon.clone())
            };

            let outcome = self.deliver(&batch, &abandon).await;

            {
                let mut state = self.lock_state();
                state.in_flight = 0;
                if let Delivery::Rejected(reason) = outcome {
                    let mut dropped = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                    while state.pop_oldest().is_some() {
                        dropped += 1;
                    }
                    state.lost_lines += dropped;
                    warn!(
                        command_id = %self.command_id,
                        dropped_lines = dropped,
                        error = %reason,
                        "deployment log delivery stopped"
                    );
                    state.delivery_stopped = Some(reason);
                }
                // An abandoned batch was counted as lost by `abandon_pending`.
            }
            self.batch_done.notify_waiters();
        }
    }

    /// Delivers one batch, retrying transient failures under the same key
    /// until it is acknowledged, rejected, or abandoned by `flush`.
    async fn deliver(&self, batch: &[DeploymentLog], abandon: &CancellationToken) -> Delivery {
        let idempotency_key = Uuid::new_v4().to_string();
        let mut backoff = self.policy.backoff();
        let mut attempt: u32 = 1;
        loop {
            let outcome = tokio::select! {
                biased;
                () = abandon.cancelled() => return Delivery::Abandoned,
                outcome = self.client.logs_attempt(self.command_id, &idempotency_key, batch) => outcome,
            };
            let (detail, retry_after) = match outcome {
                Ok(_) => {
                    if attempt > 1 {
                        info!(
                            command_id = %self.command_id,
                            attempts = attempt,
                            "deployment log batch delivered after retrying"
                        );
                    }
                    return Delivery::Delivered;
                }
                Err(ReportAttemptError::Rejected(error)) => {
                    return Delivery::Rejected(error.to_string());
                }
                Err(ReportAttemptError::Transient {
                    detail,
                    retry_after,
                }) => (detail, retry_after),
            };
            let backoff_delay = backoff.next().unwrap_or(self.policy.retry_max_delay);
            let delay = retry_after.map_or(backoff_delay, |requested| {
                backoff_delay.max(requested.min(MAX_RETRY_AFTER))
            });
            warn!(
                command_id = %self.command_id,
                error = %detail,
                attempt,
                delay_ms = delay.as_millis(),
                "deployment log batch will be retried with the same idempotency key"
            );
            tokio::select! {
                biased;
                () = abandon.cancelled() => return Delivery::Abandoned,
                () = tokio::time::sleep(delay) => {}
            }
            attempt = attempt.saturating_add(1);
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let finished = self.batch_done.notified();
            tokio::pin!(finished);
            // Registered before the check so a batch finishing in between
            // still wakes this waiter.
            finished.as_mut().enable();
            {
                let state = self.lock_state();
                if state.delivery_stopped.is_some() || state.is_idle() {
                    return;
                }
            }
            finished.await;
        }
    }

    /// Gives up every queued and in-flight line after `flush` waited `grace`.
    fn abandon_pending(&self, grace: Duration) {
        let mut state = self.lock_state();
        state.abandon.cancel();
        state.abandon = CancellationToken::new();
        // The batch in flight may still have been stored; counting it as lost
        // errs on the side of reporting a gap.
        let mut lost = u64::try_from(state.in_flight).unwrap_or(u64::MAX);
        while state.pop_oldest().is_some() {
            lost += 1;
        }
        state.lost_lines += lost;
        state.flush_requested = false;
        warn!(
            command_id = %self.command_id,
            lost_lines = lost,
            grace_seconds = grace.as_secs(),
            "deployment log lines were not delivered before the flush grace ran out"
        );
    }
}

#[async_trait]
impl RuntimeReporter for ApiRuntimeReporter {
    async fn event(&self, mut event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        event.message = self.redactor.redact(&event.message);
        redact_strings(&mut event.metadata, &self.redactor);
        self.events.send(&event).await
    }

    async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
        let spawn = {
            let mut state = self.inner.lock_state();
            if let Some(reason) = &state.delivery_stopped {
                return Err(ReporterInner::stopped_error(reason));
            }
            let Some(log) = bounded_log(
                log,
                self.inner.log_bounds,
                state.budget_used,
                &self.redactor,
            ) else {
                return Ok(());
            };
            let bytes = log.message.len();
            self.inner.make_room(&mut state, bytes);
            // The budget is reserved when a line is accepted so the local
            // accounting matches what the control plane will receive.
            state.budget_used += u64::try_from(bytes).unwrap_or(u64::MAX);
            state.queued_bytes += bytes;
            state.queue.push_back(log);
            if state.queue.len() >= self.inner.batch_max_lines
                || state.queued_bytes >= MAX_BATCH_BYTES
            {
                self.inner.batch_ready.notify_one();
            }
            !std::mem::replace(&mut state.worker_running, true)
        };
        if spawn {
            self.inner.spawn_worker();
        }
        Ok(())
    }

    async fn flush(&self) -> Result<(), RuntimeExecutionError> {
        self.flush_within(self.inner.policy.flush_grace).await
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

fn redact_strings(value: &mut Value, redactor: &ValueRedactor) {
    match value {
        Value::String(text) => *text = redactor.redact(text),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_strings(item, redactor)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| redact_strings(field, redactor)),
        _ => {}
    }
}

fn bounded_log(
    mut log: DeploymentLog,
    bounds: LogBounds,
    sent: u64,
    redactor: &ValueRedactor,
) -> Option<DeploymentLog> {
    // A zero-line batch policy means no log may be emitted.
    if bounds.max_batch_lines == Some(0) {
        return None;
    }
    log.message = sanitise_line(&redactor.redact(&log.message));
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

    use super::{LogBounds, ValueRedactor, bounded_log as bounded_log_with, redact_strings};

    fn bounded_log(log: DeploymentLog, bounds: LogBounds, sent: u64) -> Option<DeploymentLog> {
        bounded_log_with(log, bounds, sent, &ValueRedactor::default())
    }

    #[test]
    fn a_command_value_is_redacted_before_a_line_is_truncated() {
        let redactor = ValueRedactor::new(["kL9-unusual-value".to_owned()]);
        let line = bounded_log_with(
            log("using kL9-unusual-value"),
            LogBounds {
                max_line_length: Some(12),
                ..LogBounds::default()
            },
            0,
            &redactor,
        )
        .expect("line fits");

        assert_eq!(line.message, "using [REDAC");
    }

    #[test]
    fn event_metadata_is_redacted_at_any_depth() {
        let redactor = ValueRedactor::new(["kL9-unusual-value".to_owned()]);
        let mut metadata = serde_json::json!({
            "plain": "kL9-unusual-value",
            "nested": { "list": ["a kL9-unusual-value b", 7] },
        });

        redact_strings(&mut metadata, &redactor);

        assert_eq!(
            metadata,
            serde_json::json!({
                "plain": "[REDACTED]",
                "nested": { "list": ["a [REDACTED] b", 7] },
            })
        );
    }

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
