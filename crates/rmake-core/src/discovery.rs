use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rmake_model::{Diagnostic, Discovery, IgnoredFile, Language, Project, Result, SourceFile};
use walkdir::{DirEntry, WalkDir};

use crate::manifest::validate_glob;

pub fn discover(project: &Project) -> Result<Discovery> {
    let package_root = project.root.canonicalize().map_err(|error| {
        Diagnostic::new(
            "package-root",
            format!("cannot resolve {}: {error}", project.root.display()),
        )
    })?;
    let target_root = project.target.root.canonicalize().map_err(|error| {
        Diagnostic::new(
            "target-root",
            format!("cannot resolve {}: {error}", project.target.root.display()),
        )
    })?;
    if !target_root.starts_with(&package_root) {
        return Err(Diagnostic::new(
            "target-root-escape",
            format!(
                "target root {} escapes package root {}",
                target_root.display(),
                package_root.display()
            ),
        ));
    }
    if !target_root.is_dir() {
        return Err(Diagnostic::new(
            "target-root",
            format!("target root {} is not a directory", target_root.display()),
        ));
    }
    let excludes = compile_excludes(&project.target.excludes)?;
    let mut result = Discovery::default();
    let mut ignored_directories = Vec::new();

    let walker = WalkDir::new(&target_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.path() == target_root {
                return true;
            }
            if let Some(reason) = ignored_directory_reason(entry, &package_root) {
                ignored_directories.push(IgnoredFile {
                    path: entry.path().to_path_buf(),
                    reason: reason.into(),
                });
                return false;
            }
            true
        });

    for item in walker {
        let entry = item.map_err(|error| {
            Diagnostic::new(
                "discovery-io",
                format!(
                    "cannot inspect {}: {error}",
                    error.path().unwrap_or(&target_root).display()
                ),
            )
        })?;
        if entry.path() == target_root || entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(&package_root).map_err(|_| {
            Diagnostic::new(
                "discovery-path",
                format!("{} is outside package root", entry.path().display()),
            )
        })?;
        let normalized = slash_path(relative);
        if excludes.is_match(&normalized) {
            result.ignored.push(IgnoredFile {
                path: entry.path().to_path_buf(),
                reason: "excluded by manifest pattern".into(),
            });
            continue;
        }

        let Some(ext) = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
        else {
            result.ignored.push(IgnoredFile {
                path: entry.path().to_path_buf(),
                reason: "unknown file type".into(),
            });
            continue;
        };
        if entry.file_type().is_symlink() {
            if entry.path().is_dir() {
                continue;
            }
            if is_source_extension(ext) {
                return Err(Diagnostic::new(
                    "source-symlink",
                    format!(
                        "source file symlink {} is unsupported; place a regular source file under the target root",
                        entry.path().display()
                    ),
                ));
            }
            continue;
        }
        if is_unsupported_source_extension(ext) {
            return Err(Diagnostic::new(
                "unsupported-source",
                format!(
                    "{} has unsupported source extension .{ext} in M1a",
                    entry.path().display()
                ),
            ));
        }
        match source_language(ext) {
            Some(language) => result.sources.push(SourceFile {
                path: entry.path().to_path_buf(),
                relative_path: relative.to_path_buf(),
                language,
            }),
            None if is_header_extension(ext) => result.ignored.push(IgnoredFile {
                path: entry.path().to_path_buf(),
                reason: "header".into(),
            }),
            None => result.ignored.push(IgnoredFile {
                path: entry.path().to_path_buf(),
                reason: "unknown file type".into(),
            }),
        }
    }

    result.ignored.extend(ignored_directories);
    result
        .sources
        .sort_by_cached_key(|source| slash_path(&source.relative_path));
    result.ignored.sort_by(|a, b| a.path.cmp(&b.path));
    if result.sources.is_empty() {
        return Err(Diagnostic::new(
            "no-sources",
            format!(
                "no supported source files found under target root {}; add .c/.cpp/.cc/.cxx/.C files",
                target_root.display()
            ),
        ));
    }
    Ok(result)
}

