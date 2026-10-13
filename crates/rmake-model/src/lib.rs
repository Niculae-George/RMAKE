//! Shared domain types and focused compiler, archiver, and process ports.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[derive(Debug, thiserror::Error)]
#[error("error[{code}]: {message}")]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
}
impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
pub type Result<T> = std::result::Result<T, Diagnostic>;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Language {
    C,
    Cpp,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProfileKind {
    #[default]
    Dev,
    Release,
}
impl ProfileKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Release => "release",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Optimization {
    None,
    Speed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSettings {
    pub optimization: Optimization,
    pub debug: bool,
}
#[derive(Clone, Debug)]
pub struct Profiles {
    pub dev: ProfileSettings,
    pub release: ProfileSettings,
}
impl Default for Profiles {
    fn default() -> Self {
        Self {
            dev: ProfileSettings {
                optimization: Optimization::None,
                debug: true,
            },
            release: ProfileSettings {
                optimization: Optimization::Speed,
                debug: false,
            },
        }
    }
}
impl Profiles {
    pub fn get(&self, kind: ProfileKind) -> &ProfileSettings {
        match kind {
            ProfileKind::Dev => &self.dev,
            ProfileKind::Release => &self.release,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Standards {
    pub c: String,
    pub cpp: String,
}
impl Default for Standards {
    fn default() -> Self {
        Self {
            c: "c17".into(),
            cpp: "c++20".into(),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
    Bin,
    Static,
    HeaderOnly,
    Test,
    Example,
}
impl TargetKind {
    pub fn is_executable(self) -> bool {
        matches!(self, Self::Bin | Self::Test | Self::Example)
    }
    pub fn is_library(self) -> bool {
        matches!(self, Self::Static | Self::HeaderOnly)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Bin => "bin",
            Self::Static => "static",
            Self::HeaderOnly => "header-only",
            Self::Test => "test",
            Self::Example => "example",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    Private,
    Public,
    Interface,
}
#[derive(Clone, Debug)]
pub struct TargetDependency {
    pub target: String,
    pub visibility: Visibility,
}
#[derive(Clone, Debug, Default)]
pub struct UsageRequirements {
    pub include_dirs: Vec<PathBuf>,
    pub definitions: BTreeMap<String, String>,
    pub c_standard: Option<String>,
    pub cpp_standard: Option<String>,
}
/// Origin keeps imported dependencies distinct from user-selectable production targets.
#[derive(Clone, Debug)]
pub enum TargetOrigin {
    Local,
    HeaderFolder {
        alias: String,
        root: PathBuf,
        default_includes: bool,
    },
    StaticFolder {
        alias: String,
        root: PathBuf,
        default_includes: bool,
        library: PathBuf,
        language: Language,
        compatibility: StaticCompatibility,
        evidence: ArchiveEvidence,
    },
}

#[derive(Clone, Debug)]
pub struct HeaderFolderRequest {
    pub alias: String,
    pub root: PathBuf,
    pub includes: Option<Vec<PathBuf>>,
    pub definitions: BTreeMap<String, String>,
    pub c_standard: Option<String>,
    pub cpp_standard: Option<String>,
}
#[derive(Clone, Debug)]
pub struct HeaderFolderImport {
    pub alias: String,
    pub root: PathBuf,
    pub usage: UsageRequirements,
    pub default_includes: bool,
}
/// Read-only folder metadata resolution; native compilation stays in core.
pub trait HeaderFolderResolver: Send + Sync {
    fn resolve(
        &self,
        package_root: &Path,
        request: &HeaderFolderRequest,
    ) -> Result<HeaderFolderImport>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StaticCompatibility {
    UserAsserted,
}
#[derive(Clone, Debug)]
pub struct ArchiveEvidence {
    pub format: String,
    pub architecture: String,
    pub members: usize,
}
#[derive(Clone, Debug)]
pub struct StaticFolderRequest {
    pub headers: HeaderFolderRequest,
    pub library: PathBuf,
    pub language: Language,
    pub compatibility: StaticCompatibility,
}
#[derive(Clone, Debug)]
pub struct StaticFolderImport {
    pub headers: HeaderFolderImport,
    pub library: PathBuf,
    pub language: Language,
    pub compatibility: StaticCompatibility,
    pub evidence: ArchiveEvidence,
}
pub trait StaticFolderResolver: Send + Sync {
    fn resolve_static(
        &self,
        package_root: &Path,
        request: &StaticFolderRequest,
    ) -> Result<StaticFolderImport>;
}
/// Composition of the supported, focused folder-provider capabilities.
pub trait FolderResolver: HeaderFolderResolver + StaticFolderResolver {}
impl<T: HeaderFolderResolver + StaticFolderResolver> FolderResolver for T {}

pub fn folder_target_name(alias: &str) -> String {
    format!("@folder:{alias}")
}

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub origin: TargetOrigin,
    pub kind: TargetKind,
    pub root: Option<PathBuf>,
    pub include_dirs: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub inferred: bool,
    pub definitions: BTreeMap<String, String>,
    pub interface: UsageRequirements,
    pub dependencies: Vec<TargetDependency>,
    pub compile_options: Vec<OsString>,
    pub link_options: Vec<OsString>,
}
impl Target {
    pub fn is_local(&self) -> bool {
        matches!(self.origin, TargetOrigin::Local)
    }
}
#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    pub targets: Vec<Target>,
    pub default_target: Option<String>,
    pub standards: Standards,
    pub profiles: Profiles,
}
impl Project {
    pub fn target(&self, name: &str) -> Result<&Target> {
        self.targets
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| Diagnostic::new("RMAKE-T101", format!("unknown target {name}")))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub language: Language,
}
#[derive(Clone, Debug)]
pub struct IgnoredFile {
    pub path: PathBuf,
    pub reason: String,
}
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub sources: BTreeMap<String, Vec<SourceFile>>,
    pub ignored: Vec<IgnoredFile>,
}
#[derive(Clone, Debug, Default)]
pub struct CompilerOptions {
    pub c_compiler: Option<PathBuf>,
    pub cpp_compiler: Option<PathBuf>,
    pub archiver: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompilerFamily {
    Gcc,
    Clang,
    AppleClang,
}
#[derive(Clone, Debug)]
pub struct ToolchainInfo {
    pub family: CompilerFamily,
    pub c_driver: Option<PathBuf>,
    pub cpp_driver: Option<PathBuf>,
    pub version: String,
    pub target: String,
}
#[derive(Clone, Debug)]
pub struct CompileRequest {
    pub source: SourceFile,
    pub output: PathBuf,
    pub depfile: PathBuf,
    pub include_dirs: Vec<PathBuf>,
    pub definitions: BTreeMap<String, String>,
    pub standards: Standards,
    pub options: Vec<OsString>,
    pub profile: ProfileSettings,
    pub cwd: PathBuf,
}
#[derive(Clone, Debug)]
pub struct LinkRequest {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub language: Language,
    pub options: Vec<OsString>,
    pub profile: ProfileSettings,
    pub cwd: PathBuf,
}
#[derive(Clone, Debug)]
pub struct ArchiveRequest {
    pub objects: Vec<PathBuf>,
    pub output: PathBuf,
    pub cwd: PathBuf,
}
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub label: String,
}
#[derive(Clone, Copy, Debug)]
pub enum IoMode {
    Capture,
    Inherit,
}
#[derive(Clone, Debug)]
pub struct ProcessResult {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Diagnostic::new("RMAKE-X130", "operation cancelled"))
        } else {
            Ok(())
        }
    }
}
pub trait ProcessRunner: Send + Sync {
    fn run(&self, command: &CommandSpec, mode: IoMode) -> Result<ProcessResult>;
    fn run_cancelled(
        &self,
        command: &CommandSpec,
        mode: IoMode,
        cancellation: &Cancellation,
    ) -> Result<ProcessResult> {
        cancellation.check()?;
        self.run(command, mode)
    }
}
pub trait BuildReporter: Sync {
    fn started(&self, command: &CommandSpec);
    fn output(&self, result: &ProcessResult) -> Result<()>;
}
pub trait Toolchain: Send + Sync {
    fn info(&self) -> &ToolchainInfo;
    fn compile_command(&self, request: &CompileRequest) -> Result<CommandSpec>;
    fn link_command(&self, request: &LinkRequest) -> Result<CommandSpec>;
}
pub trait Archiver: Send + Sync {
    fn archive_command(&self, request: &ArchiveRequest) -> Result<CommandSpec>;
    fn identity(&self) -> Vec<u8> {
        Vec::new()
    }
}
#[derive(Clone, Debug)]
pub enum ActionKind {
    Compile {
        source: SourceFile,
        depfile: PathBuf,
        include_dirs: Vec<PathBuf>,
    },
    Archive,
    Link,
}
#[derive(Clone, Debug)]
pub struct BuildAction {
    pub id: String,
    pub target: String,
    pub kind: ActionKind,
    pub command: CommandSpec,
    pub output: PathBuf,
    pub inputs: Vec<PathBuf>,
    pub prerequisites: Vec<String>,
    pub cacheable: bool,
}
#[derive(Clone, Debug, Default)]
pub struct BuildPlan {
    pub actions: Vec<BuildAction>,
    pub artifacts: BTreeMap<String, PathBuf>,
}
#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub profile: ProfileKind,
    pub targets: Vec<String>,
    pub jobs: usize,
    pub rebuild: bool,
    pub cancellation: Cancellation,
}
impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            profile: ProfileKind::Dev,
            targets: Vec::new(),
            jobs: std::thread::available_parallelism().map_or(1, usize::from),
            rebuild: false,
            cancellation: Cancellation::default(),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct BuildResult {
    pub artifacts: BTreeMap<String, PathBuf>,
    pub compiled: usize,
    pub archived: usize,
    pub linked: usize,
    pub reused: usize,
}
