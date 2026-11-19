//! The labels a build writes on every Sakala image, and the rule for reusing
//! an image instead of building one.
//!
//! A redeploy that changes only configuration can start from the image of the
//! deployment that built its commit. The labels say whose image it is and how
//! it was built, which is everything a deployment needs from its build besides
//! the image itself: [`BuildOutput`].

use std::collections::BTreeMap;

use sakala_agent_protocol::{DeploymentBuilder, ImageReuseFallbackReason};
use uuid::Uuid;

use super::BuildOutput;

/// Marks an image as Sakala's, for image GC.
pub(crate) const MANAGED_IMAGE_LABEL: &str = "dev.sakala.managed=true";
pub(crate) const PROJECT_ID_LABEL: &str = "dev.sakala.project-id";
pub(crate) const DEPLOYMENT_ID_LABEL: &str = "dev.sakala.deployment-id";
/// [`BuildOutput::builder`], as its wire name.
pub(crate) const BUILDER_LABEL: &str = "dev.sakala.builder";
/// [`BuildOutput::package_directory`]. Absent when the image was not built
/// from a JavaScript workspace root.
pub(crate) const PACKAGE_DIRECTORY_LABEL: &str = "dev.sakala.package-directory";

/// The `--label` values of the image a deployment builds.
pub(crate) fn image_labels(
    project_id: Uuid,
    deployment_id: Uuid,
    output: &BuildOutput,
) -> Vec<String> {
    let mut labels = vec![
        MANAGED_IMAGE_LABEL.to_owned(),
        format!("{PROJECT_ID_LABEL}={project_id}"),
        format!("{DEPLOYMENT_ID_LABEL}={deployment_id}"),
        format!("{BUILDER_LABEL}={}", output.builder.as_str()),
    ];
    if let Some(directory) = &output.package_directory {
        labels.push(format!("{PACKAGE_DIRECTORY_LABEL}={directory}"));
    }
    labels
}

