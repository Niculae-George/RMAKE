use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use rmake_model::{
    Diagnostic, Optimization, ProfileSettings, Profiles, Project, Result, Standards, Target,
    TargetDependency, TargetKind, UsageRequirements, Visibility,
};
use serde::Deserialize;

const MANIFEST_NAME: &str = "Rmake.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: u32,
    package: RawPackage,
    #[serde(default)]
    build: RawBuild,
    #[serde(default)]
    profiles: RawProfiles,
    #[serde(default)]
    discovery: RawDiscovery,
    #[serde(default)]
    targets: Vec<RawTarget>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackage {
    name: String,
    version: String,
    #[serde(rename = "default-target")]
    default_target: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawBuild {
    c_standard: Option<String>,
    cpp_standard: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProfiles {
    dev: Option<RawProfile>,
    release: Option<RawProfile>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawProfile {
    optimization: Option<String>,
    debug: Option<bool>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawDiscovery {
    exclude: Vec<String>,
    tests: Option<bool>,
    examples: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    name: String,
    kind: String,
    root: Option<String>,
    #[serde(rename = "public-includes", default)]
    public_includes: Vec<String>,
    #[serde(rename = "private-includes", default)]
    private_includes: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    defines: BTreeMap<String, String>,
    #[serde(rename = "compile-options", default)]
    compile_options: Vec<String>,
    #[serde(rename = "link-options", default)]
    link_options: Vec<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    interface: RawInterface,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawInterface {
    defines: BTreeMap<String, String>,
    c_standard: Option<String>,
    cpp_standard: Option<String>,
}

pub fn find_manifest(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize().map_err(|e| {
        Diagnostic::new(
            "manifest-search-path",
            format!("cannot resolve search path {}: {e}", start.display()),
        )
    })?;
    let start = if start.is_file() {
        start.parent().unwrap_or(&start)
    } else {
        &start
    };
    let mut current = start;
    loop {
        let candidate = current.join(MANIFEST_NAME);
        if candidate.is_file() {
            return candidate.canonicalize().map_err(|e| {
                Diagnostic::new(
                    "manifest-path",
                    format!("cannot resolve {}: {e}", candidate.display()),
                )
            });
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => {
                return Err(Diagnostic::new(
                    "manifest-not-found",
                    format!(
                        "no {MANIFEST_NAME} found from {} or its parents",
                        start.display()
                    ),
                ));
            }
        }
    }
}
pub fn load_project(manifest: &Path) -> Result<Project> {
    let path = manifest.canonicalize().map_err(|e| {
        Diagnostic::new(
            "manifest-path",
            format!("cannot resolve {}: {e}", manifest.display()),
        )
    })?;
    let text = fs::read_to_string(&path).map_err(|e| {
        Diagnostic::new(
            "manifest-read",
            format!("cannot read {}: {e}", path.display()),
        )
    })?;
    parse_project(&path, &text)
}

pub fn parse_project(manifest: &Path, text: &str) -> Result<Project> {
    let raw: RawManifest = toml::from_str(text).map_err(|e| {
        let message = e.to_string();
        let feature = ["dependencies", "workspace"]
            .into_iter()
            .find(|field| message.contains(&format!("unknown field `{field}`")));
        Diagnostic::new(
            if feature.is_some() {
                "unsupported-manifest-feature"
            } else {
                "manifest-parse"
            },
            format!("{}: {message}", manifest.display()),
        )
    })?;
    if raw.schema != 1 {
        return Err(Diagnostic::new(
            "manifest-schema",
            format!("unsupported schema {}; expected schema = 1", raw.schema),
        ));
    }
    validate_name(&raw.package.name)?;
    if raw.package.version.trim().is_empty() {
        return Err(Diagnostic::new(
            "manifest-version",
            "package.version must be non-empty",
        ));
    }
    let root = manifest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .map_err(|e| {
            Diagnostic::new("package-root", format!("cannot resolve package root: {e}"))
        })?;
    let standards = Standards {
        c: raw.build.c_standard.unwrap_or_else(|| "c17".into()),
        cpp: raw.build.cpp_standard.unwrap_or_else(|| "c++20".into()),
    };
    validate_standard(&standards.c, false, "build.c-standard")?;
    validate_standard(&standards.cpp, true, "build.cpp-standard")?;
    let profiles = Profiles {
        dev: profile(
            raw.profiles.dev,
            ProfileSettings {
                optimization: Optimization::None,
                debug: true,
            },
            "profiles.dev",
        )?,
        release: profile(
            raw.profiles.release,
            ProfileSettings {
                optimization: Optimization::Speed,
                debug: false,
            },
            "profiles.release",
        )?,
    };

    let mut targets = Vec::new();
    let explicit_production = raw
        .targets
        .iter()
        .any(|t| matches!(t.kind.as_str(), "bin" | "static" | "header-only"));
    let mut explicit_names = BTreeSet::new();
    for rt in raw.targets {
        let kind = parse_kind(&rt.kind)?;
        validate_name(&rt.name)?;
        if !explicit_names.insert(rt.name.clone()) {
            return Err(Diagnostic::new(
                "target-name",
                format!("duplicate target name {:?}", rt.name),
            ));
        }
        if kind != TargetKind::HeaderOnly && rt.root.is_none() {
            return Err(Diagnostic::new(
                "target-root",
                format!("target {} requires root", rt.name),
            ));
        }
        if kind == TargetKind::HeaderOnly && rt.public_includes.is_empty() {
            return Err(Diagnostic::new(
                "header-only-includes",
                format!("header-only target {} requires public-includes", rt.name),
            ));
        }
        if !kind.is_library() && rt.dependencies.values().any(|v| v != "private") {
            return Err(Diagnostic::new(
                "dependency-visibility",
                format!(
                    "{} target {} may use only private dependencies",
                    kind.name(),
                    rt.name
                ),
            ));
        }
        if let Some(value) = rt.root.as_deref() {
            reject_symlink_path(&root, value, "target root")?;
        }
        let root_path = rt
            .root
            .as_deref()
            .map(|v| resolve_inside(&root, v, "target root"))
            .transpose()?;
        if let Some(path) = &root_path
            && !path.is_dir()
            && !path.is_file()
        {
            return Err(Diagnostic::new(
                "target-root",
                format!("target root {} is not a file or directory", path.display()),
            ));
        }
        let mut include_dirs = Vec::new();
        let mut exported = UsageRequirements::default();
        for value in rt.public_includes {
            let p = resolve_directory(&root, &value, "public include")?;
            push_unique(&mut include_dirs, p.clone());
            push_unique(&mut exported.include_dirs, p);
        }
        for value in rt.private_includes {
            let p = resolve_directory(&root, &value, "private include")?;
            push_unique(&mut include_dirs, p);
        }
        if let Some(path) = &root_path {
            let source_dir = if path.is_dir() {
                path.as_path()
            } else {
                path.parent().expect("source file parent")
            };
            push_unique(&mut include_dirs, source_dir.to_path_buf());
        }
        for (key, value) in &rt.defines {
            validate_define(key, value)?;
        }
        for (key, value) in &rt.interface.defines {
            validate_define(key, value)?;
        }
        exported.definitions = rt.interface.defines;
        if let Some(s) = rt.interface.c_standard {
            validate_standard(&s, false, "targets.interface.c-standard")?;
            exported.c_standard = Some(s);
        }
        if let Some(s) = rt.interface.cpp_standard {
            validate_standard(&s, true, "targets.interface.cpp-standard")?;
            exported.cpp_standard = Some(s);
        }
        let mut excludes = raw.discovery.exclude.clone();
        excludes.extend(rt.exclude);
        for pattern in &excludes {
            validate_glob(pattern)?;
        }
        validate_options(&rt.compile_options, false)?;
        validate_options(&rt.link_options, true)?;
        targets.push(Target{name:rt.name,kind,root:root_path,include_dirs,excludes,inferred:false,definitions:rt.defines,interface:exported,dependencies:rt.dependencies.into_iter().map(|(key,value)|{if !key.starts_with("target:"){return Err(Diagnostic::new("unsupported-dependency",format!("dependency {key:?} is not a same-package target; use target:<name> in M1")));} let name=&key[7..]; validate_name(name)?; let visibility=match value.as_str(){"private"=>Visibility::Private,"public"=>Visibility::Public,"interface"=>Visibility::Interface,_=>return Err(Diagnostic::new("dependency-visibility",format!("dependency {key:?} must be private, public, or interface")))}; Ok(TargetDependency{target:name.to_owned(),visibility})}).collect::<Result<Vec<_>>>()?,compile_options:rt.compile_options.into_iter().map(OsString::from).collect(),link_options:rt.link_options.into_iter().map(OsString::from).collect()});
    }

    if !explicit_production {
        infer_production(
            &root,
            &raw.package.name,
            &raw.discovery.exclude,
            &standards,
            &mut targets,
        )?;
    }
    infer_auxiliary(&root, &raw.package.name, &raw.discovery, &mut targets)?;
    if !targets.iter().any(|t| {
        matches!(
            t.kind,
            TargetKind::Bin | TargetKind::Static | TargetKind::HeaderOnly
        )
    }) {
        return Err(Diagnostic::new(
            "target-not-found",
            "no production target found; add src/main.c/src/main.cpp, src/lib.c/src/lib.cpp, or declare a production target with root",
        ));
    }
    ensure_unique_names(&targets)?;
    resolve_dependencies(&mut targets)?;
    validate_roots(&targets, &root)?;
    let default_target = match raw.package.default_target {
        Some(name) => {
            validate_name(&name)?;
            if !targets.iter().any(|t| t.name == name) {
                return Err(Diagnostic::new(
                    "default-target",
                    format!("package.default-target {name:?} is not a declared target"),
                ));
            }
            Some(name)
        }
        None => {
            let production: Vec<_> = targets
                .iter()
                .filter(|t| {
                    matches!(
                        t.kind,
                        TargetKind::Bin | TargetKind::Static | TargetKind::HeaderOnly
                    )
                })
                .collect();
            if production.len() == 1 {
                Some(production[0].name.clone())
            } else {
                None
            }
        }
    };
    validate_graph(&targets)?;
    let project = Project {
        root,
        name: raw.package.name,
        version: raw.package.version,
        targets,
        default_target,
        standards,
        profiles,
    };
    for t in &project.targets {
        crate::usage::compile_usage(&project, &t.name)?;
        crate::usage::exported_usage(&project, &t.name)?;
    }
    Ok(project)
}

fn infer_production(
    root: &Path,
    package: &str,
    excludes: &[String],
    standards: &Standards,
    targets: &mut Vec<Target>,
) -> Result<()> {
    let src = root.join("src");
    if fs::symlink_metadata(&src).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Diagnostic::new(
            "target-root-symlink",
            format!(
                "inferred source directory {} must not be a symlink",
                src.display()
            ),
        ));
    }
    let markers = [
        ("main.c", TargetKind::Bin),
        ("main.cpp", TargetKind::Bin),
        ("lib.c", TargetKind::Static),
        ("lib.cpp", TargetKind::Static),
    ];
    let found: Vec<_> = markers
        .iter()
        .filter(|(f, _)| src.join(f).is_file())
        .collect();
    for (file, _) in &found {
        let path = src.join(file);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Diagnostic::new(
                "source-symlink",
                format!("source marker {} is a symlink", path.display()),
            ));
        }
    }
    if found.len() > 1 {
        return Err(Diagnostic::new(
            "ambiguous-production-markers",
            "multiple src/main and src/lib markers exist; declare separate target roots explicitly",
        ));
    }
    if let Some((_file, kind)) = found.first() {
        let mut t = inferred_target(
            package,
            *kind,
            src.canonicalize()
                .map_err(|e| Diagnostic::new("target-root", e.to_string()))?,
            excludes,
        );
        add_package_include(root, &mut t)?;
        if *kind == TargetKind::Static {
            t.interface.cpp_standard = Some(standards.cpp.clone());
            t.interface.c_standard = Some(standards.c.clone());
        }
        targets.push(t);
    }
    infer_bin_container(root, &src.join("bin"), TargetKind::Bin, excludes, targets)?;
    Ok(())
}
fn infer_auxiliary(
    root: &Path,
    package: &str,
    discovery: &RawDiscovery,
    targets: &mut Vec<Target>,
) -> Result<()> {
    if discovery.tests.unwrap_or(true) {
        infer_named_container(
            root,
            &root.join("tests"),
            TargetKind::Test,
            excludes_for(discovery),
            targets,
        )?;
    }
    if discovery.examples.unwrap_or(true) {
        infer_named_container(
            root,
            &root.join("examples"),
            TargetKind::Example,
            excludes_for(discovery),
            targets,
        )?;
    }
    let libs: Vec<String> = targets
        .iter()
        .filter(|t| t.kind == TargetKind::Static || t.kind == TargetKind::HeaderOnly)
        .map(|t| t.name.clone())
        .collect();
    let is_aux = |t: &Target| {
        t.inferred
            && (matches!(t.kind, TargetKind::Test | TargetKind::Example)
                || t.kind == TargetKind::Bin && t.name != package)
    };
    if libs.len() == 1 {
        for t in targets.iter_mut().filter(|t| is_aux(t)) {
            t.dependencies.push(TargetDependency {
                target: libs[0].clone(),
                visibility: Visibility::Private,
            });
        }
    } else if libs.len() > 1 && targets.iter().any(is_aux) {
        return Err(Diagnostic::new(
            "ambiguous-auxiliary-library",
            "inferred tests/examples/auxiliary binaries have multiple libraries available; declare those targets explicitly with dependency edges",
        ));
    }
    Ok(())
}
fn excludes_for(d: &RawDiscovery) -> Vec<String> {
    d.exclude.clone()
}
fn infer_bin_container(
    package_root: &Path,
    path: &Path,
    kind: TargetKind,
    excludes: &[String],
    targets: &mut Vec<Target>,
) -> Result<()> {
    infer_named_container(package_root, path, kind, excludes.to_vec(), targets)
}
fn infer_named_container(
    package_root: &Path,
    container: &Path,
    kind: TargetKind,
    excludes: Vec<String>,
    targets: &mut Vec<Target>,
) -> Result<()> {
    if fs::symlink_metadata(container).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(());
    }
    let entries = match fs::read_dir(container) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Diagnostic::new(
                "discovery-io",
                format!("cannot read {}: {e}", container.display()),
            ));
        }
    };
    let mut entries = entries
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| Diagnostic::new("discovery-io", e.to_string()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| Diagnostic::new("target-name", "target path is not valid UTF-8"))?
            .to_owned();
        if name.starts_with('.') {
            continue;
        }
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            if path.is_file()
                && is_source_extension(path.extension().and_then(|s| s.to_str()).unwrap_or(""))
            {
                return Err(Diagnostic::new(
                    "source-symlink",
                    format!("source symlink {} is unsupported", path.display()),
                ));
            }
            continue;
        }
        if path.is_file() && is_marker_source(&path) {
            let target_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&name)
                .to_owned();
            let mut t = inferred_target(
                &target_name,
                kind,
                path.canonicalize()
                    .map_err(|e| Diagnostic::new("target-root", e.to_string()))?,
                &excludes,
            );
            add_package_include(package_root, &mut t)?;
            targets.push(t);
        } else if path.is_dir() {
            let c = path
                .canonicalize()
                .map_err(|e| Diagnostic::new("target-root", e.to_string()))?;
            let markers = ["main.c", "main.cpp"]
                .iter()
                .filter(|f| c.join(f).is_file())
                .count();
            if markers > 1 {
                return Err(Diagnostic::new(
                    "ambiguous-entry-point",
                    format!("{} contains both main.c and main.cpp", c.display()),
                ));
            }
            if markers == 1 {
                let mut t = inferred_target(&name, kind, c, &excludes);
                add_package_include(package_root, &mut t)?;
                targets.push(t);
            }
        }
    }
    Ok(())
}
fn inferred_target(name: &str, kind: TargetKind, root: PathBuf, excludes: &[String]) -> Target {
    let source_root = if root.is_dir() {
        root.clone()
    } else {
        root.parent().unwrap_or(&root).to_path_buf()
    };
    Target {
        name: name.into(),
        kind,
        root: Some(root),
        include_dirs: vec![source_root],
        excludes: excludes.to_vec(),
        inferred: true,
        definitions: BTreeMap::new(),
        interface: UsageRequirements::default(),
        dependencies: Vec::new(),
        compile_options: Vec::new(),
        link_options: Vec::new(),
    }
}
fn add_package_include(package_root: &Path, target: &mut Target) -> Result<()> {
    let include = package_root.join("include");
    if include.is_dir() {
        let path = include
            .canonicalize()
            .map_err(|e| Diagnostic::new("include-path", e.to_string()))?;
        push_unique(&mut target.include_dirs, path.clone());
        if target.kind == TargetKind::Static {
            push_unique(&mut target.interface.include_dirs, path);
        }
    }
    Ok(())
}
fn is_marker_source(path: &Path) -> bool {
    matches!(path.extension().and_then(|s| s.to_str()), Some("c" | "cpp"))
}
fn is_source_extension(extension: &str) -> bool {
    matches!(
        extension,
        "c" | "cpp" | "cc" | "cxx" | "C" | "m" | "mm" | "s" | "S" | "ixx" | "cppm"
    )
}

