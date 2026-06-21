use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid agent configuration: {0}")]
    InvalidConfiguration(String),

    #[error("control-plane API request failed: {0}")]
    Api(#[from] reqwest::Error),

    #[error("runtime execution failed: {0}")]
    Runtime(#[from] sakala_agent_runtime::RuntimeError),
}
