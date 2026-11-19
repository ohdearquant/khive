//! The shipped `kkernel code-audit` policy ranks every crate under `crates/` at
//! the longest-path depth of its production dependency graph. This test
//! recomputes that depth from the crate manifests instead of carrying a second
//! copy of the table, so a crate added without a row, or a row that has fallen
//! behind the dependency graph, fails here and names the crate.
//!
//! Production dependencies are `[dependencies]` and `[build-dependencies]`,
//! plain or under a `[target.*]` table, which is what the audit evaluates by
//! default. `[dev-dependencies]` are excluded, as the policy header says: several
//! packs carry cyclic dev-only test dependencies. Rows for projects with no
//! manifest under `crates/` (the npm and Python projects the same table also
//! ranks) are never looked up, because the walk starts from the crates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PRODUCTION_SECTIONS: [&str; 2] = ["dependencies", "build-dependencies"];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("kkernel lives directly under crates/")
        .to_path_buf()
}

fn read_table(path: &Path) -> toml::Table {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    toml::from_str(&text).unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()))
}

/// Package names a manifest depends on outside `[dev-dependencies]`. A renamed
/// dependency (`alias = { package = "real" }`) counts under its real name.
fn production_dependencies(manifest: &toml::Table) -> Vec<String> {
    let mut scopes = vec![manifest];
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        scopes.extend(targets.values().filter_map(toml::Value::as_table));
    }
    let mut names = Vec::new();
    for scope in scopes {
        for section in PRODUCTION_SECTIONS {
            let Some(table) = scope.get(section).and_then(toml::Value::as_table) else {
                continue;
            };
            for (key, spec) in table {
                let package = spec.get("package").and_then(toml::Value::as_str);
                names.push(package.unwrap_or(key.as_str()).to_string());
            }
        }
    }
    names
}

/// Every package under `crates/*/Cargo.toml`, mapped to the workspace packages
/// it depends on in production.
fn production_graph(crates: &Path) -> BTreeMap<String, Vec<String>> {
    let mut manifests = BTreeMap::new();
    for entry in std::fs::read_dir(crates).expect("read crates dir") {
        let dir = entry.expect("read crates entry").path();
        let path = dir.join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        let manifest = read_table(&path);
        let Some(name) = manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        manifests.insert(name.to_string(), manifest);
    }
    let mut graph = BTreeMap::new();
    for (name, manifest) in &manifests {
        let mut dependencies = production_dependencies(manifest);
        dependencies.retain(|dependency| manifests.contains_key(dependency));
        graph.insert(name.clone(), dependencies);
    }
    graph
}

/// Edges on the longest path from `name` down to a crate with no workspace
/// dependency. Cargo refuses a cyclic production graph, so the recursion ends.
fn longest_path(
    name: &str,
    graph: &BTreeMap<String, Vec<String>>,
    memo: &mut BTreeMap<String, i64>,
) -> i64 {
    if let Some(&known) = memo.get(name) {
        return known;
    }
    let mut deepest: i64 = 0;
    for dependency in &graph[name] {
        deepest = deepest.max(longest_path(dependency, graph, memo) + 1);
    }
    memo.insert(name.to_string(), deepest);
    deepest
}

#[test]
fn every_crate_has_a_row_at_its_longest_production_path() {
    let crates = crates_dir();
    let graph = production_graph(&crates);
    assert!(
        graph.contains_key("kkernel"),
        "the manifest walk under {} did not reach kkernel",
        crates.display()
    );

    let policy = read_table(&crates.join("kkernel/policy/code-audit-khive.toml"));
    let ranks = policy
        .get("crate_ranks")
        .and_then(toml::Value::as_table)
        .expect("the policy declares a crate_ranks table");

    let mut memo = BTreeMap::new();
    let mut problems = Vec::new();
    for name in graph.keys() {
        let expected = longest_path(name, &graph, &mut memo);
        match ranks.get(name) {
            None => problems.push(format!("{name}: no row, expected rank {expected}")),
            Some(row) if row.as_integer() == Some(expected) => {}
            Some(row) => {
                let message = format!("{name}: row is {row}, longest path is {expected}");
                problems.push(message);
            }
        }
    }
    assert!(
        problems.is_empty(),
        "crate_ranks does not match the longest production dependency path:\n{}",
        problems.join("\n")
    );
}
