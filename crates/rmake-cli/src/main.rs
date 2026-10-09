use std::ffi::OsString;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rmake_core::{
    discovery, execute, manifest, process, process::SystemProcessRunner, project, runtime, storage,
};
use rmake_model::{
    BuildOptions, BuildReporter, BuildResult, Cancellation, CommandSpec, CompilerOptions,
    Diagnostic, Discovery, Language, ProcessResult, ProfileKind, Project, Target, TargetKind,
    Toolchain,
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
    /// Build production targets, or selected targets and dependencies.
    Build(BuildArgs),
    /// Build and run an executable target.
    Run(RunArgs),
    /// Build and run all discovered tests.
    Test(TestArgs),
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
    /// Scaffold a conventional static library.
    #[arg(long)]
    lib: bool,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(long, value_enum, default_value = "cpp")]
    lang: CliLanguage,
    #[arg(long)]
    name: Option<String>,
    /// Scaffold a conventional static library.
    #[arg(long)]
    lib: bool,
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
struct BuildSettings {
    #[arg(long, value_enum, conflicts_with = "release")]
    profile: Option<CliProfile>,
    #[arg(long, conflicts_with = "profile")]
    release: bool,
    #[arg(short, long)]
    verbose: bool,
    /// Maximum number of concurrent compiler jobs.
    #[arg(long, value_name = "N", value_parser = parse_jobs)]
    jobs: Option<usize>,
    /// Rebuild every selected action.
    #[arg(long)]
    rebuild: bool,
    #[arg(long, value_name = "PATH")]
    cc: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    cxx: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    ar: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[command(flatten)]
    settings: BuildSettings,
    /// Select a target (repeatable); dependencies are included automatically.
    #[arg(long = "target", action = clap::ArgAction::Append, value_name = "NAME")]
    targets: Vec<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    settings: BuildSettings,
    /// Select a binary; otherwise use configured default binary or sole binary.
    #[arg(long, conflicts_with = "example", value_name = "NAME")]
    bin: Option<String>,
    /// Select an example executable.
    #[arg(long, conflicts_with = "bin", value_name = "NAME")]
    example: Option<String>,
    /// Arguments passed to the executable after `--`.
    #[arg(last = true, value_name = "ARGS")]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct TestArgs {
    #[command(flatten)]
    settings: BuildSettings,
    /// Arguments passed to each test executable after `--`.
    #[arg(last = true, value_name = "ARGS")]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct CompilerArgs {
    #[arg(long, value_name = "PATH")]
    cc: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    cxx: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    ar: Option<PathBuf>,
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
    let cancellation = Cancellation::default();
    if let Err(error) = process::install_interrupt_handler(&cancellation) {
        eprintln!("{error}");
        return ExitCode::from(1);
    }
    match dispatch(cli.command, &cancellation) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(if error.code == "RMAKE-X130" { 130 } else { 1 })
        }
    }
}

