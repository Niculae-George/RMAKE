use std::ffi::OsString;
use std::path::Path;

use rmake_model::{CommandSpec, IoMode, ProcessRunner, Result};

pub fn run(
    executable: &Path,
    root: &Path,
    args: &[OsString],
    runner: &dyn ProcessRunner,
) -> Result<i32> {
    let command = CommandSpec {
        program: executable.to_path_buf(),
        args: args.to_vec(),
        cwd: root.to_path_buf(),
        label: "Run program".into(),
    };
    Ok(runner.run(&command, IoMode::Inherit)?.code)
}
