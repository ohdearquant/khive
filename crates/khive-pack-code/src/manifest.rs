//! Manifest discovery + parsing for `code.ingest` L1 (ADR-085 Amendment 2 B3, B4).
//!
//! Pure filesystem + parsing helpers: no storage/runtime dependency. Directory
//! walks skip common non-source, non-manifest-bearing trees (`.git`, `target`,
//! `node_modules`, `__pycache__`, `.venv`) to keep discovery bounded.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

/// One of the three languages this PR's L1/L1.5 tiers cover. Lean is deferred
/// to the Scanner/Extractor pipeline (B2) and has no manifest tier.
pub(crate) const LANGUAGES: &[&str] = &["rust", "python", "typescript"];

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
];

/// A governing manifest found under the ingested `path`, with its declared
/// dependencies (B3 L1: `project depends_on project` edges).
#[derive(Debug, Clone)]
pub(crate) struct ManifestProject {
    pub root: PathBuf,
    pub name: String,
    pub language: &'static str,
    /// `(dependency_name, dependency_kind)`.
    pub dependencies: Vec<(String, String)>,
}

/// Walk `path` recursively and parse every governing manifest found
/// (B4: a workspace-only `Cargo.toml`/`pyproject.toml` with no package name
/// is not governing and is skipped).
pub(crate) fn discover_manifests(
    path: &Path,
    languages: &BTreeSet<&'static str>,
) -> std::io::Result<Vec<ManifestProject>> {
    let mut out = Vec::new();
    walk_dir(path, languages, &mut out)?;
    Ok(out)
}

fn walk_dir(
    dir: &Path,
    languages: &BTreeSet<&'static str>,
    out: &mut Vec<ManifestProject>,
) -> std::io::Result<()> {
    if languages.contains("rust") {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.is_file() {
            if let Ok(text) = fs::read_to_string(&cargo_toml) {
                if let Some(project) = parse_cargo_toml(dir, &text) {
                    out.push(project);
                }
            }
        }
    }
    if languages.contains("python") {
        let pyproject = dir.join("pyproject.toml");
        if pyproject.is_file() {
            if let Ok(text) = fs::read_to_string(&pyproject) {
                if let Some(project) = parse_pyproject_toml(dir, &text) {
                    out.push(project);
                }
            }
        }
    }
    if languages.contains("typescript") {
        let package_json = dir.join("package.json");
        if package_json.is_file() {
            if let Ok(text) = fs::read_to_string(&package_json) {
                if let Some(project) = parse_package_json(dir, &text) {
                    out.push(project);
                }
            }
        }
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
            continue;
        }
        walk_dir(&entry.path(), languages, out)?;
    }
    Ok(())
}

/// Parse a `Cargo.toml`; returns `None` when it declares no `[package].name`
/// (a virtual/workspace-only manifest, B4).
pub(crate) fn parse_cargo_toml(root: &Path, text: &str) -> Option<ManifestProject> {
    let doc: TomlValue = text.parse().ok()?;
    let name = doc.get("package")?.get("name")?.as_str()?.to_string();

    let mut dependencies = Vec::new();
    for (section, kind) in [
        ("dependencies", "dependencies"),
        ("dev-dependencies", "dev-dependencies"),
        ("build-dependencies", "build-dependencies"),
    ] {
        if let Some(TomlValue::Table(table)) = doc.get(section) {
            for dep_name in table.keys() {
                dependencies.push((dep_name.clone(), kind.to_string()));
            }
        }
    }

    Some(ManifestProject {
        root: root.to_path_buf(),
        name,
        language: "rust",
        dependencies,
    })
}

