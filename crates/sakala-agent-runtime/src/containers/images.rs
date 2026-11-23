//! Chooses which Sakala images garbage collection may remove, and which image
//! a successful deployment marks as the previous one.
//!
//! Every deployment image carries its own tag, so an old image never becomes
//! dangling and `docker image prune` without `-a` never removes it. Selection
//! therefore works on tagged images, per project, and keeps what a project still
//! needs: every image a container uses and the previous image, the one a
//! rollback starts from without a build. A deployment marks that image with the
//! extra tag [`PREVIOUS_IMAGE_TAG`] in the project's own repository; it keeps
//! its original tag, so reuse still finds it under the computed name. A project
//! without any container needs none of its images once they are older than a
//! grace period, which is how images of a deleted project leave the node.
//!
//! Only an image labelled with a valid project whose every name is
//! `sakala/project-<that project>:<tag>` is ever chosen or tagged.

use std::collections::{BTreeMap, HashSet};

use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::RuntimeError;

/// Tag, in the project's repository, of the image the last successful
/// deployment replaced. Moving it never removes an image by itself: the image
/// it leaves keeps its own tag.
pub const PREVIOUS_IMAGE_TAG: &str = "previous";

/// The repository every image of `project_id` is named in.
#[must_use]
pub fn project_repository(project_id: Uuid) -> String {
    format!("sakala/project-{project_id}")
}

/// `sakala/project-<project_id>:previous`.
#[must_use]
pub fn previous_image_reference(project_id: Uuid) -> String {
    format!("{}:{PREVIOUS_IMAGE_TAG}", project_repository(project_id))
}

/// `docker image ls --no-trunc` prints one row per tag, and `<none>` for a
/// dangling image.
pub(crate) const IMAGE_LIST_FORMAT: &str = "{{.ID}}\t{{.Repository}}\t{{.Tag}}";
/// `docker image ls` has no field for labels, so they come from inspect. A
/// missing label prints `<no value>`.
pub(crate) const IMAGE_INSPECT_FORMAT: &str = "{{.Id}}\t{{index .Config.Labels \"dev.sakala.project-id\"}}\t{{index .Config.Labels \"dev.sakala.deployment-id\"}}\t{{.Created}}\t{{.Size}}";
/// Every label of one image as a JSON object, or `null` when it has none. JSON,
/// because a label value may hold a tab or a newline.
pub(crate) const IMAGE_LABELS_FORMAT: &str = "{{json .Config.Labels}}";
/// What `docker image inspect` reports, in any letter case, for a reference the
/// node has no image under.
pub(crate) const NO_SUCH_IMAGE: &str = "no such image";
/// The image ID a container runs, whatever tag it was started with, and the
/// project the container belongs to. A missing label prints `<no value>`.
pub(crate) const CONTAINER_IMAGE_FORMAT: &str =
    "{{.Image}}\t{{index .Config.Labels \"dev.sakala.project-id\"}}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListedImage {
    pub id: String,
    /// `repository:tag` names. Empty for a dangling image.
    pub references: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InspectedImage {
    pub id: String,
    pub project_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub created: OffsetDateTime,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StaleImage {
    pub id: String,
    pub references: Vec<String>,
    pub project_id: Uuid,
    pub deployment_id: Option<Uuid>,
    /// Docker's size for the whole image, shared layers included.
    pub size_bytes: u64,
}

/// What the host's containers hold on to, Sakala's or not, in any state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainerUsage {
    /// Image IDs some container was created from.
    pub images: HashSet<String>,
    /// Projects that still have at least one container.
    pub projects: HashSet<Uuid>,
}

pub(crate) fn parse_container_usage(stdout: &str) -> ContainerUsage {
    let mut usage = ContainerUsage::default();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t').map(str::trim);
        if let Some(image) = fields.next().filter(|image| !image.is_empty()) {
            usage.images.insert(image.to_owned());
        }
        if let Some(project) = fields.next().and_then(|value| Uuid::parse_str(value).ok()) {
            usage.projects.insert(project);
        }
    }
    usage
}

