use std::time::Duration;

#[must_use]
pub fn exponential_delay(attempt: u32, base: Duration, maximum: Duration) -> Duration {
    let multiplier = 2_u32.saturating_pow(attempt.min(16));
    base.saturating_mul(multiplier).min(maximum)
}
