//! Action execution and atomic publication inside verified RMake-owned storage.
use crate::{depfile, fingerprint, plan, scheduler, state::ActionState, storage};
use rmake_model::{
    ActionKind, Archiver, BuildAction, BuildOptions, BuildReporter, BuildResult, Diagnostic,
    Discovery, IoMode, ProcessRunner, Project, Result, Toolchain,
};
use std::{fs, path::Path};

pub(crate) fn ensure_directory(path: &Path, owned: &Path) -> Result<()> {
    if !path.starts_with(owned) {
        return Err(Diagnostic::new(
            "RMAKE-O102",
            "output escaped owned directory",
        ));
    }
    let mut current = owned.to_path_buf();
    for part in path
        .strip_prefix(owned)
        .expect("checked prefix")
        .components()
    {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(m) if !m.is_dir() || m.file_type().is_symlink() => {
                return Err(Diagnostic::new(
                    "RMAKE-O102",
                    format!(
                        "output directory is not a real directory: {}",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = fs::create_dir(&current)
                    && e.kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(Diagnostic::new("RMAKE-O101", e.to_string()));
                }
                let m = fs::symlink_metadata(&current)
                    .map_err(|e| Diagnostic::new("RMAKE-O101", e.to_string()))?;
                if !m.is_dir() || m.file_type().is_symlink() {
                    return Err(Diagnostic::new(
                        "RMAKE-O102",
                        "output directory changed during creation",
                    ));
                }
            }
            Err(e) => return Err(Diagnostic::new("RMAKE-O101", e.to_string())),
        }
    }
    Ok(())
}
fn regular_output(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err(Diagnostic::new(
            "RMAKE-O102",
            format!("output must be a regular file: {}", path.display()),
        ));
    }
    Ok(())
}
pub(crate) fn perform(
    action: &BuildAction,
    owned: &Path,
    runner: &dyn ProcessRunner,
    options: &BuildOptions,
    reporter: &dyn BuildReporter,
) -> Result<()> {
    options.cancellation.check()?;
    ensure_directory(action.output.parent().expect("output parent"), owned)?;
    regular_output(&action.output)?;
    let temp = tempfile::Builder::new()
        .prefix(".action-")
        .tempdir_in(owned)
        .map_err(|e| Diagnostic::new("RMAKE-O101", e.to_string()))?;
    let staged = temp.path().join("output");
    let staged_dep = temp.path().join("dependencies.d");
    let mut command = action.command.clone();
    for arg in &mut command.args {
        if arg == action.output.as_os_str() {
            *arg = staged.clone().into_os_string();
        }
        if let ActionKind::Compile { depfile, .. } = &action.kind
            && arg == depfile.as_os_str()
        {
            *arg = staged_dep.clone().into_os_string();
        }
    }
    let inputs = fingerprint::observed_inputs(action);
    let before = fingerprint::input_snapshot(&inputs)?;
    reporter.started(&command);
    let result = runner.run_cancelled(&command, IoMode::Capture, &options.cancellation)?;
    reporter.output(&result)?;
    options.cancellation.check()?;
    if result.code != 0 {
        return Err(Diagnostic::new(
            "RMAKE-B101",
            format!("{} failed with status {}", command.label, result.code),
        ));
    }
    if before != fingerprint::input_snapshot(&inputs)? {
        return Err(Diagnostic::new(
            "RMAKE-B103",
            "an input changed during the native action; retry the build",
        ));
    }
    if !fs::symlink_metadata(&staged)
        .is_ok_and(|m| m.is_file() && m.len() > 0 && !m.file_type().is_symlink())
    {
        return Err(Diagnostic::new(
            "RMAKE-B102",
            format!("{} produced no regular output", command.label),
        ));
    }
    if let ActionKind::Compile { depfile, .. } = &action.kind {
        ensure_directory(depfile.parent().expect("depfile parent"), owned)?;
        regular_output(depfile)?;
        if !staged_dep.is_file() {
            return Err(Diagnostic::new(
                "RMAKE-B102",
                "compiler produced no dependency file",
            ));
        }
        depfile::parse_depfile(
            &fs::read(&staged_dep).map_err(|e| Diagnostic::new("RMAKE-O101", e.to_string()))?,
            &command.cwd,
        )?;
        fs::rename(&staged_dep, depfile)
            .map_err(|e| Diagnostic::new("RMAKE-O101", e.to_string()))?;
    }
    fs::rename(staged, &action.output).map_err(|e| Diagnostic::new("RMAKE-O101", e.to_string()))?;
    Ok(())
}
pub fn build(
    project: &Project,
    discovery: &Discovery,
    toolchain: &dyn Toolchain,
    archiver: &dyn Archiver,
    runner: &dyn ProcessRunner,
    options: &BuildOptions,
    reporter: &dyn BuildReporter,
) -> Result<BuildResult> {
    let _lock = storage::lock(&project.root)?;
    let owned = storage::prepare_owned(&project.root)?;
    let plan = plan::plan(project, discovery, toolchain, archiver, options, &owned)?;
    let identity = format!(
        "rmake-m1-action-v1\n{:?}\n{}\n{}",
        toolchain.info().family,
        toolchain.info().target,
        toolchain.info().version
    );
    let mut identity = identity.into_bytes();
    identity.extend(archiver.identity());
    let state = ActionState::new(&project.root, &owned, &identity)?;
    let completed = scheduler::run(&plan, options.jobs, &options.cancellation, &|action| {
        if !options.rebuild && action.cacheable && state.is_fresh(action)? {
            return Ok(true);
        }
        state.invalidate(action)?;
        perform(action, &owned, runner, options, reporter)?;
        state.record(action)?;
        Ok(false)
    });
    options.cancellation.check()?;
    let completed = completed?;
    let mut result = BuildResult {
        artifacts: plan.artifacts.clone(),
        ..BuildResult::default()
    };
    for (id, reused) in completed {
        if reused {
            result.reused += 1;
            continue;
        }
        let action = plan
            .actions
            .iter()
            .find(|a| a.id == id)
            .expect("completed planned action");
        match action.kind {
            ActionKind::Compile { .. } => result.compiled += 1,
            ActionKind::Archive => result.archived += 1,
            ActionKind::Link => result.linked += 1,
        }
    }
    Ok(result)
}
