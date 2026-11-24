//! Writes the Sakala labels onto an image Railpack built.
//!
//! Railpack's BuildKit frontend ignores `--label`: it reads only `build-arg:*`,
//! `platform`, and `filename`, and hands BuildKit an image config of its own
//! (checked against v0.23.0, the pinned release, and against `main` of
//! 2026-11-12). Without labels an image is never reused, never becomes a
//! project's previous image, and is never collected.
//!
//! So a Railpack build is tagged under [`unlabelled_reference`], and the labels
//! are written afterwards. With Docker's containerd image store, the image
//! store the Agent targets, they are written by a build of nothing but
//! `FROM <unlabelled>@<digest>` with `--label`, on the `default` builder: that
//! builder is the daemon's own BuildKit, which reads the image store, and the
//! build only writes a new config and manifest over the same layers, in well
//! under a second at any image size. The digest is the one the image store
//! holds for the unlabelled name, so the build cannot resolve to anything but
//! the image this node just built: if the local lookup misses, BuildKit asks
//! Docker Hub for that exact digest, which fails or returns the same bytes.
//!
//! Without the containerd image store an image has no manifest digest to pin,
//! and the labels are written as before, by committing a container of the
//! image that never runs. With the containerd image store that commit is what
//! made the step slow: it took 17 to 37 seconds for a 526 MB image on staging,
//! growing with the image, although it adds no layer. Either way CMD,
//! ENTRYPOINT, ENV, WORKDIR, USER, EXPOSE, and VOLUME stay as Railpack wrote
//! them, and no layer is added.
//!
//! Neither the build nor the container carries the labels as its own, so
//! nothing that looks for Sakala's containers ever sees them; the labels go
//! onto the image only.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use sakala_agent_protocol::{DeploymentLog, LogStream};
use time::OffsetDateTime;
use tokio::fs;
use tracing::warn;
use uuid::Uuid;

use super::sandbox::{RemoveOnDrop, StderrOnly};
use crate::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, RuntimeReporter,
    logs::ReporterOutputSink,
};

/// Appended to the tag of a Railpack image until it carries its labels.
const UNLABELLED_TAG_SUFFIX: &str = "-unlabelled";
/// Names the container the labels are committed from, so an operator can tell
/// what one left behind by a crash is.
const LABEL_CONTAINER_PREFIX: &str = "sakala-label-";
/// Names the workspace directory that holds the Dockerfile of the label build.
const LABEL_BUILD_DIRECTORY_PREFIX: &str = "railpack-label-";
/// The buildx builder that is the daemon's own BuildKit, the only one that
/// reads Docker's image store. `--builder` overrides `BUILDX_BUILDER` and the
/// builder `docker buildx use` picked, which may be a `docker-container` one.
const DAEMON_BUILDER: &str = "default";
/// The phase every label command reports under. Its prefix makes a failure a
/// build failure.
const LABEL_PHASE: &str = "railpack-label";
/// The digest algorithm Docker's image store names manifests with.
const DIGEST_ALGORITHM: &str = "sha256:";
const DIGEST_HEX_LENGTH: usize = 64;

/// The name a Railpack build is tagged with until [`label_image`] moves it,
/// labelled, to `image`. It stays in the project's repository, so image GC
/// still recognises one a crash left behind.
#[must_use]
pub fn unlabelled_reference(image: &str) -> String {
    format!("{image}{UNLABELLED_TAG_SUFFIX}")
}

/// Labels the image under `unlabelled` and names the result `image`. The
/// unlabelled name is removed either way, and so is the container of a commit;
/// when a build timeout drops this future, that container still goes, and
/// image GC collects the unlabelled name later. A failure fails the build: an
/// image without labels would break rollback and image GC without a word.
/// The deployment log gets one line when the step starts and one with its
/// duration when it succeeds, so a slow daemon does not look like a hang.
pub(crate) async fn label_image(
    runner: &Arc<dyn ProcessRunner>,
    reporter: &dyn RuntimeReporter,
    workspace: &Path,
    unlabelled: &str,
    image: &str,
    labels: &[String],
) -> Result<(), RuntimeError> {
    let started = Instant::now();
    report(
        reporter,
        "Writing the Sakala labels onto the image.".to_owned(),
    )
    .await;
    let labelled = write_labels(runner, reporter, workspace, unlabelled, image, labels).await;
    cleanup(runner.as_ref(), &remove_image_command(unlabelled)).await;
    if labelled.is_ok() {
        report(reporter, labelled_message(started.elapsed())).await;
    }
    labelled
}

