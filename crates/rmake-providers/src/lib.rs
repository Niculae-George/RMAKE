//! Local metadata adapters for external dependencies.
//!
//! Folder imports deliberately resolve only filesystem metadata. They do not
//! scan source files, invoke tools, or modify the package.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use rmake_model::{
    Diagnostic, HeaderFolderImport, HeaderFolderRequest, HeaderFolderResolver, Result,
    UsageRequirements,
};

mod archive;

/// Resolves a package-local header folder and its exported usage requirements.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFolderProvider;

impl HeaderFolderResolver for LocalFolderProvider {
    fn resolve(
        &self,
        package_root: &Path,
        request: &HeaderFolderRequest,
    ) -> Result<HeaderFolderImport> {
        validate_alias(&request.alias)?;
        for name in request.definitions.keys() {
            validate_identifier(name, "definition name")?;
        }
        validate_standard(request.c_standard.as_deref(), false)?;
        validate_standard(request.cpp_standard.as_deref(), true)?;
        if request
            .definitions
            .values()
            .any(|value| value.contains('\0'))
        {
            return Err(folder_error(
                "RMAKE-F104",
                "preprocessor definition values cannot contain NUL",
            ));
        }

        let package_root = fs::canonicalize(package_root)
            .map_err(|error| io_diagnostic("cannot resolve package root", package_root, error))?;
        if !package_root.is_dir() {
            return Err(folder_error(
                "RMAKE-F101",
                format!("package root {:?} is not a directory", package_root),
            ));
        }

        let requested_root = &request.root;
        if requested_root.as_os_str().is_empty() || requested_root.is_absolute() {
            return Err(folder_error(
                "RMAKE-F101",
                format!(
                    "folder root {:?} must be a non-empty package-relative path",
                    requested_root
                ),
            ));
        }
        reject_root_symlinks(&package_root, requested_root)?;
        let root = canonical_directory(&package_root.join(requested_root), "folder root")?;
        ensure_contained(&root, &package_root, "folder root", requested_root)?;
        reject_managed_directory(&root, &package_root)?;

        let default_includes = request.includes.is_none();
        let include_dirs = match &request.includes {
            None => default_include(&root)?,
            Some(includes) if includes.is_empty() => {
                return Err(folder_error(
                    "RMAKE-F102",
                    format!(
                        "header folder {:?} requires at least one include directory",
                        request.alias
                    ),
                ));
            }
            Some(includes) => explicit_includes(&root, includes)?,
        };

        for include in &include_dirs {
            reject_managed_directory(include, &package_root)?;
        }
        Ok(HeaderFolderImport {
            alias: request.alias.clone(),
            root: root.clone(),
            usage: UsageRequirements {
                include_dirs,
                definitions: request.definitions.clone(),
                c_standard: request.c_standard.clone(),
                cpp_standard: request.cpp_standard.clone(),
            },
            default_includes,
        })
    }
}

/// Imported inputs must survive cleanup of RMake-owned outputs.
fn reject_managed_directory(path: &Path, package_root: &Path) -> Result<()> {
    if path.starts_with(package_root.join("build/rmake")) {
        return Err(folder_error(
            "RMAKE-F106",
            format!(
                "folder inputs must be outside RMake-owned build/rmake: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn default_include(root: &Path) -> Result<Vec<PathBuf>> {
    let include = root.join("include");
    match fs::symlink_metadata(&include) {
        Ok(_) => {
            let include = canonical_directory(&include, "default include directory")?;
            ensure_contained(
                &include,
                root,
                "default include directory",
                Path::new("include"),
            )?;
            Ok(vec![include])
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(vec![root.to_path_buf()]),
        Err(error) => Err(io_diagnostic(
            "cannot inspect default include directory",
            &include,
            error,
        )),
    }
}

fn explicit_includes(root: &Path, includes: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut resolved = Vec::with_capacity(includes.len());
    for include in includes {
        if include.as_os_str().is_empty() || include.is_absolute() {
            return Err(folder_error(
                "RMAKE-F102",
                format!(
                    "include path {:?} must be a non-empty folder-relative path",
                    include
                ),
            ));
        }
        let candidate = canonical_directory(&root.join(include), "include directory")?;
        ensure_contained(&candidate, root, "include directory", include)?;
        if !resolved.contains(&candidate) {
            resolved.push(candidate);
        }
    }
    Ok(resolved)
}

/// Check each existing root component before canonicalization so even a symlink
/// that points back inside the package is rejected for the imported root.
fn reject_root_symlinks(package_root: &Path, relative: &Path) -> Result<()> {
    let mut current = package_root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current).map_err(|error| {
                    io_diagnostic("cannot inspect folder root", &current, error)
                })?;
                if metadata.file_type().is_symlink() {
                    return Err(folder_error(
                        "RMAKE-F103",
                        format!(
                            "folder root {:?} contains symlink component {:?}",
                            relative, current
                        ),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(folder_error(
                    "RMAKE-F101",
                    format!("folder root {:?} must be package-relative", relative),
                ));
            }
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| io_diagnostic(&format!("cannot resolve {description}"), path, error))?;
    let metadata = fs::metadata(&canonical).map_err(|error| {
        io_diagnostic(&format!("cannot inspect {description}"), &canonical, error)
    })?;
    if !metadata.is_dir() {
        return Err(folder_error(
            "RMAKE-F102",
            format!("{description} {:?} is not a directory", path),
        ));
    }
    Ok(canonical)
}

fn ensure_contained(path: &Path, root: &Path, description: &str, requested: &Path) -> Result<()> {
    if !path.starts_with(root) {
        return Err(folder_error(
            "RMAKE-F103",
            format!(
                "{description} {:?} resolves outside folder root {:?}",
                requested, root
            ),
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, description: &str) -> Result<()> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(folder_error(
            "RMAKE-F104",
            format!("{description} {value:?} must be an ASCII C identifier"),
        ));
    }
    Ok(())
}

fn validate_alias(value: &str) -> Result<()> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(folder_error(
            "RMAKE-F104",
            format!(
                "folder alias {value:?} must start with an ASCII letter or digit and contain only ASCII letters, digits, '_' or '-'"
            ),
        ));
    }
    Ok(())
}

fn validate_standard(value: Option<&str>, cpp: bool) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let supported = if cpp {
        matches!(value, "c++11" | "c++14" | "c++17" | "c++20" | "c++23")
    } else {
        matches!(value, "c99" | "c11" | "c17" | "c23")
    };
    if !supported {
        return Err(folder_error(
            "RMAKE-F104",
            format!(
                "unsupported {} language standard {value:?}",
                if cpp { "C++" } else { "C" }
            ),
        ));
    }
    Ok(())
}

fn io_diagnostic(operation: &str, path: &Path, error: io::Error) -> Diagnostic {
    folder_error("RMAKE-F105", format!("{operation} {:?}: {error}", path))
}

fn folder_error(code: &'static str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(code, message)
}
