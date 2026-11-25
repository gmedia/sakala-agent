//! Typed configuration from environment variables or matching CLI flags.

use std::{collections::HashMap, env, path::PathBuf, time::Duration};

use clap::Parser;
use reqwest::Url;
use sakala_agent_core::ports::SecretString;
use sakala_agent_runtime::{ForbiddenZones, ProjectDomainSuffixes};
use thiserror::Error;

use crate::{
    caddy::{CaddyCommand, DEFAULT_RELOAD_COMMAND, DEFAULT_VALIDATE_COMMAND},
    files::CUSTOM_DOMAINS_FILE,
    sync::SyncTiming,
    table::{RoutePolicy, UpstreamAllowlist},
};

pub const API_URL: &str = "SAKALA_API_URL";
pub const EDGE_TOKEN: &str = "SAKALA_EDGE_TOKEN";
pub const DOMAIN_SUFFIXES: &str = "SAKALA_EDGE_DOMAIN_SUFFIXES";
pub const UPSTREAM_CIDRS: &str = "SAKALA_EDGE_UPSTREAM_CIDRS";
pub const POLL_INTERVAL_SECONDS: &str = "SAKALA_EDGE_POLL_INTERVAL_SECONDS";
pub const MAX_BACKOFF_SECONDS: &str = "SAKALA_EDGE_MAX_BACKOFF_SECONDS";
pub const STATE_DIR: &str = "SAKALA_EDGE_STATE_DIR";
pub const CUSTOM_DOMAINS_FILE_PATH: &str = "SAKALA_EDGE_CUSTOM_DOMAINS_FILE";
pub const CUSTOM_DOMAIN_FORBIDDEN_ZONES: &str = "SAKALA_EDGE_CUSTOM_DOMAIN_FORBIDDEN_ZONES";
pub const CADDY_VALIDATE_CMD: &str = "SAKALA_EDGE_CADDY_VALIDATE_CMD";
pub const CADDY_RELOAD_CMD: &str = "SAKALA_EDGE_CADDY_RELOAD_CMD";
pub const LOG_LEVEL: &str = "SAKALA_EDGE_LOG_LEVEL";

pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
pub const DEFAULT_MAX_BACKOFF_SECONDS: u64 = 60;
pub const DEFAULT_STATE_DIR: &str = "/var/lib/sakala-edge";
pub const DEFAULT_LOG_LEVEL: &str = "info";
/// The token placeholder in examples, refused so a copied example never polls.
const PLACEHOLDER_TOKEN: &str = "change-me";

#[derive(Debug, Error, Eq, PartialEq)]
#[error("invalid configuration: {0}")]
pub struct ConfigError(String);

#[derive(Clone, Debug)]
pub struct EdgeConfig {
    pub api_url: Url,
    pub token: SecretString,
    pub policy: RoutePolicy,
    pub timing: SyncTiming,
    pub state_dir: PathBuf,
    /// Where the custom domain sites are written; `custom-domains.caddy` in
    /// the state directory by default.
    pub custom_domains_file: PathBuf,
    pub validate_command: CaddyCommand,
    pub reload_command: CaddyCommand,
    pub log_level: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "sakala-edge",
    about = "Keeps a host Caddy in step with the Sakala Edge route table",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    #[arg(long, env = API_URL)]
    api_url: Option<String>,
    #[arg(long, env = EDGE_TOKEN, hide_env_values = true)]
    edge_token: Option<String>,
    #[arg(long, env = DOMAIN_SUFFIXES)]
    domain_suffixes: Option<String>,
    #[arg(long, env = UPSTREAM_CIDRS)]
    upstream_cidrs: Option<String>,
    #[arg(long, env = POLL_INTERVAL_SECONDS)]
    poll_interval_seconds: Option<String>,
    #[arg(long, env = MAX_BACKOFF_SECONDS)]
    max_backoff_seconds: Option<String>,
    #[arg(long, env = STATE_DIR)]
    state_dir: Option<String>,
    #[arg(long, env = CUSTOM_DOMAINS_FILE_PATH)]
    custom_domains_file: Option<String>,
    #[arg(long, env = CUSTOM_DOMAIN_FORBIDDEN_ZONES)]
    custom_domain_forbidden_zones: Option<String>,
    #[arg(long, env = CADDY_VALIDATE_CMD)]
    caddy_validate_cmd: Option<String>,
    #[arg(long, env = CADDY_RELOAD_CMD)]
    caddy_reload_cmd: Option<String>,
    #[arg(long, env = LOG_LEVEL)]
    log_level: Option<String>,
}

