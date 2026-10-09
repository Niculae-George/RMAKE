//! The production subprocess implementation.
use std::{
    io::Read,
    process::{Child, Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::Duration,
};

use rmake_model::{
    Cancellation, CommandSpec, Diagnostic, IoMode, ProcessResult, ProcessRunner, Result,
};

pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, spec: &CommandSpec, mode: IoMode) -> Result<ProcessResult> {
        self.run_cancelled(spec, mode, &Cancellation::default())
    }

    fn run_cancelled(
        &self,
        spec: &CommandSpec,
        mode: IoMode,
        cancellation: &Cancellation,
    ) -> Result<ProcessResult> {
        cancellation.check()?;

        let mut command = Command::new(&spec.program);
        command.args(&spec.args).current_dir(&spec.cwd);
        match mode {
            IoMode::Capture => run_captured(command, spec, cancellation),
            IoMode::Inherit => run_inherited(command, spec, cancellation),
        }
    }
}

/// Install the process-wide Ctrl-C hook. The hook only flips the shared token;
/// command polling performs termination and reaping outside the signal handler.
pub fn install_interrupt_handler(cancellation: &Cancellation) -> Result<()> {
    let cancellation = cancellation.clone();
    ctrlc::set_handler(move || cancellation.cancel()).map_err(|error| {
        Diagnostic::new(
            "RMAKE-P101",
            format!("cannot install Ctrl-C handler: {error}"),
        )
    })
}

fn run_captured(
    mut command: Command,
    spec: &CommandSpec,
    cancellation: &Cancellation,
) -> Result<ProcessResult> {
    use std::os::unix::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| spawn_error(spec, error))?;
    let process_group = child.id() as i32;

    let Some(stdout) = child.stdout.take() else {
        terminate_group(process_group);
        let _ = child.wait();
        return Err(Diagnostic::new(
            "RMAKE-P102",
            "captured child stdout pipe was unavailable",
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_group(process_group);
        let _ = child.wait();
        return Err(Diagnostic::new(
            "RMAKE-P102",
            "captured child stderr pipe was unavailable",
        ));
    };

    let stdout_reader = match spawn_reader(stdout, "rmake-stdout") {
        Ok(reader) => reader,
        Err(error) => {
            terminate_group(process_group);
            let _ = child.wait();
            return Err(Diagnostic::new(
                "RMAKE-P102",
                format!("cannot drain stdout for {}: {error}", spec.label),
            ));
        }
    };
    let stderr_reader = match spawn_reader(stderr, "rmake-stderr") {
        Ok(reader) => reader,
        Err(error) => {
            terminate_group(process_group);
            let _ = child.wait();
            let _ = join_reader(stdout_reader, "stdout", spec);
            return Err(Diagnostic::new(
                "RMAKE-P102",
                format!("cannot drain stderr for {}: {error}", spec.label),
            ));
        }
    };

    let status = match wait_until_done(&mut child, cancellation, Some(process_group)) {
        Ok(status) => status,
        Err(error) => {
            terminate_group(process_group);
            let _ = child.wait();
            let _ = join_reader(stdout_reader, "stdout", spec);
            let _ = join_reader(stderr_reader, "stderr", spec);
            return Err(error);
        }
    };

    // A compiler can leave a helper holding either captured pipe open after
    // its parent exits. End the dedicated group before joining drain threads.
    terminate_group(process_group);
    let stdout = join_reader(stdout_reader, "stdout", spec)?;
    let stderr = join_reader(stderr_reader, "stderr", spec)?;
    Ok(ProcessResult {
        code: exit_code(status),
        stdout,
        stderr,
    })
}

fn run_inherited(
    mut command: Command,
    spec: &CommandSpec,
    cancellation: &Cancellation,
) -> Result<ProcessResult> {
    // Keep interactive commands in the foreground process group so terminal
    // input and Ctrl-C continue to follow normal shell semantics.
    let mut child = command.spawn().map_err(|error| spawn_error(spec, error))?;
    let status = wait_until_done(&mut child, cancellation, None)?;
    Ok(ProcessResult {
        code: exit_code(status),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn wait_until_done(
    child: &mut Child,
    cancellation: &Cancellation,
    process_group: Option<i32>,
) -> Result<ExitStatus> {
    loop {
        if cancellation.is_cancelled() {
            if let Some(group) = process_group {
                terminate_group(group);
            } else {
                let _ = child.kill();
            }
            let _ = child.wait();
            return Err(cancelled());
        }

        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                if let Some(group) = process_group {
                    terminate_group(group);
                } else {
                    let _ = child.kill();
                }
                let _ = child.wait();
                return Err(Diagnostic::new(
                    "RMAKE-P102",
                    format!("cannot poll child process: {error}"),
                ));
            }
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    name: &'static str,
) -> std::io::Result<JoinHandle<std::io::Result<Vec<u8>>>> {
    thread::Builder::new().name(name.into()).spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_reader(
    reader: JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
    spec: &CommandSpec,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| {
            Diagnostic::new(
                "RMAKE-P102",
                format!("{stream} reader panicked for {}", spec.label),
            )
        })?
        .map_err(|error| {
            Diagnostic::new(
                "RMAKE-P102",
                format!("cannot read {stream} for {}: {error}", spec.label),
            )
        })
}

#[cfg(unix)]
fn terminate_group(group: i32) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    // ESRCH means the command group has already disappeared. Other failures
    // cannot be surfaced during best-effort cleanup, but the leader is still
    // reaped by the caller.
    let _ = killpg(Pid::from_raw(group), Signal::SIGKILL);
}

fn spawn_error(spec: &CommandSpec, error: std::io::Error) -> Diagnostic {
    Diagnostic::new(
        "RMAKE-P101",
        format!(
            "cannot execute {} ({}): {error}",
            spec.program.display(),
            spec.label
        ),
    )
}

fn cancelled() -> Diagnostic {
    Diagnostic::new("RMAKE-X130", "operation cancelled")
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
