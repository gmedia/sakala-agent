//! Which stopped replicas the Agent starts again, and when.
//!
//! Under `--restart unless-stopped` Docker restarts a replica that crashes or is
//! killed for memory, but not one stopped by `docker stop` or `docker kill`: it
//! stays `exited` until something starts it. Without this, a deployment of three
//! replicas kept serving on two until its next deployment.
//!
//! A replica qualifies when its deployment still serves: at least one other
//! replica is up, and it is the only deployment of its project that is. That
//! leaves out a sleeping project (no replica up), a one-replica deployment whose
//! container stopped (reported as before through `crashed_details`), and a
//! project in the middle of a rolling deploy (two deployments up). A project
//! with a command in flight is left alone as well.
//!
//! Each replica gets [`RESTORE_MAX_ATTEMPTS`] starts, the first one right away
//! and the next ones after a doubling delay. Staying up for the crash stable
//! window clears its count. After the last attempt the Agent stops trying and
//! logs a warning; the deployment keeps reporting fewer ready replicas than it
//! wants, and a redeploy or `RestartProject` brings the replica back.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroU32,
    time::Duration,
};

use sakala_agent_core::support::retry::exponential_delay;
use tokio::time::Instant;
use uuid::Uuid;

use crate::containers::{ListedReplica, ReplicaSlot};

/// Delay before the second start of a replica; later ones double from here.
pub(crate) const RESTORE_BASE_DELAY: Duration = Duration::from_secs(30);
/// Longest delay between two starts of the same replica.
pub(crate) const RESTORE_MAX_DELAY: Duration = Duration::from_secs(300);
/// Starts of one replica before the Agent gives up on it.
pub(crate) const RESTORE_MAX_ATTEMPTS: u32 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RestorePolicy {
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Duration,
    pub(crate) max_attempts: u32,
    /// How long a started replica must stay up before its attempts reset.
    pub(crate) stable_window: Duration,
}

impl RestorePolicy {
    pub(crate) fn new(stable_window: Duration) -> Self {
        Self {
            base_delay: RESTORE_BASE_DELAY,
            max_delay: RESTORE_MAX_DELAY,
            max_attempts: RESTORE_MAX_ATTEMPTS,
            stable_window,
        }
    }
}

/// A stopped replica to start now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RestoreTarget {
    pub(crate) project_id: Uuid,
    pub(crate) deployment_id: Uuid,
    pub(crate) container_id: String,
    pub(crate) replica: ReplicaSlot,
    /// Which start of this replica it is, from 1.
    pub(crate) attempt: u32,
}

#[derive(Clone, Copy, Debug)]
struct Backoff {
    attempts: u32,
    retry_at: Instant,
    started_at: Instant,
    gave_up: bool,
}

/// Restore attempts per container, kept in memory. An Agent restart starts
/// every replica's count again, which is at most one more round of attempts.
#[derive(Debug)]
pub(crate) struct ReplicaRestorer {
    policy: RestorePolicy,
    backoff: HashMap<String, Backoff>,
    /// Missing replicas already warned about, so each is logged once.
    missing_reported: HashSet<(Uuid, Uuid, u32)>,
}

impl ReplicaRestorer {
    pub(crate) fn new(policy: RestorePolicy) -> Self {
        Self {
            policy,
            backoff: HashMap::new(),
            missing_reported: HashSet::new(),
        }
    }

    /// The replicas to start now, from one node-wide listing. `busy` says
    /// whether a project has a command in flight.
    pub(crate) fn plan(
        &mut self,
        listing: &[ListedReplica],
        now: Instant,
        busy: impl Fn(Uuid) -> bool,
    ) -> Vec<RestoreTarget> {
        self.forget_gone(listing, now);

        let mut projects = BTreeMap::<Uuid, BTreeMap<Uuid, Vec<&ListedReplica>>>::new();
        for listed in listing {
            projects
                .entry(listed.project_id)
                .or_default()
                .entry(listed.deployment_id)
                .or_default()
                .push(listed);
        }

        let mut targets = Vec::new();
        for (project_id, deployments) in projects {
            let mut serving = deployments
                .iter()
                .filter(|(_, replicas)| replicas.iter().any(|replica| replica.is_running()));
            let (Some((&deployment_id, replicas)), None) = (serving.next(), serving.next()) else {
                continue;
            };
            if replicas.iter().all(|replica| replica.is_running())
                && replicas.len() >= desired(replicas).get() as usize
            {
                continue;
            }
            if busy(project_id) {
                tracing::debug!(
                    %project_id,
                    %deployment_id,
                    "replica restore waits for the project's command to finish"
                );
                continue;
            }
            self.report_missing(project_id, deployment_id, replicas);
            for replica in replicas.iter().filter(|replica| replica.is_exited()) {
                if let Some(attempt) = self.next_attempt(replica, now) {
                    targets.push(RestoreTarget {
                        project_id,
                        deployment_id,
                        container_id: replica.container_id.clone(),
                        replica: replica.replica,
                        attempt,
                    });
                }
            }
        }
        targets
    }

