use std::sync::Arc;

use sakala_agent_protocol::CommandStatus;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{AgentConfig, api::ApiClient, commands::CommandProcessor, ports::RuntimeExecutor};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    runtime: Arc<dyn RuntimeExecutor>,
    mut shutdown: watch::Receiver<bool>,
) {
    let handler = client
        .as_ref()
        .map(|client| CommandProcessor::new(client.clone(), runtime, config.command_timeout()));

    'polling: loop {
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

                        tokio::select! {
                            result = handler.process(&command) => {
                                if let Err(error) = result {
                                    warn!(command_id = %command.id, %error, "command execution failed");
                                }
                            }
                            result = shutdown.changed() => {
                                if result.is_err() || *shutdown.borrow() {
                                    info!(command_id = %command.id, "cancelling in-flight command during shutdown");
                                    break 'polling;
                                }
                            }
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
                    break;
                }
            }
        }
    }

    info!("command poller stopped");
}
