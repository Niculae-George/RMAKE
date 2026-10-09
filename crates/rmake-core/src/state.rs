//! Persistent action receipts. A missing or malformed receipt always means stale.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use rmake_model::{ActionKind, BuildAction, Diagnostic, Result};
use serde::{Deserialize, Serialize};

use crate::fingerprint::{self, CurrentFingerprint};

const RECEIPT_VERSION: u32 = 1;

#[derive(Clone)]
pub struct ActionState {
    root: PathBuf,
    owned: PathBuf,
    identity: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    key: String,
    fingerprint: String,
    output_digest: String,
    depfile_digest: Option<String>,
}

impl ActionState {
    pub fn new(root: &Path, owned: &Path, identity: &[u8]) -> Result<Self> {
        if !owned.starts_with(root) {
            return Err(Diagnostic::new(
                "RMAKE-S203",
                "state directory escaped project root",
            ));
        }
        validate_directory_chain(root, owned)?;
        Ok(Self {
            root: root.to_path_buf(),
            owned: owned.to_path_buf(),
            identity: identity.to_vec(),
        })
    }

    pub fn is_fresh(&self, action: &BuildAction) -> Result<bool> {
        self.validate_owned()?;
        let directory = self.owned.join("state");
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(state_io("inspect state directory", &directory, e)),
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(Diagnostic::new(
                    "RMAKE-S203",
                    format!(
                        "state directory is not a real directory: {}",
                        directory.display()
                    ),
                ));
            }
            Ok(_) => {}
        }
        let path = self.receipt_path(action);
        let bytes = match read_receipt_file(&path)? {
            Some(bytes) => bytes,
            None => return Ok(false),
        };
        let receipt: Receipt = match toml::from_str(std::str::from_utf8(&bytes).unwrap_or("")) {
            Ok(receipt) => receipt,
            Err(_) => return Ok(false),
        };
        let key = fingerprint::action_key(action);
        if receipt.version != RECEIPT_VERSION || receipt.key != key {
            return Ok(false);
        }
        let Some(current) = fingerprint::current(&self.root, &self.owned, &self.identity, action)?
        else {
            return Ok(false);
        };
        Ok(matches_receipt(&receipt, &current))
    }

    /// Record only after caller publishes output and dependency files atomically.
    pub fn record(&self, action: &BuildAction) -> Result<()> {
        self.validate_owned()?;
        let current = fingerprint::current(&self.root, &self.owned, &self.identity, action)?
            .ok_or_else(|| Diagnostic::new("RMAKE-S204", "cannot record action without regular output, inputs, and a valid dependency file"))?;
        let output_digest = current
            .output_digest
            .clone()
            .ok_or_else(|| Diagnostic::new("RMAKE-S204", "action output is missing"))?;
        if let ActionKind::Compile { depfile, .. } = &action.kind {
            let depfile = fingerprint::resolve(depfile, &action.command.cwd);
            ensure_regular(&depfile, "dependency file")?;
        }
        let directory = self.ensure_state_directory()?;
        let path = self.receipt_path(action);
        reject_non_regular_existing(&path)?;
        let receipt = Receipt {
            version: RECEIPT_VERSION,
            key: fingerprint::action_key(action),
            fingerprint: current.digest,
            output_digest,
            depfile_digest: current.depfile_digest,
        };
        let contents = toml::to_string(&receipt).map_err(|e| {
            Diagnostic::new(
                "RMAKE-S204",
                format!("cannot serialize action receipt: {e}"),
            )
        })?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".receipt-")
            .tempfile_in(&directory)
            .map_err(|e| state_io("create receipt temporary file in", &directory, e))?;
        temporary
            .write_all(contents.as_bytes())
            .map_err(|e| state_io("write receipt temporary file", temporary.path(), e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| state_io("sync receipt temporary file", temporary.path(), e))?;
        let temp_path = temporary.into_temp_path();
        fs::rename(&temp_path, &path).map_err(|e| state_io("publish receipt", &path, e))?;
        Ok(())
    }

    /// Remove a receipt before a caller begins replacing this action's output.
    pub fn invalidate(&self, action: &BuildAction) -> Result<()> {
        self.validate_owned()?;
        let directory = self.owned.join("state");
        match fs::symlink_metadata(&directory) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(state_io("inspect state directory", &directory, e)),
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(Diagnostic::new(
                    "RMAKE-S203",
                    format!(
                        "state directory is not a real directory: {}",
                        directory.display()
                    ),
                ));
            }
            Ok(_) => {}
        }
        let path = self.receipt_path(action);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(state_io("inspect action receipt", &path, e)),
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(Diagnostic::new(
                "RMAKE-S203",
                format!("action receipt is not a regular file: {}", path.display()),
            )),
            Ok(_) => {
                fs::remove_file(&path).map_err(|e| state_io("remove action receipt", &path, e))
            }
        }
    }

    fn receipt_path(&self, action: &BuildAction) -> PathBuf {
        self.owned
            .join("state")
            .join(format!("{}.toml", fingerprint::action_key(action)))
    }

    fn validate_owned(&self) -> Result<()> {
        validate_directory_chain(&self.root, &self.owned)
    }

    fn ensure_state_directory(&self) -> Result<PathBuf> {
        self.validate_owned()?;
        let directory = self.owned.join("state");
        match fs::symlink_metadata(&directory) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(Diagnostic::new(
                    "RMAKE-S203",
                    format!(
                        "state directory is not a real directory: {}",
                        directory.display()
                    ),
                ));
            }
            Ok(_) => return Ok(directory),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(state_io("inspect state directory", &directory, e));
            }
            Err(_) => {}
        }
        if let Err(error) = fs::create_dir(&directory)
            && error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(state_io("create state directory", &directory, error));
        }
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|e| state_io("inspect state directory", &directory, e))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Diagnostic::new(
                "RMAKE-S203",
                format!(
                    "state directory is not a real directory: {}",
                    directory.display()
                ),
            ));
        }
        Ok(directory)
    }
}

