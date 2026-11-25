//! The route table the control plane publishes for Sakala Edge, and the checks
//! that decide whether Edge may serve it.
//!
//! Every value in the table ends up inside the generated Caddyfile, so nothing
//! is escaped later: a table is either fully valid, with hosts limited to
//! `a-z`, `0-9`, `-`, and `.`, and upstreams that are IP socket addresses, or it
//! is refused as a whole and the previous table keeps serving (ADR-019).
//!
//! Custom domains (ADR-024) come in a separate `custom_routes` array, which an
//! older Edge ignores, and are checked separately: a bad custom route refuses
//! only the custom part, the project routes of the same table still apply, and
//! the custom routes Edge served before stay.

use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use ipnet::IpNet;
use sakala_agent_runtime::{CustomDomainPolicy, ForbiddenZones, ProjectDomainSuffixes};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

/// Upper bound on routes in one table. Far above one Edge's expected load; it
/// only stops a runaway response from producing a Caddyfile Caddy chokes on.
pub const MAX_ROUTES: usize = 10_000;
/// Upper bound on upstreams of one route: every node serving the project, each
/// listed once. The Agent caps replicas per node at 64, and Edge balances
/// between nodes, so a route never legitimately needs more.
pub const MAX_UPSTREAMS_PER_ROUTE: usize = 64;
/// Longest accepted revision. A content hash in hex is 64 characters.
pub const MAX_REVISION_LEN: usize = 128;
/// Characters a revision may hold besides ASCII letters and digits. The revision
/// is written into a Caddyfile comment, the status file, and an `If-None-Match`
/// header, so it stays within characters that are safe in all three.
const REVISION_PUNCTUATION: &str = "._:-";
/// Longest excerpt of an offending value quoted in a rejection message.
const MAX_EXCERPT_CHARS: usize = 80;
/// Longest parser message kept for a malformed table. The parser quotes the
/// offending value, which the response controls.
const MAX_MALFORMED_DETAIL_CHARS: usize = 300;

/// Private address ranges Edge forwards to when the operator names none: the
/// RFC 1918 IPv4 ranges and IPv6 unique local addresses (RFC 4193). Node
/// traffic travels over a private network such as WireGuard (ADR-019).
pub const DEFAULT_UPSTREAM_CIDRS: [&str; 4] =
    ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"];

/// The table exactly as the API sends it, and as Edge keeps it on disk.
/// Unknown fields are ignored, so the API can add fields without breaking an
/// older Edge.
///
/// The custom part is kept as raw JSON here and read on its own by
/// [`RoutePolicy`], so a custom route of the wrong shape refuses only the
/// custom part instead of the whole table. Both fields are optional: an API
/// that does not publish custom domains serves none.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireRouteTable {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub revision: String,
    pub expected_minimum_routes: u64,
    pub routes: Vec<WireRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_minimum_custom_routes: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_routes: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireRoute {
    pub host: String,
    pub project_id: Uuid,
    pub upstreams: Vec<String>,
}

/// A table that passed every check, with routes sorted by host so the same
/// table always renders the same file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteTable {
    revision: String,
    generated_at: OffsetDateTime,
    expected_minimum_routes: u64,
    routes: Vec<Route>,
    custom_routes: CustomRoutes,
    /// Why the custom part of this table was refused. Its custom routes are
    /// then empty until [`RouteTable::with_custom_routes`] puts back the ones
    /// served before.
    custom_rejection: Option<CustomRouteRejection>,
}

/// The custom domains of a table, sorted by host. A custom route may have no
/// upstream: its project has no workload, and its site answers not found
/// while Caddy keeps its certificate.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CustomRoutes {
    expected_minimum: u64,
    routes: Vec<Route>,
}

