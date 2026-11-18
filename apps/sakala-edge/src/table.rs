//! The route table the control plane publishes for Sakala Edge, and the checks
//! that decide whether Edge may serve it.
//!
//! Every value in the table ends up inside the generated Caddyfile, so nothing
//! is escaped later: a table is either fully valid, with hosts limited to
//! `a-z`, `0-9`, `-`, and `.`, and upstreams that are IP socket addresses, or it
//! is refused as a whole and the previous table keeps serving (ADR-019).

use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use ipnet::IpNet;
use sakala_agent_runtime::ProjectDomainSuffixes;
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireRouteTable {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub revision: String,
    pub expected_minimum_routes: u64,
    pub routes: Vec<WireRoute>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Route {
    pub host: String,
    pub project_id: Uuid,
    pub upstreams: Vec<SocketAddr>,
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

    /// The table in the API's own format, which is what the snapshot keeps, so
    /// a snapshot goes through exactly the checks a fresh response does.
    #[must_use]
    pub fn to_wire(&self) -> WireRouteTable {
        WireRouteTable {
            generated_at: self.generated_at,
            revision: self.revision.clone(),
            expected_minimum_routes: self.expected_minimum_routes,
            routes: self
                .routes
                .iter()
                .map(|route| WireRoute {
                    host: route.host.clone(),
                    project_id: route.project_id,
                    upstreams: route.upstreams.iter().map(ToString::to_string).collect(),
                })
                .collect(),
        }
    }
}

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

        Ok(RouteTable {
            revision: table.revision,
            generated_at: table.generated_at,
            expected_minimum_routes: table.expected_minimum_routes,
            routes,
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
