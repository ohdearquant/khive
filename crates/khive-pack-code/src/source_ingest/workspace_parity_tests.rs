use super::*;

use rusqlite::{Connection, OpenFlags};

type EdgeSet = BTreeSet<(String, String)>;
type ScopedEdges = BTreeMap<String, EdgeSet>;
type ReverseGraph = BTreeMap<String, BTreeSet<String>>;

const SCOPES: [&str; 3] = ["normal", "dev", "build"];

fn empty_scopes() -> ScopedEdges {
    SCOPES
        .into_iter()
        .map(|scope| (scope.to_string(), BTreeSet::new()))
        .collect()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("metadata {key} must be text"))
}

fn declared_members(workspace: &Path) -> BTreeSet<PathBuf> {
    let source =
        fs::read_to_string(workspace.join("Cargo.toml")).expect("read workspace declarations");
    let document: toml::Value = source.parse().expect("parse workspace declarations");
    let workspace_table = document.get("workspace").expect("workspace table");
    let paths = |key: &str| -> Vec<PathBuf> {
        workspace_table.get(key).map(|value| {
            value.as_array().unwrap_or_else(|| panic!("workspace.{key} must be an array")).iter().map(|value| {
                let path = value.as_str().unwrap_or_else(|| panic!("workspace.{key} entry must be text"));
                assert!(!path.chars().any(|character| "*?[]{}".contains(character)),
                    "workspace.{key} glob requires qualified expansion before this test can run: {path}");
                workspace.join(path).canonicalize().unwrap_or_else(|error| panic!("workspace.{key} path {path}: {error}"))
            }).collect()
        }).unwrap_or_default()
    };
    let excluded: BTreeSet<_> = paths("exclude").into_iter().collect();
    let mut declared: BTreeSet<_> = paths("members")
        .into_iter()
        .filter(|path| !excluded.contains(path))
        .collect();
    if document.get("package").is_some() {
        declared.insert(workspace.to_path_buf());
    }
    assert!(
        !declared.is_empty(),
        "declared workspace population must not be empty"
    );
    declared
}

struct WorkspaceGraph {
    members: BTreeMap<String, PathBuf>,
    edges: ScopedEdges,
    declared_count: usize,
    metadata_count: usize,
}

#[derive(Clone, Copy)]
enum WorkspaceKind {
    Checkout,
    ControlFixture,
}

