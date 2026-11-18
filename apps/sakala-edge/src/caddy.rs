//! Validates and reloads the host Caddy through operator-configured commands.
//!
//! Edge runs next to a Caddy managed by the host (systemd by default), not one
//! in a container, so both steps are plain commands. They run without a shell,
//! with the same reduced environment as every Agent child process: `SAKALA_*`,
//! including the Edge token, never reaches them.

use std::{fmt, str::FromStr, sync::Arc, time::Duration};

use sakala_agent_runtime::{CommandSpec, NullOutputSink, ProcessRunner, RuntimeError};
use thiserror::Error;
use tracing::{debug, warn};

/// Default validation for a systemd host Caddy.
pub const DEFAULT_VALIDATE_COMMAND: &str =
    "caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile";
/// Default reload for a systemd host Caddy.
pub const DEFAULT_RELOAD_COMMAND: &str = "systemctl reload caddy";
/// How long one validate or reload may take before it counts as failed. Caddy
/// validates a large config in well under a second; this only stops a hung
/// command from stalling the sync loop forever.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Caddy output kept for the operator when validation or reload fails.
const OPERATOR_OUTPUT_LINES: usize = 20;

/// A program and its arguments, written as one whitespace-separated line.
/// Quoting is not interpreted, so an argument cannot contain spaces; Caddy and
/// systemd paths do not need them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaddyCommand {
    program: String,
    args: Vec<String>,
}

impl FromStr for CaddyCommand {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut words = value.split_whitespace().map(str::to_owned);
        let program = words.next().ok_or_else(|| "names no command".to_owned())?;
        if value.contains(['"', '\'', '\\']) {
            return Err(
                "must not use quotes or backslashes; arguments are split on whitespace without a shell"
                    .to_owned(),
            );
        }
        Ok(Self {
            program,
            args: words.collect(),
        })
    }
}

impl fmt::Display for CaddyCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.program)?;
        for arg in &self.args {
            write!(formatter, " {arg}")?;
        }
        Ok(())
    }
}

impl CaddyCommand {
    fn spec(&self) -> CommandSpec {
        self.args
            .iter()
            .fold(CommandSpec::new(&self.program), |spec, arg| spec.arg(arg))
            .timeout(COMMAND_TIMEOUT)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaddyAction {
    Validate,
    Reload,
}

impl fmt::Display for CaddyAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Validate => "validate",
            Self::Reload => "reload",
        })
    }
}

#[derive(Debug, Error)]
pub enum CaddyError {
    #[error("Caddy could not {action} the routes: {status}")]
    Refused { action: CaddyAction, status: String },
    #[error("the Caddy {action} command could not run: {source}")]
    Unavailable {
        action: CaddyAction,
        #[source]
        source: RuntimeError,
    },
}

/// Runs the configured validate and reload commands.
#[derive(Clone)]
pub struct CaddyControl {
    runner: Arc<dyn ProcessRunner>,
    validate: CaddyCommand,
    reload: CaddyCommand,
}

impl CaddyControl {
    #[must_use]
    pub fn new(
        runner: Arc<dyn ProcessRunner>,
        validate: CaddyCommand,
        reload: CaddyCommand,
    ) -> Self {
        Self {
            runner,
            validate,
            reload,
        }
    }

    /// Validates the host Caddyfile, which imports the routes file, then
    /// reloads Caddy. Stops at the first step that fails.
    pub async fn validate_and_reload(&self) -> Result<(), CaddyError> {
        self.run(CaddyAction::Validate).await?;
        self.run(CaddyAction::Reload).await
    }

    /// Reloads after the previous routes were put back. A failure is only
    /// logged: the running Caddy still holds the configuration it had before.
    pub async fn reload_after_rollback(&self) {
        if let Err(error) = self.run(CaddyAction::Reload).await {
            warn!(error = %error, "Caddy reload after restoring the previous routes failed");
        }
    }

    async fn run(&self, action: CaddyAction) -> Result<(), CaddyError> {
        let command = match action {
            CaddyAction::Validate => &self.validate,
            CaddyAction::Reload => &self.reload,
        };
        let output = self
            .runner
            .run(&command.spec(), &NullOutputSink)
            .await
            .map_err(|source| CaddyError::Unavailable { action, source })?;
        let status = output.code.map_or_else(
            || "no exit code because a signal stopped it".to_owned(),
            |code| format!("exit code {code}"),
        );
        if !output.success {
            let lines = output
                .stdout
                .lines()
                .chain(output.stderr.lines())
                .collect::<Vec<_>>();
            let tail = lines[lines.len().saturating_sub(OPERATOR_OUTPUT_LINES)..].join("\n");
            warn!(%action, command = %command, %status, output = %tail, "Caddy refused the routes");
            return Err(CaddyError::Refused { action, status });
        }
        debug!(%action, command = %command, "Caddy accepted the routes");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_on_whitespace() {
        let command = CaddyCommand::from_str("  caddy  validate --config /etc/caddy/Caddyfile ")
            .expect("valid");
        assert_eq!(
            command.to_string(),
            "caddy validate --config /etc/caddy/Caddyfile"
        );
        let spec = command.spec();
        assert_eq!(spec.program, "caddy");
        assert_eq!(spec.args.len(), 3);
        assert_eq!(spec.timeout, Some(COMMAND_TIMEOUT));
    }

    #[test]
    fn the_defaults_parse() {
        assert!(DEFAULT_VALIDATE_COMMAND.parse::<CaddyCommand>().is_ok());
        assert!(DEFAULT_RELOAD_COMMAND.parse::<CaddyCommand>().is_ok());
    }

    #[test]
    fn empty_or_quoted_commands_are_refused() {
        assert!("   ".parse::<CaddyCommand>().is_err());
        assert!("sh -c 'caddy reload'".parse::<CaddyCommand>().is_err());
        assert!("caddy \"validate\"".parse::<CaddyCommand>().is_err());
    }
}