async fn write_labels(
    runner: &Arc<dyn ProcessRunner>,
    reporter: &dyn RuntimeReporter,
    workspace: &Path,
    unlabelled: &str,
    image: &str,
    labels: &[String],
) -> Result<(), RuntimeError> {
    // Built before anything else, so a label that cannot be written fails the
    // same way on every image store, before anything exists.
    let commit = commit_command(image, labels)?;
    let inspected = run_step(runner.as_ref(), reporter, &inspect_command(unlabelled)).await?;
    match manifest_digest(&inspected) {
        Some(digest) => {
            let dockerfile = write_label_dockerfile(workspace, unlabelled, &digest).await?;
            run_step(
                runner.as_ref(),
                reporter,
                &label_build_command(&dockerfile, image, labels),
            )
            .await
            .map(drop)
        }
        None => commit_labels(runner, reporter, unlabelled, commit).await,
    }
}

/// Creates a container of the image and commits it with the labels. The
/// container goes whether the commit succeeds, fails, or is dropped.
async fn commit_labels(
    runner: &Arc<dyn ProcessRunner>,
    reporter: &dyn RuntimeReporter,
    unlabelled: &str,
    commit: CommitCommand,
) -> Result<(), RuntimeError> {
    let container = format!("{LABEL_CONTAINER_PREFIX}{}", Uuid::new_v4());
    let guard = RemoveOnDrop {
        runner: Arc::clone(runner),
        name: Some(container.clone()),
    };
    let committed = match run_step(
        runner.as_ref(),
        reporter,
        &create_command(unlabelled, &container),
    )
    .await
    {
        Ok(_) => run_step(runner.as_ref(), reporter, &commit.of(&container))
            .await
            .map(drop),
        Err(error) => Err(error),
    };
    cleanup(runner.as_ref(), &remove_container_command(&container)).await;
    guard.disarm();
    committed
}

async fn cleanup(runner: &dyn ProcessRunner, command: &CommandSpec) {
    match runner.run(command, &NullOutputSink).await {
        Ok(output) if output.success => {}
        Ok(output) => warn!(
            command = ?command.args,
            stderr = %output.stderr.trim(),
            "cleanup after labelling the Railpack image failed"
        ),
        Err(error) => warn!(
            command = ?command.args,
            %error,
            "cleanup after labelling the Railpack image failed"
        ),
    }
}

/// Logs are telemetry: a line that cannot be delivered does not fail the build.
async fn report(reporter: &dyn RuntimeReporter, message: String) {
    let _ = reporter
        .log(DeploymentLog {
            stream: LogStream::System,
            message: format!("[{LABEL_PHASE}] {message}"),
            recorded_at: OffsetDateTime::now_utc(),
            process: None,
        })
        .await;
}

fn labelled_message(elapsed: Duration) -> String {
    format!("Image labelled in {:.1} s.", elapsed.as_secs_f64())
}

/// Docker's errors go to the deployment log; what it prints on stdout does
/// not, and is returned for the caller to read.
async fn run_step(
    runner: &dyn ProcessRunner,
    reporter: &dyn RuntimeReporter,
    command: &CommandSpec,
) -> Result<String, RuntimeError> {
    let sink = StderrOnly(ReporterOutputSink::new(reporter, LABEL_PHASE));
    let output = runner.run(command, &sink).await?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(RuntimeError::failed_process(
            LABEL_PHASE,
            output.code,
            &output.stderr,
        ))
    }
}

/// The whole inspect document. A template that names `.Descriptor` fails the
/// command on an image store without one, since Docker formats image inspect
/// with `missingkey=error`.
fn inspect_command(unlabelled: &str) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("image")
        .arg("inspect")
        .arg("--format")
        .arg("{{json .}}")
        .arg(unlabelled)
}

