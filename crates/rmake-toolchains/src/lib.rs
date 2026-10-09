//! Native GCC and Clang toolchain discovery and command construction.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use rmake_model::{
    ArchiveRequest, Archiver, CommandSpec, CompileRequest, CompilerFamily, CompilerOptions,
    Diagnostic, IoMode, Language, LinkRequest, Optimization, ProcessRunner, Result, Toolchain,
    ToolchainInfo,
};
use serde::Deserialize;

const CONFIG_PATH: &str = ".rmake/config.toml";

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: Option<u32>,
    toolchain: Option<ConfigToolchain>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigToolchain {
    #[serde(rename = "c-compiler")]
    c_compiler: Option<PathBuf>,
    #[serde(rename = "cpp-compiler")]
    cpp_compiler: Option<PathBuf>,
    archiver: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct GnuToolchain {
    info: ToolchainInfo,
    archiver: PathBuf,
    archiver_probe: Option<String>,
    c_probe_version: Option<String>,
    cpp_probe_version: Option<String>,
    #[cfg(target_os = "macos")]
    sdk_probe: SdkProbe,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
struct SdkProbe {
    path: PathBuf,
    version: String,
    settings_files: Vec<(PathBuf, Vec<u8>)>,
}

#[derive(Debug)]
struct Probe {
    path: PathBuf,
    family: CompilerFamily,
    version: String,
    target: String,
    arch: String,
    os: String,
}

/// Discover and validate native C/C++ drivers for requested source languages.
/// An empty language list is used by doctor and probes both drivers.
pub fn detect(
    project_root: Option<&Path>,
    languages: &[Language],
    overrides: &CompilerOptions,
    runner: &dyn ProcessRunner,
) -> Result<GnuToolchain> {
    let root = project_root.unwrap_or_else(|| Path::new("."));
    let root = absolute_lexical(root)?;
    let config = read_config(project_root)?;
    let configured = config.as_ref().and_then(|c| c.toolchain.as_ref());

    let wants_c = languages.is_empty() || languages.contains(&Language::C);
    let wants_cpp = languages.is_empty() || languages.contains(&Language::Cpp);
    let c_selection = wants_c.then(|| {
        choose_compiler(
            overrides.c_compiler.as_ref(),
            configured.and_then(|t| t.c_compiler.as_ref()),
            env::var_os("CC").map(PathBuf::from).as_ref(),
            Path::new("cc"),
        )
    });
    let cpp_selection = wants_cpp.then(|| {
        choose_compiler(
            overrides.cpp_compiler.as_ref(),
            configured.and_then(|t| t.cpp_compiler.as_ref()),
            env::var_os("CXX").map(PathBuf::from).as_ref(),
            Path::new("c++"),
        )
    });
    let archiver_selection = choose_compiler(
        overrides.archiver.as_ref(),
        configured.and_then(|t| t.archiver.as_ref()),
        env::var_os("AR").map(PathBuf::from).as_ref(),
        Path::new("ar"),
    );

    let c_probe = c_selection
        .as_deref()
        .map(|selection| probe(selection, "C", &root, runner))
        .transpose()?;
    let cpp_probe = cpp_selection
        .as_deref()
        .map(|selection| probe(selection, "C++", &root, runner))
        .transpose()?;
    let archiver = resolve_executable(&archiver_selection, &root).map_err(|message| {
        Diagnostic::new(
            "toolchain-archiver",
            format!(
                "cannot use archiver selection {}: {message}",
                archiver_selection.display()
            ),
        )
    })?;
    let archiver_probe = probe_archiver(&archiver, &root, runner);
    #[cfg(target_os = "macos")]
    let sdk_probe = probe_default_macos_sdk(&root, runner)?;

    let representative = c_probe
        .as_ref()
        .or(cpp_probe.as_ref())
        .ok_or_else(|| Diagnostic::new("toolchain", "no compiler driver selected"))?;
    validate_platform(
        &representative.arch,
        &representative.os,
        &representative.target,
    )?;

    if let (Some(c), Some(cpp)) = (&c_probe, &cpp_probe) {
        if c.family != cpp.family {
            return Err(Diagnostic::new(
                "toolchain-mismatch",
                format!(
                    "C driver {} is {:?}, while C++ driver {} is {:?}; choose drivers from one compiler family",
                    c.path.display(),
                    c.family,
                    cpp.path.display(),
                    cpp.family
                ),
            ));
        }
        if c.arch != cpp.arch || c.os != cpp.os {
            return Err(Diagnostic::new(
                "toolchain-mismatch",
                format!(
                    "C driver {} targets {} ({}), while C++ driver {} targets {} ({}); native architecture and OS must match",
                    c.path.display(),
                    c.target,
                    c.arch,
                    cpp.path.display(),
                    cpp.target,
                    cpp.arch
                ),
            ));
        }
    }

    let version = c_probe
        .as_ref()
        .or(cpp_probe.as_ref())
        .map(|p| p.version.clone())
        .unwrap_or_default();
    let target = c_probe
        .as_ref()
        .or(cpp_probe.as_ref())
        .map(|p| p.target.clone())
        .unwrap_or_default();
    let c_probe_version = c_probe.as_ref().map(|p| p.version.clone());
    let cpp_probe_version = cpp_probe.as_ref().map(|p| p.version.clone());
    Ok(GnuToolchain {
        archiver,
        archiver_probe,
        c_probe_version,
        cpp_probe_version,
        #[cfg(target_os = "macos")]
        sdk_probe,
        info: ToolchainInfo {
            family: representative.family,
            c_driver: c_probe.map(|p| p.path),
            cpp_driver: cpp_probe.map(|p| p.path),
            version,
            target,
        },
    })
}

impl Toolchain for GnuToolchain {
    fn info(&self) -> &ToolchainInfo {
        &self.info
    }

    fn compile_command(&self, request: &CompileRequest) -> Result<CommandSpec> {
        let program = match request.source.language {
            Language::C => self.info.c_driver.as_ref(),
            Language::Cpp => self.info.cpp_driver.as_ref(),
        }
        .ok_or_else(|| {
            Diagnostic::new(
                "toolchain-driver",
                format!(
                    "no {:?} driver was selected for this project",
                    request.source.language
                ),
            )
        })?
        .clone();

        let standard = match request.source.language {
            Language::C => &request.standards.c,
            Language::Cpp => &request.standards.cpp,
        };
        let mut args = vec![OsString::from(format!("-std={standard}"))];
        add_profile_flags(&mut args, &request.profile);
        args.push("-Wall".into());
        args.push("-Wextra".into());
        for include in &request.include_dirs {
            args.push("-I".into());
            args.push(include.as_os_str().to_owned());
        }
        for (name, value) in &request.definitions {
            args.push(format!("-D{name}={value}").into());
        }
        args.extend(request.options.iter().cloned());
        args.extend([
            OsString::from("-MD"),
            OsString::from("-MF"),
            request.depfile.as_os_str().to_owned(),
            OsString::from("-MQ"),
            request.output.as_os_str().to_owned(),
        ]);
        args.push("-c".into());
        args.push(request.source.path.as_os_str().to_owned());
        args.push("-o".into());
        args.push(request.output.as_os_str().to_owned());

        Ok(CommandSpec {
            program,
            args,
            cwd: request.cwd.clone(),
            label: format!("compile {}", request.source.path.display()),
        })
    }

    fn link_command(&self, request: &LinkRequest) -> Result<CommandSpec> {
        let program = match request.language {
            Language::C => self.info.c_driver.as_ref(),
            Language::Cpp => self.info.cpp_driver.as_ref(),
        }
        .ok_or_else(|| {
            Diagnostic::new(
                "toolchain-driver",
                format!("no {:?} driver was selected for linking", request.language),
            )
        })?
        .clone();
        let mut args = Vec::new();
        add_profile_flags(&mut args, &request.profile);
        args.extend(
            request
                .inputs
                .iter()
                .map(|path| path.as_os_str().to_owned()),
        );
        args.extend(request.options.iter().cloned());
        args.push("-o".into());
        args.push(request.output.as_os_str().to_owned());
        Ok(CommandSpec {
            program,
            args,
            cwd: request.cwd.clone(),
            label: format!("link {}", request.output.display()),
        })
    }
}

impl Archiver for GnuToolchain {
    fn identity(&self) -> Vec<u8> {
        self.identity_material()
    }
    fn archive_command(&self, request: &ArchiveRequest) -> Result<CommandSpec> {
        let mut args = vec![OsString::from("rcs"), request.output.as_os_str().to_owned()];
        args.extend(
            request
                .objects
                .iter()
                .map(|path| path.as_os_str().to_owned()),
        );
        Ok(CommandSpec {
            program: self.archiver.clone(),
            args,
            cwd: request.cwd.clone(),
            label: format!("archive {}", request.output.display()),
        })
    }
}

impl GnuToolchain {
    pub fn archiver_path(&self) -> &Path {
        &self.archiver
    }
    /// Stable probe material for callers that include tool identity in cache keys.
    pub fn identity_material(&self) -> Vec<u8> {
        let mut identity = format!(
            "family={:?}\nc={:?}\nc-probe-version={:?}\ncxx={:?}\ncxx-probe-version={:?}\nversion={}\ntarget={}\nar={}\nar-probe={}\n",
            self.info.family,
            self.info.c_driver,
            self.c_probe_version,
            self.info.cpp_driver,
            self.cpp_probe_version,
            self.info.version,
            self.info.target,
            self.archiver.display(),
            self.archiver_probe.as_deref().unwrap_or("")
        )
        .into_bytes();
        #[cfg(target_os = "macos")]
        {
            append_identity_field(
                &mut identity,
                b"macos-sdk-path",
                self.sdk_probe.path.as_os_str().as_encoded_bytes(),
            );
            append_identity_field(
                &mut identity,
                b"macos-sdk-version",
                self.sdk_probe.version.as_bytes(),
            );
            for (path, contents) in &self.sdk_probe.settings_files {
                append_identity_field(
                    &mut identity,
                    b"macos-sdk-settings-path",
                    path.as_os_str().as_encoded_bytes(),
                );
                append_identity_field(&mut identity, b"macos-sdk-settings-content", contents);
            }
        }
        identity
    }
}

fn append_identity_field(identity: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    identity.extend_from_slice(&(name.len() as u64).to_le_bytes());
    identity.extend_from_slice(name);
    identity.extend_from_slice(&(value.len() as u64).to_le_bytes());
    identity.extend_from_slice(value);
}

fn add_profile_flags(args: &mut Vec<OsString>, profile: &rmake_model::ProfileSettings) {
    args.push(match profile.optimization {
        Optimization::None => "-O0".into(),
        Optimization::Speed => "-O2".into(),
    });
    if profile.debug {
        args.push("-g".into());
    }
}

fn choose_compiler(
    override_path: Option<&PathBuf>,
    config_path: Option<&PathBuf>,
    environment_path: Option<&PathBuf>,
    default_path: &Path,
) -> PathBuf {
    override_path
        .or(config_path)
        .or(environment_path)
        .cloned()
        .unwrap_or_else(|| default_path.to_path_buf())
}

fn read_config(project_root: Option<&Path>) -> Result<Option<Config>> {
    let Some(root) = project_root else {
        return Ok(None);
    };
    let path = root.join(CONFIG_PATH);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Diagnostic::new(
                "toolchain-config",
                format!("cannot read {}: {error}", path.display()),
            ));
        }
    };
    let config: Config = toml::from_str(&contents).map_err(|error| {
        Diagnostic::new(
            "toolchain-config",
            format!("invalid {}: {error}", path.display()),
        )
    })?;
    if config.schema != Some(1) {
        return Err(Diagnostic::new(
            "toolchain-config",
            format!("{} must declare schema = 1", path.display()),
        ));
    }
    Ok(Some(config))
}

