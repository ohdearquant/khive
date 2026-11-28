//! Exercise the real export/sync/import executable, using isolated local files.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const LOW: &str = "00000000-0000-0000-0000-000000000001";
const MID: &str = "00000000-0000-0000-0000-000000000002";
const HIGH: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";
const EDGE_LOW: &str = "10000000-0000-0000-0000-000000000001";
const EDGE_MID: &str = "10000000-0000-0000-0000-000000000002";
const EDGE_HIGH: &str = "10000000-0000-0000-0000-000000000003";
const CREATED: &str = "2026-01-02T03:04:05Z";
const UPDATED: &str = "2026-01-03T04:05:06Z";

fn command(tmp: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("KHIVE_VOLUME_LOCK_DIR", tmp.path().join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1");
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn entity(id: &str) -> Value {
    json!({
        "id": id, "kind": "concept", "name": format!("entity-{id}"),
        "description": "source text", "properties": {"z": {"z": 2, "a": 1}, "a": true},
        "tags": ["zeta", "alpha"], "created_at": CREATED, "updated_at": UPDATED
    })
}

fn edge(id: &str, source: &str, target: &str) -> Value {
    json!({
        "edge_id": id, "source": source, "target": target, "relation": "extends",
        "weight": 0.7, "properties": {"z": [3, 2], "a": {"z": false, "a": true}},
        "created_at": CREATED, "updated_at": UPDATED
    })
}

fn archive(entities: Vec<Value>, edges: Vec<Value>) -> Value {
    json!({
        "format": "khive-kg", "version": "0.1", "namespace": "local",
        "exported_at": CREATED, "entities": entities, "edges": edges
    })
}

fn import_archive(tmp: &TempDir, source: &Path, db: &Path) {
    let config = tmp.path().join("explicit-empty.toml");
    std::fs::write(&config, "").unwrap();
    let output = command(tmp)
        .args(["kg", "import"])
        .arg(source)
        .arg("--db")
        .arg(db)
        .arg("--config")
        .arg(config)
        .args(["--format", "archive"])
        .output()
        .unwrap();
    assert_success(&output);
}

fn seed(tmp: &TempDir, data: Value) -> PathBuf {
    let source = tmp.path().join("seed.json");
    std::fs::write(&source, data.to_string()).unwrap();
    let db = tmp.path().join("source.db");
    import_archive(tmp, &source, &db);
    db
}

fn export_project(tmp: &TempDir, db: &Path, project: &Path) -> Output {
    command(tmp)
        .args(["kg", "export", "--ndjson"])
        .arg(project)
        .arg("--db")
        .arg(db)
        .output()
        .unwrap()
}

fn export_archive(tmp: &TempDir, db: &Path, output: &Path) -> Value {
    assert_success(
        &command(tmp)
            .args(["kg", "export"])
            .arg(output)
            .arg("--db")
            .arg(db)
            .output()
            .unwrap(),
    );
    serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap()
}

fn graph(archive: &Value) -> (Vec<Value>, Vec<Value>) {
    let mut entities = archive["entities"].as_array().unwrap().clone();
    for entity in &mut entities {
        entity["tags"]
            .as_array_mut()
            .unwrap()
            .sort_by(|a, b| a.as_str().unwrap().cmp(b.as_str().unwrap()));
    }
    entities.sort_by_key(|record| record["id"].as_str().unwrap().to_ascii_lowercase());
    let mut edges = archive["edges"].as_array().unwrap().clone();
    edges.sort_by_key(|record| {
        ["source", "target", "relation"].map(|key| record[key].as_str().unwrap().to_owned())
    });
    (entities, edges)
}

#[test]
fn project_export_is_sorted_deterministic_and_real_sync_preserves_graph() {
    let tmp = TempDir::new().unwrap();
    let db = seed(
        &tmp,
        archive(
            vec![entity(HIGH), entity(MID), entity(LOW)],
            vec![
                edge(EDGE_HIGH, MID, HIGH),
                edge(EDGE_MID, LOW, HIGH),
                edge(EDGE_LOW, LOW, MID),
            ],
        ),
    );
    let before = export_archive(&tmp, &db, &tmp.path().join("before.json"));
    let project = tmp.path().join("new/project");
    assert_success(&export_project(&tmp, &db, &project));
    let entity_path = project.join(".khive/kg/entities.ndjson");
    let edge_path = project.join(".khive/kg/edges.ndjson");
    let first_entities = std::fs::read(&entity_path).unwrap();
    let first_edges = std::fs::read(&edge_path).unwrap();
    for bytes in [&first_entities, &first_edges] {
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 3);
    }
    let entities: Vec<Value> = std::str::from_utf8(&first_entities)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        entities
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [LOW, MID, HIGH]
    );
    for row in &entities {
        assert_eq!(row["tags"], json!(["alpha", "zeta"]));
        assert_ne!(row["created_at"], row["updated_at"]);
    }
    let edges: Vec<Value> = std::str::from_utf8(&first_edges)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        edges
            .iter()
            .map(|row| row["edge_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [EDGE_LOW, EDGE_MID, EDGE_HIGH]
    );
    assert_success(&export_project(&tmp, &db, &project));
    assert_eq!(std::fs::read(&entity_path).unwrap(), first_entities);
    assert_eq!(std::fs::read(&edge_path).unwrap(), first_edges);

    let rebuilt = tmp.path().join("rebuilt/fresh.db");
    assert_success(
        &command(&tmp)
            .arg("sync")
            .arg("--repo")
            .arg(&project)
            .arg("--db")
            .arg(&rebuilt)
            .output()
            .unwrap(),
    );
    let after = export_archive(&tmp, &rebuilt, &tmp.path().join("after.json"));
    assert_eq!(graph(&after), graph(&before));
    let imported = tmp.path().join("imported.db");
    import_archive(&tmp, &tmp.path().join("before.json"), &imported);
    let imported_graph = export_archive(&tmp, &imported, &tmp.path().join("imported.json"));
    assert_eq!(graph(&imported_graph), graph(&before));
}

#[test]
fn empty_project_export_writes_two_empty_files() {
    let tmp = TempDir::new().unwrap();
    let db = seed(&tmp, archive(vec![], vec![]));
    let project = tmp.path().join("project");
    assert_success(&export_project(&tmp, &db, &project));
    assert!(std::fs::read(project.join(".khive/kg/entities.ndjson"))
        .unwrap()
        .is_empty());
    assert!(std::fs::read(project.join(".khive/kg/edges.ndjson"))
        .unwrap()
        .is_empty());
}

fn assert_database_alias_refused(link: fn(&Path, &Path) -> std::io::Result<()>) {
    for leaf in ["entities.ndjson", "edges.ndjson"] {
        let tmp = TempDir::new().unwrap();
        let db = seed(&tmp, archive(vec![entity(LOW)], vec![]));
        let before = std::fs::read(&db).unwrap();
        let project = tmp.path().join("project");
        let kg = project.join(".khive/kg");
        std::fs::create_dir_all(&kg).unwrap();
        let alias = kg.join(leaf);
        link(&db, &alias).unwrap();
        let other = kg.join(if leaf == "entities.ndjson" {
            "edges.ndjson"
        } else {
            "entities.ndjson"
        });
        std::fs::write(&other, b"keep incumbent").unwrap();
        let output = export_project(&tmp, &db, &project);
        assert!(
            !output.status.success(),
            "export accepted {leaf} alias of source database"
        );
        assert_eq!(std::fs::read(&db).unwrap(), before);
        assert_eq!(std::fs::read(alias).unwrap(), before);
        assert_eq!(std::fs::read(other).unwrap(), b"keep incumbent");
    }
}

#[test]
fn project_export_refuses_either_database_hardlink_before_publication() {
    assert_database_alias_refused(|source, alias| std::fs::hard_link(source, alias));
}

#[cfg(unix)]
#[test]
fn project_export_refuses_either_database_symlink_before_publication() {
    assert_database_alias_refused(|source, alias| std::os::unix::fs::symlink(source, alias));
}

#[cfg(unix)]
#[test]
fn project_export_refuses_database_alias_after_absent_component_and_parent() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    let kg = target.join(".khive/kg");
    std::fs::create_dir_all(&kg).unwrap();
    let db = kg.join("entities.ndjson");
    let source = tmp.path().join("seed.json");
    std::fs::write(&source, archive(vec![entity(LOW)], vec![]).to_string()).unwrap();
    import_archive(&tmp, &source, &db);
    let before = std::fs::read(&db).unwrap();

    let edges = kg.join("edges.ndjson");
    std::fs::write(&edges, b"keep incumbent edges").unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let absent = tmp.path().join("new");
    assert!(!absent.exists(), "the path component must start absent");
    let project = absent.join("..").join("alias");

    let output = export_project(&tmp, &db, &project);
    assert!(
        !output.status.success(),
        "export accepted a database alias through an absent component and ..; stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "alias refusal must preserve the source database"
    );
    assert_eq!(
        std::fs::read(&edges).unwrap(),
        b"keep incumbent edges",
        "alias refusal must preserve the peer export leaf"
    );
    assert!(
        !absent.exists(),
        "alias refusal must precede destination directory creation"
    );
}