/// The digest of the manifest the containerd image store holds for the image,
/// from `docker image inspect --format '{{json .}}'`. `None` on the classic
/// image store, which has no `Descriptor`, and for output it cannot read, so
/// the slower commit labels the image instead.
fn manifest_digest(inspected: &str) -> Option<String> {
    let document = match serde_json::from_str::<serde_json::Value>(inspected.trim()) {
        Ok(document) => document,
        Err(error) => {
            warn!(%error, "unreadable image inspect output; labelling by commit");
            return None;
        }
    };
    let digest = document.get("Descriptor")?.get("digest")?.as_str()?;
    let hex = digest.strip_prefix(DIGEST_ALGORITHM)?;
    let valid = hex.len() == DIGEST_HEX_LENGTH
        && hex
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character));
    valid.then(|| digest.to_owned())
}

/// The Dockerfile of the label build, alone in its own directory of the
/// workspace, which is also the build context. It has no parser directive, so
/// the daemon's built-in Dockerfile frontend reads it.
async fn write_label_dockerfile(
    workspace: &Path,
    unlabelled: &str,
    digest: &str,
) -> Result<std::path::PathBuf, RuntimeError> {
    let directory = workspace.join(format!("{LABEL_BUILD_DIRECTORY_PREFIX}{}", Uuid::new_v4()));
    fs::create_dir(&directory).await?;
    let dockerfile = directory.join("Dockerfile");
    fs::write(&dockerfile, label_dockerfile(unlabelled, digest)).await?;
    Ok(dockerfile)
}

fn label_dockerfile(unlabelled: &str, digest: &str) -> String {
    format!("FROM {unlabelled}@{digest}\n")
}

/// Changes only the config: no step runs, so `--network none` and the absent
/// secrets are a formality, and nothing is cached that another project could
/// read. `--pull=false` lets the base resolve from the image store; the digest
/// keeps it to this image. Attestations would make the image an index and add
/// nothing a node uses. `--quiet` keeps BuildKit's progress out of the
/// deployment log; errors still reach it.
fn label_build_command(dockerfile: &Path, image: &str, labels: &[String]) -> CommandSpec {
    let context = dockerfile.parent().unwrap_or(dockerfile);
    let command = CommandSpec::new("docker")
        .arg("buildx")
        .arg("build")
        .arg("--builder")
        .arg(DAEMON_BUILDER)
        .arg("--pull=false")
        .arg("--provenance=false")
        .arg("--sbom=false")
        .arg("--network")
        .arg("none")
        .arg("--quiet");
    crate::builders::with_image_labels(command, labels)
        .arg("--file")
        .arg(dockerfile.as_os_str())
        .arg("--tag")
        .arg(image)
        .arg(context.as_os_str())
}

/// A container of the image that is never started, so `commit` has nothing
/// to pause. `--pull never` keeps it to the image this node just built, and
/// `--network none` keeps it off every network.
fn create_command(unlabelled: &str, container: &str) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("container")
        .arg("create")
        .arg("--pull")
        .arg("never")
        .arg("--network")
        .arg("none")
        .arg("--name")
        .arg(container)
        .arg(unlabelled)
}

/// A commit of the labels onto `image`, short of the container it commits.
struct CommitCommand {
    image: String,
    changes: Vec<String>,
}

impl CommitCommand {
    fn of(&self, container: &str) -> CommandSpec {
        let mut command = CommandSpec::new("docker").arg("container").arg("commit");
        for change in &self.changes {
            command = command.arg("--change").arg(change);
        }
        command.arg(container).arg(&self.image)
    }
}

/// Every label as the `--change` of a commit. A label that cannot be written
/// as one is refused here, whichever way the image is then labelled.
fn commit_command(image: &str, labels: &[String]) -> Result<CommitCommand, RuntimeError> {
    Ok(CommitCommand {
        image: image.to_owned(),
        changes: labels
            .iter()
            .map(|label| label_instruction(label))
            .collect::<Result<_, _>>()?,
    })
}

