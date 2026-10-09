//! Project scaffolding that never overwrites an existing manifest or source.
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::manifest;
use rmake_model::{Diagnostic, Language, Result};
use walkdir::WalkDir;

fn error(path: &Path, operation: &str, error: std::io::Error) -> Diagnostic {
    Diagnostic::new(
        "RMAKE-N101",
        format!("{operation} {}: {error}", path.display()),
    )
}

fn name_for(path: &Path, name: Option<&str>) -> Result<String> {
    let name = match name {
        Some(value) => value.to_owned(),
        None => path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                Diagnostic::new(
                    "RMAKE-N102",
                    "directory has no usable package name; supply --name",
                )
            })?
            .to_owned(),
    };
    manifest::validate_name(&name)?;
    Ok(name)
}

pub fn create_new(path: &Path, name: Option<&str>, language: Language) -> Result<PathBuf> {
    create_new_kind(path, name, language, false)
}

pub fn create_new_library(path: &Path, name: Option<&str>, language: Language) -> Result<PathBuf> {
    create_new_kind(path, name, language, true)
}

fn create_new_kind(
    path: &Path,
    name: Option<&str>,
    language: Language,
    library: bool,
) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| Diagnostic::new("RMAKE-N101", error.to_string()))?
            .join(path)
    };
    let name = name_for(&absolute, name)?;
    let existed = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(Diagnostic::new(
                    "RMAKE-N103",
                    "new requires a real empty directory or a new path",
                ));
            }
            if fs::read_dir(&absolute)
                .map_err(|e| error(&absolute, "read directory", e))?
                .next()
                .is_some()
            {
                return Err(Diagnostic::new(
                    "RMAKE-N103",
                    "new refuses a non-empty directory; use init for existing code",
                ));
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(error(&absolute, "inspect directory", e)),
    };
    fs::create_dir_all(&absolute).map_err(|e| error(&absolute, "create project", e))?;
    let result = init_kind(&absolute, Some(&name), language, library);
    if result.is_err() && !existed {
        // Only remove a directory we created if rollback left it empty.
        let _ = fs::remove_dir(&absolute);
    }
    result
}

pub fn init(path: &Path, name: Option<&str>, language: Language) -> Result<PathBuf> {
    init_kind(path, name, language, false)
}

pub fn init_library(path: &Path, name: Option<&str>, language: Language) -> Result<PathBuf> {
    init_kind(path, name, language, true)
}

