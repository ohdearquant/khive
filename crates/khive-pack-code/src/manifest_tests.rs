use super::*;

#[test]
fn governing_manifest_index_uses_nearest_sweep_snapshot() {
    use tempfile::TempDir;

    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().canonicalize().expect("canonical root");
    let nested = root.join("nested");
    fs::create_dir(&nested).expect("nested directory");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"outer\"\n").expect("outer manifest");
    fs::write(nested.join("Cargo.toml"), "[package]\nname = \"inner\"\n").expect("inner manifest");
    let (projects, failures) =
        discover_manifests(&root, &BTreeSet::from(["rust"])).expect("discovery");
    assert!(failures.is_empty());
    let index = ManifestIndex::new(&projects);
    fs::remove_file(root.join("Cargo.toml")).expect("remove outer");
    fs::remove_file(nested.join("Cargo.toml")).expect("remove inner");
    assert_eq!(
        index.governing(&nested, &root, "rust"),
        Some((nested, "inner".to_string()))
    );
    assert_eq!(
        index.governing(&root, &root, "rust"),
        Some((root.clone(), "outer".to_string()))
    );
}

#[cfg(unix)]
#[test]
fn swapped_manifest_symlink_outside_root_is_refused() {
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    let fixture = TempDir::new().expect("fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("root");
    let manifest = root.join("Cargo.toml");
    fs::write(&manifest, "[package]\nname = \"inside\"\n").expect("manifest");
    let outside = fixture.path().join("outside.toml");
    fs::write(&outside, "[package]\nname = \"outside\"\n").expect("outside manifest");

    let canonical_root = root.canonicalize().expect("canonical root");
    assert!(manifest
        .canonicalize()
        .expect("checked manifest")
        .starts_with(&canonical_root));
    fs::remove_file(&manifest).expect("remove checked manifest");
    symlink(&outside, &manifest).expect("swap to outside symlink");

    let (projects, failures) = discover_manifests(&canonical_root, &["rust"].into_iter().collect())
        .expect("manifest walk");
    assert!(projects.is_empty());
    assert!(failures.iter().any(|failure| {
        failure.path == canonical_root.join("Cargo.toml")
            && matches!(&failure.error, SourceReadError::Refused(_))
    }));
}

#[test]
fn cargo_toml_without_package_is_not_governing() {
    let text = "[workspace]\nmembers = [\"a\", \"b\"]\n";
    assert!(parse_cargo_toml(Path::new("/tmp"), text).is_none());
}

#[test]
fn cargo_toml_with_package_collects_dependency_kinds_and_scopes() {
    let text = r#"
[package]
name = "foo"

[dependencies]
serde = "1.0"

[dev-dependencies]
tempfile = "3"

[target.'cfg(unix)'.build-dependencies]
cc = "1"
"#;
    let project = parse_cargo_toml(Path::new("/tmp"), text).expect("governing");
    assert_eq!(project.name, "foo");
    assert_eq!(project.language, "rust");
    assert!(project.dependencies.contains(&(
        "serde".to_string(),
        "dependencies".to_string(),
        "normal".to_string()
    )));
    assert!(project.dependencies.contains(&(
        "tempfile".to_string(),
        "dev-dependencies".to_string(),
        "dev".to_string()
    )));
    assert!(project.dependencies.contains(&(
        "cc".to_string(),
        "build-dependencies".to_string(),
        "build".to_string()
    )));
}

#[test]
fn cargo_toml_renamed_dependency_indexes_alias_and_package() {
    let text = r#"
[package]
name = "foo"

[dev-dependencies]
mock = { package = "real-mock", version = "1" }

[target.'cfg(unix)'.dependencies]
unix_alias = { package = "real_unix", version = "2" }
"#;
    let project = parse_cargo_toml(Path::new("/tmp"), text).expect("governing");
    // Alias key catches source-level `use mock::…` imports (Rust
    // imports a renamed dependency under the alias).
    assert!(project.dependencies.contains(&(
        "mock".to_string(),
        "dev-dependencies".to_string(),
        "dev".to_string()
    )));
    // Package key is indexed too: it is the dependency's own project
    // identity, and its declared `dev` scope must survive the lookup.
    assert!(project.dependencies.contains(&(
        "real-mock".to_string(),
        "dev-dependencies".to_string(),
        "dev".to_string()
    )));
    assert!(project
        .renames
        .contains(&("mock".to_string(), "real-mock".to_string())));
    assert!(project.dependencies.contains(&(
        "real_unix".to_string(),
        "dependencies".to_string(),
        "normal".to_string()
    )));
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
    assert!(project.dependencies.contains(&(
        "requests".to_string(),
        "dependencies".to_string(),
        "normal".to_string()
    )));
    assert!(project.dependencies.contains(&(
        "click".to_string(),
        "dependencies".to_string(),
        "normal".to_string()
    )));
}

#[test]
fn package_json_extracts_dependency_sections() {
    let text = r#"{"name": "baz", "dependencies": {"left-pad": "1.0.0"}, "devDependencies": {"jest": "29.0.0"}}"#;
    let project = parse_package_json(Path::new("/tmp"), text).expect("governing");
    assert_eq!(project.name, "baz");
    assert!(project.dependencies.contains(&(
        "left-pad".to_string(),
        "dependencies".to_string(),
        "normal".to_string()
    )));
    assert!(project.dependencies.contains(&(
        "jest".to_string(),
        "devDependencies".to_string(),
        "dev".to_string()
    )));
}
