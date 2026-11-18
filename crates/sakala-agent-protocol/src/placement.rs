//! Where a node is and how Sakala Edge reaches it, reported on heartbeat as
//! `metadata.runtime`.
//!
//! Both values follow the control plane's validation exactly (`RegionSlug` and
//! `EdgeAddress` in `sakala-api`). The API rejects the whole heartbeat when one
//! of them is malformed, so the agent checks them once at startup and only
//! ever holds values the API accepts.

use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
};

use serde::{Deserialize, Serialize};

/// Longest region label the API accepts, in bytes.
pub const REGION_MAX_LENGTH: usize = 32;

/// The API's region pattern. [`Region::parse`] implements it without a regex
/// engine: groups of `a-z0-9` joined by single hyphens.
pub const REGION_PATTERN: &str = "^[a-z0-9]+(?:-[a-z0-9]+)*$";

/// Longest edge address the API accepts, in bytes.
pub const EDGE_ADDRESS_MAX_LENGTH: usize = 255;

/// Highest TCP port of an edge address. The lowest is 1.
pub const EDGE_ADDRESS_MAX_PORT: u16 = u16::MAX;

/// Most digits of a port, matching the API's `^[1-9][0-9]{0,4}$`.
const PORT_MAX_DIGITS: usize = 5;

/// Longest DNS name the API accepts as an edge address host.
const HOSTNAME_MAX_LENGTH: usize = 253;

/// Longest single DNS label.
const DNS_LABEL_MAX_LENGTH: usize = 63;

const REGION_SEPARATOR: char = '-';
const HOST_PORT_SEPARATOR: char = ':';
const DNS_LABEL_SEPARATOR: char = '.';
const DNS_LABEL_HYPHEN: char = '-';
const IPV6_OPEN_BRACKET: char = '[';
const IPV6_CLOSE_BRACKET: char = ']';

/// A region label such as `jkt` or `id-jog-1`, from `SAKALA_AGENT_REGION`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Region(String);

