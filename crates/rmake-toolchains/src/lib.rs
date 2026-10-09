//! Native GCC and Clang toolchain discovery and command construction.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use rmake_model::{
    CommandSpec, CompileRequest, CompilerFamily, CompilerOptions, Diagnostic, IoMode, Language,
    LinkRequest, Optimization, ProcessRunner, Result, Toolchain, ToolchainInfo,
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
}

#[derive(Clone, Debug)]
pub struct GnuToolchain {
    info: ToolchainInfo,
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

    let c_probe = c_selection
        .as_deref()
        .map(|selection| probe(selection, "C", &root, runner))
        .transpose()?;
    let cpp_probe = cpp_selection
        .as_deref()
        .map(|selection| probe(selection, "C++", &root, runner))
        .transpose()?;

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
    Ok(GnuToolchain {
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
                .objects
                .iter()
                .map(|path| path.as_os_str().to_owned()),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmake_model::{ProfileSettings, SourceFile, Standards};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        banners: Vec<(String, String)>,
        targets: Vec<(String, String)>,
    }

    impl ProcessRunner for FakeRunner {
        fn run(&self, command: &CommandSpec, mode: IoMode) -> Result<rmake_model::ProcessResult> {
            assert!(matches!(mode, IoMode::Capture));
            self.calls.lock().unwrap().push(command.clone());
            let name = command.program.file_name().unwrap().to_string_lossy();
            let arg = command.args[0].to_string_lossy();
            let value = if arg == "--version" {
                self.banners
                    .iter()
                    .find(|(key, _)| key == &name)
                    .map(|(_, value)| value)
            } else {
                self.targets
                    .iter()
                    .find(|(key, _)| key == &name)
                    .map(|(_, value)| value)
            };
            let Some(value) = value else {
                return Err(Diagnostic::new(
                    "fake",
                    format!("unexpected probe for {name}"),
                ));
            };
            Ok(rmake_model::ProcessResult {
                code: 0,
                stdout: value.as_bytes().to_vec(),
                stderr: vec![],
            })
        }
    }

    struct LocalRunner;

    impl ProcessRunner for LocalRunner {
        fn run(&self, command: &CommandSpec, _mode: IoMode) -> Result<rmake_model::ProcessResult> {
            let output = std::process::Command::new(&command.program)
                .args(&command.args)
                .current_dir(&command.cwd)
                .output()
                .map_err(|error| {
                    Diagnostic::new(
                        "test-process",
                        format!("cannot run {}: {error}", command.program.display()),
                    )
                })?;
            Ok(rmake_model::ProcessResult {
                code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
    }

    fn native_triple() -> String {
        let arch = env::consts::ARCH;
        if cfg!(target_os = "macos") {
            format!("{arch}-apple-darwin25.2.0")
        } else {
            format!("{arch}-unknown-linux-gnu")
        }
    }

    fn fake(root: &Path, names: &[&str], family: &str) -> FakeRunner {
        for name in names {
            let path = root.join(name);
            fs::write(&path, "fake compiler").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        FakeRunner {
            calls: Mutex::new(Vec::new()),
            banners: names
                .iter()
                .map(|name| ((*name).to_owned(), family.to_owned()))
                .collect(),
            targets: names
                .iter()
                .map(|name| ((*name).to_owned(), native_triple()))
                .collect(),
        }
    }

    fn overrides(root: &Path, c: Option<&str>, cpp: Option<&str>) -> CompilerOptions {
        CompilerOptions {
            c_compiler: c.map(|name| root.join(name)),
            cpp_compiler: cpp.map(|name| root.join(name)),
        }
    }

    #[test]
    fn classifies_gcc_clang_and_apple_clang_banners() {
        assert_eq!(
            classify_family("cc (GCC) 14\nFree Software Foundation"),
            Some(CompilerFamily::Gcc)
        );
        assert_eq!(
            classify_family("clang version 18"),
            Some(CompilerFamily::Clang)
        );
        assert_eq!(
            classify_family("Apple clang version 16"),
            Some(CompilerFamily::AppleClang)
        );
        assert_eq!(classify_family("unknown compiler"), None);
    }

    #[test]
    fn parses_versioned_darwin_and_macos_target_components() {
        assert_eq!(
            parse_target("arm64-apple-darwin25.2.0"),
            Some(("aarch64".into(), "macos".into()))
        );
        assert_eq!(
            parse_target("x86_64-apple-macos14.0.0"),
            Some(("x86_64".into(), "macos".into()))
        );
        assert_eq!(
            parse_target("aarch64-apple-macosx15.1"),
            Some(("aarch64".into(), "macos".into()))
        );
    }

    #[test]
    fn detects_installed_native_cc_and_cxx_when_available() {
        let Some(cc) = find_on_path(OsStr::new("cc")) else {
            return;
        };
        let Some(cxx) = find_on_path(OsStr::new("c++")) else {
            return;
        };
        let tc = detect(
            None,
            &[Language::C, Language::Cpp],
            &CompilerOptions {
                c_compiler: Some(cc.clone()),
                cpp_compiler: Some(cxx.clone()),
            },
            &LocalRunner,
        )
        .unwrap();
        assert_eq!(tc.info().c_driver.as_deref(), Some(cc.as_path()));
        assert_eq!(tc.info().cpp_driver.as_deref(), Some(cxx.as_path()));
    }

    #[test]
    fn compiler_selection_prefers_override_then_config_then_environment() {
        let cli = PathBuf::from("cli compiler");
        let config = PathBuf::from("config compiler");
        let environment = PathBuf::from("environment compiler");
        assert_eq!(
            choose_compiler(
                Some(&cli),
                Some(&config),
                Some(&environment),
                Path::new("native")
            ),
            cli
        );
        assert_eq!(
            choose_compiler(None, Some(&config), Some(&environment), Path::new("native")),
            config
        );
        assert_eq!(
            choose_compiler(None, None, Some(&environment), Path::new("native")),
            environment
        );
        assert_eq!(
            choose_compiler(None, None, None, Path::new("native")),
            PathBuf::from("native")
        );
    }

    #[test]
    fn only_probes_requested_driver_and_reports_missing_other_driver() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake(dir.path(), &["only-c"], "gcc version 14");
        let tc = detect(
            Some(dir.path()),
            &[Language::C],
            &overrides(dir.path(), Some("only-c"), None),
            &runner,
        )
        .unwrap();
        assert!(tc.info().c_driver.is_some());
        assert!(tc.info().cpp_driver.is_none());
        let request = LinkRequest {
            objects: vec![],
            output: "out".into(),
            language: Language::Cpp,
            profile: ProfileSettings {
                optimization: Optimization::None,
                debug: false,
            },
            cwd: dir.path().into(),
        };
        assert!(
            tc.link_command(&request)
                .unwrap_err()
                .message
                .contains("no Cpp driver")
        );
        assert_eq!(runner.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn detects_family_alias_and_rejects_mixed_families() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake(dir.path(), &["cc", "cxx"], "Apple clang version 16");
        let tc = detect(
            Some(dir.path()),
            &[Language::C, Language::Cpp],
            &overrides(dir.path(), Some("cc"), Some("cxx")),
            &runner,
        )
        .unwrap();
        assert_eq!(tc.info().family, CompilerFamily::AppleClang);
        let mixed = FakeRunner {
            calls: Mutex::new(Vec::new()),
            banners: vec![
                ("cc".into(), "gcc 14 Free Software Foundation".into()),
                ("cxx".into(), "clang version 18".into()),
            ],
            targets: vec![
                ("cc".into(), native_triple()),
                ("cxx".into(), native_triple()),
            ],
        };
        let error = detect(
            Some(dir.path()),
            &[Language::C, Language::Cpp],
            &overrides(dir.path(), Some("cc"), Some("cxx")),
            &mixed,
        )
        .unwrap_err();
        assert!(error.message.contains("one compiler family"));
    }

    #[test]
    fn compile_and_link_keep_paths_with_spaces_as_single_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let runner = fake(
            dir.path(),
            &["cc", "cxx", "compiler with spaces"],
            "gcc version 14",
        );
        let tc = detect(
            Some(dir.path()),
            &[Language::C, Language::Cpp],
            &overrides(dir.path(), Some("cc"), Some("cxx")),
            &runner,
        )
        .unwrap();
        let include = PathBuf::from("include files");
        let source = SourceFile {
            path: "src/main file.cpp".into(),
            relative_path: "main file.cpp".into(),
            language: Language::Cpp,
        };
        let compile = tc
            .compile_command(&CompileRequest {
                source,
                output: "obj/main file.o".into(),
                include_dirs: vec![include.clone()],
                standards: Standards::default(),
                profile: ProfileSettings {
                    optimization: Optimization::None,
                    debug: true,
                },
                cwd: dir.path().into(),
            })
            .unwrap();
        assert_eq!(compile.args[0], "-std=c++20");
        assert_eq!(compile.args[5], "-I");
        assert_eq!(compile.args[6], include.as_os_str());
        assert_eq!(compile.args[8], "src/main file.cpp");
        assert_eq!(compile.args[10], "obj/main file.o");
        assert_eq!(compile.program, dir.path().join("cxx"));
        let link = tc
            .link_command(&LinkRequest {
                objects: vec!["obj/a file.o".into()],
                output: "bin/my app".into(),
                language: Language::Cpp,
                profile: ProfileSettings {
                    optimization: Optimization::Speed,
                    debug: false,
                },
                cwd: dir.path().into(),
            })
            .unwrap();
        assert_eq!(link.program, dir.path().join("cxx"));
        assert_eq!(
            link.args,
            vec![
                OsString::from("-O2"),
                OsString::from("obj/a file.o"),
                OsString::from("-o"),
                OsString::from("bin/my app")
            ]
        );
    }

    #[test]
    fn rejects_shell_expressions_unknown_fields_and_cross_targets() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("compiler with spaces");
        fs::write(&spaced, "compiler").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&spaced, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            resolve_executable(Path::new("clang -O2"), dir.path())
                .unwrap_err()
                .contains("embedded compiler options")
        );
        assert_eq!(
            resolve_executable(Path::new("compiler with spaces"), dir.path()).unwrap(),
            dir.path().join("compiler with spaces")
        );
        let cfg = dir.path().join(".rmake");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("config.toml"), "schema = 1\nunknown = true\n").unwrap();
        let err = detect(
            Some(dir.path()),
            &[Language::C],
            &CompilerOptions::default(),
            &FakeRunner::default(),
        )
        .unwrap_err();
        assert!(err.message.contains("unknown"));

        fs::remove_file(cfg.join("config.toml")).unwrap();
        let other_arch = if canonical_arch(env::consts::ARCH) == Some("aarch64") {
            "x86_64"
        } else {
            "aarch64"
        };
        let other_target = if cfg!(target_os = "macos") {
            format!("{other_arch}-apple-darwin25.2.0")
        } else {
            format!("{other_arch}-unknown-linux-gnu")
        };
        let runner = FakeRunner {
            calls: Mutex::new(Vec::new()),
            banners: vec![("cross".into(), "gcc version 14".into())],
            targets: vec![("cross".into(), other_target)],
        };
        let path = dir.path().join("cross");
        fs::write(&path, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let err = detect(
            Some(dir.path()),
            &[Language::C],
            &overrides(dir.path(), Some("cross"), None),
            &runner,
        )
        .unwrap_err();
        assert!(err.message.contains("cross compilation is not supported"));
    }
}
