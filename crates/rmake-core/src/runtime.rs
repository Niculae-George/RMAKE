use std::ffi::OsString;
use std::path::Path;

use rmake_model::{Cancellation, CommandSpec, IoMode, ProcessResult, ProcessRunner, Result};

pub fn run(
    executable: &Path,
    root: &Path,
    args: &[OsString],
    runner: &dyn ProcessRunner,
) -> Result<i32> {
    run_cancelled(executable, root, args, runner, &Cancellation::default())
}

/// Run an interactive program while sharing the build command's cancellation token.
pub fn run_cancelled(
    executable: &Path,
    root: &Path,
    args: &[OsString],
    runner: &dyn ProcessRunner,
    cancellation: &Cancellation,
) -> Result<i32> {
    let command = CommandSpec {
        program: executable.to_path_buf(),
        args: args.to_vec(),
        cwd: root.to_path_buf(),
        label: "Run program".into(),
    };
    Ok(runner
        .run_cancelled(&command, IoMode::Inherit, cancellation)?
        .code)
}

/// Run a test executable with captured output for aggregate test reporting.
pub fn test_cancelled(
    executable: &Path,
    root: &Path,
    args: &[OsString],
    runner: &dyn ProcessRunner,
    cancellation: &Cancellation,
) -> Result<ProcessResult> {
    let command = CommandSpec {
        program: executable.to_path_buf(),
        args: args.to_vec(),
        cwd: root.to_path_buf(),
        label: "Run test".into(),
    };
    runner.run_cancelled(&command, IoMode::Capture, cancellation)
}