pub(crate) fn parse_image_list(stdout: &str) -> Vec<ListedImage> {
    let mut images: Vec<ListedImage> = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let id = fields.next().unwrap_or_default().trim().to_owned();
        let repository = fields.next().unwrap_or_default().trim();
        let tag = fields.next().unwrap_or_default().trim();
        if id.is_empty() {
            continue;
        }
        let reference = (repository != "<none>" && tag != "<none>" && !repository.is_empty())
            .then(|| format!("{repository}:{tag}"));
        if let Some(existing) = images.iter_mut().find(|image| image.id == id) {
            existing.references.extend(reference);
        } else {
            images.push(ListedImage {
                id,
                references: reference.into_iter().collect(),
            });
        }
    }
    images
}

/// Reads what [`IMAGE_LABELS_FORMAT`] prints for one image.
pub(crate) fn parse_image_labels(stdout: &str) -> Result<BTreeMap<String, String>, RuntimeError> {
    serde_json::from_str::<Option<BTreeMap<String, String>>>(stdout.trim())
        .map(Option::unwrap_or_default)
        .map_err(|error| {
            RuntimeError::Container(format!(
                "docker image inspect returned unreadable labels: {error}"
            ))
        })
}

pub(crate) fn parse_image_inspect(stdout: &str) -> Result<Vec<InspectedImage>, RuntimeError> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = line.split('\t').map(str::trim).collect::<Vec<_>>();
            let [id, project, deployment, created, size] = fields[..] else {
                return Err(RuntimeError::Container(
                    "docker image inspect returned an unknown format".to_owned(),
                ));
            };
            let created = OffsetDateTime::parse(created, &Rfc3339).map_err(|error| {
                RuntimeError::Container(format!(
                    "Docker returned an invalid image creation timestamp: {error}"
                ))
            })?;
            Ok(InspectedImage {
                id: id.to_owned(),
                project_id: Uuid::parse_str(project).ok(),
                deployment_id: Uuid::parse_str(deployment).ok(),
                created,
                size_bytes: size.parse().unwrap_or(0),
            })
        })
        .collect()
}

/// How long images of a project without any container are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Retention {
    /// A project without containers keeps only images younger than this.
    pub orphaned_grace: Duration,
}

/// Per project with a container, keeps every image a container uses, in any
/// state, and the previous image: the one tagged [`PREVIOUS_IMAGE_TAG`], or,
/// when no image carries that tag yet (images of an older Agent), the newest
/// image older than the newest one in use, or the newest image when no
/// container uses any. Age protects none of them: a deployment still between
/// build and start holds its project, and garbage collection leaves a held
/// project alone.
///
/// A project with no container at all, and none of whose images a container
/// uses, keeps only what is younger than `orphaned_grace`: its newest and
/// previous images go too, since nothing will roll back to them.
pub(crate) fn select_stale_images(
    listed: &[ListedImage],
    inspected: &[InspectedImage],
    usage: &ContainerUsage,
    now: OffsetDateTime,
    retention: Retention,
) -> Vec<StaleImage> {
    let mut projects: BTreeMap<Uuid, Vec<&InspectedImage>> = BTreeMap::new();
    for image in inspected {
        if let Some(project_id) = image.project_id {
            projects.entry(project_id).or_default().push(image);
        }
    }
    let in_use = &usage.images;
    let mut stale = Vec::new();
    for (project_id, mut images) in projects {
        images.sort_by(|left, right| {
            right
                .created
                .cmp(&left.created)
                .then_with(|| left.id.cmp(&right.id))
        });
        let orphaned = !usage.projects.contains(&project_id)
            && !images.iter().any(|image| in_use.contains(&image.id));
        if orphaned {
            for image in images {
                if now - image.created >= retention.orphaned_grace {
                    stale.extend(stale_image(listed, image, project_id));
                }
            }
            continue;
        }
        let previous = tagged_previous(listed, &images, project_id)
            .or_else(|| untagged_previous(&images, in_use));
        for image in images {
            let kept = in_use.contains(&image.id) || previous.as_deref() == Some(image.id.as_str());
            if !kept {
                stale.extend(stale_image(listed, image, project_id));
            }
        }
    }
    stale
}

