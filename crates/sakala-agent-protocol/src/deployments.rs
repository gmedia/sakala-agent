use std::{collections::BTreeMap, num::NonZeroU32};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use crate::RepositoryAccess;

/// Capability of a node that reads `DeployProject.reuse_image_from_deployment_id`.
/// The control plane sends the field only to a node that advertises it.
pub const IMAGE_REUSE_CAPABILITY: &str = "image-reuse";

/// Capability of a node that keeps, per project, the live image and the image
/// of the workload the last successful deployment replaced, and removes the
/// rest. Reusing the previous image for a rollback then needs no build, as long
/// as the rollback targets the deployment that built it.
pub const IMAGE_ROLLBACK_CAPABILITY: &str = "image-rollback";

/// Event of a deployment that starts from the image of an earlier deployment
/// instead of building one.
pub const IMAGE_REUSED_EVENT: &str = "deployment.image.reused";

/// Event of a deployment that was asked to reuse an image but builds instead.
/// Its metadata names one [`ImageReuseFallbackReason`].
pub const IMAGE_REUSE_SKIPPED_EVENT: &str = "deployment.image.reuse_skipped";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentBuilder {
    #[default]
    Auto,
    Dockerfile,
    Railpack,
}

impl DeploymentBuilder {
    /// The builders that actually build an image. `Auto` only picks one.
    pub const CONCRETE: [Self; 2] = [Self::Dockerfile, Self::Railpack];

    /// The wire name, the same one serde writes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dockerfile => "dockerfile",
            Self::Railpack => "railpack",
        }
    }
}

/// Product-level resource request resolved by the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeResourceLimits {
    pub memory_mb: Option<u64>,
    pub cpu_millis: Option<u32>,
    pub pids_limit: Option<u32>,
}

/// Product-level phase deadlines resolved by the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeTimeoutLimits {
    pub build_timeout_seconds: Option<u64>,
    pub start_timeout_seconds: Option<u64>,
    pub command_timeout_seconds: Option<u64>,
}

/// Product-level bounds for deployment logs sent to the control plane.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LogBounds {
    pub max_line_length: Option<u64>,
    pub max_batch_lines: Option<u64>,
    pub max_total_bytes: Option<u64>,
}

/// Concrete resource limits enforced by the runtime node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppliedRuntimeResources {
    pub memory_mb: u64,
    pub cpu_millis: u32,
    pub pids_limit: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployProjectResult {
    pub requested_resources: RuntimeResourceLimits,
    pub applied_resources: AppliedRuntimeResources,
    #[serde(default, skip_serializing_if = "is_false")]
    pub finalization_deferred: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalization_deferred_reason: Option<FinalizationDeferredReason>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationDeferredReason {
    GraceElapsed,
    RuntimeError,
}

/// Immutable input required to deploy one HTTP application.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployProjectPayload {
    pub repository_url: String,
    pub commit_sha: String,
    #[serde(default)]
    pub repository_access: RepositoryAccess,
    pub domain: String,
    #[serde(default = "default_container_port")]
    pub container_port: u16,
    #[serde(default)]
    pub builder: DeploymentBuilder,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: RuntimeResourceLimits,
    #[serde(default)]
    pub timeouts: RuntimeTimeoutLimits,
    #[serde(default)]
    pub log_bounds: LogBounds,
    /// Replaces the detected build command. Runs only inside the build, and only
    /// a Railpack build can use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_command: Option<String>,
    /// Replaces the image's start command. Runs only inside the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_command: Option<String>,
    /// Runs once per deployment, in a container of the new image, after the
    /// build and before any replica starts. A non-zero exit fails the deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_command: Option<String>,
    /// Build context relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_directory: Option<String>,
    /// How many containers serve the deployment behind the node's router.
    #[serde(default, skip_serializing_if = "RequestedReplicas::is_absent")]
    pub replicas: RequestedReplicas,
    /// The deployment whose image this one starts from instead of building.
    #[serde(default, skip_serializing_if = "RequestedImageReuse::is_absent")]
    pub reuse_image_from_deployment_id: RequestedImageReuse,
}

/// The `reuse_image_from_deployment_id` field of a `DeployProject` payload: the
/// deployment that built the image to start from.
///
/// Read leniently, like [`RequestedReplicas`]: a value that is not a UUID string
/// never fails the payload. The deployment then builds, and reports why.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RequestedImageReuse {
    /// The field is missing or `null`: build, as before image reuse existed.
    #[default]
    Absent,
    /// The deployment that built the image to reuse.
    Deployment(Uuid),
    /// Present, but not a UUID string.
    Invalid,
}