pub fn load() -> Result<EdgeConfig, ConfigError> {
    let cli = Cli::parse();
    let mut values = env::vars().collect::<HashMap<_, _>>();
    for (key, value) in [
        (API_URL, cli.api_url),
        (EDGE_TOKEN, cli.edge_token),
        (DOMAIN_SUFFIXES, cli.domain_suffixes),
        (UPSTREAM_CIDRS, cli.upstream_cidrs),
        (POLL_INTERVAL_SECONDS, cli.poll_interval_seconds),
        (MAX_BACKOFF_SECONDS, cli.max_backoff_seconds),
        (STATE_DIR, cli.state_dir),
        (CUSTOM_DOMAINS_FILE_PATH, cli.custom_domains_file),
        (
            CUSTOM_DOMAIN_FORBIDDEN_ZONES,
            cli.custom_domain_forbidden_zones,
        ),
        (CADDY_VALIDATE_CMD, cli.caddy_validate_cmd),
        (CADDY_RELOAD_CMD, cli.caddy_reload_cmd),
        (LOG_LEVEL, cli.log_level),
    ] {
        if let Some(value) = value {
            values.insert(key.to_owned(), value);
        }
    }
    from_values(&values)
}

pub fn from_values(values: &HashMap<String, String>) -> Result<EdgeConfig, ConfigError> {
    let poll_interval = seconds(values, POLL_INTERVAL_SECONDS, DEFAULT_POLL_INTERVAL_SECONDS)?;
    let max_backoff = seconds(values, MAX_BACKOFF_SECONDS, DEFAULT_MAX_BACKOFF_SECONDS)?;
    if max_backoff < poll_interval {
        return Err(ConfigError(format!(
            "{MAX_BACKOFF_SECONDS} ({}) must be at least {POLL_INTERVAL_SECONDS} ({})",
            max_backoff.as_secs(),
            poll_interval.as_secs()
        )));
    }

    let state_dir = PathBuf::from(set(values, STATE_DIR).unwrap_or(DEFAULT_STATE_DIR));
    Ok(EdgeConfig {
        api_url: api_url(values)?,
        token: token(values)?,
        policy: RoutePolicy {
            domain_suffixes: match set(values, DOMAIN_SUFFIXES) {
                None => ProjectDomainSuffixes::default(),
                Some(value) => ProjectDomainSuffixes::parse(value)
                    .map_err(|reason| ConfigError(format!("{DOMAIN_SUFFIXES} {reason}")))?,
            },
            upstreams: match set(values, UPSTREAM_CIDRS) {
                None => UpstreamAllowlist::default(),
                Some(value) => UpstreamAllowlist::parse(value)
                    .map_err(|reason| ConfigError(format!("{UPSTREAM_CIDRS} {reason}")))?,
            },
            forbidden_zones: match set(values, CUSTOM_DOMAIN_FORBIDDEN_ZONES) {
                None => ForbiddenZones::default(),
                Some(value) => ForbiddenZones::parse(value).map_err(|reason| {
                    ConfigError(format!("{CUSTOM_DOMAIN_FORBIDDEN_ZONES} {reason}"))
                })?,
            },
        },
        timing: SyncTiming {
            poll_interval,
            max_backoff,
        },
        state_dir: state_dir.clone(),
        custom_domains_file: set(values, CUSTOM_DOMAINS_FILE_PATH)
            .map_or_else(|| state_dir.join(CUSTOM_DOMAINS_FILE), PathBuf::from),
        validate_command: command(values, CADDY_VALIDATE_CMD, DEFAULT_VALIDATE_COMMAND)?,
        reload_command: command(values, CADDY_RELOAD_CMD, DEFAULT_RELOAD_COMMAND)?,
        log_level: set(values, LOG_LEVEL)
            .unwrap_or(DEFAULT_LOG_LEVEL)
            .to_owned(),
    })
}

/// A value that is present and not blank. Blank means unset, as in an env file
/// that lists a variable without a value.
fn set<'a>(values: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    values
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

