//! Custom domains of a project (ADR-024): names its owner proved to control,
//! served by the node router next to the project's own domain, so the app
//! receives the visitor's real `Host`.
//!
//! The control plane checks ownership and pointing; the node only checks the
//! names themselves and that no other project on it serves them. Every command
//! that writes a project route may carry the project's current list, and
//! [`CommandType::UpdateProjectDomains`](crate::CommandType::UpdateProjectDomains)
//! changes it without a redeploy. Sent only to a node that advertises
//! [`CUSTOM_DOMAINS_CAPABILITY`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Capability of a node that serves custom domains: `custom_domains` on
/// `DeployProject`, `WakeProject`, `RestartProject`, and `SleepProject`, the
/// `UpdateProjectDomains` command, and idle-sleep traffic counted for custom
/// hosts too.
pub const CUSTOM_DOMAINS_CAPABILITY: &str = "custom-domains";

/// Most custom domains one project's route may carry on a node. Far above
/// the per-workspace quota the control plane enforces (one by default); it
/// only bounds the route file and the conflict check.
pub const MAX_CUSTOM_DOMAINS_PER_PROJECT: usize = 32;

/// Error codes of commands that carry custom domains.
pub mod error_codes {
    /// A custom domain is not a plain DNS name, is an IP address, or sits under
    /// a project domain suffix or a forbidden zone of the node, or the list is
    /// longer than [`super::MAX_CUSTOM_DOMAINS_PER_PROJECT`]. Nothing changed.
    pub const CUSTOM_DOMAIN_INVALID: &str = "runtime_custom_domain_invalid";
    /// Another project's route on the node already serves the custom domain.
    /// Nothing changed.
    pub const CUSTOM_DOMAIN_TAKEN: &str = "runtime_custom_domain_taken";
}

/// The `custom_domains` field of a command that writes a project route.
///
/// Absent (missing or `null`) keeps the custom domains the project's route on
/// the node has now, which is none for a project that never had any, so an
/// older control plane and a command that does not know the list (such as a
/// route refresh) never drop them. A list replaces them, an empty one removes
/// them all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RequestedCustomDomains {
    #[default]
    Absent,
    List(Vec<String>),
}

impl RequestedCustomDomains {
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<'de> Deserialize<'de> for RequestedCustomDomains {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<Vec<String>>::deserialize(deserializer)?.map_or(Self::Absent, Self::List))
    }
}

impl Serialize for RequestedCustomDomains {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::List(domains) => domains.serialize(serializer),
        }
    }
}

/// The part of a `WakeProject`, `RestartProject`, or `SleepProject` payload
/// this feature reads. Other fields of the payload are ignored here.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CustomDomainsPayload {
    #[serde(default, skip_serializing_if = "RequestedCustomDomains::is_absent")]
    pub custom_domains: RequestedCustomDomains,
}

/// Payload of `UpdateProjectDomains`: the project's complete list. The field
/// is required; an empty list removes every custom domain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateProjectDomainsPayload {
    pub custom_domains: Vec<String>,
}

/// What `UpdateProjectDomains` did.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDomainsUpdate {
    /// The route now serves exactly the requested custom domains; the router
    /// validated and reloaded it.
    Updated,
    /// The route already served exactly these; nothing was written.
    Unchanged,
    /// The project has no route on this node, so there was nothing to
    /// change. The next command that writes its route carries the list.
    NoRoute,
}

/// Result of `UpdateProjectDomains`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateProjectDomainsResult {
    pub status: ProjectDomainsUpdate,
    /// The custom domains the route serves now, sorted, each once. Empty with
    /// `no_route`.
    pub custom_domains: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_missing_or_null_list_keeps_what_the_route_has() {
        for payload in [
            json!({}),
            json!({ "custom_domains": null }),
            json!({ "page": "waking" }),
        ] {
            assert_eq!(
                serde_json::from_value::<CustomDomainsPayload>(payload.clone())
                    .expect("payload")
                    .custom_domains,
                RequestedCustomDomains::Absent,
                "{payload}"
            );
        }
        assert_eq!(
            serde_json::to_value(CustomDomainsPayload::default()).expect("serialize"),
            json!({})
        );
    }

    #[test]
    fn a_list_replaces_and_an_empty_list_removes() {
        let read = |payload| {
            serde_json::from_value::<CustomDomainsPayload>(payload)
                .expect("payload")
                .custom_domains
        };
        assert_eq!(
            read(json!({ "custom_domains": ["www.contoh.com"] })),
            RequestedCustomDomains::List(vec!["www.contoh.com".to_owned()])
        );
        assert_eq!(
            read(json!({ "custom_domains": [] })),
            RequestedCustomDomains::List(Vec::new())
        );
        assert_eq!(
            serde_json::to_value(CustomDomainsPayload {
                custom_domains: RequestedCustomDomains::List(vec!["contoh.com".to_owned()]),
            })
            .expect("serialize"),
            json!({ "custom_domains": ["contoh.com"] })
        );
    }

    #[test]
    fn a_list_of_anything_but_strings_is_invalid() {
        for payload in [
            json!({ "custom_domains": "www.contoh.com" }),
            json!({ "custom_domains": [1] }),
            json!({ "custom_domains": [null] }),
            json!({ "custom_domains": { "www.contoh.com": true } }),
        ] {
            assert!(
                serde_json::from_value::<CustomDomainsPayload>(payload.clone()).is_err(),
                "{payload}"
            );
        }
    }

    #[test]
    fn the_update_requires_the_whole_list_and_reports_snake_case() {
        assert!(serde_json::from_value::<UpdateProjectDomainsPayload>(json!({})).is_err());
        assert!(
            serde_json::from_value::<UpdateProjectDomainsPayload>(
                json!({ "custom_domains": null })
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(UpdateProjectDomainsResult {
                status: ProjectDomainsUpdate::NoRoute,
                custom_domains: Vec::new(),
            })
            .expect("serialize"),
            json!({ "status": "no_route", "custom_domains": [] })
        );
    }
}
