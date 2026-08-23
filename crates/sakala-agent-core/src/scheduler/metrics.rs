use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared scheduler counters exposed through the node heartbeat.
///
/// Commands that cannot start remain pending at the control plane; the Agent
/// intentionally has no unbounded local queue. `queued_local_commands` is
/// therefore always zero today, but is reported explicitly so its meaning is
/// unambiguous if local queuing is introduced later.
#[derive(Default)]
pub struct SchedulerMetrics {
    active_commands: AtomicUsize,
}

impl SchedulerMetrics {
    pub fn command_started(&self) {
        self.active_commands.fetch_add(1, Ordering::Relaxed);
    }

    pub fn command_finished(&self) {
        self.active_commands.fetch_sub(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn active_commands(&self) -> usize {
        self.active_commands.load(Ordering::Relaxed)
    }

    #[must_use]
    pub const fn queued_local_commands(&self) -> usize {
        0
    }
}
