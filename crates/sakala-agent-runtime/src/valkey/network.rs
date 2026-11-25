//! Store networks: the operator's address pool, the /27 each store takes from
//! it, and the check a deployment runs before its containers join one.

use std::{fmt, net::Ipv4Addr, str::FromStr};

use ipnet::Ipv4Net;
use sakala_agent_protocol::ValkeyAttachment;

use crate::{CommandSpec, NullOutputSink, ProcessRunner, RuntimeError};

/// Every store network is a /27: 32 addresses, of which Docker takes `.1` as
/// the bridge, the store `.2`, and up to 28 containers of attached projects
/// the rest, enough for two projects in the middle of a redeploy.
pub const VALKEY_SUBNET_PREFIX: u8 = 27;

/// The store's address inside its /27.
const STORE_HOST_OFFSET: u32 = 2;

/// `docker network inspect` of a store network: its kind, its agent, and its
/// subnet.
pub(super) const STORE_NETWORK_FORMAT: &str = "{{index .Labels \"dev.sakala.network-kind\"}}\t{{index .Labels \"dev.sakala.agent-id\"}}\t{{range .IPAM.Config}}{{.Subnet}} {{end}}";

/// The value of `dev.sakala.network-kind` on a store network.
pub(crate) const VALKEY_NETWORK_KIND: &str = "valkey";

/// Address ranges a store network must never take: this host, link-local,
/// multicast, and the reserved blocks.
const FORBIDDEN_RANGES: [&str; 4] = ["0.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "224.0.0.0/3"];

/// `SAKALA_VALKEY_NETWORK_POOL`: the IPv4 range store networks are cut from,
/// such as `10.213.0.0/16` (2,048 stores).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValkeyNetworkPool(Ipv4Net);

impl ValkeyNetworkPool {
    #[must_use]
    pub fn net(self) -> Ipv4Net {
        self.0
    }

    /// How many store networks the pool holds.
    #[must_use]
    pub fn capacity(self) -> u64 {
        1_u64 << (VALKEY_SUBNET_PREFIX - self.0.prefix_len())
    }

    /// The first /27 of the pool that overlaps none of `taken`.
    pub(super) fn free_subnet(self, taken: &[Ipv4Net]) -> Option<Ipv4Net> {
        self.0
            .subnets(VALKEY_SUBNET_PREFIX)
            .ok()?
            .find(|candidate| !taken.iter().any(|used| overlaps(*candidate, *used)))
    }

    /// Ranges of `others` the pool overlaps, for the startup warning.
    pub(super) fn overlapping(self, others: &[Ipv4Net]) -> Vec<Ipv4Net> {
        others
            .iter()
            .copied()
            .filter(|other| overlaps(self.0, *other))
            .collect()
    }
}

impl fmt::Display for ValkeyNetworkPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ValkeyNetworkPool {
    type Err = &'static str;

    /// An IPv4 network in CIDR form, written at its network address, with
    /// room for at least one /27, outside this host's, link-local, and
    /// multicast ranges.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let net: Ipv4Net = value
            .trim()
            .parse()
            .map_err(|_| "must be an IPv4 network in CIDR form, such as 10.213.0.0/16")?;
        if net.trunc() != net {
            return Err("must name its network address, such as 10.213.0.0/16");
        }
        if net.prefix_len() > VALKEY_SUBNET_PREFIX {
            return Err("must hold at least one /27, so its prefix must be /27 or shorter");
        }
        if FORBIDDEN_RANGES
            .iter()
            .filter_map(|range| range.parse::<Ipv4Net>().ok())
            .any(|range| overlaps(net, range))
        {
            return Err("must not overlap 0.0.0.0/8, 127.0.0.0/8, 169.254.0.0/16, or 224.0.0.0/3");
        }
        Ok(Self(net))
    }
}

/// A store network as `docker network inspect` with
/// [`STORE_NETWORK_FORMAT`] reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct NetworkState {
    pub kind: String,
    pub agent_id: String,
    pub subnets: Vec<Ipv4Net>,
}

impl NetworkState {
    pub fn parse(output: &str) -> Self {
        let mut fields = output.trim().split('\t');
        Self {
            kind: fields.next().unwrap_or_default().to_owned(),
            agent_id: fields.next().unwrap_or_default().to_owned(),
            subnets: parse_subnets(fields.next().unwrap_or_default()),
        }
    }

    /// Whether it is a store network of `agent_id`.
    pub fn owned_by(&self, agent_id: &str) -> bool {
        self.kind == VALKEY_NETWORK_KIND && self.agent_id == agent_id
    }
}

/// The store's own address in its network.
pub(super) fn store_host(subnet: Ipv4Net) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.network()) + STORE_HOST_OFFSET)
}

pub(super) fn overlaps(left: Ipv4Net, right: Ipv4Net) -> bool {
    left.contains(&right.network()) || right.contains(&left.network())
}

/// The destinations of `/proc/net/route`, the default route left out. Each
/// line holds the interface, then the destination and, in the eighth column,
/// the mask, both as hex of the host's native integer.
pub(super) fn parse_routes(table: &str) -> Vec<Ipv4Net> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            // The kernel prints the address as a native integer, so its
            // native bytes are the address in network order.
            let address = |column: &str| {
                u32::from_str_radix(column, 16)
                    .ok()
                    .map(|value| Ipv4Addr::from(value.to_ne_bytes()))
            };
            let destination = address(columns.get(1)?)?;
            let mask = u32::from(address(columns.get(7)?)?);
            if mask == 0 {
                return None;
            }
            let prefix = u8::try_from(mask.leading_ones()).ok()?;
            Ipv4Net::new(destination, prefix)
                .ok()
                .map(|net| net.trunc())
        })
        .collect()
}

