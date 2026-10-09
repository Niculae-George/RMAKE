//! Shared domain types and narrow ports. No filesystem or subprocess operations.

use std::ffi::OsString;
use std::path::PathBuf;

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

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct BinaryTarget {
    pub name: String,
    pub root: PathBuf,
    pub include_dirs: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub inferred: bool,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    pub target: BinaryTarget,
    pub standards: Standards,
    pub profiles: Profiles,
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
    pub sources: Vec<SourceFile>,
    pub ignored: Vec<IgnoredFile>,
}

#[derive(Clone, Debug, Default)]
pub struct CompilerOptions {
    pub c_compiler: Option<PathBuf>,
    pub cpp_compiler: Option<PathBuf>,
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
    pub include_dirs: Vec<PathBuf>,
    pub standards: Standards,
    pub profile: ProfileSettings,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LinkRequest {
    pub objects: Vec<PathBuf>,
    pub output: PathBuf,
    pub language: Language,
    pub profile: ProfileSettings,
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

pub trait ProcessRunner {
    fn run(&self, command: &CommandSpec, mode: IoMode) -> Result<ProcessResult>;
}

/// Rendering belongs to the caller; the build engine only emits observations.
pub trait BuildReporter {
    fn started(&self, command: &CommandSpec);
    fn output(&self, result: &ProcessResult) -> Result<()>;
}

pub trait Toolchain {
    fn info(&self) -> &ToolchainInfo;
    fn compile_command(&self, request: &CompileRequest) -> Result<CommandSpec>;
    fn link_command(&self, request: &LinkRequest) -> Result<CommandSpec>;
}

#[derive(Clone, Debug)]
pub struct CompileAction {
    pub command: CommandSpec,
    pub output: PathBuf,
}

#[derive(Clone, Debug)]
pub struct BuildPlan {
    pub compiles: Vec<CompileAction>,
    pub link: CommandSpec,
    pub staged_executable: PathBuf,
    pub final_executable: PathBuf,
}

#[derive(Clone, Debug)]
pub struct BuildResult {
    pub executable: PathBuf,
    pub compiled: usize,
}
