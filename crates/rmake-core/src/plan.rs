//! Pure expansion of discovered translation units into compile and link requests.
use std::path::Path;

use rmake_model::{
    BuildPlan, CompileAction, CompileRequest, Diagnostic, Discovery, Language, LinkRequest,
    ProfileKind, Project, Result, Toolchain,
};

pub fn plan(
    project: &Project,
    discovery: &Discovery,
    toolchain: &dyn Toolchain,
    profile: ProfileKind,
    staging: &Path,
    final_executable: &Path,
) -> Result<BuildPlan> {
    if discovery.sources.is_empty() {
        return Err(Diagnostic::new(
            "RMAKE-S101",
            "no C/C++ sources were discovered",
        ));
    }
    let settings = project.profiles.get(profile).clone();
    let mut compiles = Vec::new();
    let mut objects = Vec::new();
    for source in &discovery.sources {
        // Keep the source extension, so foo.c and foo.cpp never share an object.
        let relative = source.relative_path.as_os_str().to_os_string();
        let mut relative_object = relative;
        relative_object.push(".o");
        let output = staging.join("objects").join(relative_object);
        let command = toolchain.compile_command(&CompileRequest {
            source: source.clone(),
            output: output.clone(),
            include_dirs: project.target.include_dirs.clone(),
            standards: project.standards.clone(),
            profile: settings.clone(),
            cwd: project.root.clone(),
        })?;
        objects.push(output.clone());
        compiles.push(CompileAction { command, output });
    }
    let language = if discovery
        .sources
        .iter()
        .any(|source| source.language == Language::Cpp)
    {
        Language::Cpp
    } else {
        Language::C
    };
    let staged_executable = staging.join("executable");
    let link = toolchain.link_command(&LinkRequest {
        objects,
        output: staged_executable.clone(),
        language,
        profile: settings,
        cwd: project.root.clone(),
    })?;
    Ok(BuildPlan {
        compiles,
        link,
        staged_executable,
        final_executable: final_executable.to_path_buf(),
    })
}