    /// Counts a start of the replica, whatever its outcome, and schedules the
    /// earliest next one.
    pub(crate) fn record_attempt(&mut self, container_id: &str, now: Instant) {
        let entry = self
            .backoff
            .entry(container_id.to_owned())
            .or_insert(Backoff {
                attempts: 0,
                retry_at: now,
                started_at: now,
                gave_up: false,
            });
        let delay = exponential_delay(
            entry.attempts,
            self.policy.base_delay,
            self.policy.max_delay,
        );
        entry.attempts += 1;
        entry.started_at = now;
        entry.retry_at = now + delay;
    }

    /// Drops the state of containers that are gone, and the attempts of
    /// replicas that stayed up for the stable window.
    fn forget_gone(&mut self, listing: &[ListedReplica], now: Instant) {
        let present = listing
            .iter()
            .map(|listed| (listed.container_id.as_str(), listed))
            .collect::<HashMap<_, _>>();
        let stable_window = self.policy.stable_window;
        self.backoff.retain(
            |container_id, backoff| match present.get(container_id.as_str()) {
                None => false,
                Some(listed) if listed.is_running() => {
                    let stable = now.saturating_duration_since(backoff.started_at) >= stable_window;
                    if stable {
                        tracing::debug!(
                            container_id = %container_id,
                            "restored replica stayed up; its restore attempts are reset"
                        );
                    }
                    !stable
                }
                Some(_) => true,
            },
        );
        let deployments = listing
            .iter()
            .map(|listed| (listed.project_id, listed.deployment_id))
            .collect::<HashSet<_>>();
        self.missing_reported
            .retain(|(project_id, deployment_id, _)| {
                deployments.contains(&(*project_id, *deployment_id))
            });
    }

    fn next_attempt(&mut self, replica: &ListedReplica, now: Instant) -> Option<u32> {
        let Some(backoff) = self.backoff.get_mut(&replica.container_id) else {
            return Some(1);
        };
        if backoff.gave_up {
            return None;
        }
        if backoff.attempts >= self.policy.max_attempts {
            backoff.gave_up = true;
            tracing::warn!(
                container_id = %replica.container_id,
                project_id = %replica.project_id,
                deployment_id = %replica.deployment_id,
                replica = replica.replica.index,
                attempts = backoff.attempts,
                "replica keeps stopping; the agent stops restoring it until the next deployment or restart"
            );
            return None;
        }
        (now >= backoff.retry_at).then_some(backoff.attempts + 1)
    }

    /// A replica whose container is gone cannot be started again: the Agent
    /// keeps no copy of the deployment's environment to create it from.
    fn report_missing(
        &mut self,
        project_id: Uuid,
        deployment_id: Uuid,
        replicas: &[&ListedReplica],
    ) {
        let present = replicas
            .iter()
            .map(|replica| replica.replica.index)
            .collect::<HashSet<_>>();
        for slot in
            ReplicaSlot::all(desired(replicas)).filter(|slot| !present.contains(&slot.index))
        {
            if self
                .missing_reported
                .insert((project_id, deployment_id, slot.index))
            {
                tracing::warn!(
                    %project_id,
                    %deployment_id,
                    replica = slot.index,
                    "replica container is gone; it returns with the next deployment"
                );
            }
        }
    }
}

