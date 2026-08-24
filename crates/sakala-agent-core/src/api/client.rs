use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder, Url, header::ACCEPT};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CompleteCommandPayload, DeploymentEvent, DeploymentLog,
    HeartbeatPayload, NodeLifecyclePayload,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    AgentConfig, CoreError,
    ports::{RepositoryCredential, SecretString},
};

use super::endpoints;

/// Authenticated outbound client for the Sakala control-plane agent API.
#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    base_url: String,
    agent_id: String,
    token: String,
}

impl ApiClient {
    pub fn from_config(config: &AgentConfig) -> Result<Self, CoreError> {
        let token = config.agent_token.clone().ok_or_else(|| {
            CoreError::InvalidConfiguration("connected mode requires SAKALA_AGENT_TOKEN".to_owned())
        })?;

        Self::new(&config.api_url, &config.agent_id, token)
    }

    pub fn new(
        base_url: impl AsRef<str>,
        agent_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let base_url = base_url.as_ref().trim_end_matches('/').to_owned();
        let parsed_url = Url::parse(&base_url).map_err(|error| {
            CoreError::InvalidConfiguration(format!("SAKALA_API_URL is invalid: {error}"))
        })?;

        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(CoreError::InvalidConfiguration(
                "SAKALA_API_URL must use http or https".to_owned(),
            ));
        }

        let agent_id = agent_id.into();
        let token = token.into();

        if agent_id.trim().is_empty() || token.trim().is_empty() {
            return Err(CoreError::InvalidConfiguration(
                "agent id and token must not be empty".to_owned(),
            ));
        }

        let http = Client::builder()
            // Agent requests carry a machine credential. The control plane URL is an
            // explicit runtime setting, so do not leak that traffic through ambient
            // HTTP proxy environment variables.
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("sakala-agent/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            http,
            base_url,
            agent_id,
            token,
        })
    }

    pub async fn poll_commands(&self) -> Result<Vec<AgentCommand>, CoreError> {
        let response = self
            .request(Method::GET, endpoints::COMMANDS)
            .send()
            .await?
            .error_for_status()?;

        let envelope = response.json::<ApiEnvelope<Vec<AgentCommand>>>().await?;

        Ok(envelope.data)
    }

    pub async fn heartbeat(&self, payload: &HeartbeatPayload) -> Result<(), CoreError> {
        self.post(endpoints::HEARTBEAT, payload).await
    }

    /// Fetches the authoritative lifecycle state before the scheduler can claim work.
    pub async fn node_lifecycle(&self) -> Result<NodeLifecyclePayload, CoreError> {
        let response = self
            .request(Method::GET, endpoints::NODE_STATE)
            .send()
            .await?
            .error_for_status()?;
        Ok(response
            .json::<ApiEnvelope<NodeLifecyclePayload>>()
            .await?
            .data)
    }

    pub async fn claim(&self, command_id: Uuid) -> Result<(), CoreError> {
        let response = self
            .request(
                Method::POST,
                &endpoints::command_action(command_id, "claim"),
            )
            .json(&json!({}))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            return Err(CoreError::CommandNotClaimable);
        }
        response.error_for_status()?;
        Ok(())
    }

    pub async fn event(
        &self,
        command_id: Uuid,
        payload: &DeploymentEvent,
    ) -> Result<(), CoreError> {
        self.post(&endpoints::command_action(command_id, "events"), payload)
            .await
    }

    pub async fn log(&self, command_id: Uuid, payload: &DeploymentLog) -> Result<(), CoreError> {
        self.post(&endpoints::command_action(command_id, "logs"), payload)
            .await
    }

    pub async fn complete(
        &self,
        command_id: Uuid,
        payload: &CompleteCommandPayload,
    ) -> Result<(), CoreError> {
        self.post_terminal(
            &endpoints::command_action(command_id, "complete"),
            payload,
            CommandStatus::Succeeded,
        )
        .await
    }

    pub async fn fail(
        &self,
        command_id: Uuid,
        error_code: &str,
        error_message: &str,
    ) -> Result<(), CoreError> {
        self.post_terminal(
            &endpoints::command_action(command_id, "fail"),
            &FailCommandPayload {
                error_code,
                error_message,
            },
            CommandStatus::Failed,
        )
        .await
    }

    pub async fn repository_credential(
        &self,
        command_id: Uuid,
    ) -> Result<RepositoryCredential, CoreError> {
        let response = self
            .request(Method::POST, &endpoints::repository_credential(command_id))
            .json(&json!({}))
            .send()
            .await?
            .error_for_status()?;
        let payload = response.json::<RepositoryCredentialLeasePayload>().await?;
        if payload.username.trim().is_empty() || payload.token.trim().is_empty() {
            return Err(CoreError::InvalidConfiguration(
                "repository credential lease is missing username or token".to_owned(),
            ));
        }
        Ok(RepositoryCredential {
            username: payload.username,
            token: SecretString::new(payload.token),
        })
    }

    fn request(&self, method: Method, endpoint: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{endpoint}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-Agent-Id", &self.agent_id)
            .header(ACCEPT, "application/json")
    }

    async fn post<T: Serialize + ?Sized>(
        &self,
        endpoint: &str,
        payload: &T,
    ) -> Result<(), CoreError> {
        self.request(Method::POST, endpoint)
            .json(payload)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    async fn post_terminal<T: Serialize + ?Sized>(
        &self,
        endpoint: &str,
        payload: &T,
        expected: CommandStatus,
    ) -> Result<(), CoreError> {
        let response = self
            .request(Method::POST, endpoint)
            .json(payload)
            .send()
            .await?;
        if response.status() != reqwest::StatusCode::CONFLICT {
            response.error_for_status()?;
            return Ok(());
        }
        let terminal = response.json::<TerminalConflictPayload>().await?;
        if terminal.status == expected {
            return Ok(());
        }
        Err(CoreError::CommandTerminalConflict(format!(
            "{:?}",
            terminal.status
        )))
    }
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Debug, Serialize)]
struct FailCommandPayload<'a> {
    error_code: &'a str,
    error_message: &'a str,
}

#[derive(Deserialize)]
struct TerminalConflictPayload {
    status: CommandStatus,
}

#[derive(Deserialize)]
struct RepositoryCredentialLeasePayload {
    username: String,
    token: String,
}
