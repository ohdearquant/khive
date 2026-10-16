//! Regex-based import scan for `code.ingest` L1.5 (ADR-085 Amendment 2 B3).
//!
//! Syntax-only, per B2: no type-checking, no compilation. Coverage-floor
//! extraction — good enough to produce static-coupling `depends_on` edges and
//! module paths, not a full parser. The regexes are deliberately scope-blind:
//! guarded, type-check-only, and function-local imports are included, so these
//! edges do not assert a module-initialization or runtime dependency. Pure
//! functions; no filesystem or storage access beyond the caller-supplied file
//! path (used only for path arithmetic).

use std::path::{Component, Path};

use regex::Regex;
use std::sync::OnceLock;

/// File extensions this PR's L1.5 tier scans, keyed by language.
pub(crate) fn extension_for_language(language: &str) -> Option<&'static str> {
    match language {
        "rust" => Some("rs"),
        "python" => Some("py"),
        "typescript" => Some("ts"),
        _ => None,
    }
}

fn rust_use_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+([A-Za-z_][A-Za-z0-9_:]*)").unwrap()
    })
}

fn rust_extern_crate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*extern\s+crate\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap())
}

fn python_import_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*import\s+([A-Za-z_][A-Za-z0-9_.]*)").unwrap())
}

fn python_from_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*from\s+([.]*[A-Za-z0-9_.]*)\s+import\s+([^\n#;]+)").unwrap()
    })
}

fn ts_import_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"import(?:[^'";]*?from)?\s*['"]([^'"]+)['"]"#).unwrap())
}

fn ts_require_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"require\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap())
}