fn init_kind(
    path: &Path,
    name: Option<&str>,
    language: Language,
    library: bool,
) -> Result<PathBuf> {
    let root = path
        .canonicalize()
        .map_err(|e| error(path, "resolve project", e))?;
    if !root.is_dir() {
        return Err(Diagnostic::new(
            "RMAKE-N103",
            "init requires an existing directory",
        ));
    }
    let name = name_for(&root, name)?;
    let manifest_path = root.join("Rmake.toml");
    if fs::symlink_metadata(&manifest_path).is_ok() {
        return Err(Diagnostic::new(
            "RMAKE-N104",
            "Rmake.toml already exists; init never overwrites it",
        ));
    }
    let text = format!("schema = 1\n\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
    let src = root.join("src");
    if fs::symlink_metadata(&src)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(Diagnostic::new(
            "RMAKE-N105",
            "src must be a real directory; init does not follow source-root symlinks",
        ));
    }
    let library_markers = ["lib.c", "lib.cpp"]
        .iter()
        .any(|name| src.join(name).is_file());
    let app_markers = ["main.c", "main.cpp"]
        .iter()
        .any(|name| src.join(name).is_file());
    let has_marker = ["main.c", "main.cpp", "lib.c", "lib.cpp"]
        .iter()
        .any(|name| src.join(name).is_file());
    if library && app_markers {
        return Err(Diagnostic::new(
            "RMAKE-N106",
            "init --lib found a conventional binary entry point; preserve it and use init without --lib",
        ));
    }
    let has_include = root.join("include").is_dir();
    let seed = if has_marker {
        // Validate ambiguity, boundaries and unsupported library layouts before writing anything.
        manifest::parse_project(&manifest_path, &text)?;
        false
    } else if has_native_sources(&root)? {
        return Err(Diagnostic::new(
            "RMAKE-N106",
            "existing C/C++ sources have no conventional src/main.c or src/main.cpp; preserve them and declare a [[targets]] binary with root in Rmake.toml",
        ));
    } else {
        true
    };
    if library && !seed && !library_markers {
        return Err(Diagnostic::new(
            "RMAKE-N106",
            "init --lib requires conventional src/lib.c or src/lib.cpp when native sources already exist",
        ));
    }
    let include = root.join("include");
    if fs::symlink_metadata(&include)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(Diagnostic::new(
            "RMAKE-N105",
            "include must be a real directory; init does not follow include-root symlinks",
        ));
    }
    let mut created = Vec::new();
    let src_created = seed && !src.exists();
    let include_created = library && seed && !has_include;
    let outcome = (|| {
        if seed {
            fs::create_dir_all(&src).map_err(|e| error(&src, "create sources", e))?;
            if library {
                fs::create_dir_all(&include).map_err(|e| error(&include, "create includes", e))?;
                let mut stem = name.replace('-', "_");
                if stem.as_bytes()[0].is_ascii_digit() {
                    stem.insert_str(0, "rmake_");
                }
                let (filename, source) = match language {
                    Language::C => (
                        "lib.c",
                        format!(
                            "#include \"{stem}.h\"\n\nint {stem}_answer(void) {{\n    return 42;\n}}\n"
                        ),
                    ),
                    Language::Cpp => (
                        "lib.cpp",
                        format!(
                            "#include \"{stem}.h\"\n\nint {stem}_answer() {{\n    return 42;\n}}\n"
                        ),
                    ),
                };
                write_new(&src.join(filename), source.as_bytes(), &mut created)?;
                let header = format!(
                    "#ifndef RMAKE_{}_H\n#define RMAKE_{}_H\n\n#ifdef __cplusplus\nextern \"C\" {{\n#endif\nint {stem}_answer(void);\n#ifdef __cplusplus\n}}\n#endif\n\n#endif\n",
                    stem.to_ascii_uppercase(),
                    stem.to_ascii_uppercase()
                );
                write_new(
                    &include.join(format!("{stem}.h")),
                    header.as_bytes(),
                    &mut created,
                )?;
            } else {
                let (filename, source) = match language {
                    Language::C => (
                        "main.c",
                        "#include <stdio.h>\n\nint main(void) {\n    puts(\"Hello from RMake!\");\n    return 0;\n}\n",
                    ),
                    Language::Cpp => (
                        "main.cpp",
                        "#include <iostream>\n\nint main() {\n    std::cout << \"Hello from RMake!\\n\";\n    return 0;\n}\n",
                    ),
                };
                write_new(&src.join(filename), source.as_bytes(), &mut created)?;
            }
        }
        write_new(&manifest_path, text.as_bytes(), &mut created)?;
        let ignore = root.join(".gitignore");
        if fs::symlink_metadata(&ignore).is_err() {
            write_new(&ignore, b"/build/\n", &mut created)?;
        }
        Ok(manifest_path.clone())
    })();
    if outcome.is_err() {
        for path in created.iter().rev() {
            let _ = fs::remove_file(path);
        }
        if src_created {
            let _ = fs::remove_dir(&src);
        }
        if include_created {
            let _ = fs::remove_dir(&include);
        }
    }
    outcome
}

fn write_new(path: &Path, contents: &[u8], created: &mut Vec<PathBuf>) -> Result<()> {
    let mut file = File::create_new(path).map_err(|e| error(path, "create", e))?;
    created.push(path.to_path_buf());
    file.write_all(contents)
        .map_err(|e| error(path, "write", e))
}

fn has_native_sources(root: &Path) -> Result<bool> {
    let entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with('.')
                        || matches!(
                            name,
                            "build" | "vendor" | "third_party" | "tests" | "examples"
                        )
                })
        });
    for entry in entries {
        let entry = entry
            .map_err(|e| Diagnostic::new("RMAKE-N101", format!("scan existing sources: {e}")))?;
        if entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| matches!(extension, "c" | "cpp" | "cc" | "cxx" | "C"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_init_never_overwrite_existing_sources_or_ignore() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("existing");
        fs::create_dir_all(root.join("src")).unwrap();
        let source = "int main() { return 7; }";
        fs::write(root.join("src/main.c"), source).unwrap();
        fs::write(root.join(".gitignore"), "keep\n").unwrap();
        init(&root, None, Language::Cpp).unwrap();
        assert_eq!(fs::read_to_string(root.join("src/main.c")).unwrap(), source);
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "keep\n"
        );
        assert!(!root.join("src/main.cpp").exists());
        assert!(init(&root, None, Language::C).is_err());
        assert!(create_new(&root, None, Language::C).is_err());
    }

    #[test]
    fn ambiguous_or_unconventional_sources_leave_directory_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("main.cpp"), "int main() {} ").unwrap();
        assert!(init(temp.path(), Some("sample"), Language::Cpp).is_err());
        assert!(!temp.path().join("Rmake.toml").exists());
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.c"), "int main() {} ").unwrap();
        fs::write(temp.path().join("src/main.cpp"), "int main() {} ").unwrap();
        assert!(init(temp.path(), Some("sample"), Language::Cpp).is_err());
        assert!(!temp.path().join("Rmake.toml").exists());
    }
}
