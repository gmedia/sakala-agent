//! Per-container usage for `metadata.workloads.metrics`.
//!
//! One sample is three Docker calls: a `docker ps` of the managed workloads,
//! one `docker stats --no-stream` for every running container on the node,
//! and one `docker inspect` of the listed workloads for `RestartCount` and the
//! configured memory limit. `docker stats` is called without container IDs on
//! purpose: given one ID that no longer exists it fails as a whole and prints
//! nothing, while the full listing simply leaves a removed container out.
//!
//! `docker stats` prints human sizes such as `35.8MiB / 11.68GiB`, written by
//! Docker's own `units.BytesSize` with four significant digits and binary
//! suffixes. The size parser here only reads that fixed format, which is why
//! it is a few lines instead of a dependency (AGENTS.md, Dependency Rules).

use std::collections::HashMap;

use sakala_agent_protocol::WorkloadMetrics;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{RuntimeError, containers::replicas::ReplicaSlot};

/// `docker stats` format: one JSON object per container.
pub(crate) const STATS_FORMAT: &str = "{{json .}}";

/// `docker inspect` format of a sample: full ID, `RestartCount`, and the
/// memory limit in bytes that Docker was given, 0 when there is none.
pub(crate) const METRICS_INSPECT_FORMAT: &str =
    "{{.Id}}\t{{.RestartCount}}\t{{.HostConfig.Memory}}";

/// What Docker prints instead of a value it could not read.
const UNAVAILABLE_VALUE: &str = "--";

/// Separates usage from limit in `MemUsage`.
const MEMORY_USAGE_SEPARATOR: &str = " / ";

/// Byte multipliers of the size suffixes Docker prints, compared without
/// case. `units.BytesSize` writes the binary ones; the decimal ones are what
/// `units.HumanSize` writes for other columns, accepted in case a Docker
/// version switches.
const SIZE_UNITS: [(&str, f64); 13] = [
    ("b", 1.0),
    ("kib", 1_024.0),
    ("mib", 1_048_576.0),
    ("gib", 1_073_741_824.0),
    ("tib", 1_099_511_627_776.0),
    ("pib", 1_125_899_906_842_624.0),
    ("eib", 1_152_921_504_606_846_976.0),
    ("kb", 1e3),
    ("mb", 1e6),
    ("gb", 1e9),
    ("tb", 1e12),
    ("pb", 1e15),
    ("eb", 1e18),
];

/// One running container of a managed workload, from the sample's listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MetricsTarget {
    pub container_id: String,
    pub project_id: Uuid,
    pub deployment_id: Uuid,
    pub replica: ReplicaSlot,
}

/// One line of `docker stats`. Values Docker could not read are `None`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StatsReading {
    pub container_id: String,
    pub usage: Option<StatsUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StatsUsage {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
}

/// One line of [`METRICS_INSPECT_FORMAT`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InspectedState {
    pub restart_count: u32,
    pub memory_limit_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct StatsLine {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "CPUPerc")]
    cpu_percent: String,
    #[serde(rename = "MemUsage")]
    memory_usage: String,
}

/// Reads one line of the metrics listing: ID, status, project, deployment,
/// replica index, replica count, and role. A container with a role, which today
/// is only a release container, is not a workload, and only a container Docker
/// shows as up is sampled: a restarting one has no usage to report.
pub(crate) fn parse_metrics_target(line: &str) -> Option<MetricsTarget> {
    let fields = line.split('\t').collect::<Vec<_>>();
    let container_id = fields
        .first()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())?;
    let running = fields
        .get(1)
        .is_some_and(|status| status.trim().to_ascii_lowercase().starts_with("up"));
    let has_role = fields.get(6).is_some_and(|role| !role.trim().is_empty());
    if !running || has_role {
        return None;
    }
    Some(MetricsTarget {
        container_id: container_id.to_owned(),
        project_id: Uuid::parse_str(fields.get(2)?.trim()).ok()?,
        deployment_id: Uuid::parse_str(fields.get(3)?.trim()).ok()?,
        replica: ReplicaSlot::from_labels(fields.get(4).copied(), fields.get(5).copied()),
    })
}

