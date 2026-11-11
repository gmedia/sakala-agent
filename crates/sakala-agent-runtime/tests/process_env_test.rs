//! The agent's token must not reach a child process. Setting the variable needs a
//! process of its own, so the test re-runs this binary with the agent's environment.

use std::{process::Command, time::Duration};

use sakala_agent_runtime::{CommandSpec, NullOutputSink, ProcessRunner, TokioProcessRunner};

const CHILD_MARKER: &str = "SAKALA_ENV_TEST_CHILD";
const TOKEN: &str = "agent-token-value-for-this-test";
/// Docker reads registry credentials from this variable, and it matches `DOCKER_*`.
const DOCKER_AUTH: &str = "docker-auth-config-for-this-test";

/// Does nothing unless it was started by the test below.
#[tokio::test]
async fn child_process_environment_report() {
    if std::env::var_os(CHILD_MARKER).is_none() {
        return;
    }
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let output = runner
        .run(
            &CommandSpec::new("env").env("EXPLICITLY_PASSED", "kept"),
            &NullOutputSink,
        )
        .await
        .expect("env runs");
    print!("CHILD_ENV_START\n{}CHILD_ENV_END\n", output.stdout);
    let output = runner
        .run(
            &CommandSpec::new("env")
                .without_tool_environment()
                .env("DOCKER_CONFIG", "/explicit/config"),
            &NullOutputSink,
        )
        .await
        .expect("env runs");
    print!("TOOLLESS_ENV_START\n{}TOOLLESS_ENV_END\n", output.stdout);
}

fn section<'a>(stdout: &'a str, name: &str) -> &'a str {
    stdout
        .split(&format!("{name}_START\n"))
        .nth(1)
        .and_then(|rest| rest.split(&format!("{name}_END")).next())
        .unwrap_or_else(|| panic!("no {name} report in {stdout}"))
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
        .env("DOCKER_AUTH_CONFIG", DOCKER_AUTH)
        .env("BUILDKIT_HOST", "tcp://kept:1234")
        .output()
        .expect("test binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let report = section(&stdout, "CHILD_ENV");

    assert!(!report.contains(TOKEN), "{report}");
    assert!(!report.contains("SAKALA_"), "{report}");
    assert!(!report.contains("LD_PRELOAD"), "{report}");
    assert!(report.contains("PATH="), "{report}");
    assert!(report.contains("DOCKER_HOST=unix:///kept.sock"), "{report}");
    assert!(report.contains("BUILDX_BUILDER=kept-builder"), "{report}");
    assert!(report.contains("EXPLICITLY_PASSED=kept"), "{report}");

    // A command that must not use the operator's Docker settings, such as the
    // registry lookup of a base image, gets none of them unless it passes them.
    let toolless = section(&stdout, "TOOLLESS_ENV");
    assert!(!toolless.contains(TOKEN), "{toolless}");
    assert!(!toolless.contains(DOCKER_AUTH), "{toolless}");
    assert!(!toolless.contains("SAKALA_"), "{toolless}");
    assert!(!toolless.contains("DOCKER_HOST"), "{toolless}");
    assert!(!toolless.contains("BUILDX_BUILDER"), "{toolless}");
    assert!(!toolless.contains("BUILDKIT_HOST"), "{toolless}");
    assert!(toolless.contains("PATH="), "{toolless}");
    assert!(
        toolless.contains("DOCKER_CONFIG=/explicit/config"),
        "{toolless}"
    );
}
