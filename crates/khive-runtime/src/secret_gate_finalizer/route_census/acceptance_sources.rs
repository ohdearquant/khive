use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::source_inventory::source_path;

fn automatic_target(path: &str) -> bool {
    !path.contains('/') || path.ends_with("/main.rs") && path.matches('/').count() == 1
}

fn enabled(target: &toml::Value) -> bool {
    target.get("test").and_then(toml::Value::as_bool) != Some(false)
        && target.get("harness").and_then(toml::Value::as_bool) != Some(false)
}

pub(super) fn acceptance_roots(
    sources: &[(String, String)],
    manifests: &[(String, String)],
) -> Result<BTreeSet<String>, String> {
    let paths = sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let mut roots = BTreeSet::new();
    for (member, text) in manifests {
        let manifest = text
            .parse::<toml::Value>()
            .map_err(|error| format!("{member}/Cargo.toml: {error}"))?;
        let automatic = |key| {
            manifest
                .get("package")
                .and_then(|package| package.get(key))
                .and_then(toml::Value::as_bool)
                != Some(false)
        };
        let library = manifest.get("lib");
        if library.is_some_and(enabled) || library.is_none() && automatic("autolib") {
            let name = library
                .and_then(|lib| lib.get("path"))
                .and_then(toml::Value::as_str)
                .unwrap_or("src/lib.rs");
            if let Some(path) = source_path(&Path::new(member).join(name)) {
                if paths.contains(&path) {
                    roots.insert(path);
                }
            }
        }
        for (kind, directory, auto_key) in [
            ("bin", "src/bin", "autobins"),
            ("test", "tests", "autotests"),
        ] {
            let mut target_roots = BTreeSet::new();
            let mut explicit_roots = BTreeSet::new();
            if kind == "bin"
                && automatic(auto_key)
                && paths.contains(&format!("{member}/src/main.rs"))
            {
                target_roots.insert(format!("{member}/src/main.rs"));
            }
            let prefix = format!("{member}/{directory}/");
            if automatic(auto_key) {
                target_roots.extend(
                    paths
                        .iter()
                        .filter(|path| path.strip_prefix(&prefix).is_some_and(automatic_target))
                        .cloned(),
                );
            }
            for target in manifest
                .get(kind)
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = target
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| format!("{member}: {kind} target has no name"))?;
                let single = format!("{member}/{directory}/{name}.rs");
                let nested = format!("{member}/{directory}/{name}/main.rs");
                target_roots.remove(&single);
                target_roots.remove(&nested);
                let package_name = manifest
                    .get("package")
                    .and_then(|package| package.get("name"))
                    .and_then(toml::Value::as_str);
                if kind == "bin" && package_name == Some(name) {
                    target_roots.remove(&format!("{member}/src/main.rs"));
                }
                let path = if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                    source_path(&Path::new(member).join(path)).ok_or_else(|| {
                        format!("{member}: target {name} leaves workspace sources")
                    })?
                } else if kind == "bin"
                    && package_name == Some(name)
                    && paths.contains(&format!("{member}/src/main.rs"))
                {
                    format!("{member}/src/main.rs")
                } else if paths.contains(&single) {
                    single
                } else {
                    nested
                };
                // An explicit alias owns this source even when its harness is disabled.
                target_roots.remove(&path);
                if !enabled(target) {
                    continue;
                }
                if !paths.contains(&path) {
                    return Err(format!("{member}: missing test target source {path}"));
                }
                explicit_roots.insert(path);
            }
            target_roots.extend(explicit_roots);
            roots.extend(target_roots);
        }
    }
    Ok(roots)
}

fn test_sources(dir: &Path, root: &Path, output: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
    {
        let entry = entry.expect("test source entry");
        let path = entry.path();
        if entry.file_type().expect("test source file type").is_dir() {
            test_sources(&path, root, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push((
                path.strip_prefix(root)
                    .expect("workspace test source")
                    .to_string_lossy()
                    .replace('\\', "/"),
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
            ));
        }
    }
}

pub(super) fn live_acceptance_sources() -> (Vec<(String, String)>, BTreeSet<String>) {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf();
    let workspace = std::fs::read_to_string(crates.join("Cargo.toml"))
        .expect("workspace manifest")
        .parse::<toml::Value>()
        .expect("parse workspace manifest");
    let mut sources = Vec::new();
    let mut manifests = Vec::new();
    for member in workspace["workspace"]["members"]
        .as_array()
        .expect("workspace members")
    {
        let member = member.as_str().expect("workspace member path");
        let directory = crates.join(member);
        manifests.push((
            member.to_owned(),
            std::fs::read_to_string(directory.join("Cargo.toml")).expect("member manifest"),
        ));
        for source_dir in ["src", "tests"] {
            let path = directory.join(source_dir);
            if path.is_dir() {
                test_sources(&path, &crates, &mut sources);
            }
        }
    }
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let roots = acceptance_roots(&sources, &manifests).expect("acceptance test targets");
    (sources, roots)
}
