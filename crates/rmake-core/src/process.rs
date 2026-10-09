//! The only production subprocess implementation.
use std::process::{Command, ExitStatus};

use rmake_model::{CommandSpec, Diagnostic, IoMode, ProcessResult, ProcessRunner, Result};

pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, spec: &CommandSpec, mode: IoMode) -> Result<ProcessResult> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).current_dir(&spec.cwd);
        let spawn_error = |error: std::io::Error| {
            Diagnostic::new(
                "RMAKE-P101",
                format!(
                    "cannot execute {} ({}): {error}",
                    spec.program.display(),
                    spec.label
                ),
            )
        };
        match mode {
            IoMode::Capture => {
                let output = command.output().map_err(spawn_error)?;
                Ok(ProcessResult {
                    code: exit_code(output.status),
                    stdout: output.stdout,
                    stderr: output.stderr,
                })
            }
            IoMode::Inherit => {
                let status = command.status().map_err(spawn_error)?;
                Ok(ProcessResult {
                    code: exit_code(status),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}