impl Region {
    /// Accepts exactly what the API's `RegionSlug` rule accepts.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRegion`] when the value is empty, longer than
    /// [`REGION_MAX_LENGTH`], or does not match [`REGION_PATTERN`].
    pub fn parse(value: &str) -> Result<Self, InvalidRegion> {
        let groups_are_valid = value.split(REGION_SEPARATOR).all(|group| {
            !group.is_empty()
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
        if value.len() <= REGION_MAX_LENGTH && groups_are_valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidRegion)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for Region {
    type Error = InvalidRegion;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Region> for String {
    fn from(region: Region) -> Self {
        region.0
    }
}

/// A region label the API would reject.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRegion;

impl fmt::Display for InvalidRegion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "must match {REGION_PATTERN} (lowercase letters and digits in groups joined by single hyphens) and be at most {REGION_MAX_LENGTH} characters"
        )
    }
}

impl std::error::Error for InvalidRegion {}

/// The `host:port` where Sakala Edge reaches this node's runtime router, from
/// `SAKALA_AGENT_EDGE_ADDRESS`, for example `10.0.47.201:8180`,
/// `[fd00::21]:8180`, or `runtime-01.internal:8180`. The Edge uses it verbatim
/// as an upstream.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct EdgeAddress(String);

impl EdgeAddress {
    /// Accepts exactly what the API's `EdgeAddress` rule accepts: an IPv4
    /// address, a bracketed IPv6 address, or a DNS name, then a port from 1 to
    /// [`EDGE_ADDRESS_MAX_PORT`] without leading zeros. No scheme, no path.
    ///
    /// # Errors
    ///
    /// Returns the first rule the value breaks.
    pub fn parse(value: &str) -> Result<Self, InvalidEdgeAddress> {
        if value.is_empty() || value.len() > EDGE_ADDRESS_MAX_LENGTH {
            return Err(InvalidEdgeAddress::Length);
        }
        let (host, port) = value
            .rsplit_once(HOST_PORT_SEPARATOR)
            .ok_or(InvalidEdgeAddress::MissingPort)?;
        if !is_valid_port(port) {
            return Err(InvalidEdgeAddress::Port);
        }
        if is_valid_host(host) {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidEdgeAddress::Host)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `^[1-9][0-9]{0,4}$` and at most [`EDGE_ADDRESS_MAX_PORT`].
fn is_valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= PORT_MAX_DIGITS
        && !port.starts_with('0')
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

fn is_valid_host(host: &str) -> bool {
    if let Some(address) = host
        .strip_prefix(IPV6_OPEN_BRACKET)
        .and_then(|rest| rest.strip_suffix(IPV6_CLOSE_BRACKET))
    {
        return address.parse::<Ipv6Addr>().is_ok();
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    // A dotted quad that is not a valid IPv4 address is a typo, not a name.
    if !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || char::from(byte) == DNS_LABEL_SEPARATOR)
    {
        return false;
    }
    is_valid_hostname(host)
}

/// Dot-separated labels of letters, digits, and inner hyphens, each 1 to 63
/// characters, at most 253 characters in total and without a trailing dot.
fn is_valid_hostname(host: &str) -> bool {
    (1..=HOSTNAME_MAX_LENGTH).contains(&host.len())
        && host.split(DNS_LABEL_SEPARATOR).all(|label| {
            let edges_are_alphanumeric = label
                .starts_with(|first: char| first.is_ascii_alphanumeric())
                && label.ends_with(|last: char| last.is_ascii_alphanumeric());
            label.len() <= DNS_LABEL_MAX_LENGTH
                && edges_are_alphanumeric
                && label.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == DNS_LABEL_HYPHEN
                })
        })
}

impl fmt::Display for EdgeAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for EdgeAddress {
    type Error = InvalidEdgeAddress;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<EdgeAddress> for String {
    fn from(address: EdgeAddress) -> Self {
        address.0
    }
}

/// The rule an edge address breaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidEdgeAddress {
    Length,
    MissingPort,
    Port,
    Host,
}

impl fmt::Display for InvalidEdgeAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length => write!(
                formatter,
                "must be host:port of 1 to {EDGE_ADDRESS_MAX_LENGTH} characters"
            ),
            Self::MissingPort => {
                formatter.write_str("must be host:port without a scheme or a path")
            }
            Self::Port => write!(
                formatter,
                "must end in a port from 1 to {EDGE_ADDRESS_MAX_PORT} without leading zeros"
            ),
            Self::Host => formatter.write_str(
                "must start with an IPv4 address, a bracketed IPv6 address, or a DNS name",
            ),
        }
    }
}

impl std::error::Error for InvalidEdgeAddress {}

/// The heartbeat's `metadata.runtime` object. A key is omitted when it is not
/// configured, and the object itself is left out of the heartbeat when it has
/// no key, so the API never receives an empty value.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodePlacement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_address: Option<EdgeAddress>,
}

impl NodePlacement {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.region.is_none() && self.edge_address.is_none()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn region_accepts_the_api_pattern_up_to_its_length() {
        let longest = "a".repeat(REGION_MAX_LENGTH);
        for valid in ["jkt", "id-jog-1", "0", "a1-b2-c3", longest.as_str()] {
            assert_eq!(
                Region::parse(valid).map(|region| region.to_string()),
                Ok(valid.to_owned()),
                "{valid}"
            );
        }
    }

    #[test]
    fn region_rejects_what_the_api_rejects() {
        let too_long = "a".repeat(REGION_MAX_LENGTH + 1);
        for invalid in [
            "",
            "JKT",
            "id_jog",
            "-jkt",
            "jkt-",
            "id--jog",
            "id jog",
            " jkt",
            "jkt\n",
            "jkt.1",
            "jakárta",
            too_long.as_str(),
        ] {
            assert_eq!(Region::parse(invalid), Err(InvalidRegion), "{invalid:?}");
        }
    }

    #[test]
    fn edge_address_accepts_ipv4_bracketed_ipv6_and_dns_names() {
        let longest_label = "a".repeat(DNS_LABEL_MAX_LENGTH);
        let label_address = format!("{longest_label}.internal:8180");
        for valid in [
            "10.0.47.201:8180",
            "0.0.0.0:1",
            "255.255.255.255:65535",
            "[fd00::21]:8180",
            "[::1]:80",
            "[::ffff:10.0.0.1]:8180",
            "runtime-01.internal:8180",
            "runtime:8180",
            "Runtime-01.Example.COM:443",
            "1a.example:80",
            "10.0.0.1a:80",
            label_address.as_str(),
        ] {
            assert_eq!(
                EdgeAddress::parse(valid).map(|address| address.to_string()),
                Ok(valid.to_owned()),
                "{valid}"
            );
        }
    }