fn parse_kind(value: &str) -> Result<TargetKind> {
    match value {
        "bin" => Ok(TargetKind::Bin),
        "static" => Ok(TargetKind::Static),
        "header-only" => Ok(TargetKind::HeaderOnly),
        "test" => Ok(TargetKind::Test),
        "example" => Ok(TargetKind::Example),
        _ => Err(Diagnostic::new(
            "unsupported-target-kind",
            format!(
                "unsupported target kind {value:?}; M1 supports bin, static, header-only, test, and example"
            ),
        )),
    }
}
fn resolve_dependencies(targets: &mut [Target]) -> Result<()> {
    let names: BTreeSet<_> = targets.iter().map(|t| t.name.clone()).collect();
    for t in targets.iter() {
        for d in &t.dependencies {
            if !names.contains(&d.target) {
                return Err(Diagnostic::new(
                    "target-dependency",
                    format!("target {} depends on unknown target {}", t.name, d.target),
                ));
            }
            let dep = targets.iter().find(|x| x.name == d.target).unwrap();
            if !dep.kind.is_library() {
                return Err(Diagnostic::new(
                    "target-dependency-kind",
                    format!(
                        "{} depends on {}, which is not a library target",
                        t.name, d.target
                    ),
                ));
            }
        }
    }
    Ok(())
}
fn ensure_unique_names(targets: &[Target]) -> Result<()> {
    let mut names = BTreeSet::new();
    for t in targets {
        if !names.insert(t.name.to_ascii_lowercase()) {
            return Err(Diagnostic::new(
                "target-name",
                format!(
                    "target name {:?} is declared more than once (names are case-insensitive)",
                    t.name
                ),
            ));
        }
    }
    Ok(())
}
fn validate_roots(targets: &[Target], package: &Path) -> Result<()> {
    for (i, a) in targets.iter().enumerate() {
        let Some(ar) = &a.root else { continue };
        if !ar.starts_with(package) {
            return Err(Diagnostic::new(
                "target-root-escape",
                format!("target {} root escapes package root", a.name),
            ));
        }
        for b in targets.iter().skip(i + 1) {
            let Some(br) = &b.root else { continue };
            if ar == br {
                return Err(Diagnostic::new(
                    "equal-target-roots",
                    format!(
                        "targets {} and {} have the same root {}",
                        a.name,
                        b.name,
                        ar.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}
fn validate_graph(targets: &[Target]) -> Result<()> {
    fn visit(
        name: &str,
        targets: &[Target],
        state: &mut BTreeMap<String, u8>,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => {
                stack.push(name.into());
                return Err(Diagnostic::new(
                    "target-cycle",
                    format!("target dependency cycle: {}", stack.join(" -> ")),
                ));
            }
            _ => (),
        }
        state.insert(name.into(), 1);
        stack.push(name.into());
        let t = targets.iter().find(|t| t.name == name).unwrap();
        for d in &t.dependencies {
            visit(&d.target, targets, state, stack)?;
        }
        stack.pop();
        state.insert(name.into(), 2);
        Ok(())
    }
    let mut s = BTreeMap::new();
    for t in targets {
        visit(&t.name, targets, &mut s, &mut Vec::new())?;
    }
    Ok(())
}

fn validate_standard(value: &str, cpp: bool, label: &str) -> Result<()> {
    let good = if cpp {
        matches!(value, "c++11" | "c++14" | "c++17" | "c++20" | "c++23")
    } else {
        matches!(value, "c99" | "c11" | "c17" | "c23")
    };
    if good {
        Ok(())
    } else {
        Err(Diagnostic::new(
            "language-standard",
            format!("{label} has unsupported value {value:?}"),
        ))
    }
}
fn validate_define(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_alphabetic() && name.as_bytes()[0] != b'_'
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || value.contains('\0')
    {
        return Err(Diagnostic::new(
            "define",
            format!("invalid preprocessor definition {name:?}"),
        ));
    }
    Ok(())
}
fn validate_options(options: &[String], link: bool) -> Result<()> {
    for opt in options {
        let s = opt.as_str();
        let joined_output = (s.starts_with("-o")
            && s.len() > 2
            && !s.starts_with("-openmp")
            && !s.starts_with("-objc"))
            || ["-MF", "-MT", "-MQ", "-MJ"]
                .iter()
                .any(|flag| s.starts_with(flag) && s.len() > flag.len())
            || ["/Fo", "/Fe", "/Fd", "/sourceDependencies:"]
                .iter()
                .any(|flag| s.starts_with(flag) && s.len() > flag.len());
        if s.is_empty()
            || joined_output
            || matches!(
                s,
                "-o" | "-c"
                    | "-S"
                    | "-E"
                    | "-MD"
                    | "-MMD"
                    | "-MP"
                    | "-MF"
                    | "-MT"
                    | "-MQ"
                    | "-MJ"
                    | "-M"
                    | "-MM"
                    | "-Xclang"
                    | "-Xpreprocessor"
                    | "-Xlinker"
            )
            || s.starts_with('@')
            || s.starts_with("--output")
            || s.starts_with("-dependency-file")
            || s.starts_with("-Wp,-M")
            || s.strip_prefix("-Wl,").is_some_and(|flags| {
                flags.split(',').any(|flag| {
                    flag.starts_with("-o")
                        || flag.starts_with("--output")
                        || flag.starts_with("--dependency-file")
                })
            })
            || s.starts_with("-Wp,-M")
            || s.starts_with("/Fo")
            || s.starts_with("/Fe")
            || s.starts_with("/Fd")
            || s == "/sourceDependencies"
        {
            return Err(Diagnostic::new(
                "reserved-build-option",
                format!("raw option {s:?} controls a reserved output/dependency setting"),
            ));
        }
        if link && matches!(s, "-S" | "-E") {
            return Err(Diagnostic::new(
                "reserved-build-option",
                format!("raw link option {s:?} is not supported"),
            ));
        }
    }
    Ok(())
}

fn profile(
    raw: Option<RawProfile>,
    default: ProfileSettings,
    label: &str,
) -> Result<ProfileSettings> {
    let Some(raw) = raw else { return Ok(default) };
    let optimization =
        match raw
            .optimization
            .as_deref()
            .unwrap_or(if default.optimization == Optimization::None {
                "none"
            } else {
                "speed"
            }) {
            "none" => Optimization::None,
            "speed" => Optimization::Speed,
            v => {
                return Err(Diagnostic::new(
                    "profile-optimization",
                    format!("{label}.optimization must be none or speed, got {v:?}"),
                ));
            }
        };
    Ok(ProfileSettings {
        optimization,
        debug: raw.debug.unwrap_or(default.debug),
    })
}
fn resolve_inside(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let p = Path::new(value);
    if p.is_absolute() {
        return Err(Diagnostic::new(
            "manifest-path",
            format!("{label} must be package-relative: {value:?}"),
        ));
    }
    let joined = root.join(p);
    let resolved = joined.canonicalize().map_err(|e| {
        Diagnostic::new(
            "manifest-path",
            format!("cannot resolve {label} {}: {e}", joined.display()),
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(Diagnostic::new(
            "manifest-path-escape",
            format!("{label} {} escapes package root", p.display()),
        ));
    }
    Ok(resolved)
}
fn reject_symlink_path(root: &Path, value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(());
    }
    let mut current = root.to_path_buf();
    for component in path.components() {
        current.push(component.as_os_str());
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(Diagnostic::new(
                "target-root-symlink",
                format!("{label} {} crosses symlink {}", value, current.display()),
            ));
        }
    }
    Ok(())
}
fn resolve_directory(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = resolve_inside(root, value, label)?;
    if !path.is_dir() {
        return Err(Diagnostic::new(
            "include-path",
            format!("{label} {} is not a directory", path.display()),
        ));
    }
    Ok(path)
}
fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path)
    }
}
pub(crate) fn validate_glob(pattern: &str) -> Result<()> {
    if pattern.is_empty()
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains('{')
        || pattern.contains('}')
        || pattern.contains('!')
        || pattern.contains('[')
        || pattern.contains(']')
        || pattern.split('/').any(|p| p == "." || p == "..")
    {
        return Err(Diagnostic::new(
            "invalid-exclude",
            format!(
                "unsupported exclusion pattern {pattern:?}; use relative paths with only *, ?, and **"
            ),
        ));
    }
    globset::Glob::new(pattern).map(|_| ()).map_err(|e| {
        Diagnostic::new(
            "invalid-exclude",
            format!("invalid exclusion pattern {pattern:?}: {e}"),
        )
    })
}
pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(Diagnostic::new(
            "invalid-name",
            "name must be non-empty and start with an ASCII letter or digit",
        ));
    };
    if !first.is_ascii_alphanumeric()
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Diagnostic::new(
            "invalid-name",
            format!(
                "invalid name {name:?}; use ASCII letters, digits, '_' or '-', starting with a letter or digit"
            ),
        ));
    }
    if matches!(
        name,
        "all"
            | "clean"
            | "new"
            | "init"
            | "build"
            | "test"
            | "run"
            | "inspect"
            | "help"
            | "version"
    ) {
        return Err(Diagnostic::new(
            "reserved-name",
            format!("{name:?} is reserved; choose another package or target name"),
        ));
    }
    Ok(())
}
