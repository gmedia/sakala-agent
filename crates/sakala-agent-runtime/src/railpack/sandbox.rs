use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine;
use sakala_agent_core::ports::SecretString;
use serde_json::Value;
use tokio::fs;
use uuid::Uuid;

use crate::{
    CommandSpec, NullOutputSink, ProcessOutputSink, ProcessRunner, ProcessStream, RuntimeError,
    RuntimeReporter, logs::ReporterOutputSink,
};

const MEMORY_LIMIT: &str = "512m";
const CPU_LIMIT: &str = "1";
const PIDS_LIMIT: &str = "256";
const TMPFS: &str = "/tmp:rw,noexec,nosuid,nodev,size=256m,mode=1777";
/// `nobody`. Without capabilities or a Docker socket, the read-only source is the
/// only host path it can reach.
const SANDBOX_USER: &str = "65534:65534";
const APP_DIR: &str = "/app";
const RESULT_FILE: &str = "/tmp/sakala-result.json";
const PLAN_FILE: &str = "railpack-plan.json";
const SANDBOX_TIMEOUT: Duration = Duration::from_secs(300);
/// Lets the deadline inside the container fire before the client gives up.
const CLIENT_TIMEOUT_GRACE: Duration = Duration::from_secs(10);
const RAILPACK_CONFIG_PREFIX: &str = "RAILPACK_";
/// Railpack writes the names it receives into the plan and needs no value to do
/// so, except for its own `RAILPACK_*` settings. Giving it this instead keeps real
/// values out of process arguments, of the plan, and of its output.
const VALUE_PLACEHOLDER: &str = "provided-by-buildkit-secret";
/// Only this node has these images, so only a project's plan naming another
/// project's image could reach them. The Railpack frontend ignores `--pull`.
const LOCAL_IMAGE_PREFIX: &str = "sakala/project-";

/// A floating tag could change what runs on untrusted repositories without anyone
/// reviewing it, so only a digest or a local image ID counts.
#[must_use]
pub fn is_pinned_image(image: &str) -> bool {
    image.contains("@sha256:") || image.starts_with("sha256:")
}

/// Runs Railpack on a repository inside a throwaway container.
///
/// Analysing a repository runs its code: mise renders tool versions from the
/// repository's config, including Tera `exec`, and Railpack follows symlinks when it
/// reads version files. So everything an analysis produces is untrusted too.
pub struct RailpackSandbox {
    runner: Arc<dyn ProcessRunner>,
    image: String,
    network: String,
}

impl RailpackSandbox {
    #[must_use]
    pub fn new(runner: Arc<dyn ProcessRunner>, image: String, network: String) -> Self {
        Self {
            runner,
            image,
            network,
        }
    }

    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Railpack resolves versions over the network, so it cannot run with none. Its
    /// own network has no other members, and containers on it cannot talk to each
    /// other, so it never reaches the runtime network or the router.
    pub async fn ensure_network(&self) -> Result<(), RuntimeError> {
        let inspect = CommandSpec::new("docker")
            .arg("network")
            .arg("inspect")
            .arg(&self.network);
        if self.runner.run(&inspect, &NullOutputSink).await?.success {
            return Ok(());
        }
        let create = CommandSpec::new("docker")
            .arg("network")
            .arg("create")
            .arg("--driver")
            .arg("bridge")
            .arg("--opt")
            .arg("com.docker.network.bridge.enable_icc=false")
            .arg(&self.network);
        let created = self.runner.run(&create, &NullOutputSink).await?;
        if created.success || created.stderr.contains("already exists") {
            Ok(())
        } else {
            Err(RuntimeError::Dependency(format!(
                "the Railpack sandbox network {} could not be created",
                self.network
            )))
        }
    }