fn api_url(values: &HashMap<String, String>) -> Result<Url, ConfigError> {
    let value =
        set(values, API_URL).ok_or_else(|| ConfigError(format!("{API_URL} is required")))?;
    let url =
        Url::parse(value).map_err(|error| ConfigError(format!("{API_URL} is invalid: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError(format!("{API_URL} must use http or https")));
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err(ConfigError(format!(
            "{API_URL} must not hold credentials, a query, or a fragment"
        )));
    }
    Ok(url)
}

fn token(values: &HashMap<String, String>) -> Result<SecretString, ConfigError> {
    match set(values, EDGE_TOKEN) {
        Some(token) if token != PLACEHOLDER_TOKEN => Ok(SecretString::new(token)),
        _ => Err(ConfigError(format!(
            "{EDGE_TOKEN} must hold the Edge token issued by the API"
        ))),
    }
}

fn seconds(
    values: &HashMap<String, String>,
    key: &str,
    default: u64,
) -> Result<Duration, ConfigError> {
    let seconds = match set(values, key) {
        None => default,
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| ConfigError(format!("{key} must be a whole number of seconds")))?,
    };
    if seconds == 0 {
        return Err(ConfigError(format!("{key} must be greater than zero")));
    }
    Ok(Duration::from_secs(seconds))
}

fn command(
    values: &HashMap<String, String>,
    key: &str,
    default: &str,
) -> Result<CaddyCommand, ConfigError> {
    set(values, key)
        .unwrap_or(default)
        .parse()
        .map_err(|reason| ConfigError(format!("{key} {reason}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut values = HashMap::from([
            (API_URL.to_owned(), "https://api.sakala.dev".to_owned()),
            (EDGE_TOKEN.to_owned(), "edge-token".to_owned()),
        ]);
        for (key, value) in pairs {
            values.insert((*key).to_owned(), (*value).to_owned());
        }
        values
    }

    #[test]
    fn defaults_suit_a_systemd_host_caddy() {
        let config = from_values(&values(&[])).expect("valid");

        assert_eq!(config.timing.poll_interval, Duration::from_secs(5));
        assert_eq!(config.timing.max_backoff, Duration::from_secs(60));
        assert_eq!(config.state_dir, PathBuf::from("/var/lib/sakala-edge"));
        assert_eq!(
            config.validate_command.to_string(),
            DEFAULT_VALIDATE_COMMAND
        );
        assert_eq!(config.reload_command.to_string(), DEFAULT_RELOAD_COMMAND);
        assert_eq!(
            config.policy.domain_suffixes,
            ProjectDomainSuffixes::default()
        );
        assert_eq!(config.policy.upstreams, UpstreamAllowlist::default());
        assert_eq!(config.policy.forbidden_zones, ForbiddenZones::default());
        assert_eq!(
            config.custom_domains_file,
            PathBuf::from("/var/lib/sakala-edge/custom-domains.caddy")
        );
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn every_setting_can_be_overridden() {
        let config = from_values(&values(&[
            (DOMAIN_SUFFIXES, "run.example.ac.id"),
            (UPSTREAM_CIDRS, "10.20.0.0/16"),
            (POLL_INTERVAL_SECONDS, "2"),
            (MAX_BACKOFF_SECONDS, "30"),
            (STATE_DIR, "/srv/edge"),
            (CUSTOM_DOMAINS_FILE_PATH, "/etc/caddy/edge/custom.caddy"),
            (CUSTOM_DOMAIN_FORBIDDEN_ZONES, "example.ac.id"),
            (CADDY_VALIDATE_CMD, "caddy validate --config /srv/Caddyfile"),
            (CADDY_RELOAD_CMD, "caddy reload --config /srv/Caddyfile"),
            (LOG_LEVEL, "debug"),
        ]))
        .expect("valid");

        assert!(config.policy.domain_suffixes.accepts("a.run.example.ac.id"));
        assert!(
            config
                .policy
                .upstreams
                .allows("10.20.0.9".parse().expect("ip"))
        );
        assert!(
            !config
                .policy
                .upstreams
                .allows("10.0.0.9".parse().expect("ip"))
        );
        assert_eq!(config.timing.poll_interval, Duration::from_secs(2));
        assert_eq!(config.state_dir, PathBuf::from("/srv/edge"));
        assert_eq!(
            config.custom_domains_file,
            PathBuf::from("/etc/caddy/edge/custom.caddy")
        );
        assert_eq!(config.policy.forbidden_zones.as_slice(), ["example.ac.id"]);
        assert_eq!(
            config.reload_command.to_string(),
            "caddy reload --config /srv/Caddyfile"
        );
    }

    #[test]
    fn invalid_settings_stop_the_service_with_the_variable_name() {
        for (pairs, expected) in [
            (vec![(API_URL, "")], API_URL),
            (vec![(API_URL, "ftp://api")], API_URL),
            (vec![(API_URL, "https://user:pw@api")], API_URL),
            (vec![(EDGE_TOKEN, "")], EDGE_TOKEN),
            (vec![(EDGE_TOKEN, "change-me")], EDGE_TOKEN),
            (vec![(DOMAIN_SUFFIXES, "*.sakala.app")], DOMAIN_SUFFIXES),
            (vec![(UPSTREAM_CIDRS, "0.0.0.0/0")], UPSTREAM_CIDRS),
            (
                vec![(CUSTOM_DOMAIN_FORBIDDEN_ZONES, "*.sakala.app")],
                CUSTOM_DOMAIN_FORBIDDEN_ZONES,
            ),
            (vec![(POLL_INTERVAL_SECONDS, "0")], POLL_INTERVAL_SECONDS),
            (vec![(POLL_INTERVAL_SECONDS, "fast")], POLL_INTERVAL_SECONDS),
            (
                vec![(POLL_INTERVAL_SECONDS, "30"), (MAX_BACKOFF_SECONDS, "10")],
                MAX_BACKOFF_SECONDS,
            ),
            (vec![(CADDY_RELOAD_CMD, "sh -c 'x'")], CADDY_RELOAD_CMD),
        ] {
            let error = from_values(&values(&pairs)).expect_err(expected);
            assert!(error.to_string().contains(expected), "{pairs:?}: {error}");
        }
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let config = from_values(&values(&[(EDGE_TOKEN, "secret-edge-token")])).expect("valid");
        assert!(!format!("{config:?}").contains("secret-edge-token"));
    }

    #[test]
    fn exposes_the_binary_version() {
        let error = Cli::try_parse_from(["sakala-edge", "--version"]).expect_err("version exits");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