impl CustomRoutes {
    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    fn to_wire(&self) -> (Value, Value) {
        (
            Value::from(self.expected_minimum),
            Value::Array(
                self.routes
                    .iter()
                    .map(|route| {
                        serde_json::to_value(route.to_wire()).expect("a route always serializes")
                    })
                    .collect(),
            ),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Route {
    pub host: String,
    pub project_id: Uuid,
    pub upstreams: Vec<SocketAddr>,
}

impl Route {
    fn to_wire(&self) -> WireRoute {
        WireRoute {
            host: self.host.clone(),
            project_id: self.project_id,
            upstreams: self.upstreams.iter().map(ToString::to_string).collect(),
        }
    }
}

impl RouteTable {
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    #[must_use]
    pub fn generated_at(&self) -> OffsetDateTime {
        self.generated_at
    }

    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    #[must_use]
    pub fn custom_routes(&self) -> &CustomRoutes {
        &self.custom_routes
    }

    /// Why the custom part of the table was refused, if it was.
    #[must_use]
    pub fn custom_rejection(&self) -> Option<&CustomRouteRejection> {
        self.custom_rejection.as_ref()
    }

    /// The same table with other custom routes, such as the ones Edge served
    /// before when this table's custom part was refused.
    #[must_use]
    pub fn with_custom_routes(mut self, custom_routes: CustomRoutes) -> Self {
        self.custom_routes = custom_routes;
        self
    }

    /// The table in the API's own format, which is what the snapshot keeps, so
    /// a snapshot goes through exactly the checks a fresh response does. The
    /// custom part is the one served, not a refused one.
    #[must_use]
    pub fn to_wire(&self) -> WireRouteTable {
        let (expected_minimum_custom_routes, custom_routes) = self.custom_routes.to_wire();
        WireRouteTable {
            generated_at: self.generated_at,
            revision: self.revision.clone(),
            expected_minimum_routes: self.expected_minimum_routes,
            routes: self.routes.iter().map(Route::to_wire).collect(),
            expected_minimum_custom_routes: Some(expected_minimum_custom_routes),
            custom_routes: Some(custom_routes),
        }
    }
}

/// Why the custom part of a table was refused. The project routes of the same
/// table still apply, and the custom routes served before stay.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CustomRouteRejection {
    #[error("the custom routes are not in the expected shape: {0}")]
    Malformed(String),
    #[error(
        "the table lists {routes} custom routes, fewer than the {expected} the API expects to serve"
    )]
    BelowExpectedMinimum { routes: usize, expected: u64 },
    #[error("the table lists {routes} custom routes, more than the {MAX_ROUTES} Edge accepts")]
    TooManyRoutes { routes: usize },
    #[error("custom domain {host} {reason}")]
    InvalidHost { host: String, reason: &'static str },
    #[error("custom domain {host} is listed more than once")]
    DuplicateHost { host: String },
    #[error(
        "custom domain {host} lists {upstreams} upstreams, more than the {MAX_UPSTREAMS_PER_ROUTE} Edge accepts"
    )]
    TooManyUpstreams { host: String, upstreams: usize },
    #[error(
        "custom domain {host} has upstream {upstream}, which is not an IP address and non-zero port"
    )]
    InvalidUpstream { host: String, upstream: String },
    #[error("custom domain {host} has upstream {upstream}, outside the allowed upstream networks")]
    UpstreamNotAllowed { host: String, upstream: SocketAddr },
    #[error("custom domain {host} lists upstream {upstream} more than once")]
    DuplicateUpstream { host: String, upstream: SocketAddr },
}

impl CustomRouteRejection {
    /// A stable code for the status file and logs, telling reasons apart under
    /// the one status code [`CUSTOM_ROUTES_REJECTED`].
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::BelowExpectedMinimum { .. } => "below_expected_minimum",
            Self::TooManyRoutes { .. } => "too_many_routes",
            Self::InvalidHost { .. } => "invalid_host",
            Self::DuplicateHost { .. } => "duplicate_host",
            Self::TooManyUpstreams { .. } => "too_many_upstreams",
            Self::InvalidUpstream { .. } => "invalid_upstream",
            Self::UpstreamNotAllowed { .. } => "upstream_not_allowed",
            Self::DuplicateUpstream { .. } => "duplicate_upstream",
        }
    }
}

/// Status code of an Edge whose last table had its custom part refused, by a
/// check here or by Caddy, while its project routes applied.
pub const CUSTOM_ROUTES_REJECTED: &str = "custom_routes_rejected";

/// Why a table was refused. Each reason keeps the previous table serving.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TableRejection {
    #[error("the route table is not valid JSON in the expected shape: {0}")]
    Malformed(String),
    #[error(
        "the revision must be 1 to {MAX_REVISION_LEN} characters of letters, digits, and {REVISION_PUNCTUATION:?}"
    )]
    InvalidRevision,
    #[error("the route table lists no routes")]
    Empty,
    #[error(
        "the route table lists {routes} routes, fewer than the {expected} the API expects to serve"
    )]
    BelowExpectedMinimum { routes: usize, expected: u64 },
    #[error("the route table lists {routes} routes, more than the {MAX_ROUTES} Edge accepts")]
    TooManyRoutes { routes: usize },
    #[error("host {host} is not one label under an allowed domain suffix")]
    HostOutsideSuffixes { host: String },
    #[error("host {host} is listed more than once")]
    DuplicateHost { host: String },
    #[error("host {host} has no upstream")]
    NoUpstreams { host: String },
    #[error(
        "host {host} lists {upstreams} upstreams, more than the {MAX_UPSTREAMS_PER_ROUTE} Edge accepts"
    )]
    TooManyUpstreams { host: String, upstreams: usize },
    #[error("host {host} has upstream {upstream}, which is not an IP address and non-zero port")]
    InvalidUpstream { host: String, upstream: String },
    #[error("host {host} has upstream {upstream}, outside the allowed upstream networks")]
    UpstreamNotAllowed { host: String, upstream: SocketAddr },
    #[error("host {host} lists upstream {upstream} more than once")]
    DuplicateUpstream { host: String, upstream: SocketAddr },
}

