use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid agent configuration: {0}")]
    InvalidConfiguration(String),

    #[error("dashboard request failed: {0}")]
    Dashboard(#[from] reqwest::Error),

    #[error("runtime execution failed: {0}")]
    Runtime(#[from] sakala_agent_runtime::RuntimeError),
}
