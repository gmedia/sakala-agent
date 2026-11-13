//! Request activity per host, read from the router's own request counters.
//!
//! Caddy counts requests in `caddy_http_requests_total`, and with
//! `metrics { per_host }` it labels them with the site's host. The counters say
//! how many requests a host received since Caddy started, not when, so the
//! agent compares each read with the previous one: a counter that changed means
//! the host was visited since the last read.
//!
//! Only this one counter family is read, so the parser understands just the
//! sample lines of the Prometheus text exposition format (version 0.0.4) that
//! Caddy writes: `name{label="value",...} value [timestamp]`.

use std::collections::HashMap;

use time::OffsetDateTime;

const REQUESTS_METRIC: &str = "caddy_http_requests_total";
const HOST_LABEL: &str = "host";
/// Caddy's bucket for every host that has no site of its own.
const OTHER_HOSTS: &str = "_other";

/// Total requests per host, summed over every series of the counter.
///
/// `None` when no series carries a host label: either the router does not count
/// per host (`metrics { per_host }` is missing), or it has served no request
/// since it started. Neither tells the agent anything about activity.
pub(crate) fn request_totals_by_host(text: &str) -> Option<HashMap<String, u64>> {
    let mut totals = HashMap::new();
    let mut labelled = false;
    for line in text.lines() {
        let Some(sample) = parse_sample(line) else {
            continue;
        };
        let Some(host) = sample.host else {
            continue;
        };
        labelled = true;
        let host = normalize_host(&host);
        if host.is_empty() || host == OTHER_HOSTS {
            continue;
        }
        let total = totals.entry(host).or_insert(0_u64);
        *total = total.saturating_add(sample.value);
    }
    labelled.then_some(totals)
}

struct Sample {
    host: Option<String>,
    value: u64,
}

/// Reads one line of the requests counter. Comments, other metric families, and
/// lines that do not parse are skipped: one odd line must not hide the rest.
fn parse_sample(line: &str) -> Option<Sample> {
    let rest = line.trim_start().strip_prefix(REQUESTS_METRIC)?;
    let (labels, rest) = match rest.strip_prefix('{') {
        Some(labelled) => parse_labels(labelled)?,
        // The name must end here, or this is another metric with a longer name.
        None if rest.starts_with([' ', '\t']) => (Vec::new(), rest),
        None => return None,
    };
    let value = rest.split_whitespace().next()?.parse::<f64>().ok()?;
    // A request counter only holds whole, non-negative numbers. Large ones are
    // written in exponent form, such as `1e+06`.
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u64::MAX as f64 {
        return None;
    }
    let host = labels
        .into_iter()
        .find_map(|(name, value)| (name == HOST_LABEL).then_some(value));
    Some(Sample {
        host,
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        value: value as u64,
    })
}

/// Parses `name="value",...}` and returns the labels and what follows the `}`.
/// Label values may hold `\\`, `\"`, and `\n` escapes.
fn parse_labels(input: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels = Vec::new();
    let mut rest = input;
    loop {
        rest = rest.trim_start_matches([' ', ',']);
        if let Some(after) = rest.strip_prefix('}') {
            return Some((labels, after));
        }
        let name_end =
            rest.find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))?;
        let name = &rest[..name_end];
        if name.is_empty() {
            return None;
        }
        rest = rest[name_end..].trim_start().strip_prefix('=')?;
        rest = rest.trim_start().strip_prefix('"')?;
        let mut value = String::new();
        let mut characters = rest.char_indices();
        let end = loop {
            let (index, character) = characters.next()?;
            match character {
                '"' => break index,
                '\\' => match characters.next()?.1 {
                    'n' => value.push('\n'),
                    escaped => value.push(escaped),
                },
                other => value.push(other),
            }
        };
        labels.push((name.to_owned(), value));
        rest = &rest[end + 1..];
    }
}

