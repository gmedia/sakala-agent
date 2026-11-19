//! Picks the image a deployment starts from: the one it builds, or, when the
//! control plane asks for it, the image of the deployment that built the same
//! commit, provided the node still has it and it is provably the project's.

use sakala_agent_protocol::{
    DeployProjectPayload, DeploymentEventLevel, IMAGE_REUSE_SKIPPED_EVENT, IMAGE_REUSED_EVENT,
    ImageReuseFallbackReason, RequestedImageReuse,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    RuntimeError, RuntimeReporter,
    builders::{BuildOutput, reusable_build},
    containers::{ContainerEngine, image_name},
    logs::emit_event_at,
};

/// Why a deployment asked to reuse an image builds instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReuseSkipped {
    /// `None` when the requested value was not a UUID.
    pub source_deployment_id: Option<Uuid>,
    pub reason: ImageReuseFallbackReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ImagePlan {
    /// Build `image`, as every deployment did before image reuse existed.
    Build {
        image: String,
        /// Set when reuse was asked for and refused.
        skipped: Option<ReuseSkipped>,
    },
    /// Start from `image`, which `source_deployment_id` built as `build` says.
    Reuse {
        image: String,
        source_deployment_id: Uuid,
        build: BuildOutput,
    },
}

impl ImagePlan {
    /// The image is looked up under the reference the Agent computes from the
    /// requesting project, never under one from the payload, so one project
    /// can only ever reach images in its own repository.
    pub(super) async fn resolve(
        containers: &dyn ContainerEngine,
        project_id: Uuid,
        deployment_id: Uuid,
        payload: &DeployProjectPayload,
    ) -> Self {
        let build = |skipped| Self::Build {
            image: image_name(project_id, deployment_id, &payload.commit_sha),
            skipped,
        };
        let source_deployment_id = match payload.reuse_image_from_deployment_id {
            RequestedImageReuse::Absent => return build(None),
            RequestedImageReuse::Invalid => {
                return build(Some(ReuseSkipped {
                    source_deployment_id: None,
                    reason: ImageReuseFallbackReason::InvalidReference,
                }));
            }
            RequestedImageReuse::Deployment(id) => id,
        };
        let image = image_name(project_id, source_deployment_id, &payload.commit_sha);
        // Building is always a correct answer, only a slower one, so an image
        // Docker cannot describe counts as one the node does not have.
        let labels = containers
            .image_labels(&image)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    %project_id,
                    %deployment_id,
                    %error,
                    "the image to reuse could not be inspected; building instead"
                );
                None
            });
        match reusable_build(labels.as_ref(), project_id, source_deployment_id) {
            Ok(output) => Self::Reuse {
                image,
                source_deployment_id,
                build: output,
            },
            Err(reason) => build(Some(ReuseSkipped {
                source_deployment_id: Some(source_deployment_id),
                reason,
            })),
        }
    }

    pub(super) fn image(&self) -> &str {
        match self {
            Self::Build { image, .. } | Self::Reuse { image, .. } => image,
        }
    }

    pub(super) const fn reuses(&self) -> bool {
        matches!(self, Self::Reuse { .. })
    }

    /// The image cleanup of a failed candidate may remove: only one the
    /// candidate built. A reused image belongs to the deployment still serving.
    pub(super) fn candidate_image(&self) -> Option<&str> {
        match self {
            Self::Build { image, .. } => Some(image),
            Self::Reuse { .. } => None,
        }
    }

    /// Tells the control plane what was decided, when reuse was asked for.
    pub(super) async fn report(&self, reporter: &dyn RuntimeReporter) -> Result<(), RuntimeError> {
        match self {
            Self::Build { skipped: None, .. } => Ok(()),
            Self::Build {
                skipped: Some(skipped),
                ..
            } => {
                emit_event_at(
                    reporter,
                    DeploymentEventLevel::Warning,
                    IMAGE_REUSE_SKIPPED_EVENT,
                    "The image of the earlier deployment cannot be reused; building the image.",
                    json!({
                        "source_deployment_id": skipped.source_deployment_id,
                        "reason": skipped.reason,
                    }),
                )
                .await
            }
            Self::Reuse {
                image,
                source_deployment_id,
                build,
            } => {
                emit_event_at(
                    reporter,
                    DeploymentEventLevel::Info,
                    IMAGE_REUSED_EVENT,
                    "Starting from the image of the earlier deployment; no build needed.",
                    json!({
                        "image": image,
                        "source_deployment_id": source_deployment_id,
                        "builder": build.builder,
                    }),
                )
                .await
            }
        }
    }
}
