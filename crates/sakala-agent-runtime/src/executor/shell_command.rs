//! Rules for the commands a project gives the Agent to run in its own
//! containers: build, start, release, worker, and job commands.

use crate::RuntimeError;

/// Longest command, in characters, the rule of the control plane.
pub(super) const MAX_COMMAND_CHARS: usize = 1_000;

/// An empty command means the project did not set one.
pub(super) fn command_field(command: Option<&str>) -> Option<String> {
    command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
}

/// The rule of `build_command` and `start_command`: one line.
pub(super) fn validate_command_field(
    name: &str,
    command: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(command) = command else {
        return Ok(());
    };
    if command.chars().count() > MAX_COMMAND_CHARS || command.contains(['\n', '\r', '\0']) {
        return Err(RuntimeError::InvalidCommand(format!(
            "{name} must be one line of at most {MAX_COMMAND_CHARS} characters"
        )));
    }
    Ok(())
}

/// The rule of the commands that run on their own in `sh -c`: release,
/// worker, and job commands. Stricter than [`validate_command_field`], like
/// the control plane: no ASCII control character at all, so the line shown in
/// the log is the line that runs.
pub(super) fn validate_shell_command(name: &str, command: &str) -> Result<(), RuntimeError> {
    if command.chars().count() > MAX_COMMAND_CHARS || command.chars().any(|c| c.is_ascii_control())
    {
        return Err(RuntimeError::InvalidCommand(format!(
            "{name} must be one line of at most {MAX_COMMAND_CHARS} characters without control characters"
        )));
    }
    Ok(())
}

/// A command that must be there: a worker's or a job's.
pub(super) fn required_shell_command(name: &str, command: &str) -> Result<String, RuntimeError> {
    validate_shell_command(name, command)?;
    command_field(Some(command))
        .ok_or_else(|| RuntimeError::InvalidCommand(format!("{name} must not be empty")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_required_command_is_trimmed_and_never_empty() {
        assert_eq!(
            required_shell_command("command", "  php artisan queue:work ").expect("command"),
            "php artisan queue:work"
        );
        for command in ["", "   "] {
            let error = required_shell_command("command", command).expect_err(command);
            assert_eq!(error.code(), "invalid_runtime_command");
            assert!(error.to_string().contains("must not be empty"), "{error}");
        }
        for command in ["a\nb".to_owned(), "a\tb".to_owned(), "a".repeat(1_001)] {
            let error = required_shell_command("command", &command).expect_err(&command);
            assert!(error.to_string().contains("one line"), "{error}");
        }
    }
}