    #[test]
    fn edge_address_rejects_what_the_api_rejects() {
        let long_label = "a".repeat(DNS_LABEL_MAX_LENGTH + 1);
        let long_label_address = format!("{long_label}.internal:8180");
        let too_long = format!("{}:80", "a.".repeat(EDGE_ADDRESS_MAX_LENGTH / 2));
        for (invalid, reason) in [
            ("", InvalidEdgeAddress::Length),
            (too_long.as_str(), InvalidEdgeAddress::Length),
            ("10.0.47.201", InvalidEdgeAddress::MissingPort),
            ("runtime.internal", InvalidEdgeAddress::MissingPort),
            ("10.0.47.201:", InvalidEdgeAddress::Port),
            ("10.0.47.201:0", InvalidEdgeAddress::Port),
            ("10.0.47.201:08180", InvalidEdgeAddress::Port),
            ("10.0.47.201:65536", InvalidEdgeAddress::Port),
            ("10.0.47.201:123456", InvalidEdgeAddress::Port),
            ("10.0.47.201:+80", InvalidEdgeAddress::Port),
            ("10.0.47.201:80/", InvalidEdgeAddress::Port),
            ("http://10.0.47.201:8180", InvalidEdgeAddress::Host),
            ("10.0.47.201/api:8180", InvalidEdgeAddress::Host),
            (":8180", InvalidEdgeAddress::Host),
            ("fd00::21:8180", InvalidEdgeAddress::Host),
            ("[fd00::21%eth0]:8180", InvalidEdgeAddress::Host),
            ("[10.0.0.1]:8180", InvalidEdgeAddress::Host),
            ("[fd00::21:8180", InvalidEdgeAddress::Host),
            ("10.0.47.256:8180", InvalidEdgeAddress::Host),
            ("10.0.47:8180", InvalidEdgeAddress::Host),
            ("010.0.47.201:8180", InvalidEdgeAddress::Host),
            ("-runtime.internal:8180", InvalidEdgeAddress::Host),
            ("runtime-.internal:8180", InvalidEdgeAddress::Host),
            ("runtime..internal:8180", InvalidEdgeAddress::Host),
            ("runtime.internal.:8180", InvalidEdgeAddress::Host),
            ("runtime_01.internal:8180", InvalidEdgeAddress::Host),
            ("run time:8180", InvalidEdgeAddress::Host),
            ("runtime\n:8180", InvalidEdgeAddress::Host),
            (long_label_address.as_str(), InvalidEdgeAddress::Host),
        ] {
            assert_eq!(EdgeAddress::parse(invalid), Err(reason), "{invalid:?}");
        }
    }

    #[test]
    fn placement_values_keep_their_wire_form_and_refuse_invalid_input() {
        let placement = NodePlacement {
            region: Region::parse("id-jog-1").ok(),
            edge_address: EdgeAddress::parse("10.0.47.201:8180").ok(),
        };
        let wire = serde_json::to_value(&placement).expect("placement serializes");
        assert_eq!(
            wire,
            json!({ "region": "id-jog-1", "edge_address": "10.0.47.201:8180" })
        );
        assert_eq!(
            serde_json::from_value::<NodePlacement>(wire).expect("placement deserializes"),
            placement
        );
        assert!(serde_json::from_value::<NodePlacement>(json!({ "region": "JKT" })).is_err());
        assert!(
            serde_json::from_value::<NodePlacement>(json!({ "edge_address": "10.0.0.1" })).is_err()
        );
    }

    #[test]
    fn unset_placement_values_are_omitted_not_sent_empty() {
        assert!(NodePlacement::default().is_empty());
        assert_eq!(
            serde_json::to_value(NodePlacement::default()).expect("placement serializes"),
            json!({})
        );
        let region_only = NodePlacement {
            region: Region::parse("jkt").ok(),
            edge_address: None,
        };
        assert!(!region_only.is_empty());
        assert_eq!(
            serde_json::to_value(region_only).expect("placement serializes"),
            json!({ "region": "jkt" })
        );
    }
}