fn probe(
    selection: &Path,
    language: &str,
    root: &Path,
    runner: &dyn ProcessRunner,
) -> Result<Probe> {
    let path = resolve_executable(selection, root).map_err(|message| {
        Diagnostic::new(
            "toolchain-driver",
            format!(
                "cannot use {language} compiler selection {}: {message}",
                selection.display()
            ),
        )
    })?;
    let cwd = root.to_path_buf();
    let version_result = runner.run(
        &CommandSpec {
            program: path.clone(),
            args: vec!["--version".into()],
            cwd: cwd.clone(),
            label: format!("identify {language} compiler {}", path.display()),
        },
        IoMode::Capture,
    )?;
    if version_result.code != 0 {
        return Err(Diagnostic::new(
            "toolchain-probe",
            format!(
                "{} --version failed with status {}: {}",
                path.display(),
                version_result.code,
                output_text(&version_result.stdout, &version_result.stderr)
            ),
        ));
    }
    let banner = output_text(&version_result.stdout, &version_result.stderr);
    let family = classify_family(&banner).ok_or_else(|| {
        Diagnostic::new(
            "toolchain-family",
            format!(
                "unsupported compiler family for {}: {}",
                path.display(),
                first_line(&banner)
            ),
        )
    })?;
    let target_result = runner.run(
        &CommandSpec {
            program: path.clone(),
            args: vec!["-dumpmachine".into()],
            cwd,
            label: format!("inspect target for {}", path.display()),
        },
        IoMode::Capture,
    )?;
    let target = String::from_utf8_lossy(&target_result.stdout)
        .trim()
        .to_owned();
    if target_result.code != 0 || target.is_empty() {
        return Err(Diagnostic::new(
            "toolchain-probe",
            format!(
                "{} -dumpmachine failed or returned no target (status {}): {}",
                path.display(),
                target_result.code,
                String::from_utf8_lossy(&target_result.stderr).trim()
            ),
        ));
    }
    let (arch, os) = parse_target(&target).ok_or_else(|| {
        Diagnostic::new(
            "toolchain-target",
            format!(
                "cannot interpret target `{target}` reported by {}",
                path.display()
            ),
        )
    })?;
    Ok(Probe {
        path,
        family,
        version: first_line(&banner).to_owned(),
        target,
        arch,
        os,
    })
}