impl TableRejection {
    /// A stable code for the status file and logs, so monitoring can tell
    /// reasons apart without parsing messages.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::InvalidRevision => "invalid_revision",
            Self::Empty => "empty_table",
            Self::BelowExpectedMinimum { .. } => "below_expected_minimum",
            Self::TooManyRoutes { .. } => "too_many_routes",
            Self::HostOutsideSuffixes { .. } => "host_outside_suffixes",
            Self::DuplicateHost { .. } => "duplicate_host",
            Self::NoUpstreams { .. } => "no_upstreams",
            Self::TooManyUpstreams { .. } => "too_many_upstreams",
            Self::InvalidUpstream { .. } => "invalid_upstream",
            Self::UpstreamNotAllowed { .. } => "upstream_not_allowed",
            Self::DuplicateUpstream { .. } => "duplicate_upstream",
        }
    }
}

/// The networks an upstream address must fall in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamAllowlist(Vec<IpNet>);

impl Default for UpstreamAllowlist {
    fn default() -> Self {
        Self(
            DEFAULT_UPSTREAM_CIDRS
                .into_iter()
                .map(|cidr| cidr.parse().expect("default upstream CIDRs are valid"))
                .collect(),
        )
    }
}

impl UpstreamAllowlist {
    /// Parses a comma-separated list such as `10.20.0.0/16,fd00:20::/64`.
    ///
    /// Each entry must be a network address with its prefix length and no host
    /// bits set, so a typo such as `10.20.0.1/16` is caught instead of read as
    /// a wider network than meant. A `/0` network is refused because it would
    /// let the table send traffic to any address, public ones included.
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut networks = Vec::new();
        for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let network = IpNet::from_str(entry).map_err(|_| {
                format!("{entry:?} is not a network in CIDR form, such as 10.20.0.0/16")
            })?;
            if network.trunc() != network {
                return Err(format!(
                    "{entry:?} has host bits set; write the network address {}",
                    network.trunc()
                ));
            }
            if network.prefix_len() == 0 {
                return Err(format!(
                    "{entry:?} would allow every address; list the private networks nodes use"
                ));
            }
            if !networks.contains(&network) {
                networks.push(network);
            }
        }
        if networks.is_empty() {
            return Err("lists no network".to_owned());
        }
        Ok(Self(networks))
    }

    #[must_use]
    pub fn allows(&self, address: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(&address))
    }
}

impl fmt::Display for UpstreamAllowlist {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let networks = self.0.iter().map(ToString::to_string).collect::<Vec<_>>();
        formatter.write_str(&networks.join(", "))
    }
}

/// What a table must satisfy before Edge serves it.
#[derive(Clone, Debug)]
pub struct RoutePolicy {
    pub domain_suffixes: ProjectDomainSuffixes,
    pub upstreams: UpstreamAllowlist,
    /// Zones no custom domain may be or sit under, besides `domain_suffixes`.
    pub forbidden_zones: ForbiddenZones,
}

impl RoutePolicy {
    /// Parses and checks a response body or snapshot.
    pub fn parse(&self, body: &[u8]) -> Result<RouteTable, TableRejection> {
        let table = serde_json::from_slice::<WireRouteTable>(body).map_err(|error| {
            TableRejection::Malformed(truncate(&error.to_string(), MAX_MALFORMED_DETAIL_CHARS))
        })?;
        self.validate(table)
    }