/// The image of the project that carries `sakala/project-<project>:previous`.
/// Docker gives one reference to one image, so there is at most one.
fn tagged_previous(
    listed: &[ListedImage],
    images: &[&InspectedImage],
    project_id: Uuid,
) -> Option<String> {
    let reference = previous_image_reference(project_id);
    images
        .iter()
        .find(|image| {
            listed
                .iter()
                .any(|listed| listed.id == image.id && listed.references.contains(&reference))
        })
        .map(|image| image.id.clone())
}

/// The rule before the previous tag existed: the newest image older than the
/// newest one in use, or the newest image when no container uses any.
/// `images` is sorted newest first.
fn untagged_previous(images: &[&InspectedImage], in_use: &HashSet<String>) -> Option<String> {
    let newest_in_use = images
        .iter()
        .find(|image| in_use.contains(&image.id))
        .map(|image| image.created);
    images
        .iter()
        .find(|image| {
            !in_use.contains(&image.id) && newest_in_use.is_none_or(|active| image.created < active)
        })
        .map(|image| image.id.clone())
}

/// One container of a project as the post-deployment image step sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectContainer {
    /// Without Docker's leading `/`.
    pub name: String,
    /// The image ID the container was created from.
    pub image: String,
    pub running: bool,
    pub created: OffsetDateTime,
}

/// `docker container inspect` of every container of a project, for
/// [`parse_project_containers`].
pub(crate) const PROJECT_CONTAINER_FORMAT: &str =
    "{{.Name}}\t{{.Image}}\t{{.State.Running}}\t{{.Created}}";

pub(crate) fn parse_project_containers(
    stdout: &str,
) -> Result<Vec<ProjectContainer>, RuntimeError> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields = line.split('\t').map(str::trim).collect::<Vec<_>>();
            let [name, image, running, created] = fields[..] else {
                return Err(RuntimeError::Container(
                    "docker container inspect returned an unknown format".to_owned(),
                ));
            };
            let created = OffsetDateTime::parse(created, &Rfc3339).map_err(|error| {
                RuntimeError::Container(format!(
                    "Docker returned an invalid container creation timestamp: {error}"
                ))
            })?;
            Ok(ProjectContainer {
                name: name.trim_start_matches('/').to_owned(),
                image: image.to_owned(),
                running: running == "true",
                created,
            })
        })
        .collect()
}

/// The image a deployment that just went live replaced, which becomes the
/// project's previous image, or `None` when the previous image stays as it is.
///
/// `current` names the new deployment's replicas. The replaced deployment is
/// what else of the project runs, or, when nothing else runs (a sleeping
/// project), what else exists; its newest container decides. When that image
/// is one the new deployment runs, the deployment reused the live image (a
/// configuration redeploy), and the previous image does not move. A first
/// deployment replaces nothing.
pub(crate) fn replaced_image(
    containers: &[ProjectContainer],
    current: &[String],
) -> Option<String> {
    let live = containers
        .iter()
        .filter(|container| current.contains(&container.name))
        .map(|container| container.image.as_str())
        .collect::<HashSet<_>>();
    let replaced = containers
        .iter()
        .filter(|container| !current.contains(&container.name))
        .collect::<Vec<_>>();
    let any_running = replaced.iter().any(|container| container.running);
    replaced
        .into_iter()
        .filter(|container| container.running || !any_running)
        .max_by(|left, right| {
            left.created
                .cmp(&right.created)
                .then_with(|| left.name.cmp(&right.name))
        })
        .map(|container| container.image.clone())
        .filter(|image| !live.contains(image.as_str()))
}

/// The names of `image_id` when it may carry the previous tag of
/// `project_id`: it is labelled with that project (the listing is filtered by
/// that label) and every name it has is in that project's repository.
pub(crate) fn owned_references(
    listed: &[ListedImage],
    image_id: &str,
    project_id: Uuid,
) -> Option<Vec<String>> {
    let prefix = format!("{}:", project_repository(project_id));
    let references = listed
        .iter()
        .find(|listed| listed.id == image_id)
        .map(|listed| listed.references.clone())
        .unwrap_or_default();
    let owned = !references.is_empty()
        && references
            .iter()
            .all(|reference| reference.len() > prefix.len() && reference.starts_with(&prefix));
    owned.then_some(references)
}