fn dispatch(command: Command, cancellation: &Cancellation) -> rmake_model::Result<i32> {
    match command {
        Command::New(args) => {
            let path = if args.lib {
                project::create_new_library(&args.path, args.name.as_deref(), args.lang.into())?
            } else {
                project::create_new(&args.path, args.name.as_deref(), args.lang.into())?
            };
            println!("created {}", path.display());
            Ok(0)
        }
        Command::Init(args) => {
            let path = if args.lib {
                project::init_library(&args.path, args.name.as_deref(), args.lang.into())?
            } else {
                project::init(&args.path, args.name.as_deref(), args.lang.into())?
            };
            println!("initialized {}", path.display());
            Ok(0)
        }
        Command::Build(args) => build_command(args, cancellation),
        Command::Run(args) => run_command(args, cancellation),
        Command::Test(args) => test_command(args, cancellation),
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

fn parse_jobs(value: &str) -> Result<usize, String> {
    value
        .parse::<NonZeroUsize>()
        .map(NonZeroUsize::get)
        .map_err(|_| "jobs must be a positive integer".to_owned())
}

fn profile(profile: Option<CliProfile>, release: bool) -> ProfileKind {
    if release {
        ProfileKind::Release
    } else {
        profile.unwrap_or_default().into()
    }
}

fn compiler_options(
    cc: Option<PathBuf>,
    cxx: Option<PathBuf>,
    ar: Option<PathBuf>,
) -> CompilerOptions {
    CompilerOptions {
        c_compiler: cc,
        cpp_compiler: cxx,
        archiver: ar,
    }
}

fn build_options(
    settings: &BuildSettings,
    targets: Vec<String>,
    cancellation: &Cancellation,
) -> BuildOptions {
    BuildOptions {
        profile: profile(settings.profile, settings.release),
        targets,
        jobs: settings
            .jobs
            .unwrap_or_else(|| BuildOptions::default().jobs),
        rebuild: settings.rebuild,
        cancellation: cancellation.clone(),
    }
}

fn build_command(args: BuildArgs, cancellation: &Cancellation) -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let roots = if args.targets.is_empty() {
        project
            .targets
            .iter()
            .filter(|target| is_production(target.kind))
            .map(|target| target.name.clone())
            .collect::<Vec<_>>()
    } else {
        for name in &args.targets {
            project.target(name)?;
        }
        args.targets.clone()
    };
    if roots.is_empty() {
        return Err(Diagnostic::new(
            "RMAKE-T101",
            "no production targets are declared",
        ));
    }
    let closure = dependency_closure(&project, &roots)?;
    let discovered = discovery::discover_selected(&project, &closure)?;
    let options = build_options(&args.settings, args.targets, cancellation);
    let runner = SystemProcessRunner;
    let result = build_selected(
        &project,
        &discovered,
        &closure,
        &options,
        &args.settings,
        &runner,
    )?;
    print_build_result(&result);
    Ok(0)
}

fn run_command(args: RunArgs, cancellation: &Cancellation) -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let target = select_run_target(&project, args.bin.as_deref(), args.example.as_deref())?;
    let roots = vec![target.name.clone()];
    let closure = dependency_closure(&project, &roots)?;
    let discovered = discovery::discover_selected(&project, &closure)?;
    let options = build_options(&args.settings, roots, cancellation);
    let runner = SystemProcessRunner;
    let built = build_selected(
        &project,
        &discovered,
        &closure,
        &options,
        &args.settings,
        &runner,
    )?;
    print_build_result(&built);
    let executable = built.artifacts.get(&target.name).ok_or_else(|| {
        Diagnostic::new(
            "RMAKE-T101",
            format!("target {} did not produce an executable", target.name),
        )
    })?;
    runtime::run_cancelled(executable, &project.root, &args.args, &runner, cancellation)
}

fn test_command(args: TestArgs, cancellation: &Cancellation) -> rmake_model::Result<i32> {
    let project = load_current_project()?;
    let roots = project
        .targets
        .iter()
        .filter(|target| target.kind == TargetKind::Test)
        .map(|target| target.name.clone())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        println!("no test targets found");
        return Ok(0);
    }
    let test_count = roots.len();
    let closure = dependency_closure(&project, &roots)?;
    let discovered = discovery::discover_selected(&project, &closure)?;
    let options = build_options(&args.settings, roots.clone(), cancellation);
    let runner = SystemProcessRunner;
    let built = build_selected(
        &project,
        &discovered,
        &closure,
        &options,
        &args.settings,
        &runner,
    )?;
    print_build_result(&built);

    let mut failures = 0usize;
    for name in roots {
        cancellation.check()?;
        let executable = built.artifacts.get(&name).ok_or_else(|| {
            Diagnostic::new(
                "RMAKE-T101",
                format!("test target {name} did not produce an executable"),
            )
        })?;
        println!("test {name}");
        let result =
            runtime::test_cancelled(executable, &project.root, &args.args, &runner, cancellation)?;
        io::stdout().write_all(&result.stdout).map_err(|error| {
            Diagnostic::new("RMAKE-P102", format!("write test stdout: {error}"))
        })?;
        io::stderr().write_all(&result.stderr).map_err(|error| {
            Diagnostic::new("RMAKE-P102", format!("write test stderr: {error}"))
        })?;
        if result.code == 0 {
            println!("  passed");
        } else {
            failures += 1;
            eprintln!("  failed with status {}", result.code);
        }
    }
    if failures == 0 {
        println!("all {test_count} test target(s) passed");
        Ok(0)
    } else {
        eprintln!("{failures} test target(s) failed");
        Ok(1)
    }
}

