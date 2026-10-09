//! Sequential full builds with publication only after successful native linking.
use std::fs;
use std::path::Path;

use crate::{plan, storage};
use rmake_model::{
    BuildReporter, BuildResult, CommandSpec, Diagnostic, Discovery, IoMode, ProcessRunner,
    ProfileKind, Project, Result, Toolchain,
};

fn ensure_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(Diagnostic::new(
            "RMAKE-O102",
            format!("output path must be a real directory: {}", path.display()),
        ));
    }
    fs::create_dir_all(path).map_err(|error| {
        Diagnostic::new("RMAKE-O101", format!("create {}: {error}", path.display()))
    })
}

fn execute(
    command: &CommandSpec,
    runner: &dyn ProcessRunner,
    reporter: &dyn BuildReporter,
    failure_code: &'static str,
) -> Result<()> {
    reporter.started(command);
    let result = runner.run(command, IoMode::Capture)?;
    reporter.output(&result)?;
    if result.code != 0 {
        return Err(Diagnostic::new(
            failure_code,
            format!(
                "{} failed with exit status {}; executable was not published",
                command.label, result.code
            ),
        ));
    }
    Ok(())
}

pub fn build(
    project: &Project,
    discovery: &Discovery,
    toolchain: &dyn Toolchain,
    runner: &dyn ProcessRunner,
    profile: ProfileKind,
    reporter: &dyn BuildReporter,
) -> Result<BuildResult> {
    let _lock = storage::lock(&project.root)?;
    let output = storage::prepare_owned(&project.root)?;
    let staging = tempfile::Builder::new()
        .prefix(".work-")
        .tempdir_in(&output)
        .map_err(|error| {
            Diagnostic::new(
                "RMAKE-O101",
                format!("create build staging directory: {error}"),
            )
        })?;
    let artifacts = output.join("artifacts");
    ensure_directory(&artifacts)?;
    let profile_dir = artifacts.join(profile.name());
    ensure_directory(&profile_dir)?;
    let final_executable = profile_dir.join(&project.target.name);
    if fs::symlink_metadata(&final_executable)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(Diagnostic::new(
            "RMAKE-O102",
            format!(
                "executable output is not a regular file: {}",
                final_executable.display()
            ),
        ));
    }
    let plan = plan::plan(
        project,
        discovery,
        toolchain,
        profile,
        staging.path(),
        &final_executable,
    )?;
    for action in &plan.compiles {
        let parent = action.output.parent().expect("planned object has a parent");
        fs::create_dir_all(parent).map_err(|error| {
            Diagnostic::new(
                "RMAKE-O101",
                format!("create object directory {}: {error}", parent.display()),
            )
        })?;
        execute(&action.command, runner, reporter, "RMAKE-C101")?;
        if !action.output.is_file() {
            return Err(Diagnostic::new(
                "RMAKE-C102",
                format!(
                    "compiler reported success without producing {}",
                    action.output.display()
                ),
            ));
        }
    }
    execute(&plan.link, runner, reporter, "RMAKE-L101")?;
    if !plan.staged_executable.is_file() {
        return Err(Diagnostic::new(
            "RMAKE-L102",
            "linker reported success without producing an executable",
        ));
    }
    fs::rename(&plan.staged_executable, &plan.final_executable).map_err(|error| {
        Diagnostic::new(
            "RMAKE-O101",
            format!("publish {}: {error}", plan.final_executable.display()),
        )
    })?;
    Ok(BuildResult {
        executable: plan.final_executable,
        compiled: plan.compiles.len(),
    })
}
