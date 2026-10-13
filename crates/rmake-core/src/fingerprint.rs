//! Stable action fingerprints and current-input snapshots.
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use rmake_model::{ActionKind, BuildAction, Diagnostic, Result};

#[derive(Clone, Debug)]
pub(crate) struct CurrentFingerprint {
    pub digest: String,
    pub depfile_digest: Option<String>,
    pub output_digest: Option<String>,
}

pub(crate) fn action_key(action: &BuildAction) -> String {
    let mut h = blake3::Hasher::new();
    put(&mut h, b"rmake-action-key-v1");
    put_os(&mut h, OsStr::new(&action.id));
    put_os(&mut h, OsStr::new(&action.target));
    put_path(&mut h, &action.output);
    put_path(&mut h, &action.command.cwd);
    put_path(&mut h, &action.command.program);
    put(&mut h, &(action.command.args.len() as u64).to_le_bytes());
    for arg in &action.command.args {
        put_os(&mut h, arg);
    }
    match &action.kind {
        ActionKind::Compile {
            source,
            depfile,
            include_dirs,
        } => {
            put(&mut h, b"compile");
            put_path(&mut h, &source.path);
            put_path(&mut h, depfile);
            put(&mut h, &(include_dirs.len() as u64).to_le_bytes());
            for dir in include_dirs {
                put_path(&mut h, dir);
            }
        }
        ActionKind::Archive => put(&mut h, b"archive"),
        ActionKind::Link => put(&mut h, b"link"),
    }
    h.finalize().to_hex().to_string()
}