/// Decides whether the image found under the reference of `source_deployment_id`
/// in `project_id`'s repository may be reused, and restores its build output.
///
/// `labels` is `None` when the node has no such image. The reference alone
/// proves nothing, because anyone with the Docker socket can tag any image
/// under it, so the image must say itself that `project_id` built it in
/// `source_deployment_id`.
pub(crate) fn reusable_build(
    labels: Option<&BTreeMap<String, String>>,
    project_id: Uuid,
    source_deployment_id: Uuid,
) -> Result<BuildOutput, ImageReuseFallbackReason> {
    let labels = labels.ok_or(ImageReuseFallbackReason::ImageMissing)?;
    let names = |key: &str, expected: Uuid| {
        labels
            .get(key)
            .and_then(|value| Uuid::parse_str(value).ok())
            .is_some_and(|value| value == expected)
    };
    if !names(PROJECT_ID_LABEL, project_id) || !names(DEPLOYMENT_ID_LABEL, source_deployment_id) {
        return Err(ImageReuseFallbackReason::ImageNotOwned);
    }
    let builder = labels
        .get(BUILDER_LABEL)
        .and_then(|value| {
            DeploymentBuilder::CONCRETE
                .into_iter()
                .find(|builder| builder.as_str() == value)
        })
        .ok_or(ImageReuseFallbackReason::ImageMetadataMissing)?;
    Ok(BuildOutput {
        builder,
        package_directory: labels.get(PACKAGE_DIRECTORY_LABEL).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "ff66ed4a-6303-4be6-8ef4-63c28b112680";
    const OTHER_PROJECT: &str = "11111111-1111-4111-8111-111111111111";
    const SOURCE: &str = "4f1f21ef-730d-42d5-a46d-d965353cb993";
    const OTHER_DEPLOYMENT: &str = "22222222-2222-4222-8222-222222222222";

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).expect("UUID")
    }

    /// Labels as `docker image inspect` reads them back from `image_labels`.
    fn labels_of(
        project: &str,
        deployment: &str,
        output: &BuildOutput,
    ) -> BTreeMap<String, String> {
        image_labels(uuid(project), uuid(deployment), output)
            .into_iter()
            .map(|label| {
                let (key, value) = label.split_once('=').expect("key=value");
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }

    fn dockerfile() -> BuildOutput {
        BuildOutput {
            builder: DeploymentBuilder::Dockerfile,
            package_directory: None,
        }
    }

    fn decide(
        labels: Option<&BTreeMap<String, String>>,
    ) -> Result<BuildOutput, ImageReuseFallbackReason> {
        reusable_build(labels, uuid(PROJECT), uuid(SOURCE))
    }

    #[test]
    fn an_image_of_the_project_and_deployment_is_reused_with_its_build_output() {
        assert_eq!(
            decide(Some(&labels_of(PROJECT, SOURCE, &dockerfile()))),
            Ok(dockerfile())
        );

        let workspace = BuildOutput {
            builder: DeploymentBuilder::Railpack,
            package_directory: Some("apps/web".to_owned()),
        };
        assert_eq!(
            decide(Some(&labels_of(PROJECT, SOURCE, &workspace))),
            Ok(workspace)
        );
    }

    #[test]
    fn the_package_directory_label_is_written_only_for_a_workspace_build() {
        let labels = image_labels(uuid(PROJECT), uuid(SOURCE), &dockerfile());
        assert_eq!(
            labels,
            [
                "dev.sakala.managed=true".to_owned(),
                format!("dev.sakala.project-id={PROJECT}"),
                format!("dev.sakala.deployment-id={SOURCE}"),
                "dev.sakala.builder=dockerfile".to_owned(),
            ]
        );
        let workspace = BuildOutput {
            builder: DeploymentBuilder::Railpack,
            package_directory: Some("apps/web".to_owned()),
        };
        let labels = image_labels(uuid(PROJECT), uuid(SOURCE), &workspace);
        assert!(labels.contains(&"dev.sakala.builder=railpack".to_owned()));
        assert!(labels.contains(&"dev.sakala.package-directory=apps/web".to_owned()));
    }

    #[test]
    fn an_image_labelled_for_another_project_is_not_owned() {
        assert_eq!(
            decide(Some(&labels_of(OTHER_PROJECT, SOURCE, &dockerfile()))),
            Err(ImageReuseFallbackReason::ImageNotOwned)
        );
    }

    #[test]
    fn an_image_labelled_for_another_deployment_is_not_owned() {
        assert_eq!(
            decide(Some(&labels_of(PROJECT, OTHER_DEPLOYMENT, &dockerfile()))),
            Err(ImageReuseFallbackReason::ImageNotOwned)
        );
    }

    #[test]
    fn an_image_without_ownership_labels_is_not_owned() {
        assert_eq!(
            decide(Some(&BTreeMap::new())),
            Err(ImageReuseFallbackReason::ImageNotOwned)
        );
        let mut labels = labels_of(PROJECT, SOURCE, &dockerfile());
        labels.insert(PROJECT_ID_LABEL.to_owned(), "not-a-uuid".to_owned());
        assert_eq!(
            decide(Some(&labels)),
            Err(ImageReuseFallbackReason::ImageNotOwned)
        );
    }

    #[test]
    fn an_image_built_before_the_builder_label_existed_lacks_metadata() {
        let mut labels = labels_of(PROJECT, SOURCE, &dockerfile());
        labels.remove(BUILDER_LABEL);
        assert_eq!(
            decide(Some(&labels)),
            Err(ImageReuseFallbackReason::ImageMetadataMissing)
        );
        for unknown in ["auto", "nixpacks", ""] {
            labels.insert(BUILDER_LABEL.to_owned(), unknown.to_owned());
            assert_eq!(
                decide(Some(&labels)),
                Err(ImageReuseFallbackReason::ImageMetadataMissing),
                "{unknown:?}"
            );
        }
    }

    #[test]
    fn no_image_under_the_reference_is_missing() {
        assert_eq!(decide(None), Err(ImageReuseFallbackReason::ImageMissing));
    }
}