// `ar --version` is not supported by every system archiver (notably BSD ar),
// so treat this as optional identity material rather than a validity check.
fn probe_archiver(path: &Path, root: &Path, runner: &dyn ProcessRunner) -> Option<String> {
    let result = runner
        .run(
            &CommandSpec {
                program: path.to_path_buf(),
                args: vec!["--version".into()],
                cwd: root.to_path_buf(),
                label: format!("identify archiver {}", path.display()),
            },
            IoMode::Capture,
        )
        .ok()?;
    (result.code == 0).then(|| first_line(&output_text(&result.stdout, &result.stderr)).to_owned())
}

#[cfg(target_os = "macos")]
fn probe_default_macos_sdk(root: &Path, runner: &dyn ProcessRunner) -> Result<SdkProbe> {
    fn query(runner: &dyn ProcessRunner, root: &Path, argument: &str) -> Result<String> {
        let result = runner.run(
            &CommandSpec {
                program: PathBuf::from("xcrun"),
                args: vec!["--sdk".into(), "macosx".into(), argument.into()],
                cwd: root.to_path_buf(),
                label: format!("inspect macOS SDK ({argument})"),
            },
            IoMode::Capture,
        )?;
        let value = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        if result.code != 0 || value.is_empty() {
            return Err(Diagnostic::new(
                "toolchain-sdk",
                format!(
                    "xcrun --sdk macosx {argument} failed with status {}: {}",
                    result.code,
                    output_text(&result.stdout, &result.stderr)
                ),
            ));
        }
        Ok(value)
    }

    let path = PathBuf::from(query(runner, root, "--show-sdk-path")?);
    let version = query(runner, root, "--show-sdk-version")?;
    let mut settings_files = Vec::new();
    for name in ["SDKSettings.json", "SDKSettings.plist"] {
        let settings_path = path.join(name);
        match fs::read(&settings_path) {
            Ok(contents) => settings_files.push((settings_path, contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Diagnostic::new(
                    "toolchain-sdk",
                    format!("cannot read {}: {error}", settings_path.display()),
                ));
            }
        }
    }
    Ok(SdkProbe {
        path,
        version,
        settings_files,
    })
}