/// The replica count of a deployment, from the labels of its containers.
fn desired(replicas: &[&ListedReplica]) -> NonZeroU32 {
    replicas
        .iter()
        .map(|replica| replica.replica.count)
        .max()
        .unwrap_or(ReplicaSlot::SINGLE.count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STABLE_WINDOW: Duration = Duration::from_secs(300);

    fn policy() -> RestorePolicy {
        RestorePolicy::new(STABLE_WINDOW)
    }

    fn project() -> Uuid {
        Uuid::parse_str("ff66ed4a-6303-4be6-8ef4-63c28b112680").expect("project UUID")
    }

    fn deployment() -> Uuid {
        Uuid::parse_str("4f1f21ef-730d-42d5-a46d-d965353cb993").expect("deployment UUID")
    }

    fn replica(
        project_id: Uuid,
        deployment_id: Uuid,
        index: u32,
        count: u32,
        status: &str,
    ) -> ListedReplica {
        ListedReplica {
            container_id: format!("{}-{deployment_id}-r{index}", &project_id.to_string()[..8]),
            project_id,
            deployment_id,
            replica: ReplicaSlot {
                index,
                count: NonZeroU32::new(count).expect("non-zero"),
            },
            status: status.to_owned(),
            process: None,
        }
    }

    /// Three replicas of one deployment with the given statuses.
    fn three(statuses: [&str; 3]) -> Vec<ListedReplica> {
        statuses
            .iter()
            .enumerate()
            .map(|(index, status)| replica(project(), deployment(), index as u32, 3, status))
            .collect()
    }

    fn not_busy(_: Uuid) -> bool {
        false
    }

    fn ids(targets: &[RestoreTarget]) -> Vec<&str> {
        targets
            .iter()
            .map(|target| target.container_id.as_str())
            .collect()
    }

    #[test]
    fn a_stopped_replica_of_a_serving_deployment_is_restored_at_once() {
        let mut restorer = ReplicaRestorer::new(policy());
        let listing = three(["Up 5 minutes", "Exited (137) 3 seconds ago", "Up 5 minutes"]);

        let targets = restorer.plan(&listing, Instant::now(), not_busy);

        assert_eq!(ids(&targets), [listing[1].container_id.as_str()]);
        assert_eq!(targets[0].attempt, 1);
        assert_eq!(targets[0].replica.index, 1);
        assert_eq!(targets[0].project_id, project());
        assert_eq!(targets[0].deployment_id, deployment());
    }

    #[test]
    fn a_sleeping_or_wholly_stopped_deployment_is_left_alone() {
        let mut restorer = ReplicaRestorer::new(policy());
        let sleeping = three([
            "Exited (0) 2 minutes ago",
            "Exited (0) 2 minutes ago",
            "Exited (137) 2 minutes ago",
        ]);
        assert!(
            restorer
                .plan(&sleeping, Instant::now(), not_busy)
                .is_empty()
        );

        // One replica: a stopped container is reported and reconciled through
        // the control plane, as before replica restore existed.
        let single = [replica(
            project(),
            deployment(),
            0,
            1,
            "Exited (137) 1 second ago",
        )];
        assert!(restorer.plan(&single, Instant::now(), not_busy).is_empty());
    }

    #[test]
    fn a_project_with_a_command_in_flight_is_skipped_without_using_an_attempt() {
        let mut restorer = ReplicaRestorer::new(policy());
        let listing = three(["Up 5 minutes", "Exited (137) 3 seconds ago", "Up 5 minutes"]);
        let now = Instant::now();

        assert!(
            restorer
                .plan(&listing, now, |id| id == project())
                .is_empty()
        );
        let other = Uuid::new_v4();
        let targets = restorer.plan(&listing, now, |id| id == other);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].attempt, 1);
    }

    #[test]
    fn only_exited_replicas_qualify() {
        let mut restorer = ReplicaRestorer::new(policy());
        // Docker restarts a crashed replica itself, and a `Created` or `Dead`
        // container is a leftover `docker start` cannot help.
        let listing = vec![
            replica(project(), deployment(), 0, 4, "Up 5 minutes"),
            replica(
                project(),
                deployment(),
                1,
                4,
                "Restarting (1) 2 seconds ago",
            ),
            replica(project(), deployment(), 2, 4, "Created"),
            replica(project(), deployment(), 3, 4, "Dead"),
        ];
        assert!(restorer.plan(&listing, Instant::now(), not_busy).is_empty());
    }

    #[test]
    fn a_rolling_deploy_with_two_serving_deployments_is_left_alone() {
        let mut restorer = ReplicaRestorer::new(policy());
        let candidate = Uuid::new_v4();
        let mut listing = three(["Up 5 minutes", "Exited (0) 1 second ago", "Up 5 minutes"]);
        listing.push(replica(project(), candidate, 0, 2, "Up 2 seconds"));
        assert!(restorer.plan(&listing, Instant::now(), not_busy).is_empty());

        // Once the old set is gone the remaining deployment is restored again.
        let mut after = vec![
            replica(project(), candidate, 0, 2, "Up 1 minute"),
            replica(project(), candidate, 1, 2, "Exited (137) 1 second ago"),
        ];
        after.extend(three([
            "Exited (0) 1 minute ago",
            "Exited (0) 1 minute ago",
            "Exited (0) 1 minute ago",
        ]));
        let targets = restorer.plan(&after, Instant::now(), not_busy);
        assert_eq!(ids(&targets), [after[1].container_id.as_str()]);
    }

    #[test]
    fn projects_are_judged_independently() {
        let mut restorer = ReplicaRestorer::new(policy());
        let sleeping_project = Uuid::new_v4();
        let sleeping_deployment = Uuid::new_v4();
        let mut listing = three(["Up 5 minutes", "Up 5 minutes", "Exited (137) 1 second ago"]);
        listing.push(replica(
            sleeping_project,
            sleeping_deployment,
            0,
            2,
            "Exited (0) 1 hour ago",
        ));
        listing.push(replica(
            sleeping_project,
            sleeping_deployment,
            1,
            2,
            "Exited (0) 1 hour ago",
        ));

        let targets = restorer.plan(&listing, Instant::now(), not_busy);
        assert_eq!(ids(&targets), [listing[2].container_id.as_str()]);
    }

    #[test]
    fn later_attempts_wait_a_doubling_capped_delay() {
        let mut restorer = ReplicaRestorer::new(RestorePolicy {
            max_attempts: 10,
            ..policy()
        });
        let listing = three(["Up 5 minutes", "Exited (137) 1 second ago", "Up 5 minutes"]);
        let id = listing[1].container_id.clone();
        let mut now = Instant::now();

        let mut delays = Vec::new();
        for attempt in 1..=5 {
            let targets = restorer.plan(&listing, now, not_busy);
            assert_eq!(targets.len(), 1, "attempt {attempt}");
            assert_eq!(targets[0].attempt, attempt);
            restorer.record_attempt(&id, now);
            let retry_at = restorer.backoff[&id].retry_at;
            delays.push(retry_at - now);

            // Nothing before the delay is over.
            let just_before = retry_at - Duration::from_millis(1);
            assert!(restorer.plan(&listing, just_before, not_busy).is_empty());
            now = retry_at;
        }
        assert_eq!(
            delays,
            [30, 60, 120, 240, 300].map(Duration::from_secs),
            "doubling from RESTORE_BASE_DELAY, capped at RESTORE_MAX_DELAY"
        );
    }

    #[test]
    fn the_agent_gives_up_after_the_last_attempt_and_stays_quiet() {
        let mut restorer = ReplicaRestorer::new(policy());
        let listing = three(["Up 5 minutes", "Exited (137) 1 second ago", "Up 5 minutes"]);
        let id = listing[1].container_id.clone();
        let mut now = Instant::now();
        for _ in 0..RESTORE_MAX_ATTEMPTS {
            assert_eq!(restorer.plan(&listing, now, not_busy).len(), 1);
            restorer.record_attempt(&id, now);
            now = restorer.backoff[&id].retry_at;
        }

        assert!(restorer.plan(&listing, now, not_busy).is_empty());
        assert!(restorer.backoff[&id].gave_up);
        let much_later = now + Duration::from_secs(3_600);
        assert!(restorer.plan(&listing, much_later, not_busy).is_empty());

        // A redeploy replaces the containers, which clears the state.
        let redeployed = Uuid::new_v4();
        let next = vec![
            replica(project(), redeployed, 0, 2, "Up 1 minute"),
            replica(project(), redeployed, 1, 2, "Exited (137) 1 second ago"),
        ];
        let targets = restorer.plan(&next, much_later, not_busy);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].attempt, 1);
        assert!(!restorer.backoff.contains_key(&id));
    }

    #[test]
    fn a_replica_that_stays_up_starts_its_count_again() {
        let mut restorer = ReplicaRestorer::new(policy());
        let stopped = three(["Up 5 minutes", "Exited (137) 1 second ago", "Up 5 minutes"]);
        let running = three(["Up 5 minutes", "Up 1 second", "Up 5 minutes"]);
        let id = stopped[1].container_id.clone();
        let started = Instant::now();
        restorer.plan(&stopped, started, not_busy);
        restorer.record_attempt(&id, started);

        // Up, but not yet for the stable window: a quick second stop waits.
        let soon = started + Duration::from_secs(10);
        restorer.plan(&running, soon, not_busy);
        assert!(restorer.plan(&stopped, soon, not_busy).is_empty());

        // Up for the whole window: the next stop is restored at once again.
        let stable = started + STABLE_WINDOW;
        restorer.plan(&running, stable, not_busy);
        assert!(!restorer.backoff.contains_key(&id));
        let targets = restorer.plan(&stopped, stable, not_busy);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].attempt, 1);
    }

    #[test]
    fn a_missing_replica_is_reported_once_and_not_targeted() {
        let mut restorer = ReplicaRestorer::new(policy());
        let mut listing = three(["Up 5 minutes", "Up 5 minutes", "Up 5 minutes"]);
        listing.remove(1);

        assert!(restorer.plan(&listing, Instant::now(), not_busy).is_empty());
        assert!(
            restorer
                .missing_reported
                .contains(&(project(), deployment(), 1))
        );
        assert!(restorer.plan(&listing, Instant::now(), not_busy).is_empty());
        assert_eq!(restorer.missing_reported.len(), 1);

        // The deployment is gone: nothing is kept for it.
        restorer.plan(&[], Instant::now(), not_busy);
        assert!(restorer.missing_reported.is_empty());
    }
}
