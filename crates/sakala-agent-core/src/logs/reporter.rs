use sakala_agent_protocol::DeploymentLog;
use uuid::Uuid;

use crate::{CoreError, api::ApiClient};

use super::redactor::redact_line;

pub async fn report_logs(
    client: &ApiClient,
    command_id: Uuid,
    logs: Vec<DeploymentLog>,
) -> Result<(), CoreError> {
    for mut log in logs {
        log.message = redact_line(&log.message);
        client.log(command_id, &log).await?;
    }

    Ok(())
}