/// The plain names bound by a `from ... import <names>` clause: splits on
/// commas, drops `as` aliases and empty entries, and stops before `*` or any
/// parenthesized form (neither names a resolvable module).
fn python_imported_names(names: &str) -> Vec<&str> {
    names
        .split(',')
        .filter_map(|part| part.split_whitespace().next())
        .filter(|name| {
            !name.is_empty()
                && *name != "*"
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
        .collect()
}

/// Extract raw, scope-unclassified import specifiers from `content`.
///
/// Leading whitespace is accepted intentionally, so imports nested in a
/// function or guard (including Python `if TYPE_CHECKING:` blocks) are part of
/// this static-coupling signal. See ADR-085 Amendment 2 B3.
pub(crate) fn extract_raw_imports(language: &str, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    match language {
        "rust" => {
            for c in rust_use_re().captures_iter(content) {
                out.push(c[1].trim_end_matches(':').to_string());
            }
            for c in rust_extern_crate_re().captures_iter(content) {
                out.push(c[1].to_string());
            }
        }
        "python" => {
            for c in python_import_re().captures_iter(content) {
                out.push(c[1].to_string());
            }
            for c in python_from_re().captures_iter(content) {
                let module = c[1].to_string();
                if module.chars().all(|ch| ch == '.') {
                    // A dots-only specifier names only the containing
                    // package: for `from . import z` the imported name *is*
                    // the target module, so emit one specifier per name for
                    // `classify_python` to resolve against the package base.
                    // Attribute imports (`from . import run` where `run` is a
                    // function) resolve to their containing module via the
                    // resolver's longest-prefix fallback
                    // (`module_candidate_specifiers`). When no plain name can
                    // be extracted (star or parenthesized forms), fall back to
                    // the bare package specifier so the pre-expansion behavior
                    // is preserved rather than dropping the reference.
                    let names = python_imported_names(&c[2]);
                    if names.is_empty() {
                        out.push(module);
                    } else {
                        for name in names {
                            out.push(format!("{module}{name}"));
                        }
                    }
                } else {
                    // `from .mod import ...` / `from pkg.mod import ...`: the
                    // module specifier already names the target — keep the
                    // imported names out of the specifier.
                    out.push(module);
                }
            }
        }
        "typescript" => {
            for c in ts_import_re().captures_iter(content) {
                out.push(c[1].to_string());
            }
            for c in ts_require_re().captures_iter(content) {
                out.push(c[1].to_string());
            }
        }
        _ => {}
    }
    out
}

/// Resolution outcome of one raw import specifier against the declaring
/// file's own module path and project name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// A module in the SAME `source_project` — carries the target's
    /// `module_path`.
    IntraModule(String),
    /// A different (possibly not-yet-ingested) project — carries the
    /// external project's name (B6 unresolved-specifier candidate).
    ExternalProject(String),
    /// A same-module or standard-library reference — not a project edge.
    Skip,
}

const RUST_STD_CRATES: &[&str] = &["std", "core", "alloc", "proc_macro", "test"];
const PY_STDLIB: &[&str] = &[
    "os",
    "sys",
    "re",
    "json",
    "typing",
    "collections",
    "itertools",
    "functools",
    "pathlib",
    "subprocess",
    "logging",
    "unittest",
    "abc",
    "dataclasses",
    "enum",
    "math",
    "random",
    "datetime",
    "io",
    "argparse",
    "asyncio",
];

fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

pub(crate) fn classify_import(
    language: &str,
    raw: &str,
    current_module_path: &str,
    project_name: &str,
    is_package: bool,
) -> Resolved {
    match language {
        "rust" => classify_rust(raw, current_module_path, project_name),
        "python" => classify_python(raw, current_module_path, project_name, is_package),
        "typescript" => classify_typescript(raw),
        _ => Resolved::Skip,
    }
}

fn classify_rust(raw: &str, current_module_path: &str, project_name: &str) -> Resolved {
    if let Some(rest) = raw.strip_prefix("crate::") {
        return module_or_skip(rest);
    }
    if raw.starts_with("self::") || raw == "self" {
        return Resolved::Skip;
    }
    if let Some(rest) = raw.strip_prefix("super::") {
        let mut parent: Vec<&str> = current_module_path.split("::").collect();
        parent.pop();
        if parent.is_empty() {
            return module_or_skip(rest);
        }
        return Resolved::IntraModule(format!("{}::{}", parent.join("::"), rest));
    }
    let mut segments = raw.splitn(2, "::");
    let first = segments.next().unwrap_or_default();
    if first.is_empty() || first == "crate" {
        return Resolved::Skip;
    }
    if RUST_STD_CRATES.contains(&first) {
        return Resolved::Skip;
    }
    if normalize_crate_name(first) == normalize_crate_name(project_name) {
        return module_or_skip(segments.next().unwrap_or(""));
    }
    Resolved::ExternalProject(first.to_string())
}

fn module_or_skip(module_path: &str) -> Resolved {
    if module_path.is_empty() {
        Resolved::Skip
    } else {
        Resolved::IntraModule(module_path.to_string())
    }
}

fn classify_python(
    raw: &str,
    current_module_path: &str,
    project_name: &str,
    is_package: bool,
) -> Resolved {
    if let Some(stripped) = raw.strip_prefix('.') {
        let mut level = 1usize;
        let mut rest = stripped;
        while let Some(s) = rest.strip_prefix('.') {
            level += 1;
            rest = s;
        }
        let mut base: Vec<&str> = current_module_path.split('.').collect();
        // A single leading dot means "the containing package". For a regular
        // module that is one level up from its own path; for a package
        // (`__init__.py`) it is the module path itself, because
        // `module_path_for_file` already collapsed `pkg/__init__.py` to
        // `pkg` — popping again would resolve into the parent package.
        if !is_package {
            base.pop();
        }
        for _ in 1..level {
            base.pop();
        }
        // A relative import that climbs to or above the scanner's project
        // root cannot be attributed — Python itself rejects relative imports
        // beyond the top-level package — so never record a root-level guess
        // that a later same-name module would silently satisfy.
        if base.is_empty() {
            return Resolved::Skip;
        }
        // A raw specifier that is still dots-only here (`from . import` with
        // no resolvable names, e.g. a star or parenthesized import) names the
        // package itself.
        let target = if rest.is_empty() {
            base.join(".")
        } else {
            format!("{}.{}", base.join("."), rest)
        };
        if target == current_module_path {
            return Resolved::Skip;
        }
        return module_or_skip(&target);
    }
    if raw.is_empty() {
        return Resolved::Skip;
    }
    let first = raw.split('.').next().unwrap_or_default();
    if PY_STDLIB.contains(&first) {
        return Resolved::Skip;
    }
    let normalized_first = first.replace('-', "_");
    let normalized_project = project_name.replace('-', "_");
    if normalized_first == normalized_project {
        return Resolved::IntraModule(raw.to_string());
    }
    Resolved::ExternalProject(first.to_string())
}

fn classify_typescript(raw: &str) -> Resolved {
    if raw.starts_with('.') {
        return Resolved::IntraModule(raw.to_string());
    }
    if raw.starts_with('@') {
        let mut parts = raw.splitn(3, '/');
        let scope = parts.next().unwrap_or_default();
        let pkg = parts.next().unwrap_or_default();
        if pkg.is_empty() {
            return Resolved::Skip;
        }
        return Resolved::ExternalProject(format!("{scope}/{pkg}"));
    }
    let first = raw.split('/').next().unwrap_or_default();
    if first.is_empty() {
        return Resolved::Skip;
    }
    Resolved::ExternalProject(first.to_string())
}

/// Join a TypeScript-style relative specifier (`./foo`, `../bar/baz`)
/// against the declaring file's own directory (relative to `project_root`),
/// producing an extension-stripped module path.
pub(crate) fn resolve_relative_ts_module(current_file_dir_rel: &Path, specifier: &str) -> String {
    let mut components: Vec<String> = current_file_dir_rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other.to_string()),
        }
    }
    let joined = components.join("/");
    joined
        .strip_suffix(".ts")
        .or_else(|| joined.strip_suffix(".tsx"))
        .unwrap_or(&joined)
        .to_string()
}