impl RequestedImageReuse {
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<'de> Deserialize<'de> for RequestedImageReuse {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Null => Self::Absent,
            Value::String(value) => Uuid::parse_str(&value).map_or(Self::Invalid, Self::Deployment),
            _ => Self::Invalid,
        })
    }
}

impl Serialize for RequestedImageReuse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Deployment(id) => id.serialize(serializer),
            Self::Absent | Self::Invalid => serializer.serialize_none(),
        }
    }
}

/// Why a deployment asked to reuse an image builds one instead. Reported in
/// the metadata of [`IMAGE_REUSE_SKIPPED_EVENT`]; none of them fails the
/// deployment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageReuseFallbackReason {
    /// This node has no image under the reference of that deployment.
    ImageMissing,
    /// The image's labels name another project or another deployment.
    ImageNotOwned,
    /// The image has no known builder label: an Agent built it before image
    /// reuse existed.
    ImageMetadataMissing,
    /// `reuse_image_from_deployment_id` is not a UUID string.
    InvalidReference,
}

/// The `replicas` field of a `DeployProject` payload.
///
/// Read leniently: a value that is not a positive integer never fails the
/// payload, so a deployment still runs, with one replica, when the control
/// plane sends something this Agent does not understand. The command handler
/// logs that case as a warning.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RequestedReplicas {
    /// The field is missing or `null`: one replica, as before replicas existed.
    #[default]
    Absent,
    /// A positive integer.
    Count(NonZeroU32),
    /// Present, but not a positive integer that fits in 32 bits.
    Unreadable,
}

impl RequestedReplicas {
    /// The number of replicas to run. Anything but [`Self::Count`] is one.
    #[must_use]
    pub const fn count(self) -> NonZeroU32 {
        match self {
            Self::Count(count) => count,
            Self::Absent | Self::Unreadable => NonZeroU32::MIN,
        }
    }

    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<'de> Deserialize<'de> for RequestedReplicas {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Null => Self::Absent,
            value => value
                .as_u64()
                .and_then(|count| u32::try_from(count).ok())
                .and_then(NonZeroU32::new)
                .map_or(Self::Unreadable, Self::Count),
        })
    }
}

impl Serialize for RequestedReplicas {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Count(count) => serializer.serialize_u32(count.get()),
            Self::Absent | Self::Unreadable => serializer.serialize_none(),
        }
    }
}

