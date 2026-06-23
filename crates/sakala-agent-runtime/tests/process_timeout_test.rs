use std::time::{Duration, Instant};

use sakala_agent_runtime::{
    CommandSpec, NullOutputSink, ProcessRunner, RuntimeError, TokioProcessRunner,
};

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
