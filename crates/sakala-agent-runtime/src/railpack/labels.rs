//! Writes the Sakala labels onto an image Railpack built.
//!
//! Railpack's BuildKit frontend ignores `--label`: it reads only `build-arg:*`,
//! `platform`, and `filename`, and hands BuildKit an image config of its own
//! (checked against v0.23.0, the pinned release, and against `main` of
//! 2026-11-12). Without labels an image is never reused, never becomes a
//! project's previous image, and is never collected.
//!
//! So a Railpack build is tagged under [`unlabelled_reference`], and the labels
//! are written afterwards by committing a container of that image that never
//! runs. A commit of an unchanged container adds no layer, and Docker builds
//! the new config from the old one, so CMD, ENTRYPOINT, ENV, WORKDIR, USER,
//! EXPOSE, and VOLUME stay as Railpack wrote them. It needs only the daemon: a build
//! `FROM` the local image would depend on the buildx builder seeing Docker's
//! image store, and would pull `docker.io/sakala/project-…` when it does not.
//!
//! The container carries none of the labels, so nothing that looks for Sakala's
//! containers ever sees it; the labels go onto the image through `--change`.

use std::sync::Arc;

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
/// The phase both label commands report under. Its prefix makes a failure a
/// build failure.
const LABEL_PHASE: &str = "railpack-label";

/// The name a Railpack build is tagged with until [`label_image`] moves it,
/// labelled, to `image`. It stays in the project's repository, so image GC
/// still recognises one a crash left behind.
#[must_use]
pub fn unlabelled_reference(image: &str) -> String {
    format!("{image}{UNLABELLED_TAG_SUFFIX}")
}

/// Labels the image under `unlabelled` and names the result `image`. The
/// container and the unlabelled name are removed either way; when a build
/// timeout drops this future, the container still goes, and image GC collects
/// the unlabelled name later. A failure fails the build: an image without
/// labels would break rollback and image GC without a word.
pub(crate) async fn label_image(
    runner: &Arc<dyn ProcessRunner>,
    reporter: &dyn RuntimeReporter,
    unlabelled: &str,
    image: &str,
    labels: &[String],
) -> Result<(), RuntimeError> {
    let container = format!("{LABEL_CONTAINER_PREFIX}{}", Uuid::new_v4());
    let mut cleanup = vec![remove_image_command(unlabelled)];
    let mut guard = None;
    let labelled = match commit_command(&container, image, labels) {
        Ok(commit) => {
            guard = Some(RemoveOnDrop {
                runner: Arc::clone(runner),
                name: Some(container.clone()),
            });
            cleanup.insert(0, remove_container_command(&container));
            let runner = runner.as_ref();
            match run_step(runner, reporter, &create_command(unlabelled, &container)).await {
                Ok(()) => run_step(runner, reporter, &commit).await,
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };

    for command in cleanup {
        match runner.run(&command, &NullOutputSink).await {
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
    if let Some(guard) = guard {
        guard.disarm();
    }
    labelled
}

/// Docker's errors go to the deployment log; the ID it prints on success does
/// not.
async fn run_step(
    runner: &dyn ProcessRunner,
    reporter: &dyn RuntimeReporter,
    command: &CommandSpec,
) -> Result<(), RuntimeError> {
    let sink = StderrOnly(ReporterOutputSink::new(reporter, LABEL_PHASE));
    let output = runner.run(command, &sink).await?;
    if output.success {
        Ok(())
    } else {
        Err(RuntimeError::failed_process(
            LABEL_PHASE,
            output.code,
            &output.stderr,
        ))
    }
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

/// Built before the container is created, so a label that cannot be written
/// fails before anything exists.
fn commit_command(
    container: &str,
    image: &str,
    labels: &[String],
) -> Result<CommandSpec, RuntimeError> {
    let mut command = CommandSpec::new("docker").arg("container").arg("commit");
    for label in labels {
        command = command.arg("--change").arg(label_instruction(label)?);
    }
    Ok(command.arg(container).arg(image))
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
    use std::{sync::Mutex, time::Duration};

    use async_trait::async_trait;
    use sakala_agent_core::ports::RuntimeExecutionError;
    use sakala_agent_protocol::{DeploymentEvent, DeploymentLog};

    use super::*;
    use crate::{ProcessOutput, ProcessOutputSink};

    /// Records every command. A commit never finishes, as when Docker hangs.
    #[derive(Default)]
    struct HangingCommit {
        commands: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl ProcessRunner for HangingCommit {
        async fn run(
            &self,
            spec: &CommandSpec,
            _sink: &dyn ProcessOutputSink,
        ) -> Result<ProcessOutput, RuntimeError> {
            let arguments = arguments(spec);
            let commit = arguments
                .get(1)
                .is_some_and(|argument| argument == "commit");
            self.commands.lock().expect("command lock").push(arguments);
            if commit {
                std::future::pending::<()>().await;
            }
            Ok(ProcessOutput {
                success: true,
                code: Some(0),
                ..ProcessOutput::default()
            })
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
        let runner = Arc::new(HangingCommit::default());
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();
        let labels = ["dev.sakala.managed=true".to_owned()];

        let labelled = tokio::time::timeout(
            Duration::from_millis(50),
            label_image(
                &dyn_runner,
                &NoopReporter,
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
            if removed(&runner.commands.lock().expect("command lock")) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "the container was not removed: {:?}",
            runner.commands.lock().expect("command lock")
        );
    }

    #[tokio::test]
    async fn a_label_that_cannot_be_written_creates_nothing() {
        let runner = Arc::new(HangingCommit::default());
        let dyn_runner: Arc<dyn ProcessRunner> = runner.clone();

        let error = label_image(
            &dyn_runner,
            &NoopReporter,
            "sakala/project-x:t-unlabelled",
            "sakala/project-x:t",
            &["dev.sakala.package-directory=a\nb".to_owned()],
        )
        .await
        .expect_err("refused");

        assert!(matches!(error, RuntimeError::Build(_)), "{error}");
        assert_eq!(
            *runner.commands.lock().expect("command lock"),
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
        let command = commit_command("sakala-label-1", IMAGE, &labels).expect("command");
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
}
