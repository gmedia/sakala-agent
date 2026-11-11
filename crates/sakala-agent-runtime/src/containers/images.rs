//! Chooses which Sakala images garbage collection may remove.
//!
//! Every deployment image carries its own tag, so an old image never becomes
//! dangling and `docker image prune` without `-a` never removes it. Selection
//! therefore works on tagged images, per project, and keeps what a project still
//! needs: every image a container uses, the image of the deployment before the
//! active one, and anything younger than the retention age. A project without
//! any container needs none of its images once they are older than a grace
//! period, which is how images of a deleted project leave the node.
//!
//! Only an image labelled with a valid project whose every name is
//! `sakala/project-<that project>:<tag>` is ever chosen.

use std::collections::{BTreeMap, HashSet};

use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::RuntimeError;

/// `docker image ls --no-trunc` prints one row per tag, and `<none>` for a
/// dangling image.
pub(crate) const IMAGE_LIST_FORMAT: &str = "{{.ID}}\t{{.Repository}}\t{{.Tag}}";
/// `docker image ls` has no field for labels, so they come from inspect. A
/// missing label prints `<no value>`.
pub(crate) const IMAGE_INSPECT_FORMAT: &str = "{{.Id}}\t{{index .Config.Labels \"dev.sakala.project-id\"}}\t{{index .Config.Labels \"dev.sakala.deployment-id\"}}\t{{.Created}}\t{{.Size}}";
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

/// How long images are kept, by whether their project still has a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Retention {
    /// A project with containers keeps every image younger than this.
    pub min_age: Duration,
    /// A project without containers keeps only images younger than this.
    pub orphaned_grace: Duration,
}

/// Per project with a container, keeps every image a container uses, the newest
/// image older than the newest one in use (the previous deployment, for
/// rollback), or the newest image when no container uses any, plus everything
/// younger than `min_age`.
///
/// A project with no container at all, and none of whose images a container
/// uses, keeps only what is younger than `orphaned_grace`: its newest image
/// goes too, since nothing will roll back to it.
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
        let (previous, min_age) = if orphaned {
            (None, retention.orphaned_grace)
        } else {
            let newest_in_use = images
                .iter()
                .find(|image| in_use.contains(&image.id))
                .map(|image| image.created);
            let previous = images
                .iter()
                .find(|image| {
                    !in_use.contains(&image.id)
                        && newest_in_use.is_none_or(|active| image.created < active)
                })
                .map(|image| image.id.clone());
            (previous, retention.min_age)
        };
        for image in images {
            let kept = in_use.contains(&image.id)
                || previous.as_deref() == Some(image.id.as_str())
                || now - image.created < min_age;
            if !kept {
                stale.extend(stale_image(listed, image, project_id));
            }
        }
    }
    stale
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
fn stale_image(
    listed: &[ListedImage],
    image: &InspectedImage,
    project_id: Uuid,
) -> Option<StaleImage> {
    let prefix = format!("sakala/project-{project_id}:");
    let references = listed
        .iter()
        .find(|listed| listed.id == image.id)
        .map(|listed| listed.references.clone())
        .unwrap_or_default();
    let owned = !references.is_empty()
        && references
            .iter()
            .all(|reference| reference.len() > prefix.len() && reference.starts_with(&prefix));
    owned.then(|| StaleImage {
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
        min_age: Duration::days(7),
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

    #[test]
    fn keeps_what_is_younger_than_the_retention_age() {
        let inspected = [
            image("old", PROJECT, "2026-01-01T00:00:00Z"),
            image("recent", PROJECT, "2026-01-30T00:00:00Z"),
            image("newest", PROJECT, "2026-01-31T00:00:00Z"),
        ];
        let stale = select_stale_images(
            &listed(&inspected),
            &inspected,
            &usage(&[], &[PROJECT]),
            at("2026-02-01T00:00:00Z"),
            RETENTION,
        );
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
                min_age: Duration::ZERO,
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
}
