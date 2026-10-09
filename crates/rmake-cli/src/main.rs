use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rmake_core::{
    discovery, execute, manifest, process::SystemProcessRunner, project, runtime, storage,
};
use rmake_model::{
    BuildReporter, CommandSpec, CompilerOptions, Diagnostic, Language, ProcessResult, ProfileKind,
    Project, Toolchain,
};
use rmake_toolchains::detect;

#[derive(Debug, Parser)]
#[command(
    name = "rmake",
    version,
    about = "A straightforward native C/C++ build tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new RMake project.
    New(NewArgs),
    /// Initialize an existing directory as an RMake project.
    Init(InitArgs),
    /// Build the current project.
    Build(BuildArgs),
    /// Build and run the current project's executable.
    Run(RunArgs),
    /// Remove this project's build outputs.
    Clean,
    /// Show compiler and environment availability.
    Doctor(CompilerArgs),
    /// Inspect discovered project files.
    Inspect(InspectArgs),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CliLanguage {
    C,
    #[default]
    Cpp,
}

impl From<CliLanguage> for Language {
    fn from(value: CliLanguage) -> Self {
        match value {
            CliLanguage::C => Self::C,
            CliLanguage::Cpp => Self::Cpp,
        }
    }
}

#[derive(Debug, Args)]
struct NewArgs {
    path: PathBuf,
    #[arg(long, value_enum, default_value = "cpp")]
    lang: CliLanguage,
    #[arg(long)]
    name: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long, value_enum, default_value = "cpp")]
    lang: CliLanguage,
    #[arg(long)]
    name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum CliProfile {
    #[default]
    Dev,
    Release,
}

impl From<CliProfile> for ProfileKind {
    fn from(value: CliProfile) -> Self {
        match value {
            CliProfile::Dev => Self::Dev,
            CliProfile::Release => Self::Release,
        }
    }
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[arg(long, value_enum, conflicts_with = "release")]
    profile: Option<CliProfile>,
    #[arg(long, conflicts_with = "profile")]
    release: bool,
    #[arg(short, long)]
    verbose: bool,
    #[arg(long, value_name = "PATH")]
    cc: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    cxx: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long, value_enum, conflicts_with = "release")]
    profile: Option<CliProfile>,
    #[arg(long, conflicts_with = "profile")]
    release: bool,
    #[arg(short, long)]
    verbose: bool,
    #[arg(long, value_name = "PATH")]
    cc: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    cxx: Option<PathBuf>,
    /// Arguments passed to the executable after `--`.
    #[arg(last = true, value_name = "ARGS")]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct CompilerArgs {
    #[arg(long, value_name = "PATH")]
    cc: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    cxx: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[command(subcommand)]
    command: InspectCommand,
}

#[derive(Debug, Subcommand)]
enum InspectCommand {
    /// Show source ownership and ignored paths.
    Files,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(command: Command) -> rmake_model::Result<i32> {
    match command {
        Command::New(args) => {
            let path = project::create_new(&args.path, args.name.as_deref(), args.lang.into())?;
            println!("created {}", path.display());
            Ok(0)
        }
        Command::Init(args) => {
            let path = project::init(&args.path, args.name.as_deref(), args.lang.into())?;
            println!("initialized {}", path.display());
            Ok(0)
        }
        Command::Build(args) => build_command(args),
        Command::Run(args) => run_command(args),
        Command::Clean => {
            let project = load_current_project()?;
            storage::clean(&project.root)?;
            println!("cleaned {}", project.root.display());
            Ok(0)
        }
        Command::Doctor(args) => doctor(args),
        Command::Inspect(args) => match args.command {
            InspectCommand::Files => inspect_files(),
        },
    }
}

fn profile(profile: Option<CliProfile>, release: bool) -> ProfileKind {
    if release {
        ProfileKind::Release
    } else {
        profile.unwrap_or_default().into()
    }
}

fn compiler_options(cc: Option<PathBuf>, cxx: Option<PathBuf>) -> CompilerOptions {
    CompilerOptions {
        c_compiler: cc,
        cpp_compiler: cxx,
    }
}

fn build_command(args: BuildArgs) -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let discovered = discovery::discover(&project)?;
    let languages = unique_languages(&discovered.sources);
    let runner = SystemProcessRunner;
    let toolchain = detect(
        Some(&project.root),
        &languages,
        &compiler_options(args.cc, args.cxx),
        &runner,
    )?;
    let result = execute::build(
        &project,
        &discovered,
        &toolchain,
        &runner,
        profile(args.profile, args.release),
        &TerminalReporter {
            verbose: args.verbose,
        },
    )?;
    println!(
        "built {} ({} source files compiled)",
        result.executable.display(),
        result.compiled
    );
    Ok(0)
}

fn run_command(args: RunArgs) -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let discovered = discovery::discover(&project)?;
    let languages = unique_languages(&discovered.sources);
    let runner = SystemProcessRunner;
    let toolchain = detect(
        Some(&project.root),
        &languages,
        &compiler_options(args.cc, args.cxx),
        &runner,
    )?;
    let built = execute::build(
        &project,
        &discovered,
        &toolchain,
        &runner,
        profile(args.profile, args.release),
        &TerminalReporter {
            verbose: args.verbose,
        },
    )?;
    println!(
        "built {} ({} source files compiled)",
        built.executable.display(),
        built.compiled
    );
    runtime::run(&built.executable, &project.root, &args.args, &runner)
}

