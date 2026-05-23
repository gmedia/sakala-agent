use reqwest::{Client, Method, RequestBuilder};
use sakala_agent_protocol::{AgentCommand, DeploymentEvent, DeploymentLog, HeartbeatPayload};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::{AgentConfig, CoreError};

use super::endpoints;

/// Authenticated outbound client for the dashboard agent API.
#[derive(Clone, Debug)]
pub struct DashboardClient {
    http: Client,
    base_url: String,
    agent_id: String,
    token: String,
}

impl DashboardClient {
    pub fn from_config(config: &AgentConfig) -> Result<Self, CoreError> {
        let token = config.agent_token.clone().ok_or_else(|| {
            CoreError::InvalidConfiguration("connected mode requires SAKALA_AGENT_TOKEN".to_owned())
        })?;

        Ok(Self {
            http: Client::new(),
            base_url: config.dashboard_url.trim_end_matches('/').to_owned(),
            agent_id: config.agent_id.clone(),
            token,
        })
    }

    pub async fn poll_commands(&self) -> Result<Vec<AgentCommand>, CoreError> {
        let response = self
            .request(Method::GET, endpoints::COMMANDS)
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json().await?)
    }

    pub async fn heartbeat(&self, payload: &HeartbeatPayload) -> Result<(), CoreError> {
        self.post(endpoints::HEARTBEAT, payload).await
    }

    pub async fn claim(&self, command_id: Uuid) -> Result<(), CoreError> {
        self.post(&endpoints::command_action(command_id, "claim"), &json!({}))
            .await
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

    pub async fn complete(&self, command_id: Uuid) -> Result<(), CoreError> {
        self.post(
            &endpoints::command_action(command_id, "complete"),
            &json!({}),
        )
        .await
    }

    pub async fn fail(&self, command_id: Uuid, error: &str) -> Result<(), CoreError> {
        self.post(
            &endpoints::command_action(command_id, "fail"),
            &json!({ "error": error }),
        )
        .await
    }

    fn request(&self, method: Method, endpoint: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{endpoint}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-Agent-Id", &self.agent_id)
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
}
