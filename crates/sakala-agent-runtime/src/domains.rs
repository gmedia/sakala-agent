//! Which hostnames a deployment may claim as its route.
//!
//! A deploy command names the domain Caddy routes to the new container. Accepting
//! any hostname would let one project claim another's domain, or a domain the node
//! operator never meant to serve, so the node keeps an allowlist of suffixes and
//! accepts exactly one DNS label under one of them.

use std::fmt;

use url::Host;

/// Suffixes hosted Sakala routes, kept when the operator names none: local
/// development, hosted staging, and hosted production. Projects live on a
/// registrable domain apart from the API and console (ADR-016).
pub const DEFAULT_PROJECT_DOMAIN_SUFFIXES: [&str; 3] =
    ["run.sakala.localhost", "staging.sakala.app", "sakala.app"];

/// RFC 1035 section 2.3.4: a label holds at most 63 octets and a name at most
/// 255 octets on the wire, which is 253 characters in dotted text form.
const MAX_LABEL_LEN: usize = 63;
const MAX_NAME_LEN: usize = 253;

/// The suffixes a project domain may sit under, validated when the node starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectDomainSuffixes(Vec<String>);

impl Default for ProjectDomainSuffixes {
    fn default() -> Self {
        Self(
            DEFAULT_PROJECT_DOMAIN_SUFFIXES
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }
}

impl ProjectDomainSuffixes {
    /// Parses a comma-separated list such as `run.example.ac.id,run.example.org`.
    ///
    /// Each suffix must be a lowercase DNS name of at least two labels, without
    /// wildcards, with internationalized labels in their `xn--` form. Leaves room
    /// for a one-character project label, and drops repeated entries.
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut suffixes = Vec::new();
        for suffix in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            validate_suffix(suffix)
                .map_err(|reason| format!("{suffix:?} is not a valid domain suffix: {reason}"))?;
            if !suffixes.iter().any(|known| known == suffix) {
                suffixes.push(suffix.to_owned());
            }
        }
        if suffixes.is_empty() {
            return Err("lists no domain suffix".to_owned());
        }
        Ok(Self(suffixes))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Whether `domain` is exactly one DNS label, a dot, and an allowed suffix.
    /// Nested subdomains, uppercase, and Unicode are refused rather than normalized,
    /// because the domain is written into a Caddy route as given.
    #[must_use]
    pub fn accepts(&self, domain: &str) -> bool {
        if domain.len() > MAX_NAME_LEN {
            return false;
        }
        let Some((label, suffix)) = domain.split_once('.') else {
            return false;
        };
        valid_label(label)
            && self.0.iter().any(|allowed| allowed == suffix)
            && canonical_hostname(domain)
    }

    /// The project label of an accepted domain, `portfolio` for
    /// `portfolio.sakala.app` or `portfolio.staging.sakala.app`, which is the
    /// project's slug in the control plane. `None` for a domain
    /// [`Self::accepts`] refuses.
    #[must_use]
    pub fn project_label<'a>(&self, domain: &'a str) -> Option<&'a str> {
        if !self.accepts(domain) {
            return None;
        }
        domain.split_once('.').map(|(label, _)| label)
    }
}

impl fmt::Display for ProjectDomainSuffixes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.join(", "))
    }
}

fn validate_suffix(suffix: &str) -> Result<(), &'static str> {
    if suffix.contains('*') {
        return Err("wildcards are not allowed; write run.example.org, not *.run.example.org");
    }
    if !suffix.is_ascii() {
        return Err("write internationalized labels in their xn-- (punycode) form");
    }
    if suffix.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("use lowercase letters only");
    }
    // A project label of at least one character and its dot must still fit.
    if suffix.len() > MAX_NAME_LEN - 2 {
        return Err("too long to leave room for a project label");
    }
    let labels = suffix.split('.').collect::<Vec<_>>();
    if labels.len() < 2 {
        return Err("needs at least two labels, such as run.example.org");
    }
    if !labels.iter().all(|label| valid_label(label)) {
        return Err(
            "each label must be 1 to 63 characters of a-z, 0-9, and '-', not starting or ending with '-'",
        );
    }
    if !canonical_hostname(suffix) {
        return Err("not a valid hostname, for example an xn-- label that is not valid punycode");
    }
    Ok(())
}

