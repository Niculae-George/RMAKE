//! Parser for the small Makefile dependency format emitted by C/C++ compilers.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use rmake_model::{Diagnostic, Result};

/// Parse a compiler dependency file and return normalized prerequisite paths.
///
/// The parser accepts Make escapes for whitespace, `#`, `:`, and backslash,
/// plus LF/CRLF line continuations. Relative paths are resolved against `cwd`.
pub fn parse_depfile(bytes: &[u8], cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut logical = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\n' {
                logical.push(b' ');
                i += 2;
                continue;
            }
            if bytes[i + 1] == b'\r' && bytes.get(i + 2) == Some(&b'\n') {
                logical.push(b' ');
                i += 3;
                continue;
            }
        }
        logical.push(bytes[i]);
        i += 1;
    }

    let mut prerequisites = Vec::<Vec<u8>>::new();
    let mut token = Vec::new();
    let mut escaped = false;
    let mut in_prerequisites = false;
    let mut comment = false;
    let mut saw_rule = false;

    let flush = |token: &mut Vec<u8>, prerequisites: &mut Vec<Vec<u8>>| {
        if !token.is_empty() {
            prerequisites.push(std::mem::take(token));
        }
    };

    for &byte in &logical {
        if comment {
            if byte == b'\n' {
                comment = false;
                if in_prerequisites {
                    flush(&mut token, &mut prerequisites);
                    in_prerequisites = false;
                }
            }
            continue;
        }
        if escaped {
            if byte == b'$' {
                token.push(b'$');
            } else {
                token.push(byte);
            }
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'#' => comment = true,
            b':' if !in_prerequisites => {
                saw_rule = true;
                token.clear(); // discard target list
                in_prerequisites = true;
            }
            b'\n' | b'\r' => {
                if in_prerequisites {
                    flush(&mut token, &mut prerequisites);
                    in_prerequisites = false;
                }
            }
            b' ' | b'\t' => {
                if in_prerequisites {
                    flush(&mut token, &mut prerequisites);
                } else {
                    token.clear();
                }
            }
            _ => {
                token.push(byte);
            }
        }
    }
    if escaped {
        return Err(Diagnostic::new(
            "RMAKE-S201",
            "dependency file ends with an incomplete escape",
        ));
    }
    if in_prerequisites {
        flush(&mut token, &mut prerequisites);
    }
    if !saw_rule {
        return Err(Diagnostic::new(
            "RMAKE-S201",
            "dependency file has no target rule",
        ));
    }

    // Make writes a literal dollar as `$$`. Collapse each pair after tokenizing.
    let paths = prerequisites
        .into_iter()
        .map(|mut item| {
            let mut collapsed = Vec::with_capacity(item.len());
            let mut at = 0;
            while at < item.len() {
                if item[at] == b'$' && item.get(at + 1) == Some(&b'$') {
                    collapsed.push(b'$');
                    at += 2;
                } else {
                    collapsed.push(item[at]);
                    at += 1;
                }
            }
            item = collapsed;
            path_from_bytes(item)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut unique = BTreeSet::new();
    for path in paths {
        unique.insert(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        });
    }
    Ok(unique.into_iter().collect())
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf> {
    let value = String::from_utf8(bytes).map_err(|_| {
        Diagnostic::new(
            "RMAKE-S201",
            "dependency path is not valid UTF-8 on this platform",
        )
    })?;
    Ok(PathBuf::from(value))
}