fn matches_receipt(receipt: &Receipt, current: &CurrentFingerprint) -> bool {
    receipt.fingerprint == current.digest
        && receipt.output_digest == current.output_digest.as_deref().unwrap_or_default()
        && receipt.depfile_digest == current.depfile_digest
}

fn read_receipt_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(state_io("inspect action receipt", path, e)),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(Diagnostic::new(
            "RMAKE-S203",
            format!("action receipt is not a regular file: {}", path.display()),
        )),
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|e| state_io("read action receipt", path, e)),
    }
}

fn reject_non_regular_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(state_io("inspect action receipt", path, e)),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(Diagnostic::new(
            "RMAKE-S203",
            format!("action receipt is not a regular file: {}", path.display()),
        )),
        Ok(_) => Ok(()),
    }
}

fn ensure_regular(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(Diagnostic::new(
            "RMAKE-S204",
            format!("{label} is not a regular file: {}", path.display()),
        )),
        Err(e) => Err(state_io(&format!("inspect {label}"), path, e)),
    }
}

fn validate_directory_chain(root: &Path, owned: &Path) -> Result<()> {
    let root_meta =
        fs::symlink_metadata(root).map_err(|e| state_io("inspect project root", root, e))?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(Diagnostic::new(
            "RMAKE-S203",
            format!("project root is not a real directory: {}", root.display()),
        ));
    }
    let relative = owned
        .strip_prefix(root)
        .map_err(|_| Diagnostic::new("RMAKE-S203", "state directory escaped project root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|e| state_io("inspect owned directory", &current, e))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Diagnostic::new(
                "RMAKE-S203",
                format!(
                    "owned directory is not a real directory: {}",
                    current.display()
                ),
            ));
        }
    }
    Ok(())
}

fn state_io(operation: &str, path: &Path, error: std::io::Error) -> Diagnostic {
    Diagnostic::new(
        "RMAKE-S203",
        format!("cannot {operation} {}: {error}", path.display()),
    )
}
