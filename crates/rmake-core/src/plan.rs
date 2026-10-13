//! Expand selected targets into a deterministic, dependency-aware action graph.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use rmake_model::{
    ActionKind, ArchiveRequest, Archiver, BuildAction, BuildOptions, BuildPlan, CompileRequest,
    Diagnostic, Discovery, Language, LinkRequest, Project, Result, Standards, TargetKind,
    TargetOrigin, Toolchain, UsageRequirements,
};

use crate::usage::{compile_usage, link_closure};

/// Build a plan rooted at the package-owned `build/rmake` directory.
pub fn plan(
    project: &Project,
    discovery: &Discovery,
    toolchain: &dyn Toolchain,
    archiver: &dyn Archiver,
    options: &BuildOptions,
    output: &Path,
) -> Result<BuildPlan> {
    let roots = selected_roots(project, options)?;
    let target_order = dependency_first_order(project, &roots)?;
    let settings = project.profiles.get(options.profile).clone();
    let profile = options.profile.name();
    let mut plan = BuildPlan::default();
    let mut archive_ids = BTreeMap::<String, String>::new();

    for target_name in &target_order {
        let target = project.target(target_name)?;
        if target.kind == TargetKind::HeaderOnly
            || matches!(target.origin, TargetOrigin::StaticFolder { .. })
        {
            continue;
        }
        let sources = discovery.sources.get(target_name).ok_or_else(|| {
            Diagnostic::new(
                "RMAKE-P101",
                format!("no sources were discovered for compiled target {target_name:?}"),
            )
        })?;
        if sources.is_empty() {
            return Err(Diagnostic::new(
                "RMAKE-P101",
                format!("no sources were discovered for compiled target {target_name:?}"),
            ));
        }

        let usage = compile_usage(project, target_name)?;
        let standards = resolved_standards(&project.standards, &usage);
        let mut target_objects = Vec::with_capacity(sources.len());
        let mut target_compile_ids = Vec::with_capacity(sources.len());
        let mut sorted_sources = sources.clone();
        sorted_sources.sort_by_key(|source| slash_path(&source.relative_path));

        for source in sorted_sources {
            let relative = source.relative_path.clone();
            let relative_digest = blake3::hash(relative.as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string();
            let object = output
                .join("contexts")
                .join(profile)
                .join(target_name)
                .join("objects")
                .join(append_hashed_suffix(&relative, &relative_digest, ".o"));
            let depfile = output
                .join("contexts")
                .join(profile)
                .join(target_name)
                .join("depfiles")
                .join(append_hashed_suffix(&relative, &relative_digest, ".d"));
            let include_dirs = usage.include_dirs.clone();
            let command = toolchain.compile_command(&CompileRequest {
                source: source.clone(),
                output: object.clone(),
                depfile: depfile.clone(),
                include_dirs: include_dirs.clone(),
                definitions: usage.definitions.clone(),
                standards: standards.clone(),
                options: target.compile_options.clone(),
                profile: settings.clone(),
                cwd: project.root.clone(),
            })?;
            let id = action_id(target_name, "compile", &relative, &relative_digest);
            target_compile_ids.push(id.clone());
            target_objects.push(object.clone());
            plan.actions.push(BuildAction {
                id,
                target: target_name.clone(),
                kind: ActionKind::Compile {
                    source: source.clone(),
                    depfile,
                    include_dirs,
                },
                command,
                output: object.clone(),
                inputs: vec![source.path],
                prerequisites: Vec::new(),
                cacheable: target.compile_options.is_empty(),
            });
        }
        match target.kind {
            TargetKind::Static => {
                let archive = output
                    .join("artifacts")
                    .join(profile)
                    .join(format!("lib{target_name}.a"));
                let command = archiver.archive_command(&ArchiveRequest {
                    objects: target_objects.clone(),
                    output: archive.clone(),
                    cwd: project.root.clone(),
                })?;
                let id = action_id(target_name, "archive", Path::new(""), "");
                plan.actions.push(BuildAction {
                    id: id.clone(),
                    target: target_name.clone(),
                    kind: ActionKind::Archive,
                    command,
                    output: archive.clone(),
                    inputs: target_objects,
                    prerequisites: target_compile_ids,
                    cacheable: true,
                });
                archive_ids.insert(target_name.clone(), id);
                plan.artifacts.insert(target_name.clone(), archive);
            }
            TargetKind::Bin | TargetKind::Test | TargetKind::Example => {
                let closure = link_closure(project, target_name)?;
                let linked_archives: Vec<_> = closure
                    .iter()
                    .filter_map(|dependency| {
                        if project.target(dependency).ok()?.kind == TargetKind::Static {
                            Some(dependency)
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut inputs = target_objects;
                let mut prerequisites = target_compile_ids;
                for dependency in &linked_archives {
                    if let TargetOrigin::StaticFolder { library, .. } =
                        &project.target(dependency)?.origin
                    {
                        inputs.push(library.clone());
                    } else if let Some(path) = plan.artifacts.get(*dependency) {
                        inputs.push(path.clone());
                    } else {
                        let path = archive_path(output, profile, dependency);
                        inputs.push(path);
                    }
                    if let Some(id) = archive_ids.get(*dependency) {
                        prerequisites.push(id.clone());
                    }
                }
                let language = if target_order_has_cpp(project, discovery, target_name, &closure) {
                    Language::Cpp
                } else {
                    Language::C
                };
                let executable = output.join("artifacts").join(profile).join(target_name);
                let mut link_options = target.link_options.clone();
                for dependency in &closure {
                    link_options.extend(project.target(dependency)?.link_options.iter().cloned());
                }
                let command = toolchain.link_command(&LinkRequest {
                    inputs: inputs.clone(),
                    output: executable.clone(),
                    language,
                    options: link_options.clone(),
                    profile: settings.clone(),
                    cwd: project.root.clone(),
                })?;
                plan.actions.push(BuildAction {
                    id: action_id(target_name, "link", Path::new(""), ""),
                    target: target_name.clone(),
                    kind: ActionKind::Link,
                    command,
                    output: executable.clone(),
                    inputs,
                    prerequisites,
                    cacheable: link_options.is_empty(),
                });
                plan.artifacts.insert(target_name.clone(), executable);
            }
            TargetKind::HeaderOnly => unreachable!("handled above"),
        }
    }

    Ok(plan)
}

fn selected_roots(project: &Project, options: &BuildOptions) -> Result<Vec<String>> {
    let mut roots = BTreeSet::new();
    if options.targets.is_empty() {
        roots.extend(
            project
                .targets
                .iter()
                .filter(|target| {
                    target.is_local()
                        && matches!(
                            target.kind,
                            TargetKind::Bin | TargetKind::Static | TargetKind::HeaderOnly
                        )
                })
                .map(|target| target.name.clone()),
        );
    } else {
        for name in &options.targets {
            project.target(name)?;
            roots.insert(name.clone());
        }
    }
    if roots.is_empty() {
        return Err(Diagnostic::new(
            "RMAKE-P102",
            "no production targets selected",
        ));
    }
    Ok(roots.into_iter().collect())
}

fn dependency_first_order(project: &Project, roots: &[String]) -> Result<Vec<String>> {
    fn visit(
        project: &Project,
        name: &str,
        visited: &mut BTreeSet<String>,
        active: &mut BTreeSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if visited.contains(name) {
            return Ok(());
        }
        if !active.insert(name.to_owned()) {
            return Err(Diagnostic::new(
                "RMAKE-P103",
                format!("target dependency cycle while planning at {name:?}"),
            ));
        }
        let target = project.target(name)?;
        let mut dependencies: Vec<_> = target
            .dependencies
            .iter()
            .map(|d| d.target.as_str())
            .collect();
        dependencies.sort_unstable();
        for dependency in dependencies {
            visit(project, dependency, visited, active, order)?;
        }
        active.remove(name);
        visited.insert(name.to_owned());
        order.push(name.to_owned());
        Ok(())
    }

    let mut visited = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut order = Vec::new();
    for root in roots {
        visit(project, root, &mut visited, &mut active, &mut order)?;
    }
    Ok(order)
}

fn resolved_standards(defaults: &Standards, usage: &UsageRequirements) -> Standards {
    Standards {
        c: resolved_standard(&defaults.c, usage.c_standard.as_deref(), false),
        cpp: resolved_standard(&defaults.cpp, usage.cpp_standard.as_deref(), true),
    }
}

fn resolved_standard(default: &str, minimum: Option<&str>, cpp: bool) -> String {
    minimum
        .filter(|required| {
            crate::usage::standard_rank(required, cpp) > crate::usage::standard_rank(default, cpp)
        })
        .unwrap_or(default)
        .to_owned()
}

fn target_order_has_cpp(
    project: &Project,
    discovery: &Discovery,
    target: &str,
    closure: &[String],
) -> bool {
    std::iter::once(target)
        .chain(closure.iter().map(String::as_str))
        .any(|name| {
            project.target(name).is_ok_and(|target| {
                matches!(
                    target.origin,
                    TargetOrigin::StaticFolder {
                        language: Language::Cpp,
                        ..
                    }
                )
            }) || discovery.sources.get(name).is_some_and(|sources| {
                sources
                    .iter()
                    .any(|source| source.language == Language::Cpp)
            })
        })
}

/// Drivers needed for selected compilation and actual executable link closures.
pub fn required_languages(project: &Project, discovery: &Discovery) -> Result<Vec<Language>> {
    let mut languages: BTreeSet<_> = discovery
        .sources
        .values()
        .flatten()
        .map(|s| s.language)
        .collect();
    for name in discovery.sources.keys() {
        let target = project.target(name)?;
        if target.is_local() && target.kind.is_executable() {
            let closure = link_closure(project, name)?;
            if target_order_has_cpp(project, discovery, name, &closure) {
                languages.insert(Language::Cpp);
            }
        }
    }
    Ok(languages.into_iter().collect())
}

fn archive_path(output: &Path, profile: &str, target: &str) -> PathBuf {
    output
        .join("artifacts")
        .join(profile)
        .join(format!("lib{target}.a"))
}

fn append_hashed_suffix(path: &Path, digest: &str, suffix: &str) -> PathBuf {
    let mut name = std::ffi::OsString::from(digest);
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    name.push(suffix);
    path.with_file_name(name)
}

fn action_id(target: &str, kind: &str, relative: &Path, digest: &str) -> String {
    if relative.as_os_str().is_empty() {
        format!("{target}/{kind}")
    } else {
        format!("{target}/{kind}/{}/@{digest}", slash_path(relative))
    }
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
