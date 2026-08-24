use async_trait::async_trait;
use sakala_agent_protocol::RepositoryAccess;
use uuid::Uuid;

use crate::{
    api::ApiClient,
    ports::{RepositoryCredential, RepositoryCredentialProvider, RuntimeExecutionError},
};

pub struct ApiRepositoryCredentialProvider {
    client: ApiClient,
}

impl ApiRepositoryCredentialProvider {
    #[must_use]
    pub fn new(client: ApiClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RepositoryCredentialProvider for ApiRepositoryCredentialProvider {
    async fn credential(
        &self,
        command_id: Uuid,
        access: RepositoryAccess,
    ) -> Result<Option<RepositoryCredential>, RuntimeExecutionError> {
        match access {
            RepositoryAccess::Public => Ok(None),
            RepositoryAccess::TemporaryCredential => self
                .client
                .repository_credential(command_id)
                .await
                .map(Some)
                .map_err(|error| {
                    RuntimeExecutionError::new(
                        "repository_credential_unavailable",
                        format!("could not obtain temporary repository credential: {error}"),
                    )
                }),
        }
    }
}
