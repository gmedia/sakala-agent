use std::time::Duration;

/// Bounded retry policy for outbound control-plane requests that are safe to
/// repeat (idempotent reports and terminal transitions).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts including the first request. `1` disables retries.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// Policy without retries, for callers that must observe the first failure.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    /// Delay before the retry that follows the given zero-based failed attempt.
    #[must_use]
    pub fn delay_after(&self, failed_attempt: u32) -> Duration {
        exponential_delay(failed_attempt, self.base_delay, self.max_delay)
    }
}

#[must_use]
pub fn exponential_delay(attempt: u32, base: Duration, maximum: Duration) -> Duration {
    let multiplier = 2_u32.saturating_pow(attempt.min(16));
    base.saturating_mul(multiplier).min(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_grows_exponentially_and_is_capped() {
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        };
        assert_eq!(policy.delay_after(0), Duration::from_millis(100));
        assert_eq!(policy.delay_after(1), Duration::from_millis(200));
        assert_eq!(policy.delay_after(2), Duration::from_millis(350));
        assert_eq!(policy.delay_after(40), Duration::from_millis(350));
    }
}
