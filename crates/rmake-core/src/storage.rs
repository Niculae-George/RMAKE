//! Ownership and locking of generated output. The lock survives `clean`.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rmake_model::{Diagnostic, Result};

const MARKER: &str = ".rmake-owned";

pub(crate) struct BuildLock {
    _file: File,
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> Diagnostic {
    Diagnostic::new(
        "RMAKE-O101",
        format!("{operation} {}: {error}", path.display()),
    )
}

fn check_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(Diagnostic::new(
                "RMAKE-O102",
                format!(
                    "generated-output path must be a real directory, not a symlink or file: {}",
                    path.display()
                ),
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect", path, error)),
    }
}

pub(crate) fn lock(root: &Path) -> Result<BuildLock> {
    let build = root.join("build");
    check_directory(&build)?;
    fs::create_dir_all(&build).map_err(|error| io_error("create", &build, error))?;
    let lock_path = build.join(".rmake.lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(Diagnostic::new(
            "RMAKE-O102",
            format!("lock must be a regular file: {}", lock_path.display()),
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| io_error("open lock", &lock_path, error))?;
    file.try_lock().map_err(|error| {
        Diagnostic::new(
            "RMAKE-O103",
            format!(
                "cannot acquire project build lock (another build/clean may be running): {error}"
            ),
        )
    })?;
    Ok(BuildLock { _file: file })
}

fn expected_marker(root: &Path) -> Vec<u8> {
    let mut bytes = b"rmake-owned-v1\n".to_vec();
    bytes.extend_from_slice(root.as_os_str().as_encoded_bytes());
    bytes
}

fn verify_owned(root: &Path, output: &Path) -> Result<()> {
    check_directory(output)?;
    let marker = output.join(MARKER);
    let metadata = fs::symlink_metadata(&marker).map_err(|_| {
        Diagnostic::new(
            "RMAKE-O104",
            format!(
                "refusing to use/remove unowned output directory {}; missing ownership marker",
                output.display()
            ),
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Diagnostic::new(
            "RMAKE-O104",
            "output ownership marker must be a regular file",
        ));
    }
    let bytes = fs::read(&marker).map_err(|error| io_error("read marker", &marker, error))?;
    if bytes != expected_marker(root) {
        return Err(Diagnostic::new(
            "RMAKE-O104",
            format!(
                "output ownership marker does not match project {}; clean refuses to remove it",
                root.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn prepare_owned(root: &Path) -> Result<PathBuf> {
    let output = root.join("build/rmake");
    check_directory(&output)?;
    if output.exists() {
        let mut entries =
            fs::read_dir(&output).map_err(|error| io_error("inspect output", &output, error))?;
        if entries.next().is_some() {
            verify_owned(root, &output)?;
            return Ok(output);
        }
    } else {
        fs::create_dir(&output).map_err(|error| io_error("create output", &output, error))?;
    }
    let marker = output.join(MARKER);
    let mut file =
        File::create_new(&marker).map_err(|error| io_error("create marker", &marker, error))?;
    file.write_all(&expected_marker(root))
        .map_err(|error| io_error("write marker", &marker, error))?;
    Ok(output)
}

pub fn clean(root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .map_err(|error| io_error("resolve project", root, error))?;
    let _lock = lock(&root)?;
    let output = root.join("build/rmake");
    if !output
        .try_exists()
        .map_err(|error| io_error("inspect output", &output, error))?
    {
        return Ok(());
    }
    verify_owned(&root, &output)?;
    fs::remove_dir_all(&output).map_err(|error| io_error("remove output", &output, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_preserves_unowned_data_and_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("build/rmake")).unwrap();
        fs::write(root.join("build/rmake/important"), "keep").unwrap();
        assert!(clean(&root).is_err());
        assert!(root.join("build/rmake/important").exists());
        fs::remove_dir_all(root.join("build/rmake")).unwrap();
        fs::write(root.join("source.cpp"), "keep").unwrap();
        let output = prepare_owned(&root).unwrap();
        fs::write(output.join("artifact"), "generated").unwrap();
        clean(&root).unwrap();
        assert!(root.join("source.cpp").exists());
        assert!(root.join("build/.rmake.lock").exists());
        assert!(!output.exists());
    }

    #[test]
    fn competing_build_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let _first = lock(temp.path()).unwrap();
        assert_eq!(lock(temp.path()).err().unwrap().code, "RMAKE-O103");
    }

    #[cfg(unix)]
    #[test]
    fn clean_does_not_follow_output_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("important"), "keep").unwrap();
        fs::create_dir(temp.path().join("build")).unwrap();
        std::os::unix::fs::symlink(other.path(), temp.path().join("build/rmake")).unwrap();
        assert!(clean(temp.path()).is_err());
        assert!(other.path().join("important").exists());
    }
}
