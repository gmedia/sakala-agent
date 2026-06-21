use std::sync::Arc;

use sakala_agent_runtime::RuntimeExecutor;
use tokio::{sync::watch, time::sleep};
use tracing::{info, warn};

use crate::{AgentConfig, api::ApiClient, commands::CommandHandler};

pub async fn run(
    config: AgentConfig,
    client: Option<ApiClient>,
    runtime: Arc<dyn RuntimeExecutor>,
    mut shutdown: watch::Receiver<bool>,
) {
    let handler = client
        .as_ref()
        .map(|client| CommandHandler::new(client.clone(), runtime));

    loop {
        if let (Some(client), Some(handler)) = (&client, &handler) {
            match client.poll_commands().await {
                Ok(commands) => {
                    for command in commands {
                        if let Err(error) = handler.handle(&command).await {
                            warn!(command_id = %command.id, %error, "command execution failed");
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
