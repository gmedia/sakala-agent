use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid agent configuration: {0}")]
    InvalidConfiguration(String),

    #[error("control-plane API request failed: {0}")]
    Api(#[from] reqwest::Error),

    #[error("control-plane command is no longer claimable")]
    CommandNotClaimable,

    #[error("control-plane command already reached incompatible terminal state: {0}")]
    CommandTerminalConflict(String),

    /// The control plane rejected a report with a non-retryable status such as
    /// `409` (command terminal or not owned) or `422` (log budget exhausted).
    /// Callers must stop delivering further reports for that command.
    #[error("control-plane rejected report with HTTP {status}: {detail}")]
    ReportRejected { status: u16, detail: String },

    #[error("runtime execution failed: {0}")]
    Runtime(#[from] crate::ports::RuntimeExecutionError),
}

impl CoreError {
    /// Whether delivery of further reports for the same command is pointless.
    #[must_use]
    pub fn stops_report_delivery(&self) -> bool {
        matches!(
            self,
            Self::CommandTerminalConflict(_) | Self::ReportRejected { .. }
        )
    }
}
