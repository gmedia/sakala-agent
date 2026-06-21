use sakala_agent_protocol::DeploymentLog;

use crate::{CoreError, api::ApiClient};

use super::redactor::redact_line;

pub async fn report_logs(client: &ApiClient, logs: Vec<DeploymentLog>) -> Result<(), CoreError> {
    for mut log in logs {
        log.line = redact_line(&log.line);
        client.log(log.command_id, &log).await?;
    }

    Ok(())
}