fn build_selected(
    project: &Project,
    discovered: &Discovery,
    closure: &[String],
    options: &BuildOptions,
    settings: &BuildSettings,
    runner: &dyn rmake_model::ProcessRunner,
) -> rmake_model::Result<BuildResult> {
    let has_compiled_sources = closure.iter().any(|name| {
        discovered
            .sources
            .get(name)
            .is_some_and(|sources| !sources.is_empty())
    });
    if !has_compiled_sources {
        return Ok(BuildResult::default());
    }
    let languages = unique_languages(&discovered.sources);
    let toolchain = detect(
        Some(&project.root),
        &languages,
        &compiler_options(
            settings.cc.clone(),
            settings.cxx.clone(),
            settings.ar.clone(),
        ),
        runner,
    )?;
    execute::build(
        project,
        discovered,
        &toolchain,
        &toolchain,
        runner,
        options,
        &TerminalReporter {
            verbose: settings.verbose,
        },
    )
}

fn print_build_result(result: &BuildResult) {
    println!(
        "build: {} compiled, {} archived, {} linked, {} reused",
        result.compiled, result.archived, result.linked, result.reused
    );
    for (name, artifact) in &result.artifacts {
        println!("  {name}: {}", artifact.display());
    }
}

fn is_production(kind: TargetKind) -> bool {
    matches!(
        kind,
        TargetKind::Bin | TargetKind::Static | TargetKind::HeaderOnly
    )
}

fn dependency_closure(project: &Project, roots: &[String]) -> rmake_model::Result<Vec<String>> {
    fn add(
        project: &Project,
        name: &str,
        selected: &mut std::collections::BTreeSet<String>,
    ) -> rmake_model::Result<()> {
        if !selected.insert(name.to_owned()) {
            return Ok(());
        }
        for dependency in &project.target(name)?.dependencies {
            add(project, &dependency.target, selected)?;
        }
        Ok(())
    }
    let mut selected = std::collections::BTreeSet::new();
    for name in roots {
        add(project, name, &mut selected)?;
    }
    Ok(selected.into_iter().collect())
}

fn select_run_target<'a>(
    project: &'a Project,
    bin: Option<&str>,
    example: Option<&str>,
) -> rmake_model::Result<&'a Target> {
    if let Some(name) = bin {
        let target = project.target(name)?;
        if target.kind != TargetKind::Bin {
            return Err(Diagnostic::new(
                "RMAKE-T101",
                format!("--bin target {name:?} is not a binary"),
            ));
        }
        return Ok(target);
    }
    if let Some(name) = example {
        let target = project.target(name)?;
        if target.kind != TargetKind::Example {
            return Err(Diagnostic::new(
                "RMAKE-T101",
                format!("--example target {name:?} is not an example"),
            ));
        }
        return Ok(target);
    }
    if let Some(default) = project
        .default_target
        .as_deref()
        .and_then(|name| project.target(name).ok())
        .filter(|target| target.kind == TargetKind::Bin)
    {
        return Ok(default);
    }
    let binaries = project
        .targets
        .iter()
        .filter(|target| target.kind == TargetKind::Bin)
        .collect::<Vec<_>>();
    match binaries.as_slice() {
        [only] => Ok(only),
        [] => Err(Diagnostic::new(
            "RMAKE-T101",
            "no binary target exists; use --example NAME to run an example",
        )),
        _ => Err(Diagnostic::new(
            "RMAKE-T101",
            "multiple binary targets exist; choose one with --bin NAME",
        )),
    }
}

fn load_current_project() -> rmake_model::Result<Project> {
    let cwd = current_dir()?;
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
        Diagnostic::new(
            "current-directory",
            format!("cannot read current directory: {error}"),
        )
    })
}

fn unique_languages(
    sources: &std::collections::BTreeMap<String, Vec<rmake_model::SourceFile>>,
) -> Vec<Language> {
    let mut languages = Vec::new();
    for source in sources.values().flatten() {
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
        &compiler_options(args.cc, args.cxx, args.ar),
        &runner,
    )?;
    let info = toolchain.info();
    println!("compiler family: {:?}", info.family);
    println!("C compiler: {}", display_optional(info.c_driver.as_deref()));
    println!(
        "C++ compiler: {}",
        display_optional(info.cpp_driver.as_deref())
    );
    println!("archiver: {}", toolchain.archiver_path().display());
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
        .ignored
        .sort_by(|left, right| left.path.cmp(&right.path));
    println!("sources:");
    for (target, sources) in discovery.sources {
        let kind = project.target(&target)?.kind.name();
        for source in sources {
            println!(
                "  {} [{}] -> {} ({kind})",
                source.relative_path.display(),
                language_name(source.language),
                target
            );
        }
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