pub(crate) fn current(
    root: &Path,
    owned: &Path,
    identity: &[u8],
    action: &BuildAction,
) -> Result<Option<CurrentFingerprint>> {
    let mut h = blake3::Hasher::new();
    put(&mut h, b"rmake-action-state-v1");
    put(&mut h, identity);
    put(&mut h, action_key(action).as_bytes());
    put_path(&mut h, root);
    put_os(&mut h, action.command.program.as_os_str());
    hash_driver(&mut h, &action.command.program, &action.command.cwd)?;
    hash_environment(&mut h);
    let mut file_inputs = BTreeSet::new();
    for path in &action.inputs {
        file_inputs.insert(resolve(path, &action.command.cwd));
    }
    if let ActionKind::Compile {
        source, depfile, ..
    } = &action.kind
    {
        file_inputs.insert(resolve(&source.path, &action.command.cwd));
        let depfile_path = resolve(depfile, &action.command.cwd);
        let dep_bytes = match read_regular(&depfile_path, "dependency file")? {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        let dep_digest = blake3::hash(&dep_bytes).to_hex().to_string();
        let dependencies = match crate::depfile::parse_depfile(&dep_bytes, &action.command.cwd) {
            Ok(paths) => paths,
            Err(_) => return Ok(None),
        };
        for dependency in dependencies {
            file_inputs.insert(resolve(&dependency, &action.command.cwd));
        }
        hash_sdk_settings(&mut h, action, &file_inputs)?;
        put(&mut h, b"depfile");
        put(&mut h, dep_digest.as_bytes());
        put(&mut h, &(file_inputs.len() as u64).to_le_bytes());
        for path in &file_inputs {
            if !hash_file(&mut h, path, "compile input")? {
                return Ok(None);
            }
        }
        hash_membership(&mut h, root, owned, action, &file_inputs)?;
        let output_digest = match hash_file_digest(
            &resolve(&action.output, &action.command.cwd),
            "action output",
        )? {
            Some(digest) => Some(digest),
            None => return Ok(None),
        };
        Ok(Some(CurrentFingerprint {
            digest: h.finalize().to_hex().to_string(),
            depfile_digest: Some(dep_digest),
            output_digest,
        }))
    } else {
        hash_sdk_settings(&mut h, action, &file_inputs)?;
        put(&mut h, &(file_inputs.len() as u64).to_le_bytes());
        for path in &file_inputs {
            if !hash_file(&mut h, path, "action input")? {
                return Ok(None);
            }
        }
        let output_digest = match hash_file_digest(
            &resolve(&action.output, &action.command.cwd),
            "action output",
        )? {
            Some(digest) => Some(digest),
            None => return Ok(None),
        };
        Ok(Some(CurrentFingerprint {
            digest: h.finalize().to_hex().to_string(),
            depfile_digest: None,
            output_digest,
        }))
    }
}

pub(crate) fn resolve(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn hash_file(h: &mut blake3::Hasher, path: &Path, label: &str) -> Result<bool> {
    let Some(digest) = hash_input_digest(path, label)? else {
        return Ok(false);
    };
    put_path(h, path);
    put(h, digest.as_bytes());
    Ok(true)
}

fn read_regular(path: &Path, label: &str) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Diagnostic::new(
                "RMAKE-S202",
                format!("cannot inspect {label} {}: {e}", path.display()),
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    fs::read(path).map(Some).map_err(|e| {
        Diagnostic::new(
            "RMAKE-S202",
            format!("cannot read {label} {}: {e}", path.display()),
        )
    })
}

fn hash_input_digest(path: &Path, label: &str) -> Result<Option<String>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Diagnostic::new(
                "RMAKE-S202",
                format!("cannot inspect {label} {}: {e}", path.display()),
            ));
        }
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut file = fs::File::open(path).map_err(|e| {
        Diagnostic::new(
            "RMAKE-S202",
            format!("cannot read {label} {}: {e}", path.display()),
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|e| {
            Diagnostic::new(
                "RMAKE-S202",
                format!("cannot read {label} {}: {e}", path.display()),
            )
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

fn hash_file_digest(path: &Path, label: &str) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Diagnostic::new(
                "RMAKE-S202",
                format!("cannot inspect {label} {}: {e}", path.display()),
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let mut file = fs::File::open(path).map_err(|e| {
        Diagnostic::new(
            "RMAKE-S202",
            format!("cannot read {label} {}: {e}", path.display()),
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|e| {
            Diagnostic::new(
                "RMAKE-S202",
                format!("cannot read {label} {}: {e}", path.display()),
            )
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

fn hash_driver(h: &mut blake3::Hasher, program: &Path, cwd: &Path) -> Result<()> {
    let resolved = resolve_program(program, cwd).ok_or_else(|| {
        Diagnostic::new(
            "RMAKE-S202",
            format!("cannot resolve compiler executable {}", program.display()),
        )
    })?;
    let canonical = fs::canonicalize(&resolved).map_err(|e| {
        Diagnostic::new(
            "RMAKE-S202",
            format!(
                "cannot resolve compiler executable {}: {e}",
                resolved.display()
            ),
        )
    })?;
    let digest = hash_file_digest(&canonical, "compiler executable")?.ok_or_else(|| {
        Diagnostic::new(
            "RMAKE-S202",
            format!(
                "compiler executable is not a regular file: {}",
                canonical.display()
            ),
        )
    })?;
    put_path(h, &canonical);
    put(h, digest.as_bytes());
    Ok(())
}

fn resolve_program(program: &Path, cwd: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        let candidate = resolve(program, cwd);
        return candidate.exists().then_some(candidate);
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn hash_environment(h: &mut blake3::Hasher) {
    let ignored = [
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
        "TERM",
        "COLORTERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
    ];
    let mut values = BTreeMap::<OsString, OsString>::new();
    for (key, value) in env::vars_os() {
        if key.to_str().is_some_and(|key| ignored.contains(&key)) {
            continue;
        }
        values.insert(key, value);
    }
    put(h, b"environment");
    put(h, &(values.len() as u64).to_le_bytes());
    for (key, value) in values {
        put_os(h, &key);
        put_os(h, &value);
    }
}

fn hash_sdk_settings(
    h: &mut blake3::Hasher,
    action: &BuildAction,
    inputs: &BTreeSet<PathBuf>,
) -> Result<()> {
    let roots = sdk_roots(action, inputs);
    for root in roots {
        put_path(h, &root);
        for name in ["SDKSettings.json", "SDKSettings.plist"] {
            let path = root.join(name);
            if let Some(digest) = hash_file_digest(&path, "SDK settings")? {
                put_path(h, &path);
                put(h, digest.as_bytes());
            }
        }
    }
    for key in ["SDKROOT", "DEVELOPER_DIR"] {
        if let Some(value) = env::var_os(key) {
            put_os(h, OsStr::new(key));
            put_os(h, &value);
        }
    }
    Ok(())
}

fn hash_membership(
    h: &mut blake3::Hasher,
    root: &Path,
    owned: &Path,
    action: &BuildAction,
    inputs: &BTreeSet<PathBuf>,
) -> Result<()> {
    let project_root = fs::canonicalize(root).map_err(|e| {
        Diagnostic::new(
            "RMAKE-S202",
            format!("cannot resolve project root {}: {e}", root.display()),
        )
    })?;
    let canonical_owned = fs::canonicalize(owned).ok();
    let mut project_directories = BTreeSet::new();
    let mut include_directories = BTreeSet::new();
    if let ActionKind::Compile {
        source,
        include_dirs,
        ..
    } = &action.kind
    {
        let source = resolve(&source.path, &action.command.cwd);
        if let Some(parent) = source.parent() {
            project_directories.insert(parent.to_path_buf());
        }
        for dir in include_dirs {
            include_directories.insert(resolve(dir, &action.command.cwd));
        }
        for path in inputs {
            if let Some(parent) = path.parent() {
                project_directories.insert(parent.to_path_buf());
            }
        }
    }
    let sdk_roots = sdk_roots(action, inputs);
    let mut snapshot = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut local_roots = BTreeSet::new();

    let candidate_directories = project_directories
        .iter()
        .chain(include_directories.iter())
        .cloned()
        .collect::<Vec<_>>();
    for directory in candidate_directories {
        snapshot_symlink_ancestors(&mut snapshot, &directory)?;
        let canonical = match fs::canonicalize(&directory) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                snapshot.insert(directory.clone(), b"missing-directory".to_vec());
                continue;
            }
            Err(error) => {
                return Err(Diagnostic::new(
                    "RMAKE-S202",
                    format!(
                        "cannot resolve include directory {}: {error}",
                        directory.display()
                    ),
                ));
            }
        };
        if directory.starts_with(owned)
            || canonical_owned
                .as_ref()
                .is_some_and(|owned| canonical.starts_with(owned))
        {
            continue;
        }
        if canonical.starts_with(&project_root) {
            local_roots.insert(canonical);
        } else if !sdk_roots
            .iter()
            .any(|sdk| directory.starts_with(sdk) || canonical.starts_with(sdk))
            && !is_sdk_path(&canonical)
            && include_directories.contains(&directory)
            && !is_system_path(&canonical)
        {
            snapshot_external_directory(&mut snapshot, &canonical)?;
        }
    }

    let mut roots = Vec::new();
    for candidate in local_roots {
        if !roots
            .iter()
            .any(|parent: &PathBuf| candidate.starts_with(parent))
        {
            roots.push(candidate);
        }
    }
    for traversal_root in roots {
        let entries = walkdir::WalkDir::new(&traversal_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.path() == traversal_root {
                    return true;
                }
                let path = entry.path();
                if canonical_owned
                    .as_ref()
                    .is_some_and(|owned| path.starts_with(owned))
                    || path == project_root.join("build/.rmake.lock")
                {
                    return false;
                }
                true
            });
        for entry in entries {
            let entry = entry.map_err(|error| {
                Diagnostic::new(
                    "RMAKE-S202",
                    format!(
                        "cannot inspect project include tree {}: {error}",
                        traversal_root.display()
                    ),
                )
            })?;
            let path = entry.path();
            if path == traversal_root || path == project_root.join("build/.rmake.lock") {
                continue;
            }
            let kind = entry.file_type();
            let mut value = if kind.is_dir() {
                b"directory".to_vec()
            } else if kind.is_symlink() {
                b"symlink".to_vec()
            } else if kind.is_file() {
                b"file".to_vec()
            } else {
                continue;
            };
            if kind.is_symlink() {
                let target = fs::read_link(path).map_err(|e| {
                    Diagnostic::new(
                        "RMAKE-S202",
                        format!("cannot inspect include symlink {}: {e}", path.display()),
                    )
                })?;
                value.extend_from_slice(os_bytes(target.as_os_str()));
            }
            snapshot.insert(path.to_path_buf(), value);
        }
    }

    put(h, b"include-membership");
    put(h, &(snapshot.len() as u64).to_le_bytes());
    for (path, value) in snapshot {
        put_path(h, &path);
        put(h, &value);
    }
    Ok(())
}

fn snapshot_symlink_ancestors(
    snapshot: &mut BTreeMap<PathBuf, Vec<u8>>,
    path: &Path,
) -> Result<()> {
    for ancestor in path.ancestors() {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Diagnostic::new(
                    "RMAKE-S202",
                    format!(
                        "cannot inspect include path {}: {error}",
                        ancestor.display()
                    ),
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(ancestor).map_err(|error| {
                Diagnostic::new(
                    "RMAKE-S202",
                    format!(
                        "cannot inspect include symlink {}: {error}",
                        ancestor.display()
                    ),
                )
            })?;
            let mut value = b"symlink".to_vec();
            value.extend_from_slice(os_bytes(target.as_os_str()));
            snapshot.insert(ancestor.to_path_buf(), value);
        }
    }
    Ok(())
}

fn snapshot_external_directory(
    snapshot: &mut BTreeMap<PathBuf, Vec<u8>>,
    directory: &Path,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            snapshot.insert(directory.to_path_buf(), b"missing-directory".to_vec());
            return Ok(());
        }
        Err(error) => {
            return Err(Diagnostic::new(
                "RMAKE-S202",
                format!(
                    "cannot inspect include directory {}: {error}",
                    directory.display()
                ),
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            Diagnostic::new(
                "RMAKE-S202",
                format!(
                    "cannot inspect include directory {}: {error}",
                    directory.display()
                ),
            )
        })?;
        let kind = entry.file_type().map_err(|error| {
            Diagnostic::new(
                "RMAKE-S202",
                format!(
                    "cannot inspect include entry {}: {error}",
                    entry.path().display()
                ),
            )
        })?;
        let mut value = if kind.is_dir() {
            b"directory".to_vec()
        } else if kind.is_symlink() {
            b"symlink".to_vec()
        } else if kind.is_file() {
            b"file".to_vec()
        } else {
            continue;
        };
        if kind.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|error| {
                Diagnostic::new(
                    "RMAKE-S202",
                    format!(
                        "cannot inspect include symlink {}: {error}",
                        entry.path().display()
                    ),
                )
            })?;
            value.extend_from_slice(os_bytes(target.as_os_str()));
        }
        snapshot.insert(entry.path(), value);
    }
    Ok(())
}

fn sdk_roots(action: &BuildAction, inputs: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    if let Some(root) = env::var_os("SDKROOT") {
        roots.insert(PathBuf::from(root));
    }
    let args = action
        .command
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for pair in args.windows(2) {
        if pair[0] == "-isysroot" {
            roots.insert(PathBuf::from(&pair[1]));
        }
    }
    for arg in args {
        if let Some(value) = arg.strip_prefix("--sysroot=") {
            roots.insert(PathBuf::from(value));
        }
    }
    for input in inputs {
        for ancestor in input.ancestors() {
            if ancestor.extension().is_some_and(|ext| ext == "sdk") {
                roots.insert(ancestor.to_path_buf());
                break;
            }
        }
    }
    roots
}

fn is_sdk_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        value == "SDK" || value == "SDKs" || value.ends_with(".sdk")
    })
}

fn is_system_path(path: &Path) -> bool {
    path.starts_with("/usr")
        || path.starts_with("/System")
        || path.starts_with("/Library/Developer")
        || is_sdk_path(path)
}

fn put_path(h: &mut blake3::Hasher, path: &Path) {
    put_os(h, path.as_os_str());
}
fn put_os(h: &mut blake3::Hasher, value: &OsStr) {
    put(h, os_bytes(value));
}
fn put(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}
#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> &[u8] {
    value.to_str().unwrap_or("").as_bytes()
}

/// Hash explicit inputs and previously observed dependencies around native work.
/// Keeping the path set fixed catches edits without treating a new depfile as an edit.
pub(crate) fn observed_inputs(action: &BuildAction) -> Vec<PathBuf> {
    let mut paths: BTreeSet<_> = action.inputs.iter().cloned().collect();
    if let ActionKind::Compile { depfile, .. } = &action.kind
        && let Ok(bytes) = fs::read(depfile)
        && let Ok(headers) = crate::depfile::parse_depfile(&bytes, &action.command.cwd)
    {
        paths.extend(headers);
    }
    paths.into_iter().collect()
}
pub(crate) fn input_snapshot(paths: &[PathBuf]) -> Result<Vec<Option<String>>> {
    paths
        .iter()
        .map(|path| hash_input_digest(path, "active build input"))
        .collect()
}