    /// Plans a build and returns the path of the plan.
    pub async fn prepare(
        &self,
        context: &Path,
        workspace: &Path,
        environment: &[(String, SecretString)],
        reporter: &dyn RuntimeReporter,
    ) -> Result<PathBuf, RuntimeError> {
        let mut arguments = vec![
            "prepare".into(),
            APP_DIR.into(),
            "--plan-out".into(),
            RESULT_FILE.into(),
        ];
        let mut secrets = Vec::new();
        for (name, value) in environment {
            arguments.push("--env".into());
            if name.starts_with(RAILPACK_CONFIG_PREFIX) {
                // `--env NAME` makes Railpack read the value from its environment.
                arguments.push(name.into());
                secrets.push((name.clone(), value.clone()));
            } else {
                arguments.push(format!("{name}={VALUE_PLACEHOLDER}").into());
            }
        }
        let content = self
            .run("railpack-prepare", context, arguments, secrets, reporter)
            .await?;
        // Read as JSON, because raw bytes could hide the name behind an escape.
        let plan: Value = serde_json::from_slice(&content).map_err(|_| {
            RuntimeError::Execution("Railpack produced an unreadable plan".to_owned())
        })?;
        if mentions_local_image(&plan) {
            return Err(RuntimeError::Build(
                "the Railpack plan refers to an image of another deployment".to_owned(),
            ));
        }

        let directory = workspace.join(format!("railpack-plan-{}", Uuid::new_v4()));
        fs::create_dir(&directory).await?;
        let path = directory.join(PLAN_FILE);
        fs::write(&path, &content).await?;
        Ok(path)
    }

    /// Analyses a repository and returns the bytes of the `railpack info` report.
    pub async fn info(
        &self,
        context: &Path,
        reporter: &dyn RuntimeReporter,
    ) -> Result<Vec<u8>, RuntimeError> {
        let arguments = vec![
            "info".into(),
            "--format".into(),
            "json".into(),
            "--out".into(),
            RESULT_FILE.into(),
            APP_DIR.into(),
        ];
        self.run("railpack-info", context, arguments, Vec::new(), reporter)
            .await
    }

    /// Returns what the sandbox left in `RESULT_FILE`, as its entrypoint printed it.
    async fn run(
        &self,
        phase: &str,
        context: &Path,
        arguments: Vec<OsString>,
        secrets: Vec<(String, SecretString)>,
        reporter: &dyn RuntimeReporter,
    ) -> Result<Vec<u8>, RuntimeError> {
        if !is_pinned_image(&self.image) {
            return Err(RuntimeError::Dependency(
                "SAKALA_RAILPACK_IMAGE must be pinned by digest".to_owned(),
            ));
        }
        self.ensure_network().await?;
        let name = format!("sakala-agent-railpack-{}", Uuid::new_v4());
        let command = self.command(&name, context, arguments, secrets);
        // Killing the `docker run` client leaves its container running, so a
        // cancelled or timed-out run has to remove it as well.
        let guard = RemoveOnDrop {
            runner: Arc::clone(&self.runner),
            name: Some(name),
        };
        let sink = StderrOnly(ReporterOutputSink::new(reporter, phase));
        let output = self.runner.run(&command, &sink).await?;
        guard.disarm();
        if !output.success {
            return Err(RuntimeError::failed_process(
                phase,
                output.code,
                &output.stderr,
            ));
        }
        decode_result(&output.stdout)
    }

    fn command(
        &self,
        name: &str,
        context: &Path,
        arguments: Vec<OsString>,
        secrets: Vec<(String, SecretString)>,
    ) -> CommandSpec {
        let mut command = CommandSpec::new("docker")
            .arg("run")
            .arg("--rm")
            .arg("--name")
            .arg(name)
            .arg("--network")
            .arg(&self.network)
            .arg("--user")
            .arg(SANDBOX_USER)
            .arg("--read-only")
            .arg("--tmpfs")
            .arg(TMPFS)
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--security-opt")
            .arg("no-new-privileges:true")
            .arg("--memory")
            .arg(MEMORY_LIMIT)
            .arg("--memory-swap")
            .arg(MEMORY_LIMIT)
            .arg("--cpus")
            .arg(CPU_LIMIT)
            .arg("--pids-limit")
            .arg(PIDS_LIMIT)
            .arg("--mount")
            .arg(read_only_mount(context, APP_DIR))
            .arg("--env")
            .arg(format!("SANDBOX_RESULT_FILE={RESULT_FILE}"))
            .arg("--env")
            .arg(format!(
                "SANDBOX_TIMEOUT_SECONDS={}",
                SANDBOX_TIMEOUT.as_secs()
            ))
            .timeout(SANDBOX_TIMEOUT + CLIENT_TIMEOUT_GRACE);
        for (name, value) in secrets {
            // `--env NAME` takes the value from the client's environment, so it
            // stays out of process arguments.
            command = command.arg("--env").arg(&name).secret_env(name, value);
        }
        command = command.arg(&self.image).arg("railpack");
        for argument in arguments {
            command = command.arg(argument);
        }
        command
    }
}

