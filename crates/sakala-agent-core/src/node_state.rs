use std::sync::atomic::{AtomicU8, Ordering};

/// State maintenance node yang dibagikan worker Agent dalam satu process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeLifecycleState {
    Active = 0,
    Draining = 1,
    Drained = 2,
    Maintenance = 3,
}

impl NodeLifecycleState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Draining,
            2 => Self::Drained,
            3 => Self::Maintenance,
            _ => Self::Active,
        }
    }
}

#[derive(Debug)]
pub struct NodeLifecycle {
    state: AtomicU8,
}

impl Default for NodeLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeLifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self::with_state(NodeLifecycleState::Active)
    }

    #[must_use]
    pub fn with_state(state: NodeLifecycleState) -> Self {
        Self {
            state: AtomicU8::new(state as u8),
        }
    }

    #[must_use]
    pub fn state(&self) -> NodeLifecycleState {
        NodeLifecycleState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn set(&self, state: NodeLifecycleState) {
        self.state.store(state as u8, Ordering::Release);
    }

    #[must_use]
    pub fn accepts_workload_commands(&self) -> bool {
        self.state() == NodeLifecycleState::Active
    }
}

impl From<sakala_agent_protocol::DesiredNodeLifecycleState> for NodeLifecycleState {
    fn from(value: sakala_agent_protocol::DesiredNodeLifecycleState) -> Self {
        match value {
            sakala_agent_protocol::DesiredNodeLifecycleState::Active => Self::Active,
            sakala_agent_protocol::DesiredNodeLifecycleState::Draining => Self::Draining,
            sakala_agent_protocol::DesiredNodeLifecycleState::Drained => Self::Drained,
            sakala_agent_protocol::DesiredNodeLifecycleState::Maintenance => Self::Maintenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeLifecycle, NodeLifecycleState};

    #[test]
    fn drain_state_rejects_new_workload_commands() {
        let lifecycle = NodeLifecycle::new();
        assert!(lifecycle.accepts_workload_commands());
        lifecycle.set(NodeLifecycleState::Draining);
        assert!(!lifecycle.accepts_workload_commands());
        lifecycle.set(NodeLifecycleState::Active);
        assert!(lifecycle.accepts_workload_commands());
    }

    #[test]
    fn bootstrap_state_is_applied_before_work_is_accepted() {
        let lifecycle = NodeLifecycle::with_state(NodeLifecycleState::Drained);
        assert_eq!(lifecycle.state(), NodeLifecycleState::Drained);
        assert!(!lifecycle.accepts_workload_commands());
    }
}