/// Every image of one project that no container uses, whatever its age. For a
/// project that was deleted, after its containers were removed.
pub(crate) fn select_project_images(
    listed: &[ListedImage],
    inspected: &[InspectedImage],
    usage: &ContainerUsage,
    project_id: Uuid,
) -> Vec<StaleImage> {
    inspected
        .iter()
        .filter(|image| image.project_id == Some(project_id))
        .filter(|image| !usage.images.contains(&image.id))
        .filter_map(|image| stale_image(listed, image, project_id))
        .collect()
}

/// `None` unless every name of the image is `sakala/project-<project_id>:<tag>`,
/// so an image someone tagged elsewhere, or a dangling one, is never removed.
///
/// The previous tag comes first among the names: `docker image rm` untags
/// one name after another, and when a container started using the image in
/// the meantime Docker refuses only the last one, so the image keeps its
/// original name, the one reuse looks it up under.
fn stale_image(
    listed: &[ListedImage],
    image: &InspectedImage,
    project_id: Uuid,
) -> Option<StaleImage> {
    if image.project_id != Some(project_id) {
        return None;
    }
    let previous = previous_image_reference(project_id);
    let mut references = owned_references(listed, &image.id, project_id)?;
    references.sort_by_key(|reference| *reference != previous);
    Some(StaleImage {
        id: image.id.clone(),
        references,
        project_id,
        deployment_id: image.deployment_id,
        size_bytes: image.size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "33333333-3333-4333-8333-333333333333";

    fn at(timestamp: &str) -> OffsetDateTime {
        OffsetDateTime::parse(timestamp, &Rfc3339).expect("timestamp")
    }

    #[test]
    fn reads_image_labels_including_none_at_all() {
        let labels = parse_image_labels(
            "{\"dev.sakala.builder\":\"railpack\",\"dev.sakala.package-directory\":\"apps/web\"}\n",
        )
        .expect("labels");
        assert_eq!(labels["dev.sakala.builder"], "railpack");
        assert_eq!(labels["dev.sakala.package-directory"], "apps/web");
        assert!(parse_image_labels("null\n").expect("no labels").is_empty());
        assert!(parse_image_labels("<no value>").is_err());
    }

    /// Captured from Docker 29.9.3 (containerd image store) with the formats above.
    #[test]
    fn parses_what_docker_prints() {
        let listed = parse_image_list(
            "sha256:f03e71043c3b79a782101af9f6721ea5c8a5d13c4cd6000a16149f975471536d\tsakala-agentfix/gc\tnolabel\n\
             sha256:09828b7b816011b44621fb15f0d96a8d9df465225684dc33059a5a9ba87691df\tsakala/project-11111111-1111-4111-8111-111111111111\t0123456789ab-22222222\n\
             sha256:09828b7b816011b44621fb15f0d96a8d9df465225684dc33059a5a9ba87691df\tsakala/project-11111111-1111-4111-8111-111111111111\tother\n\
             sha256:aa\t<none>\t<none>\n",
        );
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[1].references.len(), 2);
        assert!(listed[2].references.is_empty());

        let inspected = parse_image_inspect(
            "sha256:09828b7b816011b44621fb15f0d96a8d9df465225684dc33059a5a9ba87691df\t11111111-1111-4111-8111-111111111111\t22222222-2222-4222-8222-222222222222\t2026-11-11T07:06:15.079750824+07:00\t243473312\n\
             sha256:f03e71043c3b79a782101af9f6721ea5c8a5d13c4cd6000a16149f975471536d\t<no value>\t<no value>\t2026-11-11T07:06:16.287934974+07:00\t243473183\n",
        )
        .expect("inspect output");
        assert_eq!(
            inspected[0].project_id,
            Some(Uuid::parse_str(PROJECT).expect("uuid"))
        );
        assert_eq!(inspected[0].created, at("2026-11-11T00:06:15.079750824Z"));
        assert_eq!(inspected[0].size_bytes, 243_473_312);
        assert_eq!(inspected[1].project_id, None);
        assert!(parse_image_inspect("sha256:x\tonly-two-fields\n").is_err());
    }

    fn image(id: &str, project: &str, created: &str) -> InspectedImage {
        InspectedImage {
            id: id.to_owned(),
            project_id: Uuid::parse_str(project).ok(),
            deployment_id: None,
            created: at(created),
            size_bytes: 10,
        }
    }

    /// Names every image the way a deployment does.
    fn listed(inspected: &[InspectedImage]) -> Vec<ListedImage> {
        inspected
            .iter()
            .map(|image| ListedImage {
                id: image.id.clone(),
                references: vec![format!(
                    "sakala/project-{}:{}",
                    image
                        .project_id
                        .map_or_else(|| "none".to_owned(), |id| id.to_string()),
                    image.id
                )],
            })
            .collect()
    }

    fn usage(images: &[&str], projects: &[&str]) -> ContainerUsage {
        ContainerUsage {
            images: images.iter().map(|id| (*id).to_owned()).collect(),
            projects: projects
                .iter()
                .map(|id| Uuid::parse_str(id).expect("uuid"))
                .collect(),
        }
    }

    const RETENTION: Retention = Retention {
        orphaned_grace: Duration::days(1),
    };

    fn chosen(stale: &[StaleImage]) -> Vec<&str> {
        stale.iter().map(|image| image.id.as_str()).collect()
    }

    #[test]
    fn reads_the_image_and_project_of_every_container() {
        let usage = parse_container_usage(&format!(
            "sha256:a\t{PROJECT}\nsha256:b\t<no value>\nsha256:c\t\nsha256:d\n\n"
        ));
        assert_eq!(usage.images.len(), 4);
        assert_eq!(
            usage.projects,
            HashSet::from([Uuid::parse_str(PROJECT).expect("uuid")])
        );
    }

    #[test]
    fn keeps_the_active_and_the_previous_image_of_each_project() {
        let inspected = [
            image("oldest", PROJECT, "2026-01-01T00:00:00Z"),
            image("previous", PROJECT, "2026-01-02T00:00:00Z"),
            image("active", PROJECT, "2026-01-03T00:00:00Z"),
            image("failed-later", PROJECT, "2026-01-04T00:00:00Z"),
            image("other-old", OTHER, "2026-01-01T00:00:00Z"),
            image("other-newest", OTHER, "2026-01-02T00:00:00Z"),
            image("unowned", "not-a-uuid", "2020-01-01T00:00:00Z"),
        ];
        // OTHER still has a container, just not on any of these images.
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&["active", "sha256:elsewhere"], &[OTHER]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        assert_eq!(chosen(&stale), ["failed-later", "oldest", "other-old"]);
        assert_eq!(
            stale[1].references,
            [format!("sakala/project-{PROJECT}:oldest")]
        );
    }

    /// Before the previous tag, every image younger than seven days stayed.
    #[test]
    fn age_no_longer_protects_an_image_of_a_project_with_containers() {
        let inspected = [
            image("old", PROJECT, "2026-01-31T00:00:00Z"),
            image("recent", PROJECT, "2026-01-31T22:00:00Z"),
            image("newest", PROJECT, "2026-01-31T23:00:00Z"),
        ];
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&["newest"], &[PROJECT]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        // `recent` is the previous image by the rule for untagged images.
        assert_eq!(chosen(&stale), ["old"]);
    }

    #[test]
    fn every_image_in_use_is_kept_even_when_old() {
        let inspected = [
            image("a", PROJECT, "2026-01-01T00:00:00Z"),
            image("b", PROJECT, "2026-01-02T00:00:00Z"),
        ];
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&["a", "b"], &[]),
            at("2027-01-01T00:00:00Z"),
            Retention {
                orphaned_grace: Duration::ZERO,
            },
        );
        assert!(stale.is_empty());
    }

    /// A deleted project leaves no container behind, so the active-and-previous
    /// rule would keep its last two images forever.
    #[test]
    fn a_project_without_containers_loses_every_image_past_the_grace_period() {
        let inspected = [
            image("gone-old", PROJECT, "2026-01-01T00:00:00Z"),
            image("gone-newest", PROJECT, "2026-01-30T00:00:00Z"),
            image("gone-fresh", PROJECT, "2026-01-31T12:00:00Z"),
            image("kept-old", OTHER, "2026-01-01T00:00:00Z"),
            image("kept-newest", OTHER, "2026-01-02T00:00:00Z"),
        ];
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&[], &[OTHER]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        // OTHER keeps its newest image as the previous deployment.
        assert_eq!(chosen(&stale), ["gone-newest", "gone-old", "kept-old"]);
    }

    #[test]
    fn a_container_on_any_image_of_the_project_keeps_the_normal_rule() {
        let inspected = [
            image("previous", PROJECT, "2026-01-01T00:00:00Z"),
            image("held", PROJECT, "2026-01-02T00:00:00Z"),
        ];
        // Started by hand, without Sakala's labels. As an orphan the project
        // would lose `previous`; with a container it is kept for rollback.
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&["held"], &[]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        assert!(stale.is_empty(), "{:?}", chosen(&stale));
    }

    #[test]
    fn an_image_with_a_name_outside_the_project_repository_is_never_chosen() {
        let inspected = [
            image("retagged", PROJECT, "2026-01-01T00:00:00Z"),
            image("dangling", PROJECT, "2026-01-01T00:00:00Z"),
            image("foreign", PROJECT, "2026-01-01T00:00:00Z"),
            image("wrong-project", PROJECT, "2026-01-01T00:00:00Z"),
            image("owned", PROJECT, "2026-01-01T00:00:00Z"),
        ];
        let own = |tag: &str| format!("sakala/project-{PROJECT}:{tag}");
        let listed = vec![
            ListedImage {
                id: "retagged".to_owned(),
                references: vec![own("a"), "registry.example/app:latest".to_owned()],
            },
            ListedImage {
                id: "dangling".to_owned(),
                references: Vec::new(),
            },
            ListedImage {
                id: "foreign".to_owned(),
                references: vec!["sakala-imgfix/project:latest".to_owned()],
            },
            ListedImage {
                id: "wrong-project".to_owned(),
                references: vec![format!("sakala/project-{OTHER}:a")],
            },
            ListedImage {
                id: "owned".to_owned(),
                references: vec![own("a"), own("b")],
            },
        ];
        let everything = select_stale_images(
            &listed,
            &inspected,
            &usage(&[], &[]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        assert_eq!(chosen(&everything), ["owned"]);
        let project = select_project_images(
            &listed,
            &inspected,
            &usage(&[], &[]),
            Uuid::parse_str(PROJECT).expect("uuid"),
        );
        assert_eq!(chosen(&project), ["owned"]);
        assert_eq!(project[0].references, [own("a"), own("b")]);
    }

    #[test]
    fn removing_a_project_takes_all_its_images_but_those_a_container_uses() {
        let inspected = [
            image("fresh", PROJECT, "2026-01-31T23:59:00Z"),
            image("old", PROJECT, "2026-01-01T00:00:00Z"),
            image("held", PROJECT, "2026-01-01T00:00:00Z"),
            image("other", OTHER, "2026-01-01T00:00:00Z"),
        ];
        let stale = select_project_images(
            &listed(&inspected),
            &inspected,
            &usage(&["held"], &[]),
            Uuid::parse_str(PROJECT).expect("uuid"),
        );
        assert_eq!(chosen(&stale), ["fresh", "old"]);
    }

    fn project(id: &str) -> Uuid {
        Uuid::parse_str(id).expect("uuid")
    }

    /// Adds `sakala/project-<project>:previous` to the names of `id`.
    fn tag_previous(listed: &mut [ListedImage], id: &str, project_id: &str) {
        listed
            .iter_mut()
            .find(|image| image.id == id)
            .expect("listed image")
            .references
            .push(previous_image_reference(project(project_id)));
    }

    /// Deployments I1 to I4, each built a day after the one before.
    fn four_deployments() -> [InspectedImage; 4] {
        [
            image("i1", PROJECT, "2026-01-01T00:00:00Z"),
            image("i2", PROJECT, "2026-01-02T00:00:00Z"),
            image("i3", PROJECT, "2026-01-03T00:00:00Z"),
            image("i4", PROJECT, "2026-01-04T00:00:00Z"),
        ]
    }

    fn keep_rule(
        listed: &[ListedImage],
        inspected: &[InspectedImage],
        in_use: &[&str],
    ) -> Vec<String> {
        select_stale_images(
            listed,
            inspected,
            &usage(in_use, &[PROJECT]),
            at("2026-01-04T00:00:01Z"),
            RETENTION,
        )
        .into_iter()
        .map(|image| image.id)
        .collect()
    }

    #[test]
    fn the_tagged_previous_image_is_kept_instead_of_the_newest_older_one() {
        let inspected = four_deployments();
        let mut listed = listed(&inspected);
        // I4 runs after a rollback from I3 to I2... and back to I4; I2 is previous.
        tag_previous(&mut listed, "i2", PROJECT);
        assert_eq!(keep_rule(&listed, &inspected, &["i4"]), ["i3", "i1"]);
    }

    #[test]
    fn without_the_tag_the_newest_image_older_than_the_live_one_is_previous() {
        let inspected = four_deployments();
        let listed = listed(&inspected);
        // A failed deployment's I4 is newer than the live I3 and goes too.
        assert_eq!(keep_rule(&listed, &inspected, &["i3"]), ["i4", "i1"]);
    }

    #[test]
    fn every_image_a_container_uses_is_kept_in_any_state_and_for_every_replica() {
        let inspected = four_deployments();
        let mut listed = listed(&inspected);
        tag_previous(&mut listed, "i1", PROJECT);
        // Two replicas of I4 running, and a sleeping (stopped) container of I2
        // that `docker ps --all` still lists.
        let usage =
            parse_container_usage(&format!("i4\t{PROJECT}\ni4\t{PROJECT}\ni2\t{PROJECT}\n"));
        let stale = select_stale_images(
            &listed,
            &inspected,
            &usage,
            at("2026-01-04T00:00:01Z"),
            RETENTION,
        );
        assert_eq!(chosen(&stale), ["i3"]);
    }

    #[test]
    fn a_previous_tag_outside_the_project_or_on_a_foreign_image_is_never_followed() {
        let inspected = four_deployments();
        let mut listed = listed(&inspected);
        // Someone tagged I1 as OTHER's previous: not PROJECT's previous, and
        // not PROJECT's to remove either.
        tag_previous(&mut listed, "i1", OTHER);
        // I2 carries PROJECT's previous tag but also a foreign name.
        tag_previous(&mut listed, "i2", PROJECT);
        listed[1]
            .references
            .push("registry.example/app:latest".to_owned());
        assert_eq!(keep_rule(&listed, &inspected, &["i4"]), ["i3"]);
    }

    #[test]
    fn an_orphaned_project_loses_its_previous_image_past_the_grace_with_that_tag_first() {
        let inspected = four_deployments();
        let mut listed = listed(&inspected);
        tag_previous(&mut listed, "i3", PROJECT);
        let stale = select_stale_images(
            &listed,
            &inspected,
            &usage(&[], &[]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
        assert_eq!(chosen(&stale), ["i4", "i3", "i2", "i1"]);
        assert_eq!(
            stale[1].references,
            [
                previous_image_reference(project(PROJECT)),
                format!("sakala/project-{PROJECT}:i3"),
            ]
        );
    }

    fn container(name: &str, image: &str, running: bool, created: &str) -> ProjectContainer {
        ProjectContainer {
            name: name.to_owned(),
            image: image.to_owned(),
            running,
            created: at(created),
        }
    }

    fn current(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn reads_every_container_of_a_project() {
        let containers = parse_project_containers(
            "/sakala-app-11111111-a\tsha256:a\ttrue\t2026-11-23T01:02:03.456789Z\n\
             /sakala-app-11111111-b\tsha256:b\tfalse\t2026-11-23T08:02:03+07:00\n",
        )
        .expect("containers");
        assert_eq!(
            containers,
            [
                container(
                    "sakala-app-11111111-a",
                    "sha256:a",
                    true,
                    "2026-11-23T01:02:03.456789Z"
                ),
                container(
                    "sakala-app-11111111-b",
                    "sha256:b",
                    false,
                    "2026-11-23T01:02:03Z"
                ),
            ]
        );
        assert!(parse_project_containers("/a\tsha256:a\ttrue\n").is_err());
    }

    #[test]
    fn a_deployment_marks_the_image_of_the_workload_it_replaced() {
        let containers = [
            container("new-0", "i5", true, "2026-01-05T00:00:00Z"),
            container("new-1", "i5", true, "2026-01-05T00:00:01Z"),
            container("old-0", "i4", true, "2026-01-04T00:00:00Z"),
            container("old-1", "i4", true, "2026-01-04T00:00:01Z"),
        ];
        assert_eq!(
            replaced_image(&containers, &current(&["new-0", "new-1"])),
            Some("i4".to_owned())
        );
    }

    #[test]
    fn a_redeploy_that_reuses_the_live_image_does_not_move_previous() {
        let containers = [
            container("new", "i4", true, "2026-01-05T00:00:00Z"),
            container("old", "i4", true, "2026-01-04T00:00:00Z"),
        ];
        assert_eq!(replaced_image(&containers, &current(&["new"])), None);
    }

    #[test]
    fn a_rollback_from_i5_to_i3_makes_i5_previous() {
        let containers = [
            container("rollback", "i3", true, "2026-01-06T00:00:00Z"),
            container("live", "i5", true, "2026-01-05T00:00:00Z"),
        ];
        assert_eq!(
            replaced_image(&containers, &current(&["rollback"])),
            Some("i5".to_owned())
        );
    }

    #[test]
    fn a_first_deployment_replaces_nothing() {
        let containers = [container("first", "i1", true, "2026-01-01T00:00:00Z")];
        assert_eq!(replaced_image(&containers, &current(&["first"])), None);
    }

    #[test]
    fn the_running_workload_is_the_replaced_one_even_beside_a_newer_stopped_container() {
        let containers = [
            container("new", "i5", true, "2026-01-05T00:00:00Z"),
            container("serving", "i3", true, "2026-01-03T00:00:00Z"),
            container("left-behind", "i4", false, "2026-01-04T00:00:00Z"),
        ];
        assert_eq!(
            replaced_image(&containers, &current(&["new"])),
            Some("i3".to_owned())
        );
    }

    #[test]
    fn a_deployment_of_a_sleeping_project_replaces_its_stopped_workload() {
        let containers = [
            container("new", "i5", true, "2026-01-05T00:00:00Z"),
            container("asleep", "i4", false, "2026-01-04T00:00:00Z"),
            container("older", "i3", false, "2026-01-03T00:00:00Z"),
        ];
        assert_eq!(
            replaced_image(&containers, &current(&["new"])),
            Some("i4".to_owned())
        );
    }

    /// The whole post-deployment decision: deploy A, B, C, then roll back to B.
    #[test]
    fn deploying_a_b_c_keeps_c_and_b_and_a_rollback_to_b_keeps_b_and_c() {
        let inspected = [
            image("a", PROJECT, "2026-01-01T00:00:00Z"),
            image("b", PROJECT, "2026-01-02T00:00:00Z"),
            image("c", PROJECT, "2026-01-03T00:00:00Z"),
        ];
        let mut listed = listed(&inspected);

        // C goes live over B.
        let after_c = [
            container("c", "c", true, "2026-01-03T00:00:00Z"),
            container("b", "b", true, "2026-01-02T00:00:00Z"),
        ];
        let previous = replaced_image(&after_c, &current(&["c"])).expect("b is replaced");
        assert_eq!(previous, "b");
        tag_previous(&mut listed, &previous, PROJECT);
        // B's container is removed before the prune.
        assert_eq!(keep_rule(&listed, &inspected, &["c"]), ["a"]);

        // Roll back to B, reusing its image, over C. A is gone by now.
        let inspected = [inspected[1].clone(), inspected[2].clone()];
        let mut listed = self::listed(&inspected);
        tag_previous(&mut listed, "b", PROJECT);
        let after_rollback = [
            container("rollback", "b", true, "2026-01-04T00:00:00Z"),
            container("c", "c", true, "2026-01-03T00:00:00Z"),
        ];
        let previous =
            replaced_image(&after_rollback, &current(&["rollback"])).expect("c is replaced");
        assert_eq!(previous, "c");
        // Moving the tag leaves B with its own name only.
        listed[0].references.pop();
        tag_previous(&mut listed, "c", PROJECT);
        assert!(keep_rule(&listed, &inspected, &["b"]).is_empty());
    }
}
