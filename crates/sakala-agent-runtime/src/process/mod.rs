mod command;

use crate::{RuntimeError, RuntimeReporter};

pub use command::{
    CommandSpec, NullOutputSink, ProcessOutput, ProcessOutputSink, ProcessRunner, ProcessStream,
    TokioProcessRunner,
};

use crate::logs::ReporterOutputSink;

pub async fn run_checked(
    runner: &dyn ProcessRunner,
    command: &CommandSpec,
    phase: &str,
    reporter: &dyn RuntimeReporter,
) -> Result<ProcessOutput, RuntimeError> {
    let sink = ReporterOutputSink::new(reporter, phase);
    let output = runner.run(command, &sink).await?;
    if !output.success {
        return Err(RuntimeError::failed_process(
            phase,
            output.code,
            &output.stderr,
        ));
    }
    Ok(output)
}
