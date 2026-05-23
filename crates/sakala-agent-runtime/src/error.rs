use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime executor failed: {0}")]
    Execution(String),
}