    /// Checks every rule and returns the table with routes sorted by host.
    pub fn validate(&self, table: WireRouteTable) -> Result<RouteTable, TableRejection> {
        if !valid_revision(&table.revision) {
            return Err(TableRejection::InvalidRevision);
        }
        if table.routes.is_empty() {
            return Err(TableRejection::Empty);
        }
        if table.routes.len() > MAX_ROUTES {
            return Err(TableRejection::TooManyRoutes {
                routes: table.routes.len(),
            });
        }
        let below_minimum = u64::try_from(table.routes.len())
            .is_ok_and(|routes| routes < table.expected_minimum_routes);
        if below_minimum {
            return Err(TableRejection::BelowExpectedMinimum {
                routes: table.routes.len(),
                expected: table.expected_minimum_routes,
            });
        }

        let mut hosts = HashSet::with_capacity(table.routes.len());
        let mut routes = Vec::with_capacity(table.routes.len());
        for route in table.routes {
            if !self.domain_suffixes.accepts(&route.host) {
                return Err(TableRejection::HostOutsideSuffixes {
                    host: excerpt(&route.host),
                });
            }
            if !hosts.insert(route.host.clone()) {
                return Err(TableRejection::DuplicateHost { host: route.host });
            }
            let upstreams = self.upstreams(&route.host, &route.upstreams)?;
            routes.push(Route {
                host: route.host,
                project_id: route.project_id,
                upstreams,
            });
        }
        routes.sort_by(|left, right| left.host.cmp(&right.host));

        let (custom_routes, custom_rejection) =
            match self.custom_routes(table.expected_minimum_custom_routes, table.custom_routes) {
                Ok(custom_routes) => (custom_routes, None),
                Err(rejection) => (CustomRoutes::default(), Some(rejection)),
            };
        Ok(RouteTable {
            revision: table.revision,
            generated_at: table.generated_at,
            expected_minimum_routes: table.expected_minimum_routes,
            routes,
            custom_routes,
            custom_rejection,
        })
    }

    /// Checks the custom part on its own. Its hosts never sit under a project
    /// suffix, so they cannot collide with a project route.
    fn custom_routes(
        &self,
        expected_minimum: Option<Value>,
        routes: Option<Value>,
    ) -> Result<CustomRoutes, CustomRouteRejection> {
        let malformed = |error: serde_json::Error| {
            CustomRouteRejection::Malformed(truncate(
                &error.to_string(),
                MAX_MALFORMED_DETAIL_CHARS,
            ))
        };
        let expected_minimum = match expected_minimum {
            None | Some(Value::Null) => 0,
            Some(value) => serde_json::from_value::<u64>(value).map_err(malformed)?,
        };
        let routes = match routes {
            None | Some(Value::Null) => Vec::new(),
            Some(value) => serde_json::from_value::<Vec<WireRoute>>(value).map_err(malformed)?,
        };
        if routes.len() > MAX_ROUTES {
            return Err(CustomRouteRejection::TooManyRoutes {
                routes: routes.len(),
            });
        }
        if u64::try_from(routes.len()).is_ok_and(|count| count < expected_minimum) {
            return Err(CustomRouteRejection::BelowExpectedMinimum {
                routes: routes.len(),
                expected: expected_minimum,
            });
        }
        let policy = CustomDomainPolicy {
            project_domains: self.domain_suffixes.clone(),
            forbidden_zones: self.forbidden_zones.clone(),
        };
        let mut hosts = HashSet::with_capacity(routes.len());
        let mut checked = Vec::with_capacity(routes.len());
        for route in routes {
            policy
                .check(&route.host)
                .map_err(|rejection| CustomRouteRejection::InvalidHost {
                    host: excerpt(&route.host),
                    reason: rejection.reason(),
                })?;
            if !hosts.insert(route.host.clone()) {
                return Err(CustomRouteRejection::DuplicateHost { host: route.host });
            }
            // No upstream is allowed: the site answers not found.
            let upstreams = if route.upstreams.is_empty() {
                Vec::new()
            } else {
                self.upstreams(&route.host, &route.upstreams)
                    .map_err(custom_upstream_rejection)?
            };
            checked.push(Route {
                host: route.host,
                project_id: route.project_id,
                upstreams,
            });
        }
        checked.sort_by(|left, right| left.host.cmp(&right.host));
        Ok(CustomRoutes {
            expected_minimum,
            routes: checked,
        })
    }

