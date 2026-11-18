//! One lock per project, shared by commands and replica restore.
//!
//! The command poller already runs at most one command per project. This lock
//! adds the background replica restore to that rule: a command waits for a
//! restore of its project to finish, and a restore skips a project whose command
//! holds the lock. A restore therefore never starts a replica halfway through a
//! sleep, stop, wake, or deployment of the same project.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

#[derive(Debug, Default)]
pub(crate) struct ProjectLocks {
    locks: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
}

impl ProjectLocks {
    /// Waits until no one else holds the project, for a command.
    pub(crate) async fn lock(&self, project_id: Uuid) -> OwnedMutexGuard<()> {
        self.entry(project_id).lock_owned().await
    }

    /// Takes the project only when it is free, for background work that must
    /// give way to a command.
    pub(crate) fn try_lock(&self, project_id: Uuid) -> Option<OwnedMutexGuard<()>> {
        self.entry(project_id).try_lock_owned().ok()
    }

    pub(crate) fn is_locked(&self, project_id: Uuid) -> bool {
        self.locks
            .lock()
            .expect("project lock table")
            .get(&project_id)
            .is_some_and(|lock| lock.try_lock().is_err())
    }

    /// The project's lock. Entries nobody holds or waits for are dropped on the
    /// way, so the table stays as small as the set of busy projects.
    fn entry(&self, project_id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock().expect("project lock table");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(project_id).or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_held_project_refuses_background_work_until_released() {
        let locks = ProjectLocks::default();
        let project = Uuid::new_v4();
        let other = Uuid::new_v4();

        let command = locks.lock(project).await;
        assert!(locks.is_locked(project));
        assert!(locks.try_lock(project).is_none());
        assert!(!locks.is_locked(other));
        assert!(locks.try_lock(other).is_some());

        drop(command);
        assert!(!locks.is_locked(project));
        assert!(locks.try_lock(project).is_some());
    }

    #[tokio::test]
    async fn a_command_waits_for_background_work_of_its_project() {
        let locks = Arc::new(ProjectLocks::default());
        let project = Uuid::new_v4();
        let restore = locks.try_lock(project).expect("free project");

        let waiting = tokio::spawn({
            let locks = Arc::clone(&locks);
            async move {
                let _guard = locks.lock(project).await;
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        drop(restore);
        waiting.await.expect("command gets the project");
    }

    #[tokio::test]
    async fn released_entries_are_dropped() {
        let locks = ProjectLocks::default();
        for _ in 0..3 {
            drop(locks.lock(Uuid::new_v4()).await);
        }
        let held = locks.lock(Uuid::new_v4()).await;
        assert_eq!(locks.locks.lock().expect("table").len(), 1);
        drop(held);
    }
}
