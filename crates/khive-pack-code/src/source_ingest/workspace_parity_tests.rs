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

fn cargo_graph(workspace: &Path) -> WorkspaceGraph {
    let output = Command::new(env!("CARGO"))
        .current_dir(workspace)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
            "--locked",
        ])
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

#[tokio::test]
async fn workspace_reverse_dependency_sets_match_cargo_metadata() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates workspace")
        .canonicalize()
        .expect("workspace directory");
    let WorkspaceGraph {
        members,
        edges: cargo,
        declared_count,
        metadata_count,
    } = cargo_graph(&workspace);
    qualify_project_identities(&workspace, &members);
    // These oracle-side witnesses make the three production controls nonvacuous.
    // A future workspace without them needs an explicit real-pipeline fixture.
    for scope in SCOPES {
        assert!(
            !cargo[scope].is_empty(),
            "parity control population requires an internal {scope} dependency"
        );
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
            path: &workspace,
            languages: BTreeSet::from(["rust"]),
            sweep_time: Utc::now(),
            enable_l1: true,
            enable_l1_5: false,
            enable_l2: false,
        },
    )
    .await
    .expect("ingest actual workspace manifests");
    assert_eq!(
        report.blocked_count, 0,
        "workspace ingest blocked records: {:?}",
        report.blocked
    );
    assert_eq!(
        report.manifest_files_refused, 0,
        "workspace ingest refused manifests"
    );
    assert!(
        report.warnings.is_empty(),
        "workspace ingest warnings: {:?}",
        report.warnings
    );
    let map = map_graph(&database, token.namespace().as_str(), &members);
    eprintln!("workspace parity population: declared={declared_count}, metadata={metadata_count}, compared={}", members.len());
    let mut differences = Vec::new();
    for scope in SCOPES {
        eprintln!(
            "workspace parity edges: scope={scope}, cargo={}, map={}",
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
        "workspace dependency parity mismatch:\n{}",
        differences.join("\n")
    );
}
