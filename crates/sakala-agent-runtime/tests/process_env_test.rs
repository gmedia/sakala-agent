//! The agent's token must not reach a child process. Setting the variable needs a
//! process of its own, so the test re-runs this binary with the agent's environment.

use std::{process::Command, time::Duration};

use sakala_agent_runtime::{CommandSpec, NullOutputSink, ProcessRunner, TokioProcessRunner};

const CHILD_MARKER: &str = "SAKALA_ENV_TEST_CHILD";
const TOKEN: &str = "agent-token-value-for-this-test";

/// Does nothing unless it was started by the test below.
#[tokio::test]
async fn child_process_environment_report() {
    if std::env::var_os(CHILD_MARKER).is_none() {
        return;
    }
    let output = TokioProcessRunner::new(Duration::from_secs(30))
        .run(
            &CommandSpec::new("env").env("EXPLICITLY_PASSED", "kept"),
            &NullOutputSink,
        )
        .await
        .expect("env runs");
    print!("CHILD_ENV_START\n{}CHILD_ENV_END\n", output.stdout);
}

#[test]
fn spawned_commands_inherit_only_the_allowlist() {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "child_process_environment_report", "--nocapture"])
        .env(CHILD_MARKER, "1")
        .env("SAKALA_AGENT_TOKEN", TOKEN)
        .env("SAKALA_DOCKER_LOOKALIKE", TOKEN)
        .env("DATABASE_PASSWORD", TOKEN)
        .env("LD_PRELOAD", "")
        .env("GIT_DIR", TOKEN)
        .env("MISE_TRUSTED_CONFIG_PATHS", TOKEN)
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .env("DOCKER_HOST", "unix:///kept.sock")
        .env("BUILDX_BUILDER", "kept-builder")
        .output()
        .expect("test binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let report = stdout
        .split("CHILD_ENV_START\n")
        .nth(1)
        .and_then(|rest| rest.split("CHILD_ENV_END").next())
        .unwrap_or_else(|| panic!("no report in {stdout}"));

    assert!(!report.contains(TOKEN), "{report}");
    assert!(!report.contains("SAKALA_"), "{report}");
    assert!(!report.contains("LD_PRELOAD"), "{report}");
    assert!(report.contains("PATH="), "{report}");
    assert!(report.contains("DOCKER_HOST=unix:///kept.sock"), "{report}");
    assert!(report.contains("BUILDX_BUILDER=kept-builder"), "{report}");
    assert!(report.contains("EXPLICITLY_PASSED=kept"), "{report}");
}