/// Reads `docker stats --format '{{json .}}'`. A line that is not the JSON
/// Docker prints means its output changed, and fails the sample rather than
/// guessing; a value Docker could not read only makes that container's usage
/// unknown.
pub(crate) fn parse_stats(stdout: &str) -> Result<Vec<StatsReading>, RuntimeError> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let parsed = serde_json::from_str::<StatsLine>(line).map_err(|error| {
                RuntimeError::Container(format!("docker stats printed an unreadable line: {error}"))
            })?;
            Ok(StatsReading {
                usage: usage(&parsed.cpu_percent, &parsed.memory_usage),
                container_id: parsed.id,
            })
        })
        .collect()
}

/// A running process always holds some memory, so `0B` is what Docker prints
/// when it has no cgroup reading, as for a container that just stopped.
fn usage(cpu_percent: &str, memory_usage: &str) -> Option<StatsUsage> {
    let memory_bytes = memory_usage
        .split(MEMORY_USAGE_SEPARATOR)
        .next()
        .and_then(parse_size)
        .filter(|bytes| *bytes > 0)?;
    Some(StatsUsage {
        cpu_percent: parse_percent(cpu_percent)?,
        memory_bytes,
    })
}

/// Reads a `CPUPerc` value such as `12.34%`.
pub(crate) fn parse_percent(value: &str) -> Option<f64> {
    let value = value.trim();
    if value == UNAVAILABLE_VALUE {
        return None;
    }
    value
        .strip_suffix('%')?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|percent| percent.is_finite() && *percent >= 0.0)
}

/// Reads a size such as `35.8MiB`, `512B`, or `1.5GB` as whole bytes.
pub(crate) fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let number = value.trim_end_matches(|character: char| character.is_ascii_alphabetic());
    let unit = value[number.len()..].to_ascii_lowercase();
    let multiplier = SIZE_UNITS
        .iter()
        .find(|(suffix, _)| *suffix == unit)
        .map(|(_, multiplier)| *multiplier)?;
    let bytes = number.trim().parse::<f64>().ok()? * multiplier;
    // `u64::MAX as f64` rounds up to 2^64, which itself does not fit.
    #[allow(clippy::cast_precision_loss)]
    let ceiling = u64::MAX as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes >= ceiling {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(bytes.round() as u64)
}

/// Reads `docker inspect --format` [`METRICS_INSPECT_FORMAT`], by full ID.
pub(crate) fn parse_inspected_states(stdout: &str) -> HashMap<String, InspectedState> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            let restart_count = fields.next()?.trim().parse::<u32>().ok()?;
            let memory = fields.next()?.trim().parse::<i64>().ok()?;
            (!id.is_empty()).then(|| {
                (
                    id.to_owned(),
                    InspectedState {
                        restart_count,
                        memory_limit_bytes: u64::try_from(memory).ok().filter(|bytes| *bytes > 0),
                    },
                )
            })
        })
        .collect()
}

/// Joins the three readings of one sample into heartbeat entries, ordered by
/// project, deployment, and replica so that bounding the list is deterministic.
///
/// A listed container missing from `docker stats` or `docker inspect`, or one
/// whose usage Docker could not read, was most likely removed or stopped in
/// between and is left out. When containers were listed but none of them has
/// usable values, the sample fails instead: that is a runtime that does not
/// report usage at all, and an empty list would read as "nothing runs".
pub(crate) fn join_sample(
    targets: &[MetricsTarget],
    readings: &[StatsReading],
    states: &HashMap<String, InspectedState>,
    sampled_at: OffsetDateTime,
) -> Result<Vec<WorkloadMetrics>, RuntimeError> {
    let mut metrics = targets
        .iter()
        .filter_map(|target| {
            let usage = readings
                .iter()
                .find(|reading| same_container(&reading.container_id, &target.container_id))?
                .usage?;
            let state = states
                .iter()
                .find(|(id, _)| same_container(id, &target.container_id))
                .map(|(_, state)| *state)?;
            Some(WorkloadMetrics {
                project_id: target.project_id,
                deployment_id: target.deployment_id,
                replica: target.replica.index,
                sampled_at,
                cpu_percent: usage.cpu_percent,
                memory_bytes: usage.memory_bytes,
                memory_limit_bytes: state.memory_limit_bytes,
                restart_count: state.restart_count,
            })
        })
        .collect::<Vec<_>>();
    if !targets.is_empty() && metrics.is_empty() {
        return Err(RuntimeError::Container(format!(
            "docker stats reported no usable values for any of {} running workload containers",
            targets.len()
        )));
    }
    metrics.sort_by_key(|item| (item.project_id, item.deployment_id, item.replica));
    Ok(metrics)
}

