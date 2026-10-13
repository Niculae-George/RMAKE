use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rmake_model::{
    Diagnostic, Discovery, IgnoredFile, Language, Project, Result, SourceFile, Target, TargetKind,
    TargetOrigin,
};
use walkdir::{DirEntry, WalkDir};

use crate::manifest::validate_glob;

pub fn discover(project: &Project) -> Result<Discovery> {
    discover_selected(
        project,
        &project
            .targets
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>(),
    )
}
/// Scan selected targets while retaining every declared ownership boundary.
pub fn discover_selected(project: &Project, names: &[String]) -> Result<Discovery> {
    for name in names {
        project.target(name)?;
    }
    let selected: BTreeSet<_> = names.iter().collect();
    let package = project.root.canonicalize().map_err(|e| {
        Diagnostic::new(
            "package-root",
            format!("cannot resolve {}: {e}", project.root.display()),
        )
    })?;
    let mut result = Discovery::default();
    let roots: Vec<(String, PathBuf, TargetKind)> = project
        .targets
        .iter()
        .filter_map(|t| {
            let root = match &t.origin {
                TargetOrigin::HeaderFolder { root, .. }
                | TargetOrigin::StaticFolder { root, .. } => Some(root),
                TargetOrigin::Local => t.root.as_ref(),
            };
            root.map(|r| (t.name.clone(), r.clone(), t.kind))
        })
        .collect();
    for target in project
        .targets
        .iter()
        .filter(|t| selected.contains(&t.name))
    {
        if target.kind == TargetKind::HeaderOnly
            || matches!(target.origin, TargetOrigin::StaticFolder { .. })
        {
            result.sources.insert(target.name.clone(), Vec::new());
            continue;
        }
        let Some(root) = &target.root else {
            return Err(Diagnostic::new(
                "target-root",
                format!("compiled target {} has no root", target.name),
            ));
        };
        let meta = fs::symlink_metadata(root).map_err(|e| {
            Diagnostic::new(
                "target-root",
                format!("cannot inspect {}: {e}", root.display()),
            )
        })?;
        if meta.file_type().is_symlink() {
            return Err(Diagnostic::new(
                "target-root-symlink",
                format!("target root {} must not be a symlink", root.display()),
            ));
        }
        let target_root = root.canonicalize().map_err(|e| {
            Diagnostic::new(
                "target-root",
                format!("cannot resolve {}: {e}", root.display()),
            )
        })?;
        if !target_root.starts_with(&package) {
            return Err(Diagnostic::new(
                "target-root-escape",
                format!("target root {} escapes package root", target_root.display()),
            ));
        }
        if target_root.is_dir()
            && target_root.join("main.c").is_file()
            && target_root.join("main.cpp").is_file()
        {
            return Err(Diagnostic::new(
                "ambiguous-entry-point",
                format!("target {} has both main.c and main.cpp", target.name),
            ));
        }
        let excludes = compile_excludes(&target.excludes)?;
        let children: BTreeSet<PathBuf> = roots
            .iter()
            .filter(|(name, path, _)| {
                name != &target.name && path.starts_with(&target_root) && path != &target_root
            })
            .filter_map(|(_, path, _)| path.canonicalize().ok())
            .collect();
        let mut sources = Vec::new();
        if target_root.is_file() {
            process_file(
                &target_root,
                &package,
                &excludes,
                &mut sources,
                &mut result.ignored,
            )?;
        } else if target_root.is_dir() {
            let mut skipped = Vec::new();
            let walker = WalkDir::new(&target_root)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
                .filter_entry(|entry| {
                    if entry.path() == target_root {
                        return true;
                    }
                    if children.iter().any(|child| entry.path() == child) {
                        return false;
                    }
                    if let Some(reason) = ignored_directory_reason(entry, &package, target) {
                        skipped.push(IgnoredFile {
                            path: entry.path().to_path_buf(),
                            reason: reason.into(),
                        });
                        return false;
                    }
                    true
                });
            for item in walker {
                let entry = item.map_err(|e| {
                    Diagnostic::new(
                        "discovery-io",
                        format!(
                            "cannot inspect {}: {e}",
                            e.path().unwrap_or(&target_root).display()
                        ),
                    )
                })?;
                if entry.path() == target_root || entry.file_type().is_dir() {
                    continue;
                }
                if entry.file_type().is_symlink() {
                    let ext = entry
                        .path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .unwrap_or("");
                    if is_source_extension(ext) {
                        return Err(Diagnostic::new(
                            "source-symlink",
                            format!(
                                "source symlink {} is unsupported; place regular source file under target root",
                                entry.path().display()
                            ),
                        ));
                    }
                    continue;
                }
                process_file(
                    entry.path(),
                    &package,
                    &excludes,
                    &mut sources,
                    &mut result.ignored,
                )?;
            }
            result.ignored.extend(skipped);
        } else {
            return Err(Diagnostic::new(
                "target-root",
                format!(
                    "target root {} is not a file or directory",
                    target_root.display()
                ),
            ));
        }
        sources.sort_by_cached_key(|s| slash_path(&s.relative_path));
        if sources.is_empty() {
            return Err(Diagnostic::new(
                "no-sources",
                format!(
                    "no supported C/C++ sources found for target {}; add a source beneath {}",
                    target.name,
                    target_root.display()
                ),
            ));
        }
        result.sources.insert(target.name.clone(), sources);
    }
    result.ignored.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn process_file(
    path: &Path,
    package: &Path,
    excludes: &GlobSet,
    sources: &mut Vec<SourceFile>,
    ignored: &mut Vec<IgnoredFile>,
) -> Result<()> {
    let relative = path.strip_prefix(package).map_err(|_| {
        Diagnostic::new(
            "discovery-path",
            format!("{} is outside package root", path.display()),
        )
    })?;
    let normalized = slash_path(relative);
    if excludes.is_match(&normalized) {
        ignored.push(IgnoredFile {
            path: path.to_path_buf(),
            reason: "excluded by manifest pattern".into(),
        });
        return Ok(());
    }
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("");
    if is_unsupported_source_extension(ext) {
        return Err(Diagnostic::new(
            "unsupported-source",
            format!(
                "{} has unsupported source extension .{ext} in M1",
                path.display()
            ),
        ));
    }
    if let Some(language) = source_language(ext) {
        sources.push(SourceFile {
            path: path.to_path_buf(),
            relative_path: relative.to_path_buf(),
            language,
        });
    } else if is_header_extension(ext) {
        ignored.push(IgnoredFile {
            path: path.to_path_buf(),
            reason: "header".into(),
        });
    } else {
        ignored.push(IgnoredFile {
            path: path.to_path_buf(),
            reason: "unknown file type".into(),
        });
    }
    Ok(())
}
fn compile_excludes(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        validate_glob(p)?;
        b.add(
            GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    Diagnostic::new("invalid-exclude", format!("invalid pattern {p:?}: {e}"))
                })?,
        );
    }
    b.build()
        .map_err(|e| Diagnostic::new("invalid-exclude", e.to_string()))
}
fn hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.starts_with('.'))
}
fn ignored_directory_reason(
    entry: &DirEntry,
    package: &Path,
    target: &Target,
) -> Option<&'static str> {
    if entry.file_type().is_symlink() {
        return entry.path().is_dir().then_some("directory symlink");
    }
    if !entry.file_type().is_dir() {
        return None;
    }
    if hidden(entry.file_name()) {
        return Some("hidden directory");
    }
    let rel = entry.path().strip_prefix(package).ok()?;
    let parts: Vec<_> = rel
        .components()
        .filter_map(|x| x.as_os_str().to_str())
        .collect();
    let last = *parts.last()?;
    if matches!(last, ".git" | ".rmake" | "build" | "vendor" | "third_party") {
        return Some("reserved directory");
    }
    if !matches!(target.kind, TargetKind::Test | TargetKind::Example)
        && parts.len() == 1
        && matches!(last, "tests" | "examples")
    {
        return Some("reserved directory");
    }
    if !matches!(target.kind, TargetKind::Test | TargetKind::Example)
        && parts.as_slice() == ["src", "bin"]
    {
        return Some("reserved directory");
    }
    if entry.path().join("Rmake.toml").is_file() {
        return Some("nested package root");
    }
    None
}
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|x| x.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
fn source_language(ext: &str) -> Option<Language> {
    match ext {
        "c" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "C" => Some(Language::Cpp),
        _ => None,
    }
}
fn is_source_extension(ext: &str) -> bool {
    source_language(ext).is_some() || is_unsupported_source_extension(ext)
}
fn is_unsupported_source_extension(ext: &str) -> bool {
    matches!(ext, "m" | "mm" | "s" | "S" | "ixx" | "cppm")
}
fn is_header_extension(ext: &str) -> bool {
    matches!(ext, "h" | "hh" | "hpp" | "hxx")
}