/// `--volumes` also removes the anonymous volumes a `VOLUME` of the image made
/// at create.
fn remove_container_command(container: &str) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("container")
        .arg("rm")
        .arg("--force")
        .arg("--volumes")
        .arg(container)
}

/// Untags the unlabelled image. Docker keeps it, untagged, as the parent of
/// the labelled one and deletes it together with that image; its layers are
/// the labelled image's anyway.
fn remove_image_command(unlabelled: &str) -> CommandSpec {
    CommandSpec::new("docker")
        .arg("image")
        .arg("rm")
        .arg(unlabelled)
}

/// `LABEL key="value"` for a `key=value` label. Docker reads `--change` as a
/// Dockerfile line and expands variables in it, so the value is quoted with
/// `\`, `"`, and `$` escaped. A line break would end the instruction and start
/// another one, so a value with a control character is refused.
fn label_instruction(label: &str) -> Result<String, RuntimeError> {
    let invalid = || RuntimeError::Build(format!("image label {label:?} cannot be written"));
    let (key, value) = label.split_once('=').ok_or_else(invalid)?;
    let key_valid = !key.is_empty()
        && key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character));
    if !key_valid || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '$') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Ok(format!("LABEL {key}=\"{escaped}\""))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use sakala_agent_core::ports::RuntimeExecutionError;
    use sakala_agent_protocol::DeploymentEvent;

    use super::*;
    use crate::{ProcessOutput, ProcessOutputSink};

    /// Records every command. `inspect` prints `inspected`; a commit never
    /// finishes when `hang_on_commit` is set, as when Docker hangs; a build
    /// fails with `build_error` when it is set.
    #[derive(Default)]
    struct Docker {
        commands: Mutex<Vec<Vec<String>>>,
        inspected: String,
        hang_on_commit: bool,
        build_error: Option<&'static str>,
    }

    impl Docker {
        fn hanging_commit() -> Self {
            Self {
                hang_on_commit: true,
                ..Self::default()
            }
        }

        fn containerd_store() -> Self {
            Self {
                inspected: CONTAINERD_INSPECT.to_owned(),
                ..Self::default()
            }
        }

        fn commands(&self) -> Vec<Vec<String>> {
            self.commands.lock().expect("command lock").clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for Docker {
        async fn run(
            &self,
            spec: &CommandSpec,
            sink: &dyn ProcessOutputSink,
        ) -> Result<ProcessOutput, RuntimeError> {
            let arguments = arguments(spec);
            let subcommand = arguments.get(1).cloned().unwrap_or_default();
            self.commands.lock().expect("command lock").push(arguments);
            match subcommand.as_str() {
                "commit" if self.hang_on_commit => std::future::pending::<()>().await,
                "build" => {
                    if let Some(error) = self.build_error {
                        sink.line(crate::ProcessStream::Stderr, error).await?;
                        return Ok(ProcessOutput {
                            success: false,
                            code: Some(1),
                            stderr: error.to_owned(),
                            ..ProcessOutput::default()
                        });
                    }
                }
                "inspect" => {
                    return Ok(ProcessOutput {
                        success: true,
                        code: Some(0),
                        stdout: format!("{}\n", self.inspected),
                        ..ProcessOutput::default()
                    });
                }
                _ => {}
            }
            Ok(ProcessOutput {
                success: true,
                code: Some(0),
                ..ProcessOutput::default()
            })
        }
    }

    /// Shaped like what Docker 29.10 with the containerd image store prints
    /// for a Railpack image, cut down.
    const CONTAINERD_INSPECT: &str = r#"{"Id":"sha256:cf5b881311537a7813fa2264161560073aad45d5999c46eeb9210933841a0df7","Descriptor":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:cf5b881311537a7813fa2264161560073aad45d5999c46eeb9210933841a0df7","size":1442},"Config":{"Cmd":["echo hi"]}}"#;
    const DIGEST: &str = "sha256:cf5b881311537a7813fa2264161560073aad45d5999c46eeb9210933841a0df7";

    /// Shaped like the classic image store: no `Descriptor`.
    const CLASSIC_INSPECT: &str = r#"{"Id":"sha256:cf5b881311537a7813fa2264161560073aad45d5999c46eeb9210933841a0df7","Config":{"Cmd":["echo hi"]}}"#;

    #[derive(Default)]
    struct RecordingReporter {
        lines: Mutex<Vec<(LogStream, String)>>,
    }

    #[async_trait]
    impl RuntimeReporter for RecordingReporter {
        async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        async fn log(&self, log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
            self.lines
                .lock()
                .expect("log lock")
                .push((log.stream, log.message));
            Ok(())
        }
    }

    struct NoopReporter;

    #[async_trait]
    impl RuntimeReporter for NoopReporter {
        async fn event(&self, _event: DeploymentEvent) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }

        async fn log(&self, _log: DeploymentLog) -> Result<(), RuntimeExecutionError> {
            Ok(())
        }
    }

    /// A build timeout drops the future in the middle of the commit. The
    /// container must not stay: it would hold the unlabelled image for good,
    /// since image GC keeps every image a container uses.
    #[tokio::test]
    async fn a_timeout_during_the_commit_still_removes_the_container() {
        let runner = Arc::new(Docker::hanging_commit());
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();
        let labels = ["dev.sakala.managed=true".to_owned()];

        let labelled = tokio::time::timeout(
            Duration::from_millis(50),
            label_image(
                &dyn_runner,
                &NoopReporter,
                Path::new("/nonexistent"),
                "sakala/project-x:t-unlabelled",
                "sakala/project-x:t",
                &labels,
            ),
        )
        .await;
        assert!(labelled.is_err(), "the commit never finishes");

        let removed = |commands: &[Vec<String>]| {
            commands.iter().any(|command| {
                command[..3] == ["rm", "--force", "--volumes"]
                    && command[3].starts_with(LABEL_CONTAINER_PREFIX)
            })
        };
        for _ in 0..100 {
            if removed(&runner.commands()) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the container was not removed: {:?}", runner.commands());
    }

    #[tokio::test]
    async fn a_label_that_cannot_be_written_creates_nothing() {
        let runner = Arc::new(Docker::containerd_store());
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();

        let error = label_image(
            &dyn_runner,
            &NoopReporter,
            Path::new("/nonexistent"),
            "sakala/project-x:t-unlabelled",
            "sakala/project-x:t",
            &["dev.sakala.package-directory=a\nb".to_owned()],
        )
        .await
        .expect_err("refused");

        assert!(matches!(error, RuntimeError::Build(_)), "{error}");
        assert_eq!(
            runner.commands(),
            [["image", "rm", "sakala/project-x:t-unlabelled"]]
        );
    }

    fn arguments(command: &CommandSpec) -> Vec<String> {
        command
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    const IMAGE: &str = "sakala/project-ff66ed4a-6303-4be6-8ef4-63c28b112680:0123456789ab-4f1f21ef";

    #[test]
    fn the_unlabelled_name_stays_in_the_project_repository() {
        assert_eq!(
            unlabelled_reference(IMAGE),
            "sakala/project-ff66ed4a-6303-4be6-8ef4-63c28b112680:0123456789ab-4f1f21ef-unlabelled"
        );
    }

    #[test]
    fn the_container_is_never_pulled_networked_or_labelled() {
        let command = create_command("sakala/project-x:t-unlabelled", "sakala-label-1");
        assert_eq!(command.program, "docker");
        assert_eq!(
            arguments(&command),
            [
                "container",
                "create",
                "--pull",
                "never",
                "--network",
                "none",
                "--name",
                "sakala-label-1",
                "sakala/project-x:t-unlabelled",
            ]
        );
    }

    #[test]
    fn every_label_becomes_one_change_of_the_commit() {
        let labels = [
            "dev.sakala.managed=true".to_owned(),
            "dev.sakala.project-id=ff66ed4a-6303-4be6-8ef4-63c28b112680".to_owned(),
            "dev.sakala.deployment-id=4f1f21ef-730d-42d5-a46d-d965353cb993".to_owned(),
            "dev.sakala.builder=railpack".to_owned(),
            "dev.sakala.package-directory=apps/web".to_owned(),
        ];
        let command = commit_command(IMAGE, &labels)
            .expect("command")
            .of("sakala-label-1");
        assert_eq!(
            arguments(&command),
            [
                "container",
                "commit",
                "--change",
                "LABEL dev.sakala.managed=\"true\"",
                "--change",
                "LABEL dev.sakala.project-id=\"ff66ed4a-6303-4be6-8ef4-63c28b112680\"",
                "--change",
                "LABEL dev.sakala.deployment-id=\"4f1f21ef-730d-42d5-a46d-d965353cb993\"",
                "--change",
                "LABEL dev.sakala.builder=\"railpack\"",
                "--change",
                "LABEL dev.sakala.package-directory=\"apps/web\"",
                "sakala-label-1",
                IMAGE,
            ]
        );
    }

    #[test]
    fn a_value_is_quoted_so_docker_neither_splits_nor_expands_it() {
        assert_eq!(
            label_instruction(r#"dev.sakala.package-directory=apps/my "web" $HOME\x"#)
                .expect("instruction"),
            r#"LABEL dev.sakala.package-directory="apps/my \"web\" \$HOME\\x""#
        );
        assert_eq!(
            label_instruction("dev.sakala.package-directory=a=b").expect("instruction"),
            "LABEL dev.sakala.package-directory=\"a=b\""
        );
    }

    #[test]
    fn a_label_that_would_end_the_instruction_is_refused() {
        for label in [
            "dev.sakala.package-directory=apps/web\nCMD [\"sh\"]",
            "dev.sakala.package-directory=apps/web\r",
            "dev.sakala.package-directory=\t",
            "no-separator",
            "=value",
            "bad key=value",
            "key\"=value",
        ] {
            assert!(
                matches!(label_instruction(label), Err(RuntimeError::Build(_))),
                "{label:?}"
            );
        }
    }

    #[test]
    fn cleanup_removes_the_container_with_its_volumes_and_only_the_unlabelled_name() {
        assert_eq!(
            arguments(&remove_container_command("sakala-label-1")),
            ["container", "rm", "--force", "--volumes", "sakala-label-1"]
        );
        assert_eq!(
            arguments(&remove_image_command("sakala/project-x:t-unlabelled")),
            ["image", "rm", "sakala/project-x:t-unlabelled"]
        );
    }

    const UNLABELLED: &str = "sakala/project-x:t-unlabelled";
    const LABELLED: &str = "sakala/project-x:t";

    /// With the containerd image store the labels come from a build of the
    /// pinned image: no container is created, and the unlabelled name goes.
    #[tokio::test]
    async fn the_containerd_image_store_labels_by_a_build_of_the_pinned_image() {
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = Arc::new(Docker::containerd_store());
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();
        let reporter = RecordingReporter::default();

        label_image(
            &dyn_runner,
            &reporter,
            workspace.path(),
            UNLABELLED,
            LABELLED,
            &["dev.sakala.managed=true".to_owned()],
        )
        .await
        .expect("labelled");

        let commands = runner.commands();
        let subcommands: Vec<_> = commands
            .iter()
            .map(|command| command[..2].join(" "))
            .collect();
        assert_eq!(subcommands, ["image inspect", "buildx build", "image rm"]);
        let build = &commands[1];
        let dockerfile = std::path::PathBuf::from(&build[build.len() - 4]);
        assert!(dockerfile.starts_with(workspace.path()), "{build:?}");
        assert_eq!(
            std::fs::read_to_string(&dockerfile).expect("Dockerfile"),
            format!("FROM {UNLABELLED}@{DIGEST}\n")
        );
        assert_eq!(commands[2], ["image", "rm", UNLABELLED]);

        let lines = reporter.lines.lock().expect("log lock").clone();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            (
                LogStream::System,
                "[railpack-label] Writing the Sakala labels onto the image.".to_owned()
            )
        );
        assert_eq!(lines[1].0, LogStream::System);
        assert!(
            lines[1]
                .1
                .starts_with("[railpack-label] Image labelled in "),
            "{lines:?}"
        );
    }

    /// The classic image store has no manifest digest to pin, so the image is
    /// labelled by a commit, and the container goes afterwards.
    #[tokio::test]
    async fn the_classic_image_store_labels_by_a_commit() {
        let runner = Arc::new(Docker {
            inspected: CLASSIC_INSPECT.to_owned(),
            ..Docker::default()
        });
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();

        label_image(
            &dyn_runner,
            &NoopReporter,
            Path::new("/nonexistent"),
            UNLABELLED,
            LABELLED,
            &["dev.sakala.managed=true".to_owned()],
        )
        .await
        .expect("labelled");

        let commands = runner.commands();
        let subcommands: Vec<_> = commands
            .iter()
            .map(|command| command[..2].join(" "))
            .collect();
        assert_eq!(
            subcommands,
            [
                "image inspect",
                "container create",
                "container commit",
                "container rm",
                "image rm"
            ]
        );
    }

    /// A failed build fails the step with Docker's error in the deployment
    /// log, without the line that reports success, and still removes the
    /// unlabelled name.
    #[tokio::test]
    async fn a_failed_label_build_fails_with_dockers_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = Arc::new(Docker {
            build_error: Some("ERROR: failed to build: pull access denied"),
            ..Docker::containerd_store()
        });
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();
        let reporter = RecordingReporter::default();

        let error = label_image(
            &dyn_runner,
            &reporter,
            workspace.path(),
            UNLABELLED,
            LABELLED,
            &["dev.sakala.managed=true".to_owned()],
        )
        .await
        .expect_err("failed");

        assert!(error.to_string().contains("railpack-label"), "{error}");
        let lines = reporter.lines.lock().expect("log lock").clone();
        assert_eq!(
            lines[1..],
            [(
                LogStream::Stderr,
                "[railpack-label] ERROR: failed to build: pull access denied".to_owned()
            )]
        );
        assert_eq!(
            runner.commands().last().expect("command"),
            &["image", "rm", UNLABELLED]
        );
    }

    #[test]
    fn only_a_sha256_manifest_digest_is_pinned() {
        assert_eq!(manifest_digest(CONTAINERD_INSPECT).as_deref(), Some(DIGEST));
        for inspected in [
            CLASSIC_INSPECT,
            "",
            "not json",
            r#"{"Descriptor":null}"#,
            r#"{"Descriptor":{"digest":"sha512:abc"}}"#,
            r#"{"Descriptor":{"digest":"sha256:CF5B881311537A7813FA2264161560073AAD45D5999C46EEB9210933841A0DF7"}}"#,
            r#"{"Descriptor":{"digest":"sha256:cf5b\n"}}"#,
        ] {
            assert_eq!(manifest_digest(inspected), None, "{inspected:?}");
        }
    }

    #[test]
    fn the_label_build_uses_the_daemons_buildkit_and_never_runs_or_attests_anything() {
        let labels = [
            "dev.sakala.managed=true".to_owned(),
            "dev.sakala.package-directory=apps/my \"web\" $HOME".to_owned(),
        ];
        let command = label_build_command(
            Path::new("/workspace/railpack-label-1/Dockerfile"),
            IMAGE,
            &labels,
        );
        assert_eq!(command.program, "docker");
        assert_eq!(
            arguments(&command),
            [
                "buildx",
                "build",
                "--builder",
                "default",
                "--pull=false",
                "--provenance=false",
                "--sbom=false",
                "--network",
                "none",
                "--quiet",
                "--label",
                "dev.sakala.managed=true",
                "--label",
                "dev.sakala.package-directory=apps/my \"web\" $HOME",
                "--file",
                "/workspace/railpack-label-1/Dockerfile",
                "--tag",
                IMAGE,
                "/workspace/railpack-label-1",
            ]
        );
    }

    #[test]
    fn the_inspect_template_names_no_field() {
        assert_eq!(
            arguments(&inspect_command(UNLABELLED)),
            ["image", "inspect", "--format", "{{json .}}", UNLABELLED]
        );
    }

    #[test]
    fn the_duration_is_reported_in_tenths_of_a_second() {
        assert_eq!(
            labelled_message(Duration::from_millis(540)),
            "Image labelled in 0.5 s."
        );
    }
}