fn classify_family(banner: &str) -> Option<CompilerFamily> {
    let lower = banner.to_ascii_lowercase();
    if lower.contains("apple clang") || lower.contains("apple llvm") {
        Some(CompilerFamily::AppleClang)
    } else if lower.contains("clang") {
        Some(CompilerFamily::Clang)
    } else if lower.contains("gcc")
        || lower.contains("gnu compiler")
        || lower.contains("free software foundation")
    {
        Some(CompilerFamily::Gcc)
    } else {
        None
    }
}

fn parse_target(target: &str) -> Option<(String, String)> {
    let parts: Vec<_> = target
        .to_ascii_lowercase()
        .split('-')
        .map(str::to_owned)
        .collect();
    let arch = canonical_arch(parts.first()?.as_str())?;
    let is_macos_part = |part: &str, name: &str| {
        part == name
            || part
                .strip_prefix(name)
                .is_some_and(|suffix| suffix.starts_with(|ch: char| ch.is_ascii_digit()))
    };
    let os = if parts.iter().any(|part| {
        is_macos_part(part, "darwin")
            || is_macos_part(part, "macos")
            || is_macos_part(part, "macosx")
    }) {
        "macos"
    } else if parts.iter().any(|part| part == "linux") {
        "linux"
    } else {
        return None;
    };
    Some((arch.to_owned(), os.to_owned()))
}