/// Compute the language-native canonical `module_path` for a source file,
/// relative to `project_root` (B4). Returns `None` for a file outside
/// `project_root`.
pub(crate) fn module_path_for_file(
    file: &Path,
    project_root: &Path,
    language: &str,
) -> Option<String> {
    let rel = file.strip_prefix(project_root).ok()?;
    let mut components: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    // Rust's `src/` layout convention is lexical, not filesystem-probed
    // (fixtures may be constructed in-memory, and the walk already knows the
    // file exists): strip a leading `src` component for Rust only.
    if language == "rust" && components.first().map(String::as_str) == Some("src") {
        components.remove(0);
    }
    if components.is_empty() {
        return None;
    }

    match language {
        "rust" => {
            let stem = components.last()?.strip_suffix(".rs")?.to_string();
            let is_root = components.len() == 1;
            *components.last_mut()? = stem.clone();
            if is_root && stem == "lib" {
                return Some("crate".to_string());
            }
            if is_root && stem == "main" {
                return Some("crate::main".to_string());
            }
            if stem == "mod" {
                components.pop();
                if components.is_empty() {
                    return Some("crate".to_string());
                }
            }
            Some(components.join("::"))
        }
        "python" => {
            let stem = components.last()?.strip_suffix(".py")?.to_string();
            *components.last_mut()? = stem.clone();
            if stem == "__init__" {
                components.pop();
                if components.is_empty() {
                    return None;
                }
            }
            Some(components.join("."))
        }
        "typescript" => {
            let last = components.last()?;
            let stripped = last
                .strip_suffix(".ts")
                .or_else(|| last.strip_suffix(".tsx"))?
                .to_string();
            *components.last_mut()? = stripped;
            Some(components.join("/"))
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "imports_tests.rs"]
mod tests;