fn cargo_graph(workspace: &Path, kind: WorkspaceKind) -> WorkspaceGraph {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(workspace).args([
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--offline",
    ]);
    if matches!(kind, WorkspaceKind::Checkout) {
        command.arg("--locked");
    }
    // Only the test-owned path-only fixture may create its own Cargo.lock.
    let output = command
        .output()
        .expect("run cargo metadata with the test Cargo executable");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    assert_eq!(
        Path::new(text(&metadata, "workspace_root"))
            .canonicalize()
            .expect("metadata workspace root"),
        workspace
    );
    let member_ids = metadata["workspace_members"]
        .as_array()
        .expect("workspace member IDs");
    let ids: BTreeSet<_> = member_ids
        .iter()
        .map(|id| id.as_str().expect("workspace member ID text"))
        .collect();
    assert!(
        !ids.is_empty(),
        "metadata workspace population must not be empty"
    );
    assert_eq!(
        ids.len(),
        member_ids.len(),
        "duplicate metadata workspace member IDs"
    );
    let packages: Vec<_> = metadata["packages"]
        .as_array()
        .expect("metadata packages")
        .iter()
        .filter(|package| ids.contains(text(package, "id")))
        .collect();
    assert_eq!(
        packages.len(),
        ids.len(),
        "metadata package population is incomplete"
    );
    let package_ids: BTreeSet<_> = packages.iter().map(|package| text(package, "id")).collect();
    assert_eq!(
        package_ids, ids,
        "metadata must provide every workspace package exactly once"
    );
    let mut members = BTreeMap::new();
    for package in &packages {
        let name = text(package, "name").to_string();
        let manifest = Path::new(text(package, "manifest_path"));
        let directory = manifest
            .parent()
            .expect("member manifest parent")
            .canonicalize()
            .expect("member directory");
        assert!(
            members.insert(name.clone(), directory).is_none(),
            "ambiguous member package name: {name}"
        );
    }
    let declared = declared_members(workspace);
    let member_paths: BTreeSet<_> = members.values().cloned().collect();
    assert_eq!(member_paths, declared,
        "metadata must include every declared member; implicit members require qualified declaration handling");
    let by_path: BTreeMap<_, _> = members
        .iter()
        .map(|(name, path)| (path.clone(), name.clone()))
        .collect();
    assert_eq!(
        by_path.len(),
        members.len(),
        "duplicate member package paths"
    );
    let mut edges = empty_scopes();
    for package in &packages {
        let source = text(package, "name");
        for dependency in package["dependencies"]
            .as_array()
            .expect("declared package dependencies")
        {
            let scope = match dependency.get("kind") {
                Some(Value::Null) => "normal",
                Some(Value::String(kind)) if SCOPES.contains(&kind.as_str()) => kind,
                other => panic!("unexpected Cargo dependency kind: {other:?}"),
            };
            // Cargo's declared path identifies local packages. A name match alone
            // cannot turn a registry or non-member path dependency into a member.
            let path = match dependency.get("path") {
                None | Some(Value::Null) => continue,
                Some(Value::String(path)) => {
                    Path::new(path).canonicalize().expect("dependency path")
                }
                other => panic!("unexpected Cargo dependency path: {other:?}"),
            };
            let Some(target) = by_path.get(&path) else {
                continue;
            };
            assert_eq!(
                text(dependency, "name"),
                target.as_str(),
                "dependency package name/path disagreement"
            );
            // Optional and target-qualified declarations are intentionally retained.
            // The name is the package name; rename is only its consumer-side alias.
            edges
                .get_mut(scope)
                .expect("known scope")
                .insert((source.to_string(), target.clone()));
        }
    }
    WorkspaceGraph {
        members,
        edges,
        declared_count: declared.len(),
        metadata_count: ids.len(),
    }
}

fn qualify_project_identities(workspace: &Path, members: &BTreeMap<String, PathBuf>) {
    // L1 project identity is project_uuid(package name), with no persisted
    // manifest path. Reject any non-member manifest sharing a member's name
    // before accepting that identity as the workspace member's map row.
    let (projects, failures) = manifest::discover_manifests(workspace, &BTreeSet::from(["rust"]))
        .expect("qualify project manifest identities");
    assert!(
        failures.is_empty(),
        "manifest identity discovery failures: {failures:?}"
    );
    let mut paths_by_name: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for project in projects {
        paths_by_name
            .entry(project.name)
            .or_default()
            .push(project.root.canonicalize().expect("project root"));
    }
    for (name, expected_path) in members {
        let actual = paths_by_name
            .get(name)
            .unwrap_or_else(|| panic!("workspace member manifest not discovered: {name}"));
        assert_eq!(
            actual.as_slice(),
            std::slice::from_ref(expected_path),
            "member name must identify exactly its canonical manifest root: {name}"
        );
    }
}

