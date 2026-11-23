use std::{
    sync::{Mutex as StdMutex, MutexGuard},
    time::Duration,
};

use backon::{BackoffBuilder, ExponentialBuilder};
use reqwest::StatusCode;
use sakala_agent_protocol::DeploymentEvent;
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use super::runtime::{MAX_RETRY_AFTER, RETRY_MAX_DELAY, RETRY_MIN_DELAY};
use crate::{
    CoreError,
    api::{ApiClient, ReportAttemptError},
    ports::RuntimeExecutionError,
};

/// Longest wall-clock time one event may spend in delivery, attempts and
/// waits included. It outlasts the 77-second API outage seen on staging on
/// 2026-11-19, and stays far below the default command timeout (900 s).
const DELIVERY_BUDGET: Duration = Duration::from_secs(90);

/// Retry limits for deployment events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventDeliveryPolicy {
    pub retry_min_delay: Duration,
    pub retry_max_delay: Duration,
    /// Longest wall-clock time one event may spend in delivery before it is
    /// given up.
    pub budget: Duration,
}

impl Default for EventDeliveryPolicy {
    fn default() -> Self {
        Self {
            retry_min_delay: RETRY_MIN_DELAY,
            retry_max_delay: RETRY_MAX_DELAY,
            budget: DELIVERY_BUDGET,
        }
    }
}

impl EventDeliveryPolicy {
    /// Exponential backoff with jitter; `budget` bounds how long it is used.
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

/// Delivers the deployment events of one command, one request per event.
///
/// Events are timeline telemetry: the deployment status they move forward is
/// only ever finished by `complete`/`fail`, which have their own strict path.
/// So an event the control plane cannot take in time is dropped with a
/// warning and counted, and the command keeps running.
///
/// Each event keeps one `Idempotency-Key` and the same body across attempts,
/// so a retry after a lost response is answered as a duplicate instead of
/// being stored twice. Transient failures (connection, timeout, unreadable
/// body, `408`, `429`, `5xx`) are retried with exponential backoff and jitter,
/// honouring `Retry-After`, until `budget` runs out. Once an event was given
/// up, later events get one attempt each until one is delivered again, so an
/// outage costs the command one budget rather than one per event.
///
/// `409` is the exception: the control plane no longer accepts reports for
/// the command (it is terminal, expired, or owned by another node), so the
/// work it describes should stop. It is returned as an error.
pub(crate) struct EventDelivery {
    client: ApiClient,
    command_id: Uuid,
    policy: EventDeliveryPolicy,
    state: StdMutex<DeliveryState>,
}

#[derive(Default)]
struct DeliveryState {
    /// The last event was given up and none was delivered since.
    degraded: bool,
    undelivered: u64,
}

enum Outcome {
    Delivered,
    Dropped(String),
    Conflict(CoreError),
}

impl EventDelivery {
    pub(crate) fn new(client: ApiClient, command_id: Uuid, policy: EventDeliveryPolicy) -> Self {
        Self {
            client,
            command_id,
            policy,
            state: StdMutex::new(DeliveryState::default()),
        }
    }

    /// Number of events of this command that were given up.
    pub(crate) fn undelivered(&self) -> u64 {
        self.lock_state().undelivered
    }

    /// Delivers one event, or gives it up with a warning. Fails only when the
    /// control plane answers `409`.
    pub(crate) async fn send(&self, event: &DeploymentEvent) -> Result<(), RuntimeExecutionError> {
        let degraded = self.lock_state().degraded;
        match self.deliver(event, degraded).await {
            Outcome::Delivered => {
                self.lock_state().degraded = false;
                Ok(())
            }
            Outcome::Conflict(error) => Err(RuntimeExecutionError::reporting(error.to_string())),
            Outcome::Dropped(reason) => {
                let undelivered = {
                    let mut state = self.lock_state();
                    state.degraded = true;
                    state.undelivered += 1;
                    state.undelivered
                };
                warn!(
                    command_id = %self.command_id,
                    event_type = %event.event_type,
                    undelivered_events = undelivered,
                    error = %reason,
                    "deployment event was not delivered; the command continues without it"
                );
                Ok(())
            }
        }
    }

    async fn deliver(&self, event: &DeploymentEvent, degraded: bool) -> Outcome {
        let idempotency_key = Uuid::new_v4().to_string();
        let deadline = Instant::now() + self.policy.budget;
        let mut backoff = self.policy.backoff();
        let mut attempt: u32 = 1;
        loop {
            let attempt_outcome = tokio::time::timeout_at(
                deadline,
                self.client.events_attempt(
                    self.command_id,
                    &idempotency_key,
                    std::slice::from_ref(event),
                ),
            )
            .await;
            let Ok(attempt_outcome) = attempt_outcome else {
                return Outcome::Dropped(format!(
                    "not acknowledged within {} s",
                    self.policy.budget.as_secs()
                ));
            };
            let (detail, retry_after) = match attempt_outcome {
                Ok(_) => {
                    if attempt > 1 {
                        info!(
                            command_id = %self.command_id,
                            event_type = %event.event_type,
                            attempts = attempt,
                            "deployment event delivered after retrying"
                        );
                    }
                    return Outcome::Delivered;
                }
                Err(ReportAttemptError::Rejected(error)) if is_conflict(&error) => {
                    return Outcome::Conflict(error);
                }
                Err(ReportAttemptError::Rejected(error)) => {
                    return Outcome::Dropped(error.to_string());
                }
                Err(ReportAttemptError::Transient {
                    detail,
                    retry_after,
                }) => (detail, retry_after),
            };
            if degraded {
                return Outcome::Dropped(format!(
                    "{detail}; not retried because the previous event was not delivered either"
                ));
            }
            let backoff_delay = backoff.next().unwrap_or(self.policy.retry_max_delay);
            let delay = retry_after.map_or(backoff_delay, |requested| {
                backoff_delay.max(requested.min(MAX_RETRY_AFTER))
            });
            if Instant::now() + delay >= deadline {
                return Outcome::Dropped(format!(
                    "not acknowledged within {} s: {detail}",
                    self.policy.budget.as_secs()
                ));
            }
            warn!(
                command_id = %self.command_id,
                event_type = %event.event_type,
                error = %detail,
                attempt,
                delay_ms = delay.as_millis(),
                "deployment event will be retried with the same idempotency key"
            );
            tokio::time::sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, DeliveryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn is_conflict(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::ReportRejected { status, .. } if *status == StatusCode::CONFLICT.as_u16()
    )
}