fn canonical_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        "i386" | "i486" | "i586" | "i686" | "x86" => Some("x86"),
        "arm" | "armv7" | "armv7l" => Some("arm"),
        _ => None,
    }
}

fn validate_platform(arch: &str, os: &str, target: &str) -> Result<()> {
    if !cfg!(target_os = "linux") && !cfg!(target_os = "macos") {
        return Err(Diagnostic::new(
            "toolchain-platform",
            format!(
                "native GCC/Clang adapter does not support host OS `{}`",
                env::consts::OS
            ),
        ));
    }
    let native_arch = canonical_arch(env::consts::ARCH).unwrap_or(env::consts::ARCH);
    let native_os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    if arch != native_arch || os != native_os {
        return Err(Diagnostic::new(
            "toolchain-cross-target",
            format!(
                "compiler target `{target}` resolves to {arch}/{os}, but this host is {native_arch}/{native_os}; cross compilation is not supported"
            ),
        ));
    }
    Ok(())
}

fn resolve_executable(selection: &Path, root: &Path) -> std::result::Result<PathBuf, String> {
    let raw = selection.as_os_str();
    if contains_shell_syntax(raw) {
        return Err(
            "selection contains shell syntax or embedded options; provide one executable path"
                .into(),
        );
    }
    let candidate = if selection.is_absolute() {
        selection.to_path_buf()
    } else if selection.to_string_lossy().split_whitespace().count() > 1
        || selection.components().count() > 1
        || selection.components().next().is_some_and(|c| {
            matches!(
                c,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        root.join(selection)
    } else {
        let name = selection
            .file_name()
            .ok_or_else(|| "compiler selection has no file name".to_owned())?;
        find_on_path(name)
            .or_else(|| root.join(selection).exists().then(|| root.join(selection)))
            .ok_or_else(|| {
                format!(
                    "executable was not found on PATH or under {}",
                    root.display()
                )
            })?
    };
    let candidate = absolute_lexical(&candidate).map_err(|e| e.message)?;
    let metadata = fs::metadata(&candidate).map_err(|error| {
        if selection.to_string_lossy().split_whitespace().count() > 1 {
            format!("{} is not an executable path; embedded compiler options are unsupported, provide one executable path", selection.display())
        } else {
            format!("{} does not exist or cannot be inspected: {error}", candidate.display())
        }
    })?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", candidate.display()));
    }
    if !is_executable(&metadata) {
        return Err(format!("{} is not executable", candidate.display()));
    }
    Ok(candidate)
}

fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && is_executable(&m)))
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn contains_shell_syntax(value: &OsStr) -> bool {
    let Some(text) = value.to_str() else {
        return false;
    };
    text.chars()
        .any(|ch| matches!(ch, '\n' | '\r' | ';' | '|' | '&' | '$' | '`' | '<' | '>'))
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| {
                Diagnostic::new(
                    "toolchain-path",
                    format!("cannot read current directory: {error}"),
                )
            })
    }
}

fn output_text(stdout: &[u8], stderr: &[u8]) -> String {
    let out = String::from_utf8_lossy(stdout).trim().to_owned();
    if out.is_empty() {
        String::from_utf8_lossy(stderr).trim().to_string()
    } else {
        out
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}