/// Extract the bare package name from a PEP 508 requirement string, e.g.
/// `"requests>=2.0; python_version >= '3.8'"` -> `"requests"`.
fn pep508_name(spec: &str) -> Option<String> {
    let end = spec
        .find(|c: char| c.is_whitespace() || "<>=!~;[".contains(c))
        .unwrap_or(spec.len());
    let name = spec[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Parse a `pyproject.toml`; returns `None` when it declares no
/// `[project].name` (B4 — a Poetry-only or tool-only manifest is not
/// governing in v1).
pub(crate) fn parse_pyproject_toml(root: &Path, text: &str) -> Option<ManifestProject> {
    let doc: TomlValue = text.parse().ok()?;
    let project = doc.get("project")?;
    let name = project.get("name")?.as_str()?.to_string();

    let mut dependencies = Vec::new();
    if let Some(TomlValue::Array(arr)) = project.get("dependencies") {
        for item in arr {
            if let Some(spec) = item.as_str() {
                if let Some(dep_name) = pep508_name(spec) {
                    dependencies.push((dep_name, "dependencies".to_string()));
                }
            }
        }
    }
    if let Some(TomlValue::Table(groups)) = project.get("optional-dependencies") {
        for (group, arr) in groups {
            if let TomlValue::Array(arr) = arr {
                for item in arr {
                    if let Some(spec) = item.as_str() {
                        if let Some(dep_name) = pep508_name(spec) {
                            dependencies.push((dep_name, format!("optional-dependencies:{group}")));
                        }
                    }
                }
            }
        }
    }

    Some(ManifestProject {
        root: root.to_path_buf(),
        name,
        language: "python",
        dependencies,
    })
}

/// Parse a `package.json`; returns `None` when it declares no `name` (B4).
pub(crate) fn parse_package_json(root: &Path, text: &str) -> Option<ManifestProject> {
    let doc: JsonValue = serde_json::from_str(text).ok()?;
    let name = doc.get("name")?.as_str()?.to_string();

    let mut dependencies = Vec::new();
    for (section, kind) in [
        ("dependencies", "dependencies"),
        ("devDependencies", "devDependencies"),
        ("peerDependencies", "peerDependencies"),
        ("optionalDependencies", "optionalDependencies"),
    ] {
        if let Some(JsonValue::Object(obj)) = doc.get(section) {
            for dep_name in obj.keys() {
                dependencies.push((dep_name.clone(), kind.to_string()));
            }
        }
    }

    Some(ManifestProject {
        root: root.to_path_buf(),
        name,
        language: "typescript",
        dependencies,
    })
}

/// Per-ingest memo cache for `find_governing_manifest_memoized`: a directory
/// to its governing-manifest result, once known. Every file under the same
/// directory re-probing and re-parsing the identical ancestor chain of
/// manifests would be O(files x depth) over a whole-tree ingest; caching by
/// directory means each unique directory is probed once regardless of how
/// many files it (or its descendants) contain.
pub(crate) type ManifestMemo = HashMap<PathBuf, Option<(PathBuf, String)>>;

/// Find the nearest governing manifest at or above `file_dir` (B4: "the
/// nearest governing manifest at or above that file", read literally — the
/// walk is not bounded by the ingested folder; `code.ingest(path="pkg/src")`
/// must still attribute files to `pkg`'s own manifest one level above the
/// ingest root), walking up to the filesystem root before giving up. Caches
/// every ancestor
/// directory visited along the walk, not just `file_dir` itself: once one
/// leaf's lookup has walked up through and parsed N ancestors to find (or
/// fail to find) a manifest, every one of those N ancestor directories is
/// cached too, so a sibling leaf elsewhere under the same ancestor chain
/// reuses the cached result instead of re-walking and re-parsing manifests
/// its own lookup never directly visited before.
pub(crate) fn find_governing_manifest_memoized(
    file_dir: &Path,
    language: &str,
    memo: &mut ManifestMemo,
) -> Option<(PathBuf, String)> {
    let mut unresolved: Vec<PathBuf> = Vec::new();
    let mut dir = Some(file_dir);
    let result = loop {
        let Some(d) = dir else { break None };
        if let Some(cached) = memo.get(d) {
            break cached.clone();
        }
        let found = match language {
            "rust" => fs::read_to_string(d.join("Cargo.toml"))
                .ok()
                .and_then(|t| parse_cargo_toml(d, &t)),
            "python" => fs::read_to_string(d.join("pyproject.toml"))
                .ok()
                .and_then(|t| parse_pyproject_toml(d, &t)),
            "typescript" => fs::read_to_string(d.join("package.json"))
                .ok()
                .and_then(|t| parse_package_json(d, &t)),
            _ => None,
        };
        unresolved.push(d.to_path_buf());
        if let Some(project) = found {
            break Some((project.root, project.name));
        }
        dir = d.parent();
    };
    for d in unresolved {
        memo.insert(d, result.clone());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1087 item 6 regression: two sibling leaf directories under the same
    /// ancestor manifest must resolve identically, and the shared ancestors
    /// visited by the first lookup must end up cached so the second lookup
    /// does not have to walk (or re-parse) them itself.
    #[test]
    fn memoized_lookup_caches_shared_ancestor_for_sibling_leaves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"root\"\n",
        )
        .expect("write manifest");
        let leaf_a = tmp.path().join("src/a");
        let leaf_b = tmp.path().join("src/b");
        std::fs::create_dir_all(&leaf_a).expect("mkdir a");
        std::fs::create_dir_all(&leaf_b).expect("mkdir b");

        let mut memo = ManifestMemo::new();
        let (root_a, name_a) = find_governing_manifest_memoized(&leaf_a, "rust", &mut memo)
            .expect("leaf_a resolves to the root manifest");
        assert_eq!(root_a, tmp.path().to_path_buf());
        assert_eq!(name_a, "root");
        // The shared `src/` ancestor (and the root itself) are cached after
        // leaf_a's walk, even though only leaf_a's own lookup visited them --
        // leaf_b's lookup below reuses them instead of re-walking.
        assert!(memo.contains_key(tmp.path().join("src").as_path()));
        assert!(memo.contains_key(tmp.path()));

        let (root_b, name_b) = find_governing_manifest_memoized(&leaf_b, "rust", &mut memo)
            .expect("leaf_b resolves to the same root manifest");
        assert_eq!(root_a, root_b);
        assert_eq!(name_a, name_b);
        assert!(memo.contains_key(leaf_b.as_path()));
    }

    #[test]
    fn cargo_toml_without_package_is_not_governing() {
        let text = "[workspace]\nmembers = [\"a\", \"b\"]\n";
        assert!(parse_cargo_toml(Path::new("/tmp"), text).is_none());
    }

    #[test]
    fn cargo_toml_with_package_collects_dependency_kinds() {
        let text = r#"
[package]
name = "foo"

[dependencies]
serde = "1.0"

[dev-dependencies]
tempfile = "3"
"#;
        let project = parse_cargo_toml(Path::new("/tmp"), text).expect("governing");
        assert_eq!(project.name, "foo");
        assert_eq!(project.language, "rust");
        assert!(project
            .dependencies
            .contains(&("serde".to_string(), "dependencies".to_string())));
        assert!(project
            .dependencies
            .contains(&("tempfile".to_string(), "dev-dependencies".to_string())));
    }

    #[test]
    fn pyproject_toml_extracts_pep508_names() {
        let text = r#"
[project]
name = "bar"
dependencies = ["requests>=2.0", "click"]
"#;
        let project = parse_pyproject_toml(Path::new("/tmp"), text).expect("governing");
        assert_eq!(project.name, "bar");
        assert!(project
            .dependencies
            .contains(&("requests".to_string(), "dependencies".to_string())));
        assert!(project
            .dependencies
            .contains(&("click".to_string(), "dependencies".to_string())));
    }

    #[test]
    fn package_json_extracts_dependency_sections() {
        let text = r#"{"name": "baz", "dependencies": {"left-pad": "1.0.0"}, "devDependencies": {"jest": "29.0.0"}}"#;
        let project = parse_package_json(Path::new("/tmp"), text).expect("governing");
        assert_eq!(project.name, "baz");
        assert!(project
            .dependencies
            .contains(&("left-pad".to_string(), "dependencies".to_string())));
        assert!(project
            .dependencies
            .contains(&("jest".to_string(), "devDependencies".to_string())));
    }
}
