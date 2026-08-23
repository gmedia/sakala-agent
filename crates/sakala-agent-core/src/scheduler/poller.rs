use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
};

use sakala_agent_protocol::{AgentCommand, CommandStatus};
use tokio::{sync::watch, task::JoinSet, time::sleep};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{AgentConfig, api::ApiClient, commands::CommandProcessor, ports::RuntimeExecutor};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    runtime: Arc<dyn RuntimeExecutor>,
    mut shutdown: watch::Receiver<bool>,
) {
    let handler = client.as_ref().map(|client| {
        Arc::new(CommandProcessor::new(
            client.clone(),
            runtime,
            config.command_timeout(),
        ))
    });
    let mut executions = CommandExecutions::new(config.max_concurrent_commands);

    'polling: loop {
        executions.reap_completed();

        if let (Some(client), Some(handler)) = (&client, &handler) {
            match client.poll_commands().await {
                Ok(commands) => {
                    for command in commands {
                        if command.status != CommandStatus::Pending {
                            warn!(
                                command_id = %command.id,
                                status = ?command.status,
                                "skipping command that is not pending"
                            );
                            continue;
                        }

                        let command_id = command.id;
                        let project_id = command.project_id;
                        let work_command = command.clone();
                        if !executions.try_start(command, {
                            let handler = Arc::clone(handler);
                            async move {
                                if let Err(error) = handler.process(&work_command).await {
                                    warn!(command_id = %command_id, %error, "command execution failed");
                                }
                            }
                        }) {
                            debug!(
                                command_id = %command_id,
                                project_id = ?project_id,
                                "command remains pending because the bounded scheduler is at capacity or its project is busy"
                            );
                        }
                    }
                }
                Err(error) => warn!(%error, "failed to poll control-plane commands"),
            }
        } else {
            info!(
                agent_id = %config.agent_id,
                runtime_network = %config.runtime_network,
                "local command poll tick; control-plane request skipped"
            );
        }

        tokio::select! {
            () = sleep(config.poll_interval()) => {}
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    info!(in_flight_commands = executions.len(), "cancelling in-flight commands during shutdown");
                    break 'polling;
                }
            }
        }
    }

    executions.cancel_and_wait().await;
    info!("command poller stopped");
}

/// Bounds active command work without creating queued tasks. Commands that cannot
/// start remain pending in the control plane and will be considered again on a
/// later poll. A project may own at most one active command at a time.
struct CommandExecutions {
    limit: usize,
    active_projects: HashSet<Uuid>,
    task_projects: HashMap<tokio::task::Id, Uuid>,
    tasks: JoinSet<Option<Uuid>>,
}

impl CommandExecutions {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            active_projects: HashSet::new(),
            task_projects: HashMap::new(),
            tasks: JoinSet::new(),
        }
    }

    fn len(&self) -> usize {
        self.tasks.len()
    }

    fn try_start<F>(&mut self, command: AgentCommand, work: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let project_id = command.project_id;
        if self.tasks.len() >= self.limit
            || project_id.is_some_and(|project_id| self.active_projects.contains(&project_id))
        {
            return false;
        }

        if let Some(project_id) = project_id {
            self.active_projects.insert(project_id);
        }
        let task = self.tasks.spawn(async move {
            work.await;
            project_id
        });
        if let Some(project_id) = project_id {
            self.task_projects.insert(task.id(), project_id);
        }
        true
    }

    fn reap_completed(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            match result {
                Ok(Some(project_id)) => {
                    self.active_projects.remove(&project_id);
                    self.task_projects.retain(|_, id| *id != project_id);
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(project_id) = self.task_projects.remove(&error.id()) {
                        self.active_projects.remove(&project_id);
                    }
                    warn!(%error, "command task terminated unexpectedly");
                }
            }
        }
    }

    async fn cancel_and_wait(&mut self) {
        self.tasks.abort_all();
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(Some(project_id)) => {
                    self.active_projects.remove(&project_id);
                    self.task_projects.retain(|_, id| *id != project_id);
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(project_id) = self.task_projects.remove(&error.id()) {
                        self.active_projects.remove(&project_id);
                    }
                    if !error.is_cancelled() {
                        warn!(%error, "command task terminated unexpectedly during shutdown");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use sakala_agent_protocol::{CommandStatus, CommandType};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use super::CommandExecutions;

    fn command(project_id: Option<Uuid>) -> sakala_agent_protocol::AgentCommand {
        sakala_agent_protocol::AgentCommand {
            id: Uuid::new_v4(),
            command_type: CommandType::DeployProject,
            status: CommandStatus::Pending,
            project_id,
            deployment_id: None,
            payload: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn bounds_global_work_and_serializes_each_project() {
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        let mut executions = CommandExecutions::new(2);
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (release_a, wait_a) = oneshot::channel();
        let (release_b, wait_b) = oneshot::channel();

        assert!(executions.try_start(
            command(Some(project_a)),
            tracked(wait_a, Arc::clone(&active), Arc::clone(&maximum))
        ));
        assert!(!executions.try_start(command(Some(project_a)), async {}));
        assert!(executions.try_start(
            command(Some(project_b)),
            tracked(wait_b, Arc::clone(&active), Arc::clone(&maximum))
        ));
        assert!(!executions.try_start(command(None), async {}));

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        release_a.send(()).expect("first command should still run");
        release_b.send(()).expect("second command should still run");
        tokio::time::sleep(Duration::from_millis(10)).await;
        executions.reap_completed();

        assert!(executions.try_start(command(Some(project_a)), async {}));
        assert_eq!(executions.len(), 1);
    }

    async fn tracked(
        release: oneshot::Receiver<()>,
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
    ) {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        maximum.fetch_max(current, Ordering::SeqCst);
        let _ = release.await;
        active.fetch_sub(1, Ordering::SeqCst);
    }
}
