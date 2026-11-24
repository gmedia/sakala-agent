//! The one-off container of a release command or a job, and the names of the
//! one-off containers whose command still runs.

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use sakala_agent_core::ports::RuntimeReporter;
use tokio::{sync::oneshot, task::JoinHandle};

use crate::{
    RuntimeError,
    containers::{ContainerEngine, OneOffContainerRequest},
};

/// How long the removal of a one-off container waits for its `docker run` to
/// end. `docker run --detach` returns once Docker created and started the
/// container, which takes a few seconds under gVisor. Past this the
/// `docker run` is killed, and the next sweep removes whatever Docker still
/// creates.
const START_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the removal of a one-off container is tried before it is left
/// to the next sweep.
const REMOVE_ATTEMPTS: u32 = 3;
/// The wait between two removal attempts.
const REMOVE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The names of the one-off containers whose command still runs, which the
/// sweep of one-off containers leaves alone.
#[derive(Default)]
pub(super) struct ActiveOneOffs {
    names: Mutex<HashSet<String>>,
}

impl ActiveOneOffs {
    pub(super) fn contains(&self, name: &str) -> bool {
        self.lock().contains(name)
    }

    fn insert(&self, name: &str) {
        self.lock().insert(name.to_owned());
    }

    fn remove(&self, name: &str) {
        self.lock().remove(name);
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<String>> {
        self.names.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The one-off container of a release command or a job, removed on every
/// path: by the step that ran it once the command ended or timed out, by the
/// command's cleanup after a cancellation, and in the background when the
/// command is dropped before either could run, such as after the shutdown
/// grace. It counts as active from its creation until it is dropped and
/// removed, so the sweep never removes it while its command runs.
pub(super) struct OneOffContainer {
    containers: Arc<dyn ContainerEngine>,
    active: Arc<ActiveOneOffs>,
    pub(super) name: String,
    /// Set from just before `docker run` until a removal succeeded.
    started: AtomicBool,
    /// The task that runs the container's `docker run`, until a removal
    /// waited for it.
    start: Mutex<Option<JoinHandle<()>>>,
}

impl OneOffContainer {
    pub(super) fn new(
        containers: Arc<dyn ContainerEngine>,
        active: &Arc<ActiveOneOffs>,
        name: String,
    ) -> Self {
        active.insert(&name);
        Self {
            containers,
            active: Arc::clone(active),
            name,
            started: AtomicBool::new(false),
            start: Mutex::new(None),
        }
    }

    /// Creates and starts the container, as
    /// [`ContainerEngine::start_one_off`] does.
    ///
    /// `docker run` runs in a task of its own: a command cancelled or dropped
    /// while Docker creates the container must not kill it halfway, or Docker
    /// finishes creating the container after the removal found nothing. The
    /// removal waits for that task instead.
    pub(super) async fn start(
        &self,
        request: &OneOffContainerRequest,
        reporter: Arc<dyn RuntimeReporter>,
    ) -> Result<(), RuntimeError> {
        // Before `docker run`, which can leave a created container behind
        // even when it fails.
        self.started.store(true, Ordering::Release);
        let (sender, outcome) = oneshot::channel();
        let containers = Arc::clone(&self.containers);
        let request = request.clone();
        let task = tokio::spawn(async move {
            let _ = sender.send(containers.start_one_off(&request, reporter.as_ref()).await);
        });
        *self.start_task() = Some(task);
        outcome.await.unwrap_or_else(|_| {
            Err(RuntimeError::Execution(
                "the start of a one-off container ended without a result".to_owned(),
            ))
        })
    }

    /// Does nothing when no container was started, or it is gone.
    pub(super) async fn remove(&self) -> Result<(), RuntimeError> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let start = self.start_task().take();
        remove_after_start(self.containers.as_ref(), &self.name, start).await?;
        self.started.store(false, Ordering::Release);
        Ok(())
    }

    fn start_task(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        self.start.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for OneOffContainer {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        if !self.started.load(Ordering::Acquire) {
            self.active.remove(&name);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.active.remove(&name);
            tracing::warn!(container = %name, "one-off container left behind; the next sweep removes it");
            return;
        };
        let start = self
            .start
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let containers = Arc::clone(&self.containers);
        let active = Arc::clone(&self.active);
        runtime.spawn(async move {
            if let Err(error) = remove_after_start(containers.as_ref(), &name, start).await {
                tracing::warn!(container = %name, %error, "one-off container could not be removed; the next sweep removes it");
            }
            active.remove(&name);
        });
    }
}

/// Waits until the container's `docker run` ended, so the container exists
/// or never will, then removes it by name, a few times before giving up.
async fn remove_after_start(
    containers: &dyn ContainerEngine,
    name: &str,
    start: Option<JoinHandle<()>>,
) -> Result<(), RuntimeError> {
    if let Some(mut start) = start
        && tokio::time::timeout(START_SETTLE_TIMEOUT, &mut start)
            .await
            .is_err()
    {
        tracing::warn!(
            container = name,
            timeout = ?START_SETTLE_TIMEOUT,
            "docker run of a one-off container did not end and is killed"
        );
        // Dropping `docker run` kills it.
        start.abort();
        let _ = start.await;
    }
    let mut attempt = 1;
    loop {
        match containers.remove_one_off(name).await {
            Ok(()) => return Ok(()),
            Err(error) if attempt >= REMOVE_ATTEMPTS => return Err(error),
            Err(error) => {
                tracing::debug!(container = name, attempt, %error, "one-off container removal is tried again");
                tokio::time::sleep(REMOVE_RETRY_DELAY).await;
                attempt += 1;
            }
        }
    }
}
