//! A reading a heartbeat part takes at most once per interval and sends once,
//! in the heartbeat that follows a successful read, like `workloads.metrics`.
//! Shared by the managed resource roles (`metadata.databases`,
//! `metadata.buckets`), whose reads may be slow and whose failure makes the
//! node `degraded`.

use std::{
    future::Future,
    sync::Mutex,
    time::{Duration, Instant},
};

use tracing::{info, warn};

use crate::ports::RuntimeExecutionError;

/// What to log when the reads start or stop failing.
pub(crate) struct ReadMessages {
    pub failing: &'static str,
    pub recovered: &'static str,
}

/// The error code a read returns when it was skipped because the instance is
/// busy, which is neither a reading nor a failure.
pub(crate) type BusyCode = Option<&'static str>;

pub(crate) struct PeriodicRead {
    interval: Duration,
    timeout: Duration,
    messages: ReadMessages,
    busy_code: BusyCode,
    state: Mutex<ReadState>,
}

#[derive(Default)]
struct ReadState {
    last_attempt: Option<Instant>,
    failing: bool,
}

impl PeriodicRead {
    pub(crate) fn new(
        interval: Duration,
        timeout: Duration,
        messages: ReadMessages,
        busy_code: BusyCode,
    ) -> Self {
        Self {
            interval,
            timeout,
            messages,
            busy_code,
            state: Mutex::new(ReadState::default()),
        }
    }

    /// Whether the last read succeeded. A node whose reads fail stays
    /// `degraded` until one succeeds.
    pub(crate) fn available(&self) -> bool {
        !self.state.lock().map(|state| state.failing).unwrap_or(true)
    }

    /// Reads when no attempt was made within the interval. A failed read waits
    /// for the next interval too, so an instance that hangs costs one timeout
    /// per interval rather than one per heartbeat. A read skipped as busy keeps
    /// the last state and is tried again at the next heartbeat.
    pub(crate) async fn read_if_due<T, F, Fut>(&self, read: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, RuntimeExecutionError>>,
    {
        {
            let mut state = self.state.lock().ok()?;
            if state
                .last_attempt
                .is_some_and(|last| last.elapsed() < self.interval)
            {
                return None;
            }
            state.last_attempt = Some(Instant::now());
        }
        let error = match tokio::time::timeout(self.timeout, read()).await {
            Ok(Ok(value)) => {
                self.set_failing(false, None);
                return Some(value);
            }
            Ok(Err(error)) if self.busy_code == Some(error.code()) => {
                if let Ok(mut state) = self.state.lock() {
                    state.last_attempt = None;
                }
                return None;
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => format!(
                "the read did not finish within {} seconds",
                self.timeout.as_secs_f64()
            ),
        };
        self.set_failing(true, Some(&error));
        None
    }

    fn set_failing(&self, failing: bool, error: Option<&str>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let was_failing = std::mem::replace(&mut state.failing, failing);
        match (was_failing, failing) {
            (false, true) => warn!(
                error = error.unwrap_or_default(),
                "{}", self.messages.failing
            ),
            (true, false) => info!("{}", self.messages.recovered),
            _ => {}
        }
    }
}