/// A lowercase letter-digit-hyphen label (RFC 1123), which includes `xn--` labels.
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Whether the URL host parser keeps `name` exactly as a domain. This rejects
/// `xn--` labels that are not valid punycode and names it would read as an IP
/// address, and leaves the label rules above to decide everything else.
fn canonical_hostname(name: &str) -> bool {
    matches!(Host::parse(name), Ok(Host::Domain(parsed)) if parsed == name)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PROJECT_DOMAIN_SUFFIXES, ProjectDomainSuffixes};

    fn custom() -> ProjectDomainSuffixes {
        ProjectDomainSuffixes::parse("run.example.ac.id").expect("valid suffix")
    }

    #[test]
    fn the_project_label_is_the_slug_of_an_accepted_domain_only() {
        let hosted = ProjectDomainSuffixes::default();

        assert_eq!(
            hosted.project_label("portfolio.sakala.app"),
            Some("portfolio")
        );
        assert_eq!(
            custom().project_label("web-1.run.example.ac.id"),
            Some("web-1")
        );
        for domain in [
            "api.portfolio.sakala.app",
            "portfolio.example.com",
            "Portfolio.sakala.app",
        ] {
            assert_eq!(hosted.project_label(domain), None, "{domain}");
        }
    }

    #[test]
    fn the_default_accepts_the_hosted_runtime_zones() {
        let suffixes = ProjectDomainSuffixes::default();

        assert_eq!(suffixes.as_slice(), DEFAULT_PROJECT_DOMAIN_SUFFIXES);
        for domain in [
            "portfolio.run.sakala.localhost",
            "portfolio.staging.sakala.app",
            "portfolio.sakala.app",
            "a.sakala.app",
            "app-2.sakala.app",
            "2fa.sakala.app",
        ] {
            assert!(suffixes.accepts(domain), "{domain}");
        }
    }

    #[test]
    fn the_default_rejects_everything_else() {
        let suffixes = ProjectDomainSuffixes::default();

        for domain in [
            // The addresses before ADR-016 are switched off, not kept.
            "portfolio.run.sakala.dev",
            "portfolio.run.staging.sakala.dev",
            "portfolio.staging.sakala.dev",
            "portfolio.run.example.test",
            "sakala.app",
            ".sakala.app",
            "portfolio..sakala.app",
            "portfolio.sakala.app.",
            "portfolio.sakala.app.evil.test",
            "evilsakala.app",
            "Portfolio.sakala.app",
            "portfolio.SAKALA.app",
            "-app.sakala.app",
            "app-.sakala.app",
            "my_app.sakala.app",
            "app.sakala.app:443",
            "app.sakala.app {\n}",
            "*.sakala.app",
            "",
        ] {
            assert!(!suffixes.accepts(domain), "{domain:?}");
        }
    }

    #[test]
    fn a_staging_node_reads_the_slug_under_the_staging_suffix() {
        let staging = ProjectDomainSuffixes::parse("staging.sakala.app").expect("valid suffix");

        assert_eq!(
            staging.project_label("portfolio.staging.sakala.app"),
            Some("portfolio")
        );
        assert_eq!(staging.project_label("portfolio.sakala.app"), None);
    }

    #[test]
    fn a_production_node_does_not_take_staging_projects_as_its_own() {
        let production = ProjectDomainSuffixes::parse("sakala.app").expect("valid suffix");

        assert!(production.accepts("portfolio.sakala.app"));
        for domain in ["portfolio.staging.sakala.app", "x.staging.sakala.app"] {
            assert!(!production.accepts(domain), "{domain}");
            assert_eq!(production.project_label(domain), None, "{domain}");
        }
    }

    #[test]
    fn a_custom_suffix_replaces_the_default() {
        let suffixes = custom();

        assert!(suffixes.accepts("portfolio.run.example.ac.id"));
        assert!(!suffixes.accepts("portfolio.sakala.app"));
        assert!(!suffixes.accepts("portfolio.example.ac.id"));
    }

    #[test]
    fn several_suffixes_are_trimmed_and_deduplicated() {
        let suffixes = ProjectDomainSuffixes::parse(
            " run.example.ac.id , apps.example.org,,run.example.ac.id ",
        )
        .expect("valid suffixes");

        assert_eq!(
            suffixes.as_slice(),
            ["run.example.ac.id", "apps.example.org"]
        );
        assert_eq!(suffixes.to_string(), "run.example.ac.id, apps.example.org");
        assert!(suffixes.accepts("demo.apps.example.org"));
    }

    #[test]
    fn nested_subdomains_are_rejected() {
        let suffixes = custom();

        assert!(!suffixes.accepts("a.b.run.example.ac.id"));
        assert!(!suffixes.accepts("api.portfolio.run.example.ac.id"));
        assert!(!ProjectDomainSuffixes::default().accepts("a.portfolio.sakala.app"));
    }

    #[test]
    fn labels_and_names_follow_rfc_1035_length_limits() {
        let suffixes = custom();
        let longest_label = "a".repeat(63);

        assert!(suffixes.accepts(&format!("{longest_label}.run.example.ac.id")));
        assert!(!suffixes.accepts(&format!("{longest_label}a.run.example.ac.id")));

        // 63 + 1 + 63 + 1 + 63 + 1 + 61 = 253 characters, the longest allowed name.
        let long_suffix = format!("{longest_label}.{longest_label}.{}", "b".repeat(61));
        let long = ProjectDomainSuffixes::parse(&long_suffix).expect("suffix of 189 characters");
        assert!(long.accepts(&format!("{longest_label}.{long_suffix}")));

        let too_long_suffix = format!(
            "{longest_label}.{longest_label}.{longest_label}.{}",
            "c".repeat(60)
        );
        assert_eq!(too_long_suffix.len(), 252);
        assert!(ProjectDomainSuffixes::parse(&too_long_suffix).is_err());
        let fits = format!(
            "{longest_label}.{longest_label}.{longest_label}.{}",
            "c".repeat(59)
        );
        let fits = ProjectDomainSuffixes::parse(&fits).expect("suffix of 251 characters");
        assert!(fits.accepts(&format!("a.{}", fits.as_slice()[0])));
        assert!(!fits.accepts(&format!("ab.{}", fits.as_slice()[0])));
    }

    #[test]
    fn invalid_suffix_configuration_is_rejected_with_a_reason() {
        for (value, reason) in [
            ("", "lists no domain suffix"),
            (" , ", "lists no domain suffix"),
            ("*.run.example.ac.id", "wildcards are not allowed"),
            ("run.*.example.ac.id", "wildcards are not allowed"),
            ("localhost", "at least two labels"),
            ("Run.Example.ac.id", "lowercase"),
            (".run.example.ac.id", "each label"),
            ("run.example.ac.id.", "each label"),
            ("run..example.ac.id", "each label"),
            ("run.exa_mple.ac.id", "each label"),
            ("-run.example.ac.id", "each label"),
            ("run.example.ac.id:8080", "each label"),
            ("https://run.example.ac.id", "each label"),
            ("run.bücher.example", "xn-- (punycode) form"),
            ("run.xn--a.example", "not a valid hostname"),
            ("run.example.123", "not a valid hostname"),
            ("run.example.ac.id,*.evil.test", "wildcards are not allowed"),
        ] {
            let error = ProjectDomainSuffixes::parse(value).expect_err(value);
            assert!(error.contains(reason), "{value:?}: {error}");
        }
        let error = ProjectDomainSuffixes::parse("*.run.example.ac.id").expect_err("wildcard");
        assert!(error.starts_with("\"*.run.example.ac.id\" is not a valid domain suffix"));
    }

    #[test]
    fn punycode_labels_are_accepted_only_in_valid_ascii_form() {
        // bücher.example and münchen.example in their xn-- form.
        let suffixes = ProjectDomainSuffixes::parse("run.xn--bcher-kva.example")
            .expect("valid punycode suffix");

        assert!(suffixes.accepts("portfolio.run.xn--bcher-kva.example"));
        assert!(suffixes.accepts("xn--mnchen-3ya.run.xn--bcher-kva.example"));
        // The Unicode spelling is the same name, but the route must use the A-label.
        assert!(!suffixes.accepts("portfolio.run.bücher.example"));
        assert!(!suffixes.accepts("münchen.run.xn--bcher-kva.example"));
        // An xn-- label that does not decode is not a hostname.
        assert!(!suffixes.accepts("xn--a.run.xn--bcher-kva.example"));
        assert!(!ProjectDomainSuffixes::default().accepts("xn--a.sakala.app"));
        assert!(ProjectDomainSuffixes::default().accepts("xn--mnchen-3ya.sakala.app"));
    }
}
