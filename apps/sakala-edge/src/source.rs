//! Fetches the route table from the control plane.

use std::time::Duration;

use reqwest::{
    Client, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, IF_NONE_MATCH},
};
use sakala_agent_core::ports::SecretString;
use thiserror::Error;

/// Path of the route table under `SAKALA_API_URL`.
pub const ROUTES_PATH: &str = "/api/edge/v1/routes";
/// How long one fetch may take, connection included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest response body Edge reads. A table at the route limit with a few
/// upstreams each is well below this.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("the route table request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("the API answered the route table request with HTTP {0}")]
    Status(StatusCode),
    #[error("the route table response is larger than {MAX_BODY_BYTES} bytes")]
    BodyTooLarge,
}

impl FetchError {
    /// A stable code for the status file and logs.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Transport(_) => "api_unreachable",
            Self::Status(_) => "api_status",
            Self::BodyTooLarge => "body_too_large",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Fetched {
    /// A response body, not yet parsed or checked.
    Body(Vec<u8>),
    /// HTTP 304: the table still has the revision Edge sent in `If-None-Match`.
    NotModified,
}

/// Authenticated reader of the Edge route table. Its token can only read the
/// table (ADR-019) and is never logged.
#[derive(Clone)]
pub struct RouteSource {
    http: Client,
    url: Url,
    token: SecretString,
}

impl RouteSource {
    pub fn new(api_url: &Url, token: SecretString) -> Result<Self, reqwest::Error> {
        let mut url = api_url.clone();
        let base = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{base}{ROUTES_PATH}"));
        let http = Client::builder()
            // The request carries a credential to an explicit URL; it must not
            // leave through an ambient proxy, same as the Agent's client.
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("sakala-edge/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, url, token })
    }

    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches the table. With `revision`, sends it as an `If-None-Match`
    /// entity tag (`"<revision>"`), so an API that supports conditional
    /// requests can answer 304 instead of the whole table. An API that ignores
    /// the header answers 200 as usual.
    pub async fn fetch(&self, revision: Option<&str>) -> Result<Fetched, FetchError> {
        let mut request = self
            .http
            .get(self.url.clone())
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token.expose()));
        if let Some(revision) = revision {
            request = request.header(IF_NONE_MATCH, format!("\"{revision}\""));
        }
        let mut response = request.send().await?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_MODIFIED if revision.is_some() => return Ok(Fetched::NotModified),
            status => return Err(FetchError::Status(status)),
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(FetchError::BodyTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(FetchError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Fetched::Body(body))
    }
}
