use std::fs;
use std::path::{Path, PathBuf};

use rmake_model::{
    Diagnostic, Optimization, ProfileSettings, Profiles, Project, Result, Standards,
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
    root: String,
    #[serde(rename = "private-includes", default)]
    private_includes: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

pub fn find_manifest(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize().map_err(|error| {
        Diagnostic::new(
            "manifest-search-path",
            format!("cannot resolve search path {}: {error}", start.display()),
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
            return candidate.canonicalize().map_err(|error| {
                Diagnostic::new(
                    "manifest-path",
                    format!("cannot resolve {}: {error}", candidate.display()),
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
    let manifest = manifest.canonicalize().map_err(|error| {
        Diagnostic::new(
            "manifest-path",
            format!("cannot resolve {}: {error}", manifest.display()),
        )
    })?;
    let text = fs::read_to_string(&manifest).map_err(|error| {
        Diagnostic::new(
            "manifest-read",
            format!("cannot read {}: {error}", manifest.display()),
        )
    })?;
    parse_project(&manifest, &text)
}

pub fn parse_project(manifest: &Path, text: &str) -> Result<Project> {
    let raw: RawManifest = toml::from_str(text).map_err(|error| {
        let message = error.to_string();
        let unsupported = [
            "dependencies",
            "workspace",
            "public-includes",
            "defines",
            "compile-options",
            "link-options",
            "interface",
        ]
        .into_iter()
        .find(|field| message.contains(&format!("unknown field `{field}`")));
        match unsupported {
            Some(field) => Diagnostic::new(
                "unsupported-manifest-feature",
                format!("{field} is not supported in M1a: {message}"),
            ),
            None => Diagnostic::new(
                "manifest-parse",
                format!("{}: {message}", manifest.display()),
            ),
        }
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
    if raw.targets.len() > 1 {
        return Err(Diagnostic::new(
            "target-count",
            "M1a supports one executable target; declare at most one [[targets]] entry",
        ));
    }
    // M1a does not construct auxiliary targets, so these accepted switches have no target to enable.
    let _auxiliary_target_settings = (raw.discovery.tests, raw.discovery.examples);

    let root = manifest
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let root = root.canonicalize().map_err(|error| {
        Diagnostic::new(
            "package-root",
            format!("cannot resolve package root {}: {error}", root.display()),
        )
    })?;

    let target = if let Some(raw_target) = raw.targets.into_iter().next() {
        validate_name(&raw_target.name)?;
        if raw_target.kind != "bin" {
            return Err(Diagnostic::new(
                "unsupported-target-kind",
                format!(
                    "target kind {:?} is unsupported in M1a; only kind = \"bin\" is supported",
                    raw_target.kind
                ),
            ));
        }
        let target_root = resolve_inside(&root, &raw_target.root, "target root")?;
        if !target_root.is_dir() {
            return Err(Diagnostic::new(
                "target-root",
                format!("target root {} is not a directory", target_root.display()),
            ));
        }
        let mut include_dirs = vec![target_root.clone()];
        let package_include = root.join("include");
        if package_include.is_dir() {
            push_unique(
                &mut include_dirs,
                resolve_inside(&root, "include", "package include")?,
            );
        }
        for include in raw_target.private_includes {
            let resolved = resolve_inside(&root, &include, "private include")?;
            if !resolved.is_dir() {
                return Err(Diagnostic::new(
                    "include-path",
                    format!("private include {} is not a directory", resolved.display()),
                ));
            }
            push_unique(&mut include_dirs, resolved);
        }
        let mut excludes = raw.discovery.exclude;
        excludes.extend(raw_target.exclude);
        rmake_model::BinaryTarget {
            name: raw_target.name,
            root: target_root,
            include_dirs,
            excludes,
            inferred: false,
        }
    } else {
        let src = root.join("src");
        let main_c = src.join("main.c");
        let main_cpp = src.join("main.cpp");
        let lib_c = src.join("lib.c");
        let lib_cpp = src.join("lib.cpp");
        if main_c.is_file() && main_cpp.is_file() {
            return Err(Diagnostic::new(
                "ambiguous-entry-point",
                "both src/main.c and src/main.cpp exist; keep one executable entry point",
            ));
        }
        if lib_c.is_file() || lib_cpp.is_file() {
            return Err(Diagnostic::new(
                "unsupported-library-inference",
                "src/lib.c/lib.cpp indicates a library, which M1a does not support; declare a binary target with an explicit root",
            ));
        }
        if !main_c.is_file() && !main_cpp.is_file() {
            return Err(Diagnostic::new(
                "target-not-found",
                "no executable target found; add src/main.c or src/main.cpp, or declare one [[targets]] binary with root",
            ));
        }
        let src = src.canonicalize().map_err(|error| {
            Diagnostic::new(
                "target-root",
                format!("cannot resolve {}: {error}", src.display()),
            )
        })?;
        if !src.starts_with(&root) {
            return Err(Diagnostic::new(
                "target-root-escape",
                "inferred src target root resolves outside package root",
            ));
        }
        let mut include_dirs = vec![src.clone()];
        let package_include = root.join("include");
        if package_include.is_dir() {
            push_unique(
                &mut include_dirs,
                resolve_inside(&root, "include", "package include")?,
            );
        }
        rmake_model::BinaryTarget {
            name: raw.package.name.clone(),
            root: src,
            include_dirs,
            excludes: raw.discovery.exclude,
            inferred: true,
        }
    };

    if let Some(default_target) = raw.package.default_target {
        validate_name(&default_target)?;
        if default_target != target.name {
            return Err(Diagnostic::new(
                "default-target",
                format!(
                    "package.default-target {:?} does not match the sole binary target {:?}",
                    default_target, target.name
                ),
            ));
        }
    }

    let standards = Standards {
        c: raw.build.c_standard.unwrap_or_else(|| "c17".into()),
        cpp: raw.build.cpp_standard.unwrap_or_else(|| "c++20".into()),
    };
    if !matches!(standards.c.as_str(), "c99" | "c11" | "c17" | "c23") {
        return Err(Diagnostic::new(
            "language-standard",
            format!(
                "unsupported C standard {:?}; use c99, c11, c17, or c23",
                standards.c
            ),
        ));
    }
    if !matches!(
        standards.cpp.as_str(),
        "c++11" | "c++14" | "c++17" | "c++20" | "c++23"
    ) {
        return Err(Diagnostic::new(
            "language-standard",
            format!(
                "unsupported C++ standard {:?}; use c++11, c++14, c++17, c++20, or c++23",
                standards.cpp
            ),
        ));
    }
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
    for exclude in &target.excludes {
        validate_glob(exclude)?;
    }

    Ok(Project {
        root,
        name: raw.package.name,
        version: raw.package.version,
        target,
        standards,
        profiles,
    })
}

fn profile(
    raw: Option<RawProfile>,
    default: ProfileSettings,
    label: &str,
) -> Result<ProfileSettings> {
    let Some(raw) = raw else { return Ok(default) };
    let optimization = match raw
        .optimization
        .as_deref()
        .unwrap_or(match default.optimization {
            Optimization::None => "none",
            Optimization::Speed => "speed",
        }) {
        "none" => Optimization::None,
        "speed" => Optimization::Speed,
        other => {
            return Err(Diagnostic::new(
                "profile-optimization",
                format!("{label}.optimization must be \"none\" or \"speed\", got {other:?}"),
            ));
        }
    };
    Ok(ProfileSettings {
        optimization,
        debug: raw.debug.unwrap_or(default.debug),
    })
}

fn resolve_inside(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(Diagnostic::new(
            "manifest-path",
            format!("{label} must be package-relative: {value:?}"),
        ));
    }
    let joined = root.join(path);
    let resolved = joined.canonicalize().map_err(|error| {
        Diagnostic::new(
            "manifest-path",
            format!("cannot resolve {label} {}: {error}", joined.display()),
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(Diagnostic::new(
            "manifest-path-escape",
            format!("{label} {} escapes package root", path.display()),
        ));
    }
    Ok(resolved)
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
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
        || pattern.split('/').any(|part| part == "." || part == "..")
    {
        return Err(Diagnostic::new(
            "invalid-exclude",
            format!(
                "unsupported exclusion pattern {pattern:?}; use relative paths with only *, ?, and **"
            ),
        ));
    }
    globset::Glob::new(pattern).map(|_| ()).map_err(|error| {
        Diagnostic::new(
            "invalid-exclude",
            format!("invalid exclusion pattern {pattern:?}: {error}"),
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
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(Diagnostic::new(
            "invalid-name",
            format!(
                "invalid name {name:?}; use ASCII letters, digits, '_' or '-', starting with a letter or digit"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn project(text: &str) -> Result<Project> {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("src/main.c"),
            "int main(void) { return 0; }",
        )
        .unwrap();
        let manifest = temp.path().join(MANIFEST_NAME);
        parse_project(&manifest, text)
    }

    #[test]
    fn parses_defaults_and_custom_profile_settings() {
        let p = project("schema = 1\n[package]\nname = \"hello-world\"\nversion = \"0.1\"\n[profiles.release]\noptimization = \"none\"\ndebug = true\n").unwrap();
        assert_eq!(p.standards.c, "c17");
        assert_eq!(p.standards.cpp, "c++20");
        assert_eq!(
            p.profiles.dev,
            ProfileSettings {
                optimization: Optimization::None,
                debug: true
            }
        );
        assert_eq!(
            p.profiles.release,
            ProfileSettings {
                optimization: Optimization::None,
                debug: true
            }
        );
    }

    #[test]
    fn explicit_root_and_build_settings_are_loaded() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("app/custom")).unwrap();
        fs::create_dir_all(temp.path().join("private")).unwrap();
        let manifest = temp.path().join(MANIFEST_NAME);
        let p = parse_project(&manifest, "schema=1\n[package]\nname=\"app\"\nversion=\"1\"\ndefault-target=\"runner\"\n[build]\nc-standard=\"c11\"\n[[targets]]\nname=\"runner\"\nkind=\"bin\"\nroot=\"app/custom\"\nprivate-includes=[\"private\"]\nexclude=[\"old/**\"]\n").unwrap();
        assert_eq!(
            p.target.root,
            temp.path().join("app/custom").canonicalize().unwrap()
        );
        assert_eq!(p.standards.c, "c11");
        assert_eq!(p.target.include_dirs.len(), 2);
        assert_eq!(p.target.excludes, ["old/**"]);
    }

    #[test]
    fn rejects_unknown_unsupported_and_escaping_configuration() {
        assert!(
            project("schema=2\n[package]\nname=\"a\"\nversion=\"1\"\n")
                .unwrap_err()
                .code
                == "manifest-schema"
        );
        assert!(
            project("schema=1\n[package]\nname=\"a\"\nversion=\"1\"\n[dependencies]\n")
                .unwrap_err()
                .code
                == "unsupported-manifest-feature"
        );
        assert!(project("schema=1\n[package]\nname=\"a\"\nversion=\"1\"\n[[targets]]\nname=\"x\"\nkind=\"static\"\nroot=\"src\"\n").unwrap_err().code == "unsupported-target-kind");
        assert!(project("schema=1\n[package]\nname=\"a\"\nversion=\"1\"\n[[targets]]\nname=\"x\"\nkind=\"bin\"\nroot=\"../outside\"\n").unwrap_err().code == "manifest-path");
        assert!(validate_name("éclair").is_err());
    }

    #[test]
    fn rejects_canonical_root_escape_and_ambiguous_entry_markers() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        let outside = temp.path().join("outside/src");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let manifest = package.join(MANIFEST_NAME);
        assert_eq!(
            parse_project(
                &manifest,
                "schema=1\n[package]\nname=\"app\"\nversion=\"1\"\n[[targets]]\nname=\"app\"\nkind=\"bin\"\nroot=\"../outside/src\"\n",
            )
            .unwrap_err()
            .code,
            "manifest-path-escape"
        );

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/main.c"), "").unwrap();
        fs::write(temp.path().join("src/main.cpp"), "").unwrap();
        assert_eq!(
            parse_project(
                &temp.path().join(MANIFEST_NAME),
                "schema=1\n[package]\nname=\"app\"\nversion=\"1\"\n",
            )
            .unwrap_err()
            .code,
            "ambiguous-entry-point"
        );
    }

    #[test]
    fn finds_nearest_parent_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join("src/nested")).unwrap();
        fs::write(project.join(MANIFEST_NAME), "").unwrap();
        assert_eq!(
            find_manifest(&project.join("src/nested")).unwrap(),
            project.join(MANIFEST_NAME).canonicalize().unwrap()
        );
    }
}