#[test]
fn project_export_refuses_database_hardlink_after_absent_component_and_parent() {
    let tmp = TempDir::new().unwrap();
    let db = seed(&tmp, archive(vec![entity(LOW)], vec![]));
    let before = std::fs::read(&db).unwrap();
    let project = tmp.path().join("project");
    let kg = project.join(".khive/kg");
    std::fs::create_dir_all(&kg).unwrap();
    let alias = kg.join("entities.ndjson");
    std::fs::hard_link(&db, &alias).unwrap();
    let edges = kg.join("edges.ndjson");
    std::fs::write(&edges, b"keep incumbent edges").unwrap();
    let absent = tmp.path().join("new");
    assert!(!absent.exists(), "the path component must start absent");
    let output_path = absent.join("..").join("project");

    let output = export_project(&tmp, &db, &output_path);
    assert!(
        !output.status.success(),
        "export accepted a database hardlink through an absent component and ..; stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "hardlink refusal must preserve the source database"
    );
    assert_eq!(
        std::fs::read(&alias).unwrap(),
        before,
        "hardlink refusal must preserve the aliased export leaf"
    );
    assert_eq!(
        std::fs::read(&edges).unwrap(),
        b"keep incumbent edges",
        "hardlink refusal must preserve the peer export leaf"
    );
    assert!(
        !absent.exists(),
        "hardlink refusal must precede destination directory creation"
    );
}
