//! Keeps the host Caddy serving the newest valid route table, and the last good
//! one whenever the API cannot provide a newer one.

use std::time::Duration;

use sakala_agent_core::support::retry::exponential_delay;
use serde::Serialize;
use thiserror::Error;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    caddy::{CaddyControl, CaddyError},
    files::EdgeFiles,
    render::{render, render_custom},
    source::{AppliedState, FetchError, Fetched, RouteSource, STATUS_OK},
    table::{
        CUSTOM_ROUTES_REJECTED, CustomRouteRejection, CustomRoutes, RoutePolicy, RouteTable,
        TableRejection,
    },
};

/// How often to poll and how far to back off after failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncTiming {
    pub poll_interval: Duration,
    pub max_backoff: Duration,
}

/// The wait before the next poll: the poll interval while things work, then
/// doubling after each consecutive failure up to `max_backoff`.
#[must_use]
pub fn next_delay(timing: SyncTiming, consecutive_failures: u32) -> Duration {
    exponential_delay(
        consecutive_failures,
        timing.poll_interval,
        timing.max_backoff,
    )
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("refused the route table: {0}")]
    Rejected(#[from] TableRejection),
    #[error(transparent)]
    Caddy(#[from] CaddyError),
    #[error("could not update {what}: {source}")]
    Storage {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl SyncError {
    /// A stable code for the status file and logs.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Fetch(error) => error.code(),
            Self::Rejected(rejection) => rejection.code(),
            Self::Caddy(CaddyError::Refused { .. }) => "caddy_refused",
            Self::Caddy(CaddyError::Unavailable { .. }) => "caddy_unavailable",
            Self::Storage { .. } => "storage",
        }
    }
}

fn storage(what: &'static str) -> impl FnOnce(std::io::Error) -> SyncError {
    move |source| SyncError::Storage { what, source }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncOutcome {
    /// Caddy now serves a new set of routes.
    Applied,
    /// A new revision with the same routes: the snapshot moved on, Caddy did not
    /// need a reload.
    SameRoutes,
    /// The API still has the revision Edge serves; nothing was written.
    Unchanged,
}

/// Where the routes Caddy serves came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutesSource {
    Snapshot,
    Api,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StatusError {
    pub code: &'static str,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Why the custom part of the served table is not the API's (ADR-024). The
/// project routes of that table apply; the custom routes served before stay.
/// It lasts while the API keeps the same revision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustomRoutesError {
    /// Always [`CUSTOM_ROUTES_REJECTED`], the status Edge reports to the API.
    pub code: &'static str,
    /// What was wrong: a [`CustomRouteRejection::reason`], or `caddy_refused`
    /// when Caddy refused the rendered sites.
    pub reason: &'static str,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// [`CustomRoutesError::reason`] when Caddy refused the custom domains file.
const CADDY_REFUSED_CUSTOM_ROUTES: &str = "caddy_refused";

/// Written to `status.json` after every attempt, for monitoring. A healthy Edge
/// has a recent `last_success_at`; a stale table shows as an old one together
/// with `last_error`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SyncStatus {
    pub revision: Option<String>,
    pub route_count: usize,
    /// Custom domain sites Caddy serves.
    pub custom_route_count: usize,
    /// Set while the custom part of the served revision was refused.
    pub custom_routes_error: Option<CustomRoutesError>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub generated_at: Option<OffsetDateTime>,
    pub source: Option<RoutesSource>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_attempt_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_success_at: Option<OffsetDateTime>,
    pub consecutive_failures: u32,
    pub last_error: Option<StatusError>,
}

/// What one write, validate, and reload left Caddy serving.
struct Applied {
    /// Caddy took a new configuration.
    reloaded: bool,
    /// The table as served: its custom routes are the previous ones when Caddy
    /// refused the new custom sites.
    table: RouteTable,
    custom_refused: Option<CaddyError>,
}

pub struct EdgeSync {
    source: RouteSource,
    policy: RoutePolicy,
    caddy: CaddyControl,
    files: EdgeFiles,
    timing: SyncTiming,
    current: Option<RouteTable>,
    status: SyncStatus,
}

impl EdgeSync {
    #[must_use]
    pub fn new(
        source: RouteSource,
        policy: RoutePolicy,
        caddy: CaddyControl,
        files: EdgeFiles,
        timing: SyncTiming,
    ) -> Self {
        Self {
            source,
            policy,
            caddy,
            files,
            timing,
            current: None,
            status: SyncStatus::default(),
        }
    }

    #[must_use]
    pub fn status(&self) -> &SyncStatus {
        &self.status
    }

    #[must_use]
    pub fn current(&self) -> Option<&RouteTable> {
        self.current.as_ref()
    }

    /// Serves the snapshot from disk before the first poll, so a restart while
    /// the API is down keeps every route. `Ok(false)` when there is no snapshot.
    /// A snapshot that no longer passes the checks, for example after the
    /// allowed suffixes changed, is refused and the routes file left alone.
    pub async fn restore_snapshot(&mut self) -> Result<bool, SyncError> {
        self.files
            .ensure_dir()
            .await
            .map_err(storage("the state directory"))?;
        let Some(snapshot) = self
            .files
            .read_snapshot()
            .await
            .map_err(storage("the snapshot"))?
        else {
            return Ok(false);
        };
        let result = async {
            let (table, rejection) = self.keep_served_custom(self.policy.parse(&snapshot)?);
            let applied = self.apply(table).await?;
            Ok::<_, SyncError>((applied, rejection))
        }
        .await;
        match result {
            Ok((applied, rejection)) => {
                self.record_custom(rejection, applied.custom_refused.as_ref());
                let table = applied.table;
                info!(
                    revision = table.revision(),
                    routes = table.routes().len(),
                    custom_routes = table.custom_routes().routes().len(),
                    "serving the route snapshot until the API answers"
                );
                self.serve(table, RoutesSource::Snapshot);
                self.write_status().await;
                Ok(true)
            }
            Err(error) => {
                self.record_failure(&error);
                self.write_status().await;
                Err(error)
            }
        }
    }

    /// Fetches the table once and applies it when it is new and valid. Any
    /// failure leaves the routes Caddy serves untouched.
    pub async fn sync_once(&mut self) -> Result<SyncOutcome, SyncError> {
        self.sync_until(&CancellationToken::new())
            .await
            .expect("a token nobody holds is never cancelled")
    }

    /// [`Self::sync_once`], abandoned with `None` when `shutdown` is cancelled
    /// while the table is still being fetched. Once a table arrives the cycle
    /// runs to the end, so a shutdown never leaves Caddy between a write and
    /// its rollback.
    async fn sync_until(
        &mut self,
        shutdown: &CancellationToken,
    ) -> Option<Result<SyncOutcome, SyncError>> {
        self.status.last_attempt_at = Some(OffsetDateTime::now_utc());
        let result = self.fetch_and_apply(shutdown).await?;
        match &result {
            Ok(outcome) => {
                self.status.last_success_at = self.status.last_attempt_at;
                self.status.consecutive_failures = 0;
                self.status.last_error = None;
                debug!(outcome = ?outcome, "route sync finished");
            }
            Err(error) => self.record_failure(error),
        }
        self.write_status().await;
        Some(result)
    }

    /// Restores the snapshot, then polls until `shutdown` is cancelled. A fetch
    /// in flight is abandoned; an apply in progress finishes first.
    pub async fn run(mut self, shutdown: CancellationToken) {
        match self.restore_snapshot().await {
            Ok(true) => {}
            Ok(false) => info!("no route snapshot yet; waiting for the API"),
            Err(error) => warn!(
                code = error.code(),
                error = %error,
                "could not serve the route snapshot; the routes file is left as it was"
            ),
        }
        loop {
            let Some(result) = self.sync_until(&shutdown).await else {
                break;
            };
            if let Err(error) = result {
                warn!(
                    code = error.code(),
                    error = %error,
                    consecutive_failures = self.status.consecutive_failures,
                    "route sync failed; keeping the routes Caddy serves"
                );
            }
            let delay = next_delay(self.timing, self.status.consecutive_failures);
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
        }
        info!("route sync stopped");
    }

    /// The revision Caddy serves and how the last cycle ended, as reported to
    /// the API. `current` only moves once Caddy accepted a table (or the
    /// snapshot was served at start), so a rejected table or a rollback leaves
    /// the reported revision where it was.
    #[must_use]
    pub fn applied_state(&self) -> AppliedState<'_> {
        AppliedState {
            revision: self.current.as_ref().map(RouteTable::revision),
            status: self
                .status
                .last_error
                .as_ref()
                .map(|error| error.code)
                .or_else(|| {
                    self.status
                        .custom_routes_error
                        .as_ref()
                        .map(|error| error.code)
                })
                .unwrap_or(STATUS_OK),
        }
    }

    async fn fetch_and_apply(
        &mut self,
        shutdown: &CancellationToken,
    ) -> Option<Result<SyncOutcome, SyncError>> {
        let fetched = tokio::select! {
            () = shutdown.cancelled() => return None,
            fetched = self.source.fetch(self.applied_state()) => fetched,
        };
        Some(self.apply_fetched(fetched).await)
    }

    async fn apply_fetched(
        &mut self,
        fetched: Result<Fetched, FetchError>,
    ) -> Result<SyncOutcome, SyncError> {
        let revision = self.current.as_ref().map(RouteTable::revision);
        let body = match fetched? {
            Fetched::NotModified => {
                self.status.source = Some(RoutesSource::Api);
                return Ok(SyncOutcome::Unchanged);
            }
            Fetched::Body(body) => body,
        };
        let table = self.policy.parse(&body)?;
        if revision == Some(table.revision()) {
            // The API confirmed the table Edge serves, possibly from the
            // snapshot. A refused custom part of that revision stays refused.
            self.status.source = Some(RoutesSource::Api);
            return Ok(SyncOutcome::Unchanged);
        }
        let (table, rejection) = self.keep_served_custom(table);
        let applied = self.apply(table).await?;
        self.record_custom(rejection, applied.custom_refused.as_ref());
        let (table, reloaded) = (applied.table, applied.reloaded);
        info!(
            revision = table.revision(),
            routes = table.routes().len(),
            custom_routes = table.custom_routes().routes().len(),
            reloaded,
            "serving a new route table"
        );
        self.serve(table, RoutesSource::Api);
        Ok(if reloaded {
            SyncOutcome::Applied
        } else {
            SyncOutcome::SameRoutes
        })
    }

    /// The custom routes Caddy serves now; none before the first table.
    fn served_custom(&self) -> CustomRoutes {
        self.current
            .as_ref()
            .map(|table| table.custom_routes().clone())
            .unwrap_or_default()
    }

    /// `table` with the custom routes served now in place of a custom part the
    /// checks refused, and why it was refused.
    fn keep_served_custom(&self, table: RouteTable) -> (RouteTable, Option<CustomRouteRejection>) {
        match table.custom_rejection().cloned() {
            Some(rejection) => (
                table.with_custom_routes(self.served_custom()),
                Some(rejection),
            ),
            None => (table, None),
        }
    }

    /// Writes both files, validates, and reloads; on a Caddy failure puts the
    /// previous files back and reloads again, like the Agent's route manager.
    /// When the custom domains file changed too, the project routes get a
    /// second chance with the previous custom sites, so a custom site Caddy
    /// refuses never holds back project routes. Saves the snapshot only once
    /// Caddy accepted the table. Not `reloaded` when both files already held
    /// this table and Caddy was not touched.
    async fn apply(&self, table: RouteTable) -> Result<Applied, SyncError> {
        let previous_custom = self.served_custom();
        let routes_backup = self
            .files
            .replace_routes(&render(&table))
            .await
            .map_err(storage("the routes file"))?;
        let custom_backup = match self
            .files
            .replace_custom_domains(&render_custom(table.custom_routes()))
            .await
        {
            Ok(backup) => backup,
            Err(error) => {
                if let Some(routes_backup) = routes_backup {
                    self.files
                        .restore_routes(routes_backup)
                        .await
                        .map_err(storage("the routes file"))?;
                }
                return Err(storage("the custom domains file")(error));
            }
        };
        let mut applied = Applied {
            reloaded: routes_backup.is_some() || custom_backup.is_some(),
            table,
            custom_refused: None,
        };
        if applied.reloaded
            && let Err(error) = self.caddy.validate_and_reload().await
        {
            let Some(custom_backup) = custom_backup else {
                if let Some(routes_backup) = routes_backup {
                    self.files
                        .restore_routes(routes_backup)
                        .await
                        .map_err(storage("the routes file"))?;
                }
                self.caddy.reload_after_rollback().await;
                return Err(error.into());
            };
            self.files
                .restore_routes(custom_backup)
                .await
                .map_err(storage("the custom domains file"))?;
            match routes_backup {
                None => {
                    self.caddy.reload_after_rollback().await;
                    applied.reloaded = false;
                }
                Some(routes_backup) => {
                    if let Err(second) = self.caddy.validate_and_reload().await {
                        self.files
                            .restore_routes(routes_backup)
                            .await
                            .map_err(storage("the routes file"))?;
                        self.caddy.reload_after_rollback().await;
                        return Err(second.into());
                    }
                }
            }
            applied.table = applied.table.with_custom_routes(previous_custom);
            applied.custom_refused = Some(error);
        }
        let snapshot = serde_json::to_vec_pretty(&applied.table.to_wire())
            .expect("a route table always serializes");
        self.files
            .write_snapshot(&snapshot)
            .await
            .map_err(storage("the snapshot"))?;
        Ok(applied)
    }

    /// Records why the custom part of the table just applied is not served,
    /// or clears the record when it is.
    fn record_custom(
        &mut self,
        rejection: Option<CustomRouteRejection>,
        refused: Option<&CaddyError>,
    ) {
        let error = match (rejection, refused) {
            (Some(rejection), _) => Some((rejection.reason(), rejection.to_string())),
            (None, Some(refused)) => Some((
                CADDY_REFUSED_CUSTOM_ROUTES,
                format!("{refused}; the custom domain sites were put back"),
            )),
            (None, None) => None,
        };
        self.status.custom_routes_error = error.map(|(reason, message)| {
            warn!(
                code = CUSTOM_ROUTES_REJECTED,
                reason,
                error = %message,
                "custom routes refused; serving the previous custom domains"
            );
            CustomRoutesError {
                code: CUSTOM_ROUTES_REJECTED,
                reason,
                message,
                at: OffsetDateTime::now_utc(),
            }
        });
    }

    fn serve(&mut self, table: RouteTable, source: RoutesSource) {
        self.status.revision = Some(table.revision().to_owned());
        self.status.route_count = table.routes().len();
        self.status.custom_route_count = table.custom_routes().routes().len();
        self.status.generated_at = Some(table.generated_at());
        self.status.source = Some(source);
        self.current = Some(table);
    }

    fn record_failure(&mut self, error: &SyncError) {
        self.status.consecutive_failures = self.status.consecutive_failures.saturating_add(1);
        self.status.last_error = Some(StatusError {
            code: error.code(),
            message: error.to_string(),
            at: OffsetDateTime::now_utc(),
        });
    }

    /// The status file is for monitoring only; failing to write it never stops
    /// the sync.
    async fn write_status(&self) {
        let status = serde_json::to_vec_pretty(&self.status).expect("the status always serializes");
        if let Err(error) = self.files.write_status(&status).await {
            warn!(error = %error, path = %self.files.status_path().display(), "could not write the status file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_doubles_per_failure_up_to_the_cap() {
        let timing = SyncTiming {
            poll_interval: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        };

        let delays = (0..6)
            .map(|failures| next_delay(timing, failures).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
        assert_eq!(next_delay(timing, u32::MAX), Duration::from_secs(60));
    }
}