fn read_only_mount(source: &Path, target: &str) -> OsString {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(source);
    mount.push(format!(",dst={target},readonly"));
    mount
}

/// The sandbox has no host path it can write to, which also bounds what it can put
/// on the node's disk. The result comes back on stdout, in base64 because a process
/// line is cut at `MAX_LINE_BYTES`. The runner caps stdout at 1 MiB, and JSON that
/// was cut there does not parse. A repository's code can print here too, so what
/// this returns is exactly as untrusted as a file written by the sandbox would be.
fn decode_result(stdout: &str) -> Result<Vec<u8>, RuntimeError> {
    let encoded = stdout
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| RuntimeError::Execution("Railpack produced an unreadable result".to_owned()))
}

fn mentions_local_image(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains(LOCAL_IMAGE_PREFIX),
        Value::Array(items) => items.iter().any(mentions_local_image),
        Value::Object(fields) => fields
            .iter()
            .any(|(name, field)| name.contains(LOCAL_IMAGE_PREFIX) || mentions_local_image(field)),
        _ => false,
    }
}

/// Stdout carries the result, which is not a log. Railpack's own output arrives on
/// stderr, because the image's entrypoint sends it there.
struct StderrOnly<'a>(ReporterOutputSink<'a>);

#[async_trait]
impl ProcessOutputSink for StderrOnly<'_> {
    async fn line(&self, stream: ProcessStream, line: &str) -> Result<(), RuntimeError> {
        match stream {
            ProcessStream::Stdout => Ok(()),
            ProcessStream::Stderr => self.0.line(stream, line).await,
        }
    }
}

struct RemoveOnDrop {
    runner: Arc<dyn ProcessRunner>,
    name: Option<String>,
}

impl RemoveOnDrop {
    fn disarm(mut self) {
        self.name = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let runner = Arc::clone(&self.runner);
        handle.spawn(async move {
            let remove = CommandSpec::new("docker")
                .arg("rm")
                .arg("--force")
                .arg("--volumes")
                .arg(name);
            let _ = runner.run(&remove, &NullOutputSink).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_digest_or_image_id_counts_as_pinned() {
        assert!(is_pinned_image("ghcr.io/gmedia/sakala-railpack@sha256:abc"));
        assert!(is_pinned_image("sha256:abc"));
        assert!(!is_pinned_image("ghcr.io/gmedia/sakala-railpack:v1"));
        assert!(!is_pinned_image("sakala-railpack"));
        assert!(!is_pinned_image(""));
    }

    #[test]
    fn a_result_survives_the_line_wrapping_of_its_encoding() {
        assert_eq!(decode_result("e30K\n").expect("decoded"), b"{}\n");
        assert_eq!(decode_result("e3\n0K\n").expect("decoded"), b"{}\n");
    }

    #[test]
    fn a_result_that_is_not_base64_is_refused() {
        let error = decode_result("root:x:0:0:root\n").expect_err("refused");
        assert!(!error.to_string().contains("root:x"), "{error}");
    }

    #[test]
    fn a_local_image_is_found_however_the_plan_writes_it() {
        for plan in [
            r#"{"steps":[{"inputs":[{"image":"sakala/project-1:abc"}]}]}"#,
            r#"{"steps":[{"inputs":[{"image":"\u0073akala\/project-1:abc"}]}]}"#,
            r#"{"sakala/project-1:abc":true}"#,
            r#"["docker.io/sakala/project-1"]"#,
        ] {
            let plan: Value = serde_json::from_str(plan).expect("json");
            assert!(mentions_local_image(&plan), "{plan}");
        }
        let plan: Value = serde_json::from_str(r#"{"image":"node:24","n":1}"#).expect("json");
        assert!(!mentions_local_image(&plan));
    }
}
