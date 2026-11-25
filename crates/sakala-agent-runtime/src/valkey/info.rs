//! The fields of Valkey's `INFO` the heartbeat reports.

/// The sections [`StoreInfo::parse`] reads, in one `INFO` call.
pub(super) const INFO_SECTIONS: [&str; 5] =
    ["memory", "stats", "clients", "errorstats", "persistence"];

/// What one `INFO memory stats clients errorstats persistence` says.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct StoreInfo {
    pub used_memory: Option<u64>,
    pub maxmemory: Option<u64>,
    pub evicted_keys: Option<u64>,
    /// `errorstat_OOM`'s count. Valkey lists an error kind only once it
    /// happened, so a read `errorstats` section without it means 0.
    pub errorstat_oom: Option<u64>,
    pub connected_clients: Option<u64>,
    pub aof_enabled: bool,
    pub aof_last_write_status: Option<String>,
}

impl StoreInfo {
    /// Reads `field:value` lines; `# Section` headers mark which sections the
    /// reply holds. Lines end in `\r\n` as Valkey sends them.
    pub fn parse(reply: &str) -> Self {
        let mut info = Self::default();
        let mut errorstats = false;
        for line in reply.lines().map(|line| line.trim_end_matches('\r')) {
            if line.eq_ignore_ascii_case("# Errorstats") {
                errorstats = true;
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let number = || value.trim().parse::<u64>().ok();
            match field {
                "used_memory" => info.used_memory = number(),
                "maxmemory" => info.maxmemory = number(),
                "evicted_keys" => info.evicted_keys = number(),
                "connected_clients" => info.connected_clients = number(),
                "aof_enabled" => info.aof_enabled = value.trim() == "1",
                "aof_last_write_status" => {
                    info.aof_last_write_status = Some(value.trim().to_owned());
                }
                "errorstat_OOM" => {
                    info.errorstat_oom = value
                        .split(',')
                        .find_map(|pair| pair.strip_prefix("count="))
                        .and_then(|count| count.trim().parse().ok());
                }
                _ => {}
            }
        }
        if errorstats && info.errorstat_oom.is_none() {
            info.errorstat_oom = Some(0);
        }
        if !info.aof_enabled {
            info.aof_last_write_status = None;
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a Valkey 9.2.0 reply, with its `\r\n` line ends.
    const REPLY: &str = "# Memory\r\nused_memory:634968\r\nused_memory_human:620.09K\r\nmaxmemory:67108864\r\nmaxmemory_policy:noeviction\r\n\r\n# Stats\r\nevicted_keys:3\r\n\r\n# Clients\r\nconnected_clients:2\r\n\r\n# Errorstats\r\nerrorstat_NOPERM:count=1\r\nerrorstat_OOM:count=4\r\n\r\n# Persistence\r\naof_enabled:1\r\naof_last_write_status:ok\r\n";

    #[test]
    fn the_reported_fields_are_read() {
        assert_eq!(
            StoreInfo::parse(REPLY),
            StoreInfo {
                used_memory: Some(634_968),
                maxmemory: Some(67_108_864),
                evicted_keys: Some(3),
                errorstat_oom: Some(4),
                connected_clients: Some(2),
                aof_enabled: true,
                aof_last_write_status: Some("ok".to_owned()),
            }
        );
    }

    #[test]
    fn no_oom_line_is_zero_and_a_cache_store_has_no_aof_status() {
        let info = StoreInfo::parse(
            "# Errorstats\r\nerrorstat_NOAUTH:count=3\r\n# Persistence\r\naof_enabled:0\r\naof_last_write_status:ok\r\n",
        );
        assert_eq!(info.errorstat_oom, Some(0));
        assert_eq!(info.aof_last_write_status, None);
    }

    #[test]
    fn a_reply_without_a_section_leaves_its_fields_unknown() {
        let info = StoreInfo::parse("# Memory\r\nused_memory:10\r\n");
        assert_eq!(info.used_memory, Some(10));
        assert_eq!(info.errorstat_oom, None);
        assert_eq!(info.connected_clients, None);
        assert_eq!(StoreInfo::parse(""), StoreInfo::default());
    }
}