/// Subnets of `docker network inspect --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}'`
/// output, IPv6 left out.
pub(super) fn parse_subnets(output: &str) -> Vec<Ipv4Net> {
    output
        .split_whitespace()
        .filter_map(|subnet| subnet.parse::<Ipv4Net>().ok())
        .collect()
}

/// Checks, before any container of a deployment or job is created, that the
/// `valkey` field names a store network of this agent whose store is at
/// `host`. Without it a container would start with `REDIS_HOST` pointing at
/// nothing it can reach.
pub(crate) async fn check_store_network(
    runner: &dyn ProcessRunner,
    agent_id: &str,
    attachment: &ValkeyAttachment,
) -> Result<(), RuntimeError> {
    let inspect = CommandSpec::new("docker")
        .arg("network")
        .arg("inspect")
        .arg("--format")
        .arg(STORE_NETWORK_FORMAT)
        .arg(&attachment.network);
    let output = runner.run(&inspect, &NullOutputSink).await?;
    let unavailable = |reason: String| RuntimeError::ValkeyNetworkUnavailable(reason);
    if !output.success {
        let stderr = output.stderr.to_ascii_lowercase();
        if stderr.contains("not found") || stderr.contains("no such network") {
            return Err(unavailable(format!(
                "the Valkey store network {} does not exist on this node",
                attachment.network
            )));
        }
        return Err(unavailable(format!(
            "the Valkey store network {} could not be inspected: {}",
            attachment.network,
            crate::error::describe_exit(output.code)
        )));
    }
    let network = NetworkState::parse(&output.stdout);
    if !network.owned_by(agent_id) {
        return Err(unavailable(format!(
            "{} is not a Valkey store network of this agent",
            attachment.network
        )));
    }
    let Some(subnet) = network.subnets.first().copied() else {
        return Err(unavailable(format!(
            "the Valkey store network {} has no IPv4 subnet",
            attachment.network
        )));
    };
    let host = store_host(subnet);
    if host != attachment.host {
        return Err(unavailable(format!(
            "the Valkey store on {} is at {host}, not at valkey.host {}",
            attachment.network, attachment.host
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(value: &str) -> Ipv4Net {
        value.parse().expect("net")
    }

    #[test]
    fn a_pool_is_a_network_with_room_for_a_store() {
        let pool: ValkeyNetworkPool = "10.213.0.0/16".parse().expect("pool");
        assert_eq!(pool.capacity(), 2_048);
        assert_eq!(pool.to_string(), "10.213.0.0/16");
        assert_eq!(
            "10.213.0.0/27"
                .parse::<ValkeyNetworkPool>()
                .expect("one store")
                .capacity(),
            1
        );
        for invalid in [
            "",
            "10.213.0.0",
            "10.213.0.1/16",
            "10.213.0.0/28",
            "fd00::/48",
            "127.0.0.0/16",
            "169.254.0.0/24",
            "224.0.0.0/24",
            "0.0.0.0/0",
            "10.213.0.0/33",
        ] {
            assert!(invalid.parse::<ValkeyNetworkPool>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn the_first_free_twenty_seven_is_taken_and_the_store_sits_at_its_second_address() {
        let pool: ValkeyNetworkPool = "10.213.0.0/25".parse().expect("pool");
        assert_eq!(pool.free_subnet(&[]), Some(net("10.213.0.0/27")));
        assert_eq!(
            pool.free_subnet(&[net("10.213.0.0/27"), net("10.213.0.64/27")]),
            Some(net("10.213.0.32/27"))
        );
        // A wider network that covers part of the pool, such as a LAN route.
        assert_eq!(
            pool.free_subnet(&[net("10.213.0.0/26")]),
            Some(net("10.213.0.64/27"))
        );
        // A narrower one inside a candidate.
        assert_eq!(
            pool.free_subnet(&[net("10.213.0.8/29")]),
            Some(net("10.213.0.32/27"))
        );
        assert_eq!(pool.free_subnet(&[net("10.0.0.0/8")]), None);
        assert_eq!(
            store_host(net("10.213.0.32/27")),
            Ipv4Addr::new(10, 213, 0, 34)
        );
        assert_eq!(
            pool.overlapping(&[net("10.0.0.0/8"), net("172.17.0.0/16")]),
            vec![net("10.0.0.0/8")]
        );
    }

    #[test]
    fn host_routes_are_read_without_the_default_route() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t012FA8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
eth0\t002F000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
br-1\t2000D50A\t00000000\t0001\t0\t0\t0\tE0FFFFFF\t0\t0\t0\n";
        assert_eq!(
            parse_routes(table),
            vec![
                net("10.0.47.0/24"),
                net("172.17.0.0/16"),
                net("10.213.0.32/27")
            ]
        );
        assert!(parse_routes("").is_empty());
        assert!(parse_routes("Iface\tDestination\ngarbage line\n").is_empty());
    }

    #[test]
    fn subnets_are_read_from_ipam_output() {
        assert_eq!(
            parse_subnets("10.213.0.0/27 fd00::/64 \n172.18.0.0/16 \n"),
            vec![net("10.213.0.0/27"), net("172.18.0.0/16")]
        );
    }
}
