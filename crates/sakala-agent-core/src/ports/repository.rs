use std::fmt;

use async_trait::async_trait;
use sakala_agent_protocol::RepositoryAccess;
use uuid::Uuid;

use super::RuntimeExecutionError;

/// Secret material that must never appear in `Debug` output or tracing fields.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// One-use repository credential leased by the control plane after command claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryCredential {
    pub username: String,
    pub token: SecretString,
}

#[async_trait]
pub trait RepositoryCredentialProvider: Send + Sync {
    async fn credential(
        &self,
        command_id: Uuid,
        access: RepositoryAccess,
    ) -> Result<Option<RepositoryCredential>, RuntimeExecutionError>;
}

pub struct UnavailableRepositoryCredentialProvider;

#[async_trait]
impl RepositoryCredentialProvider for UnavailableRepositoryCredentialProvider {
    async fn credential(
        &self,
        _command_id: Uuid,
        access: RepositoryAccess,
    ) -> Result<Option<RepositoryCredential>, RuntimeExecutionError> {
        match access {
            RepositoryAccess::Public => Ok(None),
            RepositoryAccess::TemporaryCredential => Err(RuntimeExecutionError::new(
                "repository_credential_unavailable",
                "private repository checkout requires a connected control-plane credential lease",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SecretString;

    #[test]
    fn secret_debug_output_is_redacted() {
        assert_eq!(
            format!("{:?}", SecretString::new("credential-value")),
            "[REDACTED]"
        );
    }
}