fn compile_excludes(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        validate_glob(pattern)?;
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| {
                Diagnostic::new(
                    "invalid-exclude",
                    format!("invalid exclusion pattern {pattern:?}: {error}"),
                )
            })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| Diagnostic::new("invalid-exclude", error.to_string()))
}

fn hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| name.starts_with('.'))
}

fn ignored_directory_reason(entry: &DirEntry, package_root: &Path) -> Option<&'static str> {
    if entry.file_type().is_symlink() && entry.path().is_dir() {
        return Some("directory symlink");
    }
    if !entry.file_type().is_dir() {
        return None;
    }
    if hidden(entry.file_name()) {
        Some("hidden directory")
    } else if reserved_dir(entry.path(), package_root) {
        Some("reserved directory")
    } else if is_nested_package(entry.path()) {
        Some("nested package root")
    } else {
        None
    }
}

fn reserved_dir(path: &Path, package_root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(package_root) else {
        return true;
    };
    let components: Vec<_> = relative
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect();
    let Some(name) = components.last().copied() else {
        return false;
    };
    matches!(name, ".git" | ".rmake" | "build" | "vendor" | "third_party")
        || (components.len() == 1 && matches!(name, "tests" | "examples"))
        || (components.as_slice() == ["src", "bin"])
}