fn map_graph(path: &Path, namespace: &str, members: &BTreeMap<String, PathBuf>) -> ScopedEdges {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open fresh ingested map read-only");
    let mut projects = connection.prepare(
        "SELECT id, name, json_extract(properties, '$.source_project') FROM entities WHERE namespace = ?1 AND kind = 'project' AND deleted_at IS NULL"
    ).expect("prepare complete map project read");
    let rows = projects
        .query_map([namespace], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .expect("read map projects")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("decode map projects");
    let mut member_ids = BTreeMap::new();
    let mut found = BTreeSet::new();
    for (id, name, source_project) in rows {
        if members.contains_key(&name) {
            assert_eq!(
                source_project.as_deref(),
                Some(name.as_str()),
                "stored source project identity"
            );
            assert_eq!(
                id,
                project_uuid(&name).to_string(),
                "stored project identity for {name}"
            );
            assert!(
                found.insert(name.clone()),
                "duplicate map project for member {name}"
            );
            member_ids.insert(id, name);
        }
    }
    let expected_names: BTreeSet<_> = members.keys().cloned().collect();
    assert_eq!(
        found, expected_names,
        "map must contain every workspace member"
    );
    let mut query = connection.prepare(
        "SELECT source_id, target_id, metadata FROM graph_edges WHERE namespace = ?1 AND relation = 'depends_on' AND deleted_at IS NULL AND target_backend IS NULL"
    ).expect("prepare complete scoped map edge read");
    let rows = query
        .query_map([namespace], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .expect("read map edges")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("decode map edges");
    let mut edges = empty_scopes();
    for (source, target, metadata) in rows {
        let (Some(source), Some(target)) = (member_ids.get(&source), member_ids.get(&target))
        else {
            continue;
        };
        let metadata: Value =
            serde_json::from_str(&metadata.expect("internal dependency metadata"))
                .expect("map metadata JSON");
        for scope in metadata["dependency_scopes"]
            .as_array()
            .expect("stored dependency_scopes array")
        {
            let scope = scope.as_str().expect("stored dependency scope text");
            edges
                .get_mut(scope)
                .unwrap_or_else(|| panic!("unknown stored scope {scope}"))
                .insert((source.clone(), target.clone()));
        }
    }
    edges
}

fn reverse_graph(edges: &EdgeSet) -> ReverseGraph {
    let mut reverse = ReverseGraph::new();
    for (source, target) in edges {
        reverse
            .entry(target.clone())
            .or_default()
            .insert(source.clone());
    }
    reverse
}

fn reverse_dependents(seed: &str, reverse: &ReverseGraph) -> BTreeSet<String> {
    let mut visited = BTreeSet::from([seed.to_string()]);
    let mut pending = vec![seed.to_string()];
    while let Some(target) = pending.pop() {
        if let Some(sources) = reverse.get(&target) {
            for source in sources {
                if visited.insert(source.clone()) {
                    pending.push(source.clone());
                }
            }
        }
    }
    visited
}

async fn compare_workspace(workspace: &Path, kind: WorkspaceKind) {
    let source = match kind {
        WorkspaceKind::Checkout => "checkout",
        WorkspaceKind::ControlFixture => "control fixture",
    };
    let WorkspaceGraph {
        members,
        edges: cargo,
        declared_count,
        metadata_count,
    } = cargo_graph(workspace, kind);
    qualify_project_identities(workspace, &members);
    if matches!(kind, WorkspaceKind::ControlFixture) {
        let owner = env!("CARGO_PKG_NAME").to_string();
        let middle = "d8-parity-middle".to_string();
        let leaf = "d8-parity-leaf".to_string();
        let expected = [
            (
                "normal",
                EdgeSet::from([(owner.clone(), middle.clone()), (middle, leaf.clone())]),
            ),
            ("dev", EdgeSet::from([(owner.clone(), leaf.clone())])),
            ("build", EdgeSet::from([(owner, leaf)])),
        ];
        for (scope, edges) in expected {
            assert_eq!(
                cargo[scope], edges,
                "controlled fixture must declare its {scope} witness"
            );
            assert!(
                !cargo[scope].is_empty(),
                "controlled fixture has no {scope} witness"
            );
        }
    }
    assert!(
        cargo.values().any(|edges| edges
            .iter()
            .any(|(source, target)| source.as_str() == env!("CARGO_PKG_NAME") && source != target)),
        "member-drop control requires a non-self internal dependency from the ingest crate"
    );
    let scratch = tempfile::tempdir().expect("scratch map");
    let locks = tempfile::tempdir().expect("scratch volume locks");
    let database = scratch.path().join("map.db");
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(database.clone()),
        packs: Vec::new(),
        volume_lock_dir: Some(locks.path().to_path_buf()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("scratch runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("scratch namespace");
    let report = run_code_ingest(
        &runtime,
        &token,
        CodeSourceIngestOptions {
            path: workspace,
            languages: BTreeSet::from(["rust"]),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("ingest workspace manifests");
    assert_eq!(
        report.blocked_count, 0,
        "workspace ingest blocked records: {:?}",
        report.blocked
    );
    assert_eq!(
        report.manifest_files_refused, 0,
        "workspace ingest refused manifests"
    );
    // A fresh temporary source need not belong to a Git repository. Accept only
    // that exact provenance warning there; every checkout/manifest warning fails.
    let fixture_warning = format!(
        "git metadata unavailable for {}; source revision degraded to {UNVERSIONED_REVISION}",
        workspace.display()
    );
    let warnings_ok = report.warnings.is_empty()
        || (matches!(kind, WorkspaceKind::ControlFixture)
            && report.source_revision == UNVERSIONED_REVISION
            && report.warnings.len() == 1
            && report.warnings[0] == fixture_warning);
    assert!(
        warnings_ok,
        "workspace ingest warnings ({source}): {:?}",
        report.warnings
    );
    let map = map_graph(&database, token.namespace().as_str(), &members);
    eprintln!("workspace parity population: source={source}, declared={declared_count}, metadata={metadata_count}, compared={}", members.len());
    let mut differences = Vec::new();
    for scope in SCOPES {
        eprintln!(
            "workspace parity edges: source={source}, scope={scope}, cargo={}, map={}",
            cargo[scope].len(),
            map[scope].len()
        );
        let only_cargo: Vec<_> = cargo[scope].difference(&map[scope]).collect();
        let only_map: Vec<_> = map[scope].difference(&cargo[scope]).collect();
        if !only_cargo.is_empty() || !only_map.is_empty() {
            differences.push(format!(
                "direct edges scope={scope}: cargo-only={only_cargo:?}, map-only={only_map:?}"
            ));
        }
        let expected_reverse = reverse_graph(&cargo[scope]);
        let actual_reverse = reverse_graph(&map[scope]);
        for member in members.keys() {
            let expected = reverse_dependents(member, &expected_reverse);
            let actual = reverse_dependents(member, &actual_reverse);
            let only_cargo: Vec<_> = expected.difference(&actual).collect();
            let only_map: Vec<_> = actual.difference(&expected).collect();
            if !only_cargo.is_empty() || !only_map.is_empty() {
                differences.push(format!("member={member} scope={scope}: cargo-only={only_cargo:?}, map-only={only_map:?}"));
            }
        }
    }
    assert!(
        differences.is_empty(),
        "workspace dependency parity mismatch ({source}):\n{}",
        differences.join("\n")
    );
}

fn control_workspace() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("controlled Cargo workspace");
    fs::write(
        directory.path().join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = ["owner", "middle", "leaf"]
exclude = ["outside"]
"#,
    )
    .expect("fixture workspace manifest");
    let owner = format!(
        r#"[package]
name = "{}"
version = "0.0.0"
edition = "2021"

[dependencies]
renamed_middle = {{ package = "d8-parity-middle", path = "../middle", optional = true }}
outside = {{ package = "d8-parity-outside", path = "../outside" }}

[dev-dependencies]
d8-parity-leaf = {{ path = "../leaf" }}

[target.'cfg(any())'.build-dependencies]
d8-parity-leaf = {{ path = "../leaf" }}
"#,
        env!("CARGO_PKG_NAME")
    );
    let middle = r#"[package]
name = "d8-parity-middle"
version = "0.0.0"
edition = "2021"

[dependencies]
d8-parity-leaf = { path = "../leaf" }
"#
    .to_string();
    let leaf = r#"[package]
name = "d8-parity-leaf"
version = "0.0.0"
edition = "2021"
"#
    .to_string();
    let outside = r#"[package]
name = "d8-parity-outside"
version = "0.0.0"
edition = "2021"
"#
    .to_string();
    for (name, manifest) in [
        ("owner", owner),
        ("middle", middle),
        ("leaf", leaf),
        ("outside", outside),
    ] {
        let package = directory.path().join(name);
        fs::create_dir_all(package.join("src")).expect("fixture package source directory");
        fs::write(package.join("Cargo.toml"), manifest).expect("fixture package manifest");
        fs::write(package.join("src/lib.rs"), "pub fn fixture() {}\n")
            .expect("fixture library target");
    }
    directory
}

#[tokio::test]
async fn workspace_reverse_dependency_sets_match_cargo_metadata() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates workspace")
        .canonicalize()
        .expect("workspace directory");
    compare_workspace(&workspace, WorkspaceKind::Checkout).await;

    let fixture = control_workspace();
    let workspace = fixture
        .path()
        .canonicalize()
        .expect("controlled workspace directory");
    compare_workspace(&workspace, WorkspaceKind::ControlFixture).await;
}
