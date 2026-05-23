use sakala_agent_protocol::CommandStatus;

#[must_use]
pub fn can_transition(from: CommandStatus, to: CommandStatus) -> bool {
    matches!(
        (from, to),
        (CommandStatus::Pending, CommandStatus::Claimed)
            | (CommandStatus::Claimed, CommandStatus::Running)
            | (CommandStatus::Running, CommandStatus::Succeeded)
            | (CommandStatus::Running, CommandStatus::Failed)
            | (CommandStatus::Pending, CommandStatus::Cancelled)
            | (CommandStatus::Pending, CommandStatus::Expired)
    )
}
