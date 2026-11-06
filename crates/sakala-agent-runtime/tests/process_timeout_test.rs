use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use sakala_agent_runtime::{
    CommandSpec, NullOutputSink, ProcessOutputSink, ProcessRunner, ProcessStream, RuntimeError,
    TokioProcessRunner, process::MAX_LINE_BYTES,
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn process_runner_terminates_a_timed_out_process_group() {
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let command = CommandSpec::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .timeout(Duration::from_millis(50));
    let started = Instant::now();

    let error = runner
        .run(&command, &NullOutputSink)
        .await
        .expect_err("command should exceed its deadline");

    assert!(matches!(error, RuntimeError::Timeout { .. }));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn process_runner_terminates_a_cancelled_process_group() {
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let cancellation = CancellationToken::new();
    let command = CommandSpec::new("sh")
        .arg("-c")
        .arg("sleep 60")
        .cancellation(cancellation.clone());
    let started = Instant::now();
    let task = tokio::spawn(async move { runner.run(&command, &NullOutputSink).await });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();
    let error = task
        .await
        .expect("process task should join")
        .expect_err("cancelled process should fail");

    assert!(matches!(error, RuntimeError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[derive(Default)]
struct RecordingSink {
    lines: Mutex<Vec<(ProcessStream, String)>>,
}

#[async_trait]
impl ProcessOutputSink for RecordingSink {
    async fn line(&self, stream: ProcessStream, line: &str) -> Result<(), RuntimeError> {
        self.lines
            .lock()
            .expect("line lock")
            .push((stream, line.to_owned()));
        Ok(())
    }
}

#[tokio::test]
async fn output_that_is_not_utf8_does_not_end_the_process_or_lose_later_lines() {
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let sink = RecordingSink::default();
    let command = CommandSpec::new("sh").arg("-c").arg(
        "printf 'before\\n'; printf 'caf\\351 latin1\\n'; printf 'after\\n'; printf 'no newline' >&2",
    );

    let output = runner
        .run(&command, &sink)
        .await
        .expect("bytes a process prints must not fail it");

    assert!(output.success);
    let lines = sink.lines.into_inner().expect("line lock");
    let stdout = lines
        .iter()
        .filter(|(stream, _)| *stream == ProcessStream::Stdout)
        .map(|(_, line)| line.as_str())
        .collect::<Vec<_>>();
    assert_eq!(stdout, ["before", "caf\u{fffd} latin1", "after"]);
    assert!(
        lines
            .iter()
            .any(|(stream, line)| *stream == ProcessStream::Stderr && line == "no newline")
    );
}

#[tokio::test]
async fn crlf_line_endings_are_stripped_like_plain_line_endings() {
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let sink = RecordingSink::default();
    let command = CommandSpec::new("sh")
        .arg("-c")
        .arg("printf 'one\\r\\ntwo\\n'");

    runner.run(&command, &sink).await.expect("command runs");

    let lines = sink.lines.into_inner().expect("line lock");
    assert_eq!(lines[0].1, "one");
    assert_eq!(lines[1].1, "two");
}

#[tokio::test]
async fn sixty_four_mib_without_a_newline_stays_bounded() {
    #[derive(Default)]
    struct LengthSink {
        lengths: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl ProcessOutputSink for LengthSink {
        async fn line(&self, _stream: ProcessStream, line: &str) -> Result<(), RuntimeError> {
            self.lengths.lock().expect("length lock").push(line.len());
            Ok(())
        }
    }

    let runner = TokioProcessRunner::new(Duration::from_secs(60));
    let sink = LengthSink::default();
    let command = CommandSpec::new("sh")
        .arg("-c")
        .arg("head -c 67108864 /dev/zero | tr '\\0' x; printf '\\nafter\\n'");

    let output = runner.run(&command, &sink).await.expect("command runs");

    assert!(output.success);
    let lengths = sink.lengths.into_inner().expect("length lock");
    assert_eq!(
        lengths,
        [MAX_LINE_BYTES, "after".len()],
        "one line cut at the limit, the rest dropped, and the next line intact"
    );
    assert!(output.stdout.len() <= 1024 * 1024);
}

#[tokio::test]
async fn a_line_at_exactly_the_limit_does_not_swallow_the_next_one() {
    let runner = TokioProcessRunner::new(Duration::from_secs(30));
    let sink = RecordingSink::default();
    let command = CommandSpec::new("sh").arg("-c").arg(format!(
        "head -c {MAX_LINE_BYTES} /dev/zero | tr '\\0' x; printf '\\nnext\\n'"
    ));

    runner.run(&command, &sink).await.expect("command runs");

    let lines = sink.lines.into_inner().expect("line lock");
    assert_eq!(lines.last().expect("lines").1, "next");
}