const fn default_container_port() -> u16 {
    3000
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn replicas(value: Option<Value>) -> RequestedReplicas {
        let mut payload = json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        });
        if let Some(value) = value {
            payload["replicas"] = value;
        }
        serde_json::from_value::<DeployProjectPayload>(payload)
            .expect("a replicas value never fails the payload")
            .replicas
    }

    #[test]
    fn replicas_default_to_one_when_absent_or_null() {
        for value in [None, Some(Value::Null)] {
            let requested = replicas(value);
            assert_eq!(requested, RequestedReplicas::Absent);
            assert_eq!(requested.count().get(), 1);
        }
    }

    #[test]
    fn a_positive_integer_is_the_replica_count() {
        assert_eq!(replicas(Some(json!(1))).count().get(), 1);
        assert_eq!(replicas(Some(json!(3))).count().get(), 3);
        assert_eq!(
            replicas(Some(json!(3))),
            RequestedReplicas::Count(NonZeroU32::new(3).expect("non-zero"))
        );
    }

    #[test]
    fn anything_else_runs_one_replica_instead_of_refusing_the_deployment() {
        for value in [
            json!(0),
            json!(-2),
            json!(2.5),
            json!("3"),
            json!(true),
            json!([3]),
            json!({ "count": 3 }),
            json!(u64::from(u32::MAX) + 1),
        ] {
            let requested = replicas(Some(value.clone()));
            assert_eq!(requested, RequestedReplicas::Unreadable, "{value}");
            assert_eq!(requested.count().get(), 1, "{value}");
        }
    }

    fn release_command(value: Option<Value>) -> Option<String> {
        let mut payload = json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        });
        if let Some(value) = value {
            payload["release_command"] = value;
        }
        serde_json::from_value::<DeployProjectPayload>(payload)
            .expect("payload")
            .release_command
    }

    #[test]
    fn release_command_is_absent_when_missing_or_null() {
        assert_eq!(release_command(None), None);
        assert_eq!(release_command(Some(Value::Null)), None);
    }

    #[test]
    fn release_command_is_read_as_given() {
        assert_eq!(
            release_command(Some(json!("php artisan migrate --force"))).as_deref(),
            Some("php artisan migrate --force")
        );
    }

    #[test]
    fn a_release_command_that_is_not_a_string_fails_the_payload() {
        let payload = json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
            "release_command": ["php", "artisan", "migrate"],
        });
        assert!(serde_json::from_value::<DeployProjectPayload>(payload).is_err());
    }

    #[test]
    fn release_command_is_written_back_only_when_set() {
        let mut payload: DeployProjectPayload = serde_json::from_value(json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        }))
        .expect("payload");
        let written = serde_json::to_value(&payload).expect("serialize");
        assert!(written.get("release_command").is_none());
        payload.release_command = Some("npm run migrate".to_owned());
        assert_eq!(
            serde_json::to_value(&payload).expect("serialize")["release_command"],
            json!("npm run migrate")
        );
    }

    fn image_reuse(value: Option<Value>) -> RequestedImageReuse {
        let mut payload = json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        });
        if let Some(value) = value {
            payload["reuse_image_from_deployment_id"] = value;
        }
        serde_json::from_value::<DeployProjectPayload>(payload)
            .expect("an image reuse value never fails the payload")
            .reuse_image_from_deployment_id
    }

    #[test]
    fn image_reuse_is_absent_when_missing_or_null() {
        assert_eq!(image_reuse(None), RequestedImageReuse::Absent);
        assert_eq!(image_reuse(Some(Value::Null)), RequestedImageReuse::Absent);
    }

    #[test]
    fn a_uuid_string_names_the_deployment_whose_image_is_reused() {
        let id = Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID");
        assert_eq!(
            image_reuse(Some(json!("4f1f21ef-730d-42d5-a46d-d965353cb993"))),
            RequestedImageReuse::Deployment(id)
        );
    }

    #[test]
    fn anything_else_builds_instead_of_refusing_the_deployment() {
        for value in [
            json!(""),
            json!("not-a-uuid"),
            json!("4f1f21ef-730d-42d5-a46d"),
            json!(42),
            json!(true),
            json!(["4f1f21ef-730d-42d5-a46d-d965353cb993"]),
            json!({ "id": "4f1f21ef-730d-42d5-a46d-d965353cb993" }),
        ] {
            assert_eq!(
                image_reuse(Some(value.clone())),
                RequestedImageReuse::Invalid,
                "{value}"
            );
        }
    }

    #[test]
    fn only_a_deployment_id_is_written_back() {
        let mut payload: DeployProjectPayload = serde_json::from_value(json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        }))
        .expect("payload");
        assert!(
            serde_json::to_value(&payload)
                .expect("serialize")
                .get("reuse_image_from_deployment_id")
                .is_none()
        );
        let id = Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("UUID");
        payload.reuse_image_from_deployment_id = RequestedImageReuse::Deployment(id);
        assert_eq!(
            serde_json::to_value(&payload).expect("serialize")["reuse_image_from_deployment_id"],
            json!("4f1f21ef-730d-42d5-a46d-d965353cb993")
        );
    }

    #[test]
    fn fallback_reasons_use_their_wire_names() {
        for (reason, name) in [
            (ImageReuseFallbackReason::ImageMissing, "image_missing"),
            (ImageReuseFallbackReason::ImageNotOwned, "image_not_owned"),
            (
                ImageReuseFallbackReason::ImageMetadataMissing,
                "image_metadata_missing",
            ),
            (
                ImageReuseFallbackReason::InvalidReference,
                "invalid_reference",
            ),
        ] {
            assert_eq!(
                serde_json::to_value(reason).expect("serialize"),
                json!(name)
            );
        }
    }

    #[test]
    fn builder_names_match_what_serde_writes() {
        for builder in [
            DeploymentBuilder::Auto,
            DeploymentBuilder::Dockerfile,
            DeploymentBuilder::Railpack,
        ] {
            assert_eq!(
                serde_json::to_value(builder).expect("serialize"),
                json!(builder.as_str())
            );
        }
    }

    #[test]
    fn only_a_count_is_written_back() {
        let mut payload: DeployProjectPayload = serde_json::from_value(json!({
            "repository_url": "https://github.com/gmedia/example-app.git",
            "commit_sha": "0123456789abcdef0123456789abcdef01234567",
            "domain": "portfolio.sakala.app",
        }))
        .expect("payload");
        assert!(
            serde_json::to_value(&payload)
                .expect("serialize")
                .get("replicas")
                .is_none()
        );
        payload.replicas = RequestedReplicas::Count(NonZeroU32::new(2).expect("non-zero"));
        assert_eq!(
            serde_json::to_value(&payload).expect("serialize")["replicas"],
            json!(2)
        );
    }
}