/// Matches the domain a workload is routed under: lowercase, without a trailing
/// dot or a port. Caddy already lowercases the label and files a host with a
/// port under `_other`; this keeps the match right if that ever changes.
fn normalize_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            name.to_owned()
        }
        _ => host,
    };
    host.trim_end_matches('.').to_owned()
}

/// Keeps the last request time per host between reads.
pub(crate) struct TrafficTracker {
    started_at: OffsetDateTime,
    /// `None` until the first read that had per-host counters.
    hosts: Option<HashMap<String, HostActivity>>,
}

struct HostActivity {
    requests: u64,
    last_request_at: OffsetDateTime,
}

impl TrafficTracker {
    pub(crate) fn new(started_at: OffsetDateTime) -> Self {
        Self {
            started_at,
            hosts: None,
        }
    }

    /// Records one read and returns the last request time of every host in it.
    ///
    /// On the first read every host gets the agent's start time: the counters
    /// say nothing about when their requests arrived, and "never" would make a
    /// restarted agent report every project as idle. After that, a host whose
    /// count changed, or that was not in the previous read, was visited now. A
    /// count that went down also means a visit: the router restarted, lost its
    /// counters, and has served at least one request since. A host missing from
    /// this read is forgotten, so a deleted project does not stay in memory.
    pub(crate) fn observe(
        &mut self,
        totals: HashMap<String, u64>,
        now: OffsetDateTime,
    ) -> HashMap<String, OffsetDateTime> {
        let previous = self.hosts.take();
        let mut hosts = HashMap::with_capacity(totals.len());
        for (host, requests) in totals {
            let last_request_at = match &previous {
                None => self.started_at,
                Some(previous) => match previous.get(&host) {
                    Some(seen) if seen.requests == requests => seen.last_request_at,
                    _ => now,
                },
            };
            hosts.insert(
                host,
                HostActivity {
                    requests,
                    last_request_at,
                },
            );
        }
        let observed = hosts
            .iter()
            .map(|(host, activity)| (host.clone(), activity.last_request_at))
            .collect();
        self.hosts = Some(hosts);
        observed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use time::{Duration, OffsetDateTime};

    use super::{TrafficTracker, request_totals_by_host};

    /// Complete `GET /metrics` output of `caddy:2.11.3-alpine` with
    /// `metrics { per_host }`, captured from a real container after three
    /// requests to `alpha`, two to `beta` (one with an uppercase Host), and
    /// three that Caddy filed under `_other`: two unknown hosts and one Host
    /// with `:80`. Its histograms carry the same host labels and must not count.
    const CADDY_METRICS: &str = include_str!("../../tests/fixtures/caddy-2.11.3-metrics.txt");

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds)
    }

    #[test]
    fn sums_real_caddy_counters_per_host_and_skips_other_hosts() {
        let totals = request_totals_by_host(CADDY_METRICS).expect("per-host counters");

        assert_eq!(
            totals,
            HashMap::from([
                ("alpha.run.sakala.localhost".to_owned(), 3),
                ("beta.run.sakala.localhost".to_owned(), 2),
            ])
        );
    }

    #[test]
    fn adds_up_every_series_of_one_host() {
        let text = r#"
# HELP caddy_http_requests_total Counter of HTTP(S) requests made.
# TYPE caddy_http_requests_total counter
caddy_http_requests_total{handler="subroute",host="portfolio.run.sakala.dev",server="srv0"} 4
caddy_http_requests_total{handler="reverse_proxy",host="portfolio.run.sakala.dev",server="srv0"} 1e+06
caddy_http_requests_total{server="srv1",host="portfolio.run.sakala.dev"} 2 1700000000000
caddy_http_requests_total_created{host="portfolio.run.sakala.dev"} 1.7e+09
caddy_http_request_duration_seconds_count{host="portfolio.run.sakala.dev"} 99
"#;

        let totals = request_totals_by_host(text).expect("per-host counters");

        assert_eq!(totals["portfolio.run.sakala.dev"], 1_000_006);
        assert_eq!(totals.len(), 1);
    }

    #[test]
    fn reads_escaped_label_values_and_normalizes_the_host() {
        let text = concat!(
            r#"caddy_http_requests_total{handler="a\"b\\c\nd",host="Shop.Run.Sakala.Dev.:80",server="srv0"} 3"#,
            "\n",
        );

        let totals = request_totals_by_host(text).expect("per-host counters");

        assert_eq!(totals["shop.run.sakala.dev"], 3);
    }

    #[test]
    fn skips_lines_it_cannot_read_without_losing_the_others() {
        let text = r#"
caddy_http_requests_total{host="broken.run.sakala.dev" 5
caddy_http_requests_total{host="unterminated.run.sakala.dev} 5
caddy_http_requests_total{host="negative.run.sakala.dev"} -1
caddy_http_requests_total{host="fraction.run.sakala.dev"} 1.5
caddy_http_requests_total{host="nan.run.sakala.dev"} NaN
caddy_http_requests_total{host="infinite.run.sakala.dev"} +Inf
caddy_http_requests_total{host="good.run.sakala.dev"} 7
"#;

        let totals = request_totals_by_host(text).expect("per-host counters");

        assert_eq!(
            totals,
            HashMap::from([("good.run.sakala.dev".to_owned(), 7)])
        );
    }

    #[test]
    fn counters_without_host_labels_say_nothing_about_activity() {
        // Without `per_host`, or before the router served anything.
        assert_eq!(
            request_totals_by_host(
                "caddy_http_requests_total{handler=\"subroute\",server=\"srv0\"} 12\n"
            ),
            None
        );
        assert_eq!(request_totals_by_host("go_goroutines 12\n"), None);
        assert_eq!(request_totals_by_host(""), None);
        // Only unknown hosts so far is still a per-host read.
        assert_eq!(
            request_totals_by_host("caddy_http_requests_total{host=\"_other\"} 1\n"),
            Some(HashMap::new())
        );
    }

    #[test]
    fn first_read_dates_every_host_at_agent_start_instead_of_never() {
        let mut tracker = TrafficTracker::new(at(100));

        let observed =
            tracker.observe(HashMap::from([("a.run.sakala.dev".to_owned(), 5)]), at(160));

        assert_eq!(observed["a.run.sakala.dev"], at(100));
    }

    #[test]
    fn a_changed_count_is_a_visit_and_an_unchanged_one_keeps_its_time() {
        let mut tracker = TrafficTracker::new(at(100));
        tracker.observe(
            HashMap::from([
                ("busy.run.sakala.dev".to_owned(), 5),
                ("quiet.run.sakala.dev".to_owned(), 9),
                ("restarted.run.sakala.dev".to_owned(), 40),
            ]),
            at(110),
        );

        let observed = tracker.observe(
            HashMap::from([
                ("busy.run.sakala.dev".to_owned(), 6),
                ("quiet.run.sakala.dev".to_owned(), 9),
                // The router restarted and counted from zero again.
                ("restarted.run.sakala.dev".to_owned(), 2),
                // First request since the agent started counting.
                ("new.run.sakala.dev".to_owned(), 1),
            ]),
            at(120),
        );

        assert_eq!(observed["busy.run.sakala.dev"], at(120));
        assert_eq!(observed["quiet.run.sakala.dev"], at(100));
        assert_eq!(observed["restarted.run.sakala.dev"], at(120));
        assert_eq!(observed["new.run.sakala.dev"], at(120));

        let observed = tracker.observe(
            HashMap::from([
                ("busy.run.sakala.dev".to_owned(), 6),
                ("quiet.run.sakala.dev".to_owned(), 9),
            ]),
            at(130),
        );
        assert_eq!(observed["busy.run.sakala.dev"], at(120));
        assert_eq!(observed["quiet.run.sakala.dev"], at(100));
        assert!(!observed.contains_key("restarted.run.sakala.dev"));
    }
}