fn load_current_project() -> rmake_model::Result<Project> {
    let cwd = std::env::current_dir().map_err(|error| {
        rmake_model::Diagnostic::new(
            "current-directory",
            format!("cannot read current directory: {error}"),
        )
    })?;
    let manifest_path = manifest::find_manifest(&cwd)?;
    manifest::load_project(&manifest_path)
}

struct TerminalReporter {
    verbose: bool,
}

impl BuildReporter for TerminalReporter {
    fn started(&self, command: &CommandSpec) {
        eprintln!("{}", command.label);
        if self.verbose {
            eprintln!("  {:?} {:?}", command.program, command.args);
        }
    }

    fn output(&self, result: &ProcessResult) -> rmake_model::Result<()> {
        io::stdout().write_all(&result.stdout).map_err(|error| {
            Diagnostic::new("RMAKE-P102", format!("write compiler stdout: {error}"))
        })?;
        io::stderr().write_all(&result.stderr).map_err(|error| {
            Diagnostic::new("RMAKE-P102", format!("write compiler stderr: {error}"))
        })
    }
}

fn current_dir() -> rmake_model::Result<PathBuf> {
    std::env::current_dir().map_err(|error| {
        rmake_model::Diagnostic::new(
            "current-directory",
            format!("cannot read current directory: {error}"),
        )
    })
}

fn unique_languages(sources: &[rmake_model::SourceFile]) -> Vec<Language> {
    let mut languages = Vec::new();
    for source in sources {
        if !languages.contains(&source.language) {
            languages.push(source.language);
        }
    }
    languages.sort();
    languages
}

fn doctor(args: CompilerArgs) -> rmake_model::Result<i32> {
    let cwd = current_dir()?;
    let (root, languages) = match manifest::find_manifest(&cwd) {
        Ok(path) => {
            let project = manifest::load_project(&path)?;
            let discovery = discovery::discover(&project)?;
            (project.root, unique_languages(&discovery.sources))
        }
        Err(error) if error.code == "manifest-not-found" => (cwd, vec![Language::C, Language::Cpp]),
        Err(error) => return Err(error),
    };
    let runner = SystemProcessRunner;
    let toolchain = detect(
        Some(&root),
        &languages,
        &compiler_options(args.cc, args.cxx),
        &runner,
    )?;
    let info = toolchain.info();
    println!("compiler family: {:?}", info.family);
    println!("C compiler: {}", display_optional(info.c_driver.as_deref()));
    println!(
        "C++ compiler: {}",
        display_optional(info.cpp_driver.as_deref())
    );
    println!("version: {}", info.version);
    println!("target: {}", info.target);
    Ok(0)
}

fn display_optional(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "unavailable".into())
}

fn inspect_files() -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let mut discovery = discovery::discover(&project)?;
    discovery
        .sources
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    discovery
        .ignored
        .sort_by(|left, right| left.path.cmp(&right.path));

    println!("sources:");
    for source in discovery.sources {
        println!(
            "  {} [{}] -> {}",
            source.relative_path.display(),
            language_name(source.language),
            project.target.name
        );
    }
    println!("ignored:");
    for ignored in discovery.ignored {
        let path = ignored
            .path
            .strip_prefix(&project.root)
            .unwrap_or(&ignored.path);
        println!("  {}: {}", path.display(), ignored.reason);
    }
    Ok(0)
}

fn language_name(language: Language) -> &'static str {
    match language {
        Language::C => "c",
        Language::Cpp => "cpp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_to_cpp_and_accepts_c() {
        let default = Cli::try_parse_from(["rmake", "new", "hello"]).unwrap();
        let Command::New(default) = default.command else {
            panic!("expected new")
        };
        assert_eq!(default.lang, CliLanguage::Cpp);

        let c = Cli::try_parse_from(["rmake", "new", "hello", "--lang", "c"]).unwrap();
        let Command::New(c) = c.command else {
            panic!("expected new")
        };
        assert_eq!(c.lang, CliLanguage::C);
    }

    #[test]
    fn init_defaults_to_current_directory_and_cpp() {
        let cli = Cli::try_parse_from(["rmake", "init"]).unwrap();
        let Command::Init(init) = cli.command else {
            panic!("expected init")
        };
        assert_eq!(init.path, PathBuf::from("."));
        assert_eq!(init.lang, CliLanguage::Cpp);
    }

    #[test]
    fn profile_defaults_to_development_and_supports_release_alias() {
        let cli = Cli::try_parse_from(["rmake", "build"]).unwrap();
        let Command::Build(args) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(profile(args.profile, args.release), ProfileKind::Dev);

        let cli = Cli::try_parse_from(["rmake", "build", "--release"]).unwrap();
        let Command::Build(args) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(profile(args.profile, args.release), ProfileKind::Release);
    }

    #[test]
    fn run_forwards_arguments_after_separator_verbatim() {
        let cli = Cli::try_parse_from([
            OsString::from("rmake"),
            OsString::from("run"),
            OsString::from("--profile"),
            OsString::from("release"),
            OsString::from("--"),
            OsString::from("--input"),
            OsString::from("path with spaces"),
            OsString::from("--flag=x"),
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(
            args.args,
            [
                OsString::from("--input"),
                OsString::from("path with spaces"),
                OsString::from("--flag=x")
            ]
        );
        assert_eq!(args.profile, Some(CliProfile::Release));
    }

    #[test]
    fn rejects_unsupported_commands_and_conflicting_profiles() {
        assert!(Cli::try_parse_from(["rmake", "test"]).is_err());
        assert!(Cli::try_parse_from(["rmake", "build", "--release", "--profile", "dev"]).is_err());
        assert!(Cli::try_parse_from(["rmake", "run", "--profile", "dev", "--release"]).is_err());
    }
}
