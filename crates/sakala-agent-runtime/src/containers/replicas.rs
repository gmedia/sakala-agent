//! The containers of one deployment.
//!
//! A deployment runs as one or more replicas: containers from the same image,
//! with the same labels, on the project's network, that the router balances
//! between. Each replica knows its index and the deployment's replica count
//! through two labels, so a lifecycle command and the heartbeat can treat the
//! set as one workload again after an Agent restart.
//!
//! A deployment of one replica keeps the container name every earlier Agent
//! used. Its route and its container stay exactly what they were, and a node
//! rolled back to an older Agent still finds the container under the name that
//! Agent computes. Only a deployment of several replicas adds `-r<index>`.

use std::num::NonZeroU32;

use sakala_agent_protocol::RequestedReplicas;
use uuid::Uuid;

use crate::{RuntimeError, containers::container_name};

/// Index of a container among the replicas of its deployment, from 0.
pub(crate) const REPLICA_LABEL: &str = "dev.sakala.replica";
/// Replica count of the deployment a container belongs to.
pub(crate) const REPLICAS_LABEL: &str = "dev.sakala.replicas";

/// The highest `SAKALA_MAX_REPLICAS_PER_PROJECT` a node accepts. A replica
/// name is the deployment's container name plus `-r<index>`, and that name is
/// also the router's upstream host, so it must stay inside the 63-octet DNS
/// label Docker's embedded DNS answers for. Two digits of index keep it at 60.
pub const MAX_REPLICAS_PER_PROJECT_CEILING: u32 = 64;

/// Where one container sits in its deployment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicaSlot {
    pub index: u32,
    pub count: NonZeroU32,
}

impl ReplicaSlot {
    /// The only container of a deployment without replicas, which is also what
    /// a container started before replicas existed reads as.
    pub const SINGLE: Self = Self {
        index: 0,
        count: NonZeroU32::MIN,
    };

    /// Every slot of a deployment of `count` replicas, in index order.
    pub fn all(count: NonZeroU32) -> impl Iterator<Item = Self> {
        (0..count.get()).map(move |index| Self { index, count })
    }

    /// The container name of this replica, which the router also uses as its
    /// upstream host.
    #[must_use]
    pub fn container_name(self, project_id: Uuid, deployment_id: Uuid) -> String {
        let name = container_name(project_id, deployment_id);
        if self.count == NonZeroU32::MIN {
            name
        } else {
            format!("{name}-r{}", self.index)
        }
    }

    /// The labels that record this slot on its container.
    pub(crate) fn labels(self) -> [String; 2] {
        [
            format!("{REPLICA_LABEL}={}", self.index),
            format!("{REPLICAS_LABEL}={}", self.count),
        ]
    }

    /// Reads the slot back from a container's two labels. A container without
    /// them, or with values this Agent never writes, is a single replica.
    pub(crate) fn from_labels(index: Option<&str>, count: Option<&str>) -> Self {
        let parsed = index
            .and_then(|index| index.trim().parse::<u32>().ok())
            .zip(count.and_then(|count| count.trim().parse::<NonZeroU32>().ok()))
            .filter(|(index, count)| *index < count.get());
        match parsed {
            Some((index, count)) => Self { index, count },
            None => Self::SINGLE,
        }
    }
}

/// The replica count a deployment runs with. More than the node allows fails
/// the deployment instead of running fewer: a project quietly getting fewer
/// replicas than it asked for would look like spare capacity it does not have.
pub(crate) fn resolve_replicas(
    requested: RequestedReplicas,
    maximum: u32,
) -> Result<NonZeroU32, RuntimeError> {
    let count = requested.count();
    if count.get() > maximum {
        return Err(RuntimeError::ReplicasExceeded {
            requested: count.get(),
            maximum,
        });
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Uuid {
        Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID")
    }

    fn deployment() -> Uuid {
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID")
    }

    fn count(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("non-zero")
    }

    #[test]
    fn a_single_replica_keeps_the_name_of_earlier_agents() {
        assert_eq!(
            ReplicaSlot::SINGLE.container_name(project(), deployment()),
            container_name(project(), deployment())
        );
    }

    #[test]
    fn several_replicas_are_numbered_from_zero_inside_a_dns_label() {
        let names = ReplicaSlot::all(count(3))
            .map(|slot| slot.container_name(project(), deployment()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-r0",
                "sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-r1",
                "sakala-app-ff66ed4a-4f1f21ef-730d-42d5-a46d-d965353cb993-r2",
            ]
        );
        let last = ReplicaSlot {
            index: MAX_REPLICAS_PER_PROJECT_CEILING - 1,
            count: count(MAX_REPLICAS_PER_PROJECT_CEILING),
        }
        .container_name(project(), deployment());
        assert!(last.len() <= 63, "{last} is {} octets", last.len());
    }

    #[test]
    fn labels_round_trip_and_anything_else_reads_as_a_single_replica() {
        let slot = ReplicaSlot {
            index: 2,
            count: count(3),
        };
        let [index, total] = slot.labels();
        assert_eq!(index, "dev.sakala.replica=2");
        assert_eq!(total, "dev.sakala.replicas=3");
        assert_eq!(ReplicaSlot::from_labels(Some("2"), Some("3")), slot);

        for (index, total) in [
            (None, None),
            (Some(""), Some("")),
            (Some("0"), None),
            (Some("3"), Some("3")),
            (Some("1"), Some("0")),
            (Some("x"), Some("2")),
        ] {
            assert_eq!(
                ReplicaSlot::from_labels(index, total),
                ReplicaSlot::SINGLE,
                "{index:?} {total:?}"
            );
        }
    }

    #[test]
    fn more_replicas_than_the_node_allows_fail_with_a_stable_code() {
        assert_eq!(
            resolve_replicas(RequestedReplicas::Absent, 4)
                .expect("absent")
                .get(),
            1
        );
        assert_eq!(
            resolve_replicas(RequestedReplicas::Unreadable, 1)
                .expect("unreadable")
                .get(),
            1
        );
        assert_eq!(
            resolve_replicas(RequestedReplicas::Count(count(4)), 4)
                .expect("at the maximum")
                .get(),
            4
        );
        let error =
            resolve_replicas(RequestedReplicas::Count(count(5)), 4).expect_err("above the maximum");
        assert_eq!(error.code(), "runtime_replicas_exceeded");
        assert_eq!(
            error.to_string(),
            "the deployment asks for 5 replicas, but this node runs at most 4 replicas per project"
        );
    }
}
