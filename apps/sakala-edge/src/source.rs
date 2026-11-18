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
/// `User-Agent` of every request, so the API can tell Edge versions apart.
pub const USER_AGENT: &str = concat!("sakala-edge/", env!("CARGO_PKG_VERSION"));
/// Request header carrying the revision Caddy currently serves.
pub const APPLIED_REVISION_HEADER: &str = "Sakala-Edge-Applied-Revision";
/// [`APPLIED_REVISION_HEADER`] value before any table was applied.
pub const NO_APPLIED_REVISION: &str = "none";
/// Request header carrying the outcome of the last sync cycle.
pub const STATUS_HEADER: &str = "Sakala-Edge-Status";
/// [`STATUS_HEADER`] value when the last cycle succeeded, a 304 or an
/// unchanged revision included. Otherwise the header carries the error code
/// `status.json` records as `last_error.code`.
pub const STATUS_OK: &str = "ok";

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

/// What Edge reports about itself with every request, so the API can alert
/// when an Edge serves a stale table or keeps failing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedState<'a> {
    /// The revision Caddy serves, `None` before any table was applied. Also
    /// sent as the `If-None-Match` entity tag.
    pub revision: Option<&'a str>,
    /// [`STATUS_OK`] or the error code of the last cycle.
    pub status: &'a str,
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
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http, url, token })
    }

    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches the table, reporting `applied` in the Edge headers. With an
    /// applied revision, also sends it as an `If-None-Match` entity tag
    /// (`"<revision>"`), so an API that supports conditional requests can
    /// answer 304 instead of the whole table. An API that ignores the header
    /// answers 200 as usual.
    pub async fn fetch(&self, applied: AppliedState<'_>) -> Result<Fetched, FetchError> {
        let revision = applied.revision;
        let mut request = self
            .http
            .get(self.url.clone())
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token.expose()))
            .header(
                APPLIED_REVISION_HEADER,
                revision.unwrap_or(NO_APPLIED_REVISION),
            )
            .header(STATUS_HEADER, applied.status);
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
