use std::path::PathBuf;

use rmake_model::{Diagnostic, Project, Result, UsageRequirements, Visibility};

pub fn compile_usage(project: &Project, target: &str) -> Result<UsageRequirements> {
    let own = project.target(target)?;
    let mut merged = UsageRequirements {
        include_dirs: own.include_dirs.clone(),
        definitions: own.definitions.clone(),
        ..UsageRequirements::default()
    };
    merge(
        &mut merged,
        own.interface.clone(),
        &format!("target {} interface", target),
    )?;
    for dep in &own.dependencies {
        if dep.visibility != Visibility::Interface {
            merge(
                &mut merged,
                exported_usage(project, &dep.target)?,
                &format!("{} -> {}", target, dep.target),
            )?;
        }
    }
    Ok(merged)
}

pub fn exported_usage(project: &Project, target: &str) -> Result<UsageRequirements> {
    exported_cached(project, target, &mut std::collections::BTreeMap::new())
}

fn exported_cached(
    project: &Project,
    target: &str,
    cache: &mut std::collections::BTreeMap<String, UsageRequirements>,
) -> Result<UsageRequirements> {
    if let Some(usage) = cache.get(target) {
        return Ok(usage.clone());
    }
    let own = project.target(target)?;
    let mut merged = own.interface.clone();
    for dep in &own.dependencies {
        if matches!(dep.visibility, Visibility::Public | Visibility::Interface) {
            merge(
                &mut merged,
                exported_cached(project, &dep.target, cache)?,
                &format!("{} -> {}", target, dep.target),
            )?;
        }
    }
    cache.insert(target.to_owned(), merged.clone());
    Ok(merged)
}

/// Return transitive link requirements in consumer-before-dependency order.
/// The starting target itself is omitted; callers add its own artifact separately.
pub fn link_closure(project: &Project, target: &str) -> Result<Vec<String>> {
    let root = project.target(target)?;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = root
        .dependencies
        .iter()
        .rev()
        .map(|d| d.target.clone())
        .collect();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let t = project.target(&name)?;
        if !t.kind.is_library() {
            return Err(Diagnostic::new(
                "target-dependency-kind",
                format!("{} is not a library target", name),
            ));
        }
        order.push(name);
        for dep in t.dependencies.iter().rev() {
            stack.push(dep.target.clone());
        }
    }
    let order: Vec<String> = project
        .targets
        .iter()
        .filter(|t| seen.contains(&t.name))
        .map(|t| t.name.clone())
        .collect();
    let rank: BTreeMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect();
    let mut indegree: BTreeMap<String, usize> = order.iter().map(|n| (n.clone(), 0)).collect();
    for consumer in &order {
        let t = project.target(consumer)?;
        let mut unique = BTreeSet::new();
        for dep in &t.dependencies {
            if rank.contains_key(&dep.target) && unique.insert(dep.target.clone()) {
                *indegree.get_mut(&dep.target).unwrap() += 1;
            }
        }
    }
    let mut ready: VecDeque<String> = order
        .iter()
        .filter(|n| indegree[*n] == 0)
        .cloned()
        .collect();
    let mut out = Vec::with_capacity(order.len());
    while !ready.is_empty() {
        let mut candidates: Vec<_> = ready.drain(..).collect();
        candidates.sort_by_key(|n| rank[n]);
        let name = candidates.remove(0);
        ready.extend(candidates);
        out.push(name.clone());
        for dep in &project.target(&name)?.dependencies {
            if let Some(degree) = indegree.get_mut(&dep.target) {
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(dep.target.clone());
                }
            }
        }
    }
    if out.len() != order.len() {
        return Err(Diagnostic::new(
            "target-cycle",
            "target dependency cycle prevents link ordering",
        ));
    }
    Ok(out)
}

fn merge(into: &mut UsageRequirements, from: UsageRequirements, provenance: &str) -> Result<()> {
    for path in from.include_dirs {
        push_path(&mut into.include_dirs, path);
    }
    for (name, value) in from.definitions {
        if let Some(previous) = into.definitions.get(&name) {
            if previous != &value {
                return Err(Diagnostic::new(
                    "definition-conflict",
                    format!(
                        "definition {name} has conflicting values {previous:?} and {value:?} while applying {provenance}"
                    ),
                ));
            }
        } else {
            into.definitions.insert(name, value);
        }
    }
    into.c_standard = merge_standard(into.c_standard.take(), from.c_standard, false, provenance)?;
    into.cpp_standard = merge_standard(
        into.cpp_standard.take(),
        from.cpp_standard,
        true,
        provenance,
    )?;
    Ok(())
}
fn push_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let identity = path
        .canonicalize()
        .unwrap_or_else(|_| normalize(path.clone()));
    if !paths
        .iter()
        .any(|p| p.canonicalize().unwrap_or_else(|_| normalize(p.clone())) == identity)
    {
        paths.push(path);
    }
}
fn normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(c.as_os_str()),
        }
    }
    out
}
fn merge_standard(
    current: Option<String>,
    incoming: Option<String>,
    cpp: bool,
    provenance: &str,
) -> Result<Option<String>> {
    match (current, incoming) {
        (None, x) | (x, None) => Ok(x),
        (Some(a), Some(b)) => {
            let ar = standard_rank(&a, cpp).ok_or_else(|| {
                Diagnostic::new(
                    "language-standard",
                    format!("unsupported language standard {a:?}"),
                )
            })?;
            let br = standard_rank(&b, cpp).ok_or_else(|| {
                Diagnostic::new(
                    "language-standard",
                    format!("unsupported language standard {b:?}"),
                )
            })?;
            let _ = provenance;
            Ok(Some(if ar >= br { a } else { b }))
        }
    }
}
fn standard_rank(value: &str, cpp: bool) -> Option<u8> {
    if cpp {
        match value {
            "c++11" => Some(1),
            "c++14" => Some(2),
            "c++17" => Some(3),
            "c++20" => Some(4),
            "c++23" => Some(5),
            _ => None,
        }
    } else {
        match value {
            "c99" => Some(1),
            "c11" => Some(2),
            "c17" => Some(3),
            "c23" => Some(4),
            _ => None,
        }
    }
}