fn is_nested_package(path: &Path) -> bool {
    path.join("Rmake.toml").is_file()
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn source_language(extension: &str) -> Option<Language> {
    match extension {
        "c" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "C" => Some(Language::Cpp),
        _ => None,
    }
}

fn is_source_extension(extension: &str) -> bool {
    source_language(extension).is_some() || is_unsupported_source_extension(extension)
}

fn is_unsupported_source_extension(extension: &str) -> bool {
    matches!(extension, "m" | "mm" | "s" | "S" | "ixx" | "cppm")
}

fn is_header_extension(extension: &str) -> bool {
    matches!(extension, "h" | "hh" | "hpp" | "hxx")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rmake_model::{BinaryTarget, Profiles, Project, Standards};

    use super::*;

    fn fixture() -> (tempfile::TempDir, Project) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src/nested")).unwrap();
        let root = temp.path().canonicalize().unwrap();
        let project = Project {
            root: root.clone(),
            name: "fixture".into(),
            version: "1".into(),
            target: BinaryTarget {
                name: "fixture".into(),
                root: root.join("src"),
                include_dirs: vec![],
                excludes: vec![],
                inferred: true,
            },
            standards: Standards::default(),
            profiles: Profiles::default(),
        };
        (temp, project)
    }

    #[test]
    fn discovers_sorted_sources_and_inventory_under_nested_root() {
        let (temp, project) = fixture();
        fs::write(temp.path().join("src/z.cpp"), "").unwrap();
        fs::write(temp.path().join("src/nested/a.c"), "").unwrap();
        fs::write(temp.path().join("src/nested/a.hpp"), "").unwrap();
        fs::write(temp.path().join("src/readme.txt"), "").unwrap();
        let found = discover(&project).unwrap();
        assert_eq!(
            found
                .sources
                .iter()
                .map(|source| slash_path(&source.relative_path))
                .collect::<Vec<_>>(),
            ["src/nested/a.c", "src/z.cpp"]
        );
        assert!(found.ignored.iter().any(|file| file.reason == "header"));
        assert!(
            found
                .ignored
                .iter()
                .any(|file| file.reason == "unknown file type")
        );
    }

    #[test]
    fn exclusions_reserved_dirs_hidden_dirs_and_nested_packages_are_skipped() {
        let (temp, mut project) = fixture();
        project.target.excludes = vec!["src/skip/**".into()];
        for dir in [
            "src/skip",
            "src/.hidden",
            "src/build",
            "src/vendor",
            "src/bin",
        ] {
            fs::create_dir_all(temp.path().join(dir)).unwrap();
            fs::write(temp.path().join(dir).join("hidden.cpp"), "").unwrap();
        }
        fs::create_dir_all(temp.path().join("src/nested-package")).unwrap();
        fs::write(
            temp.path().join("src/nested-package/Rmake.toml"),
            "schema = 1",
        )
        .unwrap();
        fs::write(temp.path().join("src/nested-package/ignored.cpp"), "").unwrap();
        fs::create_dir_all(temp.path().join("src/nested/examples")).unwrap();
        fs::write(temp.path().join("src/nested/examples/helper.cpp"), "").unwrap();
        fs::write(temp.path().join("src/visible.cpp"), "").unwrap();
        let found = discover(&project).unwrap();
        assert_eq!(found.sources.len(), 2);
        assert_eq!(
            found
                .sources
                .iter()
                .map(|source| slash_path(&source.relative_path))
                .collect::<Vec<_>>(),
            ["src/nested/examples/helper.cpp", "src/visible.cpp"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn inspection_inventory_reports_pruned_directories_and_excluded_files() {
        let (temp, mut project) = fixture();
        project.target.excludes = vec!["src/skip/**".into()];
        fs::write(temp.path().join("src/main.cpp"), "").unwrap();
        for dir in ["src/.cache", "src/build", "src/nested-package", "src/skip"] {
            fs::create_dir_all(temp.path().join(dir)).unwrap();
            fs::write(temp.path().join(dir).join("ignored.cpp"), "").unwrap();
        }
        fs::write(
            temp.path().join("src/nested-package/Rmake.toml"),
            "schema = 1",
        )
        .unwrap();
        fs::create_dir_all(temp.path().join("outside")).unwrap();
        fs::write(temp.path().join("outside/linked.cpp"), "").unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside"), temp.path().join("src/linked"))
            .unwrap();

        let found = discover(&project).unwrap();
        let inventory: Vec<_> = found
            .ignored
            .iter()
            .map(|item| {
                (
                    slash_path(item.path.strip_prefix(&project.root).unwrap()),
                    item.reason.as_str(),
                )
            })
            .collect();
        assert_eq!(
            inventory,
            [
                ("src/.cache".into(), "hidden directory"),
                ("src/build".into(), "reserved directory"),
                ("src/linked".into(), "directory symlink"),
                ("src/nested-package".into(), "nested package root"),
                (
                    "src/skip/ignored.cpp".into(),
                    "excluded by manifest pattern"
                ),
            ]
        );
    }

    #[test]
    fn unsupported_sources_and_source_symlinks_fail() {
        let (temp, project) = fixture();
        fs::write(temp.path().join("src/module.cppm"), "").unwrap();
        assert_eq!(discover(&project).unwrap_err().code, "unsupported-source");
        fs::remove_file(temp.path().join("src/module.cppm")).unwrap();
        fs::write(temp.path().join("outside.cpp"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            temp.path().join("outside.cpp"),
            temp.path().join("src/link.cpp"),
        )
        .unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(
            temp.path().join("outside.cpp"),
            temp.path().join("src/link.cpp"),
        )
        .unwrap();
        assert_eq!(discover(&project).unwrap_err().code, "source-symlink");
    }

    #[test]
    fn explicit_reserved_root_is_still_scanned_and_empty_root_fails() {
        let (temp, mut project) = fixture();
        project.target.root = temp.path().join("src/bin");
        fs::create_dir_all(&project.target.root).unwrap();
        fs::write(project.target.root.join("tool.c"), "").unwrap();
        assert_eq!(discover(&project).unwrap().sources.len(), 1);
        fs::remove_file(project.target.root.join("tool.c")).unwrap();
        assert_eq!(discover(&project).unwrap_err().code, "no-sources");
    }
}