/// Docker prints a short ID in `docker ps` and `docker stats` and the full one
/// in `docker inspect`; both are prefixes of the same full ID.
fn same_container(left: &str, right: &str) -> bool {
    !left.is_empty() && !right.is_empty() && (left.starts_with(right) || right.starts_with(left))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    const PROJECT: &str = "ff66ed4a-6303-4be6-8ef4-63c28b112680";
    const DEPLOYMENT: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";
    const FULL_ID: &str = "ae8627102703f3b1c0c4b5a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1";

    /// Real lines from `docker stats --no-stream --format '{{json .}}'`
    /// (Docker 29): a running container and one that had just stopped.
    const RUNNING_LINE: &str = r#"{"BlockIO":"1.04GB / 133MB","CPUPerc":"0.11%","Container":"ae8627102703","ID":"ae8627102703","MemPerc":"0.30%","MemUsage":"35.8MiB / 11.68GiB","Name":"karsa-mcp-gateway-agentgateway-1","NetIO":"132MB / 129MB","PIDs":"21"}"#;
    const STOPPED_LINE: &str = r#"{"BlockIO":"0B / 0B","CPUPerc":"0.00%","Container":"f9877f3da824","ID":"f9877f3da824","MemPerc":"0.00%","MemUsage":"0B / 0B","Name":"bkk-lab-php","NetIO":"0B / 0B","PIDs":"0"}"#;

    fn target(id: &str, index: u32, count: u32) -> MetricsTarget {
        MetricsTarget {
            container_id: id.to_owned(),
            project_id: Uuid::parse_str(PROJECT).expect("project"),
            deployment_id: Uuid::parse_str(DEPLOYMENT).expect("deployment"),
            replica: ReplicaSlot {
                index,
                count: NonZeroU32::new(count).expect("count"),
            },
        }
    }

    #[test]
    fn sizes_in_every_unit_docker_prints_become_bytes() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("512B"), Some(512));
        assert_eq!(parse_size("1KiB"), Some(1_024));
        assert_eq!(parse_size("3.523MiB"), Some(3_694_133));
        assert_eq!(parse_size("35.8MiB"), Some(37_539_021));
        assert_eq!(parse_size("256MiB"), Some(268_435_456));
        assert_eq!(parse_size("11.68GiB"), Some(12_541_304_504));
        assert_eq!(parse_size("1.5TiB"), Some(1_649_267_441_664));
        assert_eq!(parse_size("807kB"), Some(807_000));
        assert_eq!(parse_size("1.04GB"), Some(1_040_000_000));
        assert_eq!(parse_size(" 12 MiB "), Some(12_582_912));
        assert_eq!(parse_size("2mib"), Some(2_097_152));
        assert_eq!(parse_size("1.024e+03KiB"), Some(1_048_576));
    }

    #[test]
    fn sizes_docker_could_not_read_are_unknown() {
        for value in [
            "--", "", "MiB", "12", "12XB", "-1MiB", "NaNMiB", "infGiB", "99999EiB",
        ] {
            assert_eq!(parse_size(value), None, "{value:?}");
        }
    }

    #[test]
    fn cpu_percent_may_pass_one_hundred_on_several_cores() {
        assert_eq!(parse_percent("0.00%"), Some(0.0));
        assert_eq!(parse_percent("12.34%"), Some(12.34));
        assert_eq!(parse_percent("187.5%"), Some(187.5));
        for value in ["--", "", "12.3", "-1%", "NaN%", "%"] {
            assert_eq!(parse_percent(value), None, "{value:?}");
        }
    }

    #[test]
    fn stats_lines_keep_usable_values_and_mark_unreadable_ones_unknown() {
        let readings = parse_stats(&format!("{RUNNING_LINE}\n{STOPPED_LINE}\n\n"))
            .expect("Docker's lines parse");

        assert_eq!(
            readings,
            vec![
                StatsReading {
                    container_id: "ae8627102703".to_owned(),
                    usage: Some(StatsUsage {
                        cpu_percent: 0.11,
                        memory_bytes: 37_539_021,
                    }),
                },
                StatsReading {
                    container_id: "f9877f3da824".to_owned(),
                    usage: None,
                },
            ]
        );
    }

    #[test]
    fn a_runtime_that_prints_dashes_has_unknown_usage() {
        let readings =
            parse_stats(r#"{"CPUPerc":"--","ID":"ae8627102703","MemUsage":"-- / --","Name":"x"}"#)
                .expect("the line parses");

        assert_eq!(readings[0].usage, None);
    }

    #[test]
    fn output_that_is_not_dockers_json_fails_the_sample() {
        assert!(parse_stats("CONTAINER ID   NAME   CPU %").is_err());
        assert!(parse_stats(r#"{"ID":"ae8627102703"}"#).is_err());
        assert_eq!(parse_stats("").expect("no container"), Vec::new());
    }

    #[test]
    fn listing_keeps_running_workloads_only() {
        let up = format!("abc\tUp 3 minutes\t{PROJECT}\t{DEPLOYMENT}\t1\t3\t");
        assert_eq!(parse_metrics_target(&up), Some(target("abc", 1, 3)));
        let single = format!("abc\tUp 1 second (healthy)\t{PROJECT}\t{DEPLOYMENT}\t\t\t");
        assert_eq!(parse_metrics_target(&single), Some(target("abc", 0, 1)));

        let release = format!("abc\tUp 3 seconds\t{PROJECT}\t{DEPLOYMENT}\t\t\trelease");
        let restarting =
            format!("abc\tRestarting (1) 2 seconds ago\t{PROJECT}\t{DEPLOYMENT}\t\t\t");
        let unlabelled = "abc\tUp 3 minutes\t\t\t\t\t";
        for line in [release.as_str(), restarting.as_str(), unlabelled, ""] {
            assert_eq!(parse_metrics_target(line), None, "{line:?}");
        }
    }

    #[test]
    fn inspect_states_read_the_restart_count_and_a_missing_limit_as_null() {
        let states = parse_inspected_states(&format!(
            "{FULL_ID}\t4\t268435456\nbbbb\t0\t0\nbroken line\n"
        ));

        assert_eq!(
            states.get(FULL_ID),
            Some(&InspectedState {
                restart_count: 4,
                memory_limit_bytes: Some(268_435_456),
            })
        );
        assert_eq!(
            states.get("bbbb"),
            Some(&InspectedState {
                restart_count: 0,
                memory_limit_bytes: None,
            })
        );
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn a_sample_joins_short_and_full_ids_and_skips_containers_gone_in_between() {
        let sampled_at = OffsetDateTime::UNIX_EPOCH;
        let readings = parse_stats(RUNNING_LINE).expect("stats");
        let states = parse_inspected_states(&format!("{FULL_ID}\t2\t268435456\n"));

        let metrics = join_sample(
            &[target("ae8627102703", 1, 2), target("0123456789ab", 0, 2)],
            &readings,
            &states,
            sampled_at,
        )
        .expect("one container has values");

        assert_eq!(
            metrics,
            vec![WorkloadMetrics {
                project_id: Uuid::parse_str(PROJECT).expect("project"),
                deployment_id: Uuid::parse_str(DEPLOYMENT).expect("deployment"),
                replica: 1,
                sampled_at,
                cpu_percent: 0.11,
                memory_bytes: 37_539_021,
                memory_limit_bytes: Some(268_435_456),
                restart_count: 2,
            }]
        );
    }

    #[test]
    fn a_sample_where_no_listed_container_has_values_fails() {
        let readings = parse_stats(
            r#"{"CPUPerc":"0.00%","ID":"ae8627102703","MemUsage":"0B / 0B","Name":"x"}"#,
        )
        .expect("stats");
        let states = parse_inspected_states(&format!("{FULL_ID}\t0\t268435456\n"));

        assert!(
            join_sample(
                &[target("ae8627102703", 0, 1)],
                &readings,
                &states,
                OffsetDateTime::UNIX_EPOCH,
            )
            .is_err()
        );
        assert_eq!(
            join_sample(&[], &readings, &states, OffsetDateTime::UNIX_EPOCH).expect("no target"),
            Vec::new()
        );
    }
}
