use std::time::Duration;

use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url, header::ACCEPT};
use sakala_agent_protocol::{
    AgentCommand, CommandStatus, CompleteCommandPayload, DeploymentEvent, DeploymentLog,
    HeartbeatPayload, NodeLifecyclePayload,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    AgentConfig, CoreError,
    ports::{RepositoryCredential, SecretString},
    support::retry::RetryPolicy,
};

use super::endpoints;

/// Control-plane sanitisation limits for `fail` bodies. The agent applies the
/// same limits before sending so a report is never rejected for cosmetic
/// reasons after the runtime already failed.
const MAX_ERROR_CODE_CHARS: usize = 64;
const MAX_ERROR_MESSAGE_CHARS: usize = 1_000;

/// Authenticated outbound client for the Sakala control-plane agent API.
#[derive(Clone)]
pub struct ApiClient {
    http: Client,
    base_url: String,
    agent_id: String,
    token: String,
    retry: RetryPolicy,
}

/// Acknowledgement returned by the batch report endpoints.
///
/// `accepted_count` is the number of items in the request; `duplicate_count`
/// is the subset the control plane had already persisted under the same
/// `Idempotency-Key`. All fields are mandatory on a `200` response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReportAcknowledgement {
    pub accepted_count: u64,
    pub duplicate_count: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
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
            retry: RetryPolicy::default(),
        })
    }

    /// Overrides the bounded retry policy used for idempotent requests.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
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
        self.request(Method::POST, endpoints::HEARTBEAT)
            .json(payload)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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

    /// Claims a pending command. The claim is deliberately not retried: a
    /// repeated claim after an ambiguous network failure would observe `409`
    /// for its own successful first attempt and skip the command. The
    /// response body (full command resource) is intentionally ignored; the
    /// polled record is the payload source.
    pub async fn claim(&self, command_id: Uuid) -> Result<(), CoreError> {
        let response = self
            .request(
                Method::POST,
                &endpoints::command_action(command_id, "claim"),
            )
            .json(&json!({}))
            .send()
            .await?;
        if response.status() == StatusCode::CONFLICT {
            return Err(CoreError::CommandNotClaimable);
        }
        response.error_for_status()?;
        Ok(())
    }

    pub async fn event(
        &self,
        command_id: Uuid,
        payload: &DeploymentEvent,
    ) -> Result<ReportAcknowledgement, CoreError> {
        self.events(command_id, std::slice::from_ref(payload)).await
    }

    /// Reports one batch of events under one `Idempotency-Key`. Retries reuse
    /// the same key so the control plane deduplicates repeated items.
    pub async fn events(
        &self,
        command_id: Uuid,
        events: &[DeploymentEvent],
    ) -> Result<ReportAcknowledgement, CoreError> {
        self.report(
            &endpoints::command_action(command_id, "events"),
            &json!({ "events": events }),
            events.len(),
        )
        .await
    }

    pub async fn log(
        &self,
        command_id: Uuid,
        payload: &DeploymentLog,
    ) -> Result<ReportAcknowledgement, CoreError> {
        self.logs(command_id, std::slice::from_ref(payload)).await
    }

    /// Reports one batch of redacted log lines under one `Idempotency-Key`.
    pub async fn logs(
        &self,
        command_id: Uuid,
        logs: &[DeploymentLog],
    ) -> Result<ReportAcknowledgement, CoreError> {
        self.report(
            &endpoints::command_action(command_id, "logs"),
            &json!({ "logs": logs }),
            logs.len(),
        )
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
                error_code: &sanitize_error_code(error_code),
                error_message: &sanitize_error_message(error_message),
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

    /// Sends one report batch under one `Idempotency-Key`.
    ///
    /// A `204` from an older control plane is accepted as delivered. A `200`
    /// must carry a valid acknowledgement envelope: a body that cannot be read
    /// is retried under the same key (that is what the key is for), while a
    /// body that parses but does not match the contract is surfaced as a
    /// delivery failure so a batch is never dropped as "acknowledged" on a
    /// broken response. The acknowledgement must account for every item of
    /// the batch (`accepted_count == item_count`); a partial acknowledgement is
    /// equally undelivered.
    async fn report<T: Serialize + ?Sized>(
        &self,
        endpoint: &str,
        payload: &T,
        item_count: usize,
    ) -> Result<ReportAcknowledgement, CoreError> {
        let idempotency_key = Uuid::new_v4().to_string();
        let attempts = self.retry.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            let outcome = self
                .request(Method::POST, endpoint)
                .header("Idempotency-Key", &idempotency_key)
                .json(payload)
                .send()
                .await;
            let transient = match outcome {
                Ok(response) if is_retryable_status(response.status()) => {
                    format!("HTTP {}", response.status().as_u16())
                }
                Ok(response) => match Self::report_outcome(response, item_count).await {
                    Ok(acknowledgement) => return Ok(acknowledgement),
                    Err(CoreError::Api(error)) if is_retryable_transport_error(&error) => {
                        error.to_string()
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if is_retryable_transport_error(&error) => error.to_string(),
                Err(error) => return Err(CoreError::Api(error)),
            };
            if attempt + 1 >= attempts {
                return Err(CoreError::InvalidReportAcknowledgement(format!(
                    "report not acknowledged after {attempts} attempts: {transient}"
                )));
            }
            let delay = self.retry.delay_after(attempt);
            warn!(
                error = %transient,
                attempt = attempt + 1,
                delay_ms = delay.as_millis(),
                "control-plane report will be retried with the same idempotency key"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    async fn report_outcome(
        response: Response,
        item_count: usize,
    ) -> Result<ReportAcknowledgement, CoreError> {
        let status = response.status();
        if status == StatusCode::CONFLICT {
            return Err(CoreError::ReportRejected {
                status: status.as_u16(),
                detail: conflict_detail(response).await,
            });
        }
        if status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(CoreError::ReportRejected {
                status: status.as_u16(),
                detail: rejection_detail(response).await,
            });
        }
        let response = response.error_for_status()?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(ReportAcknowledgement::default());
        }
        if response.status() != StatusCode::OK {
            return Err(CoreError::InvalidReportAcknowledgement(format!(
                "unexpected HTTP {} for report",
                response.status().as_u16()
            )));
        }
        // Body read failures are transport errors and retried by the caller.
        let body = response.bytes().await?;
        let acknowledgement = serde_json::from_slice::<ApiEnvelope<ReportAcknowledgement>>(&body)
            .map(|envelope| envelope.data)
            .map_err(|error| {
                CoreError::InvalidReportAcknowledgement(format!(
                    "acknowledgement body does not match the contract: {error}"
                ))
            })?;
        // The control plane defines `accepted_count` as the number of items in
        // the request, duplicates included. Anything else means part of the
        // batch is not known to be persisted.
        if u64::try_from(item_count)
            .is_ok_and(|expected| acknowledgement.accepted_count != expected)
        {
            return Err(CoreError::InvalidReportAcknowledgement(format!(
                "acknowledgement accepted {} of {item_count} items: {acknowledgement:?}",
                acknowledgement.accepted_count
            )));
        }
        if acknowledgement.duplicate_count > acknowledgement.accepted_count
            || acknowledgement.last_sequence < acknowledgement.first_sequence
        {
            return Err(CoreError::InvalidReportAcknowledgement(format!(
                "acknowledgement counters are inconsistent: {acknowledgement:?}"
            )));
        }
        if acknowledgement.duplicate_count > 0 {
            debug!(
                duplicate_count = acknowledgement.duplicate_count,
                "control plane deduplicated retried report items"
            );
        }
        Ok(acknowledgement)
    }

    async fn post_terminal<T: Serialize + ?Sized>(
        &self,
        endpoint: &str,
        payload: &T,
        expected: CommandStatus,
    ) -> Result<(), CoreError> {
        let response = self
            .send_with_retry(|| self.request(Method::POST, endpoint).json(payload))
            .await?;
        if response.status() != StatusCode::CONFLICT {
            response.error_for_status()?;
            return Ok(());
        }
        let terminal = response.json::<TerminalConflictPayload>().await?;
        if terminal.status == expected {
            return Ok(());
        }
        Err(CoreError::CommandTerminalConflict(
            match terminal.terminal_at {
                Some(terminal_at) => format!("{:?} at {terminal_at}", terminal.status),
                None => format!("{:?}", terminal.status),
            },
        ))
    }

    /// Sends an idempotent request, retrying transport failures and
    /// `408`/`429`/`5xx` responses with bounded exponential backoff. Any other
    /// response is returned to the caller for status-specific handling.
    async fn send_with_retry(
        &self,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response, CoreError> {
        let attempts = self.retry.max_attempts.max(1);
        let mut attempt = 0;
        loop {
            let outcome = build().send().await;
            let retryable = match &outcome {
                Ok(response) => is_retryable_status(response.status()),
                Err(error) => is_retryable_transport_error(error),
            };
            if !retryable || attempt + 1 >= attempts {
                return outcome.map_err(CoreError::Api);
            }
            let delay = self.retry.delay_after(attempt);
            match &outcome {
                Ok(response) => warn!(
                    status = response.status().as_u16(),
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis(),
                    "control-plane request will be retried"
                ),
                Err(error) => warn!(
                    %error,
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis(),
                    "control-plane request will be retried"
                ),
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
}

async fn conflict_detail(response: Response) -> String {
    match response.json::<ConflictPayload>().await {
        Ok(ConflictPayload {
            status: Some(status),
            terminal_at: Some(terminal_at),
            ..
        }) => format!("command is {status:?} at {terminal_at}"),
        Ok(ConflictPayload {
            status: Some(status),
            ..
        }) => format!("command is {status:?}"),
        Ok(ConflictPayload {
            message: Some(message),
            ..
        }) => message,
        _ => "command conflict".to_owned(),
    }
}

async fn rejection_detail(response: Response) -> String {
    response
        .json::<RejectionPayload>()
        .await
        .ok()
        .and_then(|payload| payload.message)
        .unwrap_or_else(|| "report rejected by control plane".to_owned())
}

/// Restricts the code to `[A-Za-z0-9._-]` and 64 characters as the control
/// plane does, so a rejected code never masks the runtime failure.
fn sanitize_error_code(code: &str) -> String {
    let sanitized = code
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(MAX_ERROR_CODE_CHARS)
        .collect::<String>();
    if sanitized.is_empty() {
        "runtime_execution_failed".to_owned()
    } else {
        sanitized
    }
}

/// Removes control, bidi, and zero-width characters and bounds the message to
/// 1000 characters, matching the control-plane sanitisation.
fn sanitize_error_message(message: &str) -> String {
    let sanitized = message
        .chars()
        .filter(|character| !is_disallowed_message_char(*character))
        .take(MAX_ERROR_MESSAGE_CHARS)
        .collect::<String>();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "runtime execution failed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn is_disallowed_message_char(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
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
    #[serde(default)]
    terminal_at: Option<String>,
}

#[derive(Deserialize)]
struct ConflictPayload {
    #[serde(default)]
    status: Option<CommandStatus>,
    #[serde(default)]
    terminal_at: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct RejectionPayload {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct RepositoryCredentialLeasePayload {
    username: String,
    token: String,
}

#[cfg(test)]
mod tests {
    use super::{sanitize_error_code, sanitize_error_message};

    #[test]
    fn error_code_is_restricted_to_the_control_plane_alphabet() {
        assert_eq!(
            sanitize_error_code("runtime build/failed: x"),
            "runtime_build_failed__x"
        );
        assert_eq!(sanitize_error_code("").as_str(), "runtime_execution_failed");
        assert_eq!(sanitize_error_code(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn error_message_drops_control_and_bidi_characters_and_is_bounded() {
        assert_eq!(
            sanitize_error_message("git\u{202E} failed\n\u{200B}badly\u{FEFF}"),
            "git failedbadly"
        );
        assert_eq!(
            sanitize_error_message(&"x".repeat(2_000)).chars().count(),
            1_000
        );
        assert_eq!(sanitize_error_message("\n\t"), "runtime execution failed");
    }
}