    fn upstreams(
        &self,
        host: &str,
        upstreams: &[String],
    ) -> Result<Vec<SocketAddr>, TableRejection> {
        if upstreams.is_empty() {
            return Err(TableRejection::NoUpstreams {
                host: host.to_owned(),
            });
        }
        if upstreams.len() > MAX_UPSTREAMS_PER_ROUTE {
            return Err(TableRejection::TooManyUpstreams {
                host: host.to_owned(),
                upstreams: upstreams.len(),
            });
        }
        let mut parsed = Vec::with_capacity(upstreams.len());
        for upstream in upstreams {
            let address =
                parse_upstream(upstream).ok_or_else(|| TableRejection::InvalidUpstream {
                    host: host.to_owned(),
                    upstream: excerpt(upstream),
                })?;
            if !self.upstreams.allows(address.ip()) {
                return Err(TableRejection::UpstreamNotAllowed {
                    host: host.to_owned(),
                    upstream: address,
                });
            }
            if parsed.contains(&address) {
                return Err(TableRejection::DuplicateUpstream {
                    host: host.to_owned(),
                    upstream: address,
                });
            }
            parsed.push(address);
        }
        Ok(parsed)
    }
}

/// The custom-route form of an upstream rejection. [`RoutePolicy::upstreams`]
/// only returns the upstream variants.
fn custom_upstream_rejection(rejection: TableRejection) -> CustomRouteRejection {
    match rejection {
        TableRejection::TooManyUpstreams { host, upstreams } => {
            CustomRouteRejection::TooManyUpstreams { host, upstreams }
        }
        TableRejection::InvalidUpstream { host, upstream } => {
            CustomRouteRejection::InvalidUpstream { host, upstream }
        }
        TableRejection::UpstreamNotAllowed { host, upstream } => {
            CustomRouteRejection::UpstreamNotAllowed { host, upstream }
        }
        TableRejection::DuplicateUpstream { host, upstream } => {
            CustomRouteRejection::DuplicateUpstream { host, upstream }
        }
        other => CustomRouteRejection::Malformed(other.to_string()),
    }
}

/// An IP socket address with a non-zero port, written in the canonical form
/// Rust prints back (`10.0.0.1:8180`, `[fd00::1]:8180`). Host names are refused
/// so routing never depends on DNS at the Edge, and IPv6 zone or flow
/// information is refused because it has no meaning across hosts.
fn parse_upstream(value: &str) -> Option<SocketAddr> {
    let address = SocketAddr::from_str(value).ok()?;
    if address.port() == 0 {
        return None;
    }
    if let SocketAddr::V6(v6) = address
        && (v6.scope_id() != 0 || v6.flowinfo() != 0)
    {
        return None;
    }
    (address.to_string() == value).then_some(address)
}

fn valid_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= MAX_REVISION_LEN
        && revision.chars().all(|character| {
            character.is_ascii_alphanumeric() || REVISION_PUNCTUATION.contains(character)
        })
}

/// A bounded, escaped quote of a value from the response, safe for one log line.
fn excerpt(value: &str) -> String {
    truncate(value, MAX_EXCERPT_CHARS)
}

