use async_trait::async_trait;
use sakala_agent_protocol::{AgentCommand, DeploymentEvent, DeploymentLog};

use crate::RuntimeError;

/// Output that the core worker reports back to the dashboard.
#[derive(Clone, Debug, Default)]
pub struct ExecutionOutcome {
    pub events: Vec<DeploymentEvent>,
    pub logs: Vec<DeploymentLog>,
}

/// Boundary for runtime implementations such as the future Docker executor.
#[async_trait]
pub trait RuntimeExecutor: Send + Sync {
    async fn execute(&self, command: &AgentCommand) -> Result<ExecutionOutcome, RuntimeError>;
}