/// The first `limit` characters of `value`, escaped onto one line, with `…`
/// when something was cut.
fn truncate(value: &str, limit: usize) -> String {
    let mut quoted = value
        .chars()
        .take(limit)
        .collect::<String>()
        .escape_debug()
        .to_string();
    if value.chars().count() > limit {
        quoted.push('…');
    }
    quoted
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{Value, json};

    pub(crate) const PROJECT_A: &str = "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1a";
    pub(crate) const PROJECT_B: &str = "0a6f7f3e-4f3b-4d0e-9b8e-1f1f1f1f1f1b";

    pub(crate) fn policy() -> RoutePolicy {
        RoutePolicy {
            domain_suffixes: ProjectDomainSuffixes::default(),
            upstreams: UpstreamAllowlist::default(),
            forbidden_zones: ForbiddenZones::default(),
        }
    }

    pub(crate) fn body(routes: Value, expected_minimum_routes: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "generated_at": "2026-11-18T03:00:00Z",
            "revision": "rev-1",
            "expected_minimum_routes": expected_minimum_routes,
            "routes": routes,
        }))
        .expect("serializable")
    }

    fn reject(routes: Value) -> TableRejection {
        policy().parse(&body(routes, 0)).expect_err("rejected")
    }

    #[test]
    fn a_valid_table_is_sorted_by_host_and_round_trips_through_the_snapshot_format() {
        let table = policy()
            .parse(&body(
                json!([
                    { "host": "zeta.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.47.201:8180"] },
                    { "host": "blog.staging.sakala.app", "project_id": PROJECT_B,
                      "upstreams": ["10.0.47.201:8180", "[fd00::2]:8180"], "region": "jkt" },
                ]),
                2,
            ))
            .expect("valid table");

        assert_eq!(table.revision(), "rev-1");
        let hosts = table
            .routes()
            .iter()
            .map(|r| r.host.as_str())
            .collect::<Vec<_>>();
        assert_eq!(hosts, ["blog.staging.sakala.app", "zeta.sakala.app"]);
        let snapshot = serde_json::to_vec(&table.to_wire()).expect("serializable");
        assert_eq!(policy().parse(&snapshot), Ok(table));
    }

    #[test]
    fn malformed_json_and_schema_violations_are_rejected() {
        for body in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            br#"{"generated_at":"yesterday","revision":"r","expected_minimum_routes":0,"routes":[]}"#
                .to_vec(),
            br#"{"generated_at":"2026-11-18T03:00:00Z","revision":"r","expected_minimum_routes":-1,"routes":[]}"#
                .to_vec(),
            br#"{"generated_at":"2026-11-18T03:00:00Z","revision":"r","expected_minimum_routes":0,"routes":[{"host":"a.sakala.app","project_id":"nope","upstreams":["10.0.0.1:80"]}]}"#
                .to_vec(),
        ] {
            let rejection = policy().parse(&body).expect_err("malformed");
            assert_eq!(rejection.code(), "malformed", "{rejection}");
        }
    }

    #[test]
    fn a_malformed_table_is_reported_on_one_bounded_line() {
        let long = format!("{{\"revision\": 1, \"x\": \"{}\n\"", "a".repeat(5_000));
        let TableRejection::Malformed(detail) = policy().parse(long.as_bytes()).expect_err("bad")
        else {
            panic!("expected a malformed table");
        };
        assert!(
            detail.chars().count() <= MAX_MALFORMED_DETAIL_CHARS + 1,
            "{detail}"
        );
        assert!(!detail.contains('\n'));
    }

    #[test]
    fn an_empty_table_is_rejected_even_when_the_api_expects_none() {
        assert_eq!(reject(json!([])), TableRejection::Empty);
    }

    #[test]
    fn a_table_smaller_than_the_expected_minimum_is_rejected() {
        let routes = json!([
            { "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:8180"] },
        ]);

        assert_eq!(
            policy().parse(&body(routes.clone(), 2)),
            Err(TableRejection::BelowExpectedMinimum {
                routes: 1,
                expected: 2
            })
        );
        assert!(policy().parse(&body(routes, 1)).is_ok());
    }

    #[test]
    fn revisions_outside_the_safe_alphabet_are_rejected() {
        for revision in ["", "rev 1", "rev\n1", "\"rev\"", "rev{1}", &"a".repeat(129)] {
            let mut table = serde_json::from_slice::<Value>(&body(
                json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
                0,
            ))
            .expect("json");
            table["revision"] = json!(revision);
            let body = serde_json::to_vec(&table).expect("json");
            assert_eq!(
                policy().parse(&body),
                Err(TableRejection::InvalidRevision),
                "{revision:?}"
            );
        }
        let mut table = serde_json::from_slice::<Value>(&body(
            json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
            0,
        ))
        .expect("json");
        table["revision"] = json!("sha256:9f86d081884c7d65-2.v1");
        assert!(
            policy()
                .parse(&serde_json::to_vec(&table).expect("json"))
                .is_ok()
        );
    }

    #[test]
    fn hosts_outside_the_configured_suffixes_are_rejected() {
        for host in [
            "blog.example.com",
            "api.blog.sakala.app",
            "Blog.sakala.app",
            "sakala.app",
            "blog.sakala.app {\n\trespond 200\n}",
            "*.sakala.app",
            "blog.sakala.app:443",
        ] {
            let rejection = reject(json!([
                { "host": host, "project_id": PROJECT_A, "upstreams": ["10.0.0.1:8180"] },
            ]));
            assert_eq!(rejection.code(), "host_outside_suffixes", "{host:?}");
            assert!(!rejection.to_string().contains('\n'), "{rejection}");
        }
    }

    #[test]
    fn duplicate_hosts_are_rejected() {
        assert_eq!(
            reject(json!([
                { "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:8180"] },
                { "host": "a.sakala.app", "project_id": PROJECT_B, "upstreams": ["10.0.0.2:8180"] },
            ])),
            TableRejection::DuplicateHost {
                host: "a.sakala.app".to_owned()
            }
        );
    }

    #[test]
    fn a_route_needs_at_least_one_and_at_most_the_maximum_upstreams() {
        assert_eq!(
            reject(json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": [] }])),
            TableRejection::NoUpstreams {
                host: "a.sakala.app".to_owned()
            }
        );
        let many = (1..=65)
            .map(|port| format!("10.0.0.1:{port}"))
            .collect::<Vec<_>>();
        assert_eq!(
            reject(json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": many }])),
            TableRejection::TooManyUpstreams {
                host: "a.sakala.app".to_owned(),
                upstreams: 65
            }
        );
    }

    #[test]
    fn upstreams_must_be_canonical_ip_socket_addresses() {
        for upstream in [
            "node-1.internal:8180",
            "10.0.0.1",
            "10.0.0.1:0",
            "10.0.0.1:65536",
            "http://10.0.0.1:8180",
            "10.0.0.1:8180 {",
            "fd00::1:8180",
            "[fe80::1%1]:8180",
            "010.0.0.1:8180",
            " 10.0.0.1:8180",
        ] {
            let rejection = reject(json!([
                { "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": [upstream] },
            ]));
            assert_eq!(rejection.code(), "invalid_upstream", "{upstream:?}");
        }
    }

    #[test]
    fn upstreams_outside_the_allowlist_are_rejected() {
        for upstream in [
            "8.8.8.8:80",
            "127.0.0.1:8180",
            "[2001:db8::1]:8180",
            "[::ffff:10.0.0.1]:80",
        ] {
            let rejection = reject(json!([
                { "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": [upstream] },
            ]));
            assert_eq!(rejection.code(), "upstream_not_allowed", "{upstream:?}");
        }
    }

    #[test]
    fn duplicate_upstreams_in_one_route_are_rejected() {
        assert_eq!(
            reject(json!([{ "host": "a.sakala.app", "project_id": PROJECT_A,
                            "upstreams": ["10.0.0.1:8180", "10.0.0.1:8180"] }])),
            TableRejection::DuplicateUpstream {
                host: "a.sakala.app".to_owned(),
                upstream: "10.0.0.1:8180".parse().expect("address"),
            }
        );
    }

    #[test]
    fn too_many_routes_are_rejected() {
        let routes = (0..=MAX_ROUTES)
            .map(|index| {
                json!({ "host": format!("p{index}.sakala.app"), "project_id": PROJECT_A,
                                 "upstreams": ["10.0.0.1:8180"] })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reject(Value::Array(routes)),
            TableRejection::TooManyRoutes {
                routes: MAX_ROUTES + 1
            }
        );
    }

    fn with_custom(custom: Value, expected_minimum: Option<Value>) -> RouteTable {
        let mut table = serde_json::from_slice::<Value>(&body(
            json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
            1,
        ))
        .expect("json");
        table["custom_routes"] = custom;
        if let Some(expected_minimum) = expected_minimum {
            table["expected_minimum_custom_routes"] = expected_minimum;
        }
        policy()
            .parse(&serde_json::to_vec(&table).expect("json"))
            .expect("the project routes stay valid")
    }

    fn custom_rejection(custom: Value) -> CustomRouteRejection {
        let table = with_custom(custom, None);
        assert!(table.custom_routes().routes().is_empty());
        assert_eq!(table.routes().len(), 1, "project routes still apply");
        table
            .custom_rejection()
            .cloned()
            .expect("custom part refused")
    }

    #[test]
    fn an_older_table_without_custom_routes_serves_none() {
        let table = policy()
            .parse(&body(
                json!([{ "host": "a.sakala.app", "project_id": PROJECT_A, "upstreams": ["10.0.0.1:80"] }]),
                1,
            ))
            .expect("valid");
        assert_eq!(table.custom_rejection(), None);
        assert!(table.custom_routes().routes().is_empty());
        assert_eq!(
            with_custom(Value::Null, Some(Value::Null)).custom_rejection(),
            None
        );
    }

    #[test]
    fn custom_routes_are_checked_sorted_and_kept_in_the_snapshot() {
        let table = with_custom(
            json!([
                { "host": "www.contoh.com", "project_id": PROJECT_B, "upstreams": ["10.0.47.201:8180"] },
                { "host": "contoh.net", "project_id": PROJECT_A, "upstreams": [], "domain_id": 7 },
            ]),
            Some(json!(2)),
        );

        assert_eq!(table.custom_rejection(), None);
        let hosts = table
            .custom_routes()
            .routes()
            .iter()
            .map(|route| route.host.as_str())
            .collect::<Vec<_>>();
        assert_eq!(hosts, ["contoh.net", "www.contoh.com"]);
        assert!(table.custom_routes().routes()[0].upstreams.is_empty());
        let snapshot = serde_json::to_vec(&table.to_wire()).expect("serializable");
        assert_eq!(policy().parse(&snapshot), Ok(table));
    }

    #[test]
    fn an_empty_custom_part_is_valid_only_when_the_api_expects_none() {
        assert_eq!(
            with_custom(json!([]), Some(json!(0))).custom_rejection(),
            None
        );
        assert_eq!(
            with_custom(json!([]), Some(json!(1))).custom_rejection(),
            Some(&CustomRouteRejection::BelowExpectedMinimum {
                routes: 0,
                expected: 1
            })
        );
    }

    #[test]
    fn a_bad_custom_route_refuses_only_the_custom_part() {
        for (custom, reason) in [
            (json!("www.contoh.com"), "malformed"),
            (json!([{ "host": "www.contoh.com" }]), "malformed"),
            (
                json!([{ "host": "www.contoh.com", "project_id": "nope", "upstreams": [] }]),
                "malformed",
            ),
            (
                json!([{ "host": "blog.sakala.app", "project_id": PROJECT_A, "upstreams": [] }]),
                "invalid_host",
            ),
            (
                json!([{ "host": "api.sakala.dev", "project_id": PROJECT_A, "upstreams": [] }]),
                "invalid_host",
            ),
            (
                json!([{ "host": "10.0.0.1", "project_id": PROJECT_A, "upstreams": [] }]),
                "invalid_host",
            ),
            (
                json!([{ "host": "www.contoh.com {\n\trespond 200\n}", "project_id": PROJECT_A, "upstreams": [] }]),
                "invalid_host",
            ),
            (
                json!([{ "host": "*.contoh.com", "project_id": PROJECT_A, "upstreams": [] }]),
                "invalid_host",
            ),
            (
                json!([
                    { "host": "contoh.com", "project_id": PROJECT_A, "upstreams": [] },
                    { "host": "contoh.com", "project_id": PROJECT_B, "upstreams": [] },
                ]),
                "duplicate_host",
            ),
            (
                json!([{ "host": "contoh.com", "project_id": PROJECT_A, "upstreams": ["node-1:8180"] }]),
                "invalid_upstream",
            ),
            (
                json!([{ "host": "contoh.com", "project_id": PROJECT_A, "upstreams": ["8.8.8.8:80"] }]),
                "upstream_not_allowed",
            ),
            (
                json!([{ "host": "contoh.com", "project_id": PROJECT_A,
                         "upstreams": ["10.0.0.1:80", "10.0.0.1:80"] }]),
                "duplicate_upstream",
            ),
        ] {
            let rejection = custom_rejection(custom.clone());
            assert_eq!(rejection.reason(), reason, "{custom}: {rejection}");
            assert!(!rejection.to_string().contains('\n'), "{rejection}");
        }
        let many = (1..=65)
            .map(|port| format!("10.0.0.1:{port}"))
            .collect::<Vec<_>>();
        assert_eq!(
            custom_rejection(
                json!([{ "host": "contoh.com", "project_id": PROJECT_A, "upstreams": many }])
            )
            .reason(),
            "too_many_upstreams"
        );
        let malformed_minimum = with_custom(json!([]), Some(json!(-1)));
        assert_eq!(
            malformed_minimum
                .custom_rejection()
                .map(CustomRouteRejection::reason),
            Some("malformed")
        );
    }

    #[test]
    fn the_allowlist_parses_networks_and_refuses_typos() {
        let allowlist = UpstreamAllowlist::parse(" 10.20.0.0/16 , fd00:20::/64,10.20.0.0/16 ")
            .expect("valid networks");
        assert_eq!(allowlist.to_string(), "10.20.0.0/16, fd00:20::/64");
        assert!(allowlist.allows("10.20.3.4".parse().expect("ip")));
        assert!(!allowlist.allows("10.21.0.1".parse().expect("ip")));

        for (value, reason) in [
            ("", "lists no network"),
            ("10.20.0.1/16", "host bits set"),
            ("0.0.0.0/0", "every address"),
            ("::/0", "every address"),
            ("10.20.0.0", "CIDR form"),
            ("node-1/24", "CIDR form"),
        ] {
            let error = UpstreamAllowlist::parse(value).expect_err(value);
            assert!(error.contains(reason), "{value:?}: {error}");
        }
    }

    #[test]
    fn the_default_allowlist_covers_private_networks_only() {
        let allowlist = UpstreamAllowlist::default();
        for allowed in [
            "10.0.47.201",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "fd12::1",
        ] {
            assert!(allowlist.allows(allowed.parse().expect("ip")), "{allowed}");
        }
        for refused in [
            "172.32.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "1.1.1.1",
            "::1",
            "2001:db8::1",
        ] {
            assert!(!allowlist.allows(refused.parse().expect("ip")), "{refused}");
        }
    }
}
