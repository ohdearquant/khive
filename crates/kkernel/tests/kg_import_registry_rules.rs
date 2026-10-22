//! Archive import preserves registry ownership and loaded pack edge rules.

#![cfg(any(feature = "pack-code", feature = "pack-tool"))]

#[cfg(feature = "pack-tool")]
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;
use uuid::Uuid;

const CREATED: &str = "2025-01-02T03:04:05.123456Z";
#[cfg(feature = "pack-code")]
const CREATED_MICROS: i64 = 1_735_787_045_123_456;
const EXPORTED: &str = "2026-01-02T03:04:05.654321Z";
#[cfg(feature = "pack-tool")]
const TOOL_NAME: &str = "ImportOwnershipFixture";

fn uuid(number: u128) -> Uuid {
    Uuid::from_u128(number)
}

fn entity(id: u128, name: &str) -> Value {
    json!({
        "id": uuid(id), "kind": "concept", "entity_type": "algorithm", "name": name,
        "description": "import endpoint", "properties": {"fixture": "pack-rule"},
        "tags": ["import-fixture"], "created_at": CREATED, "updated_at": CREATED
    })
}

#[cfg(feature = "pack-code")]
fn code_entity(id: u128, name: &str) -> Value {
    let mut value = entity(id, name);
    value["entity_type"] = json!("function");
    value["description"] = json!("code endpoint");
    value
}

fn archive(entities: Vec<Value>, edges: Vec<Value>) -> Value {
    json!({
        "format": "khive-kg", "version": "0.1", "namespace": "local",
        "exported_at": EXPORTED, "entities": entities, "edges": edges
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_slice(&output.stdout).expect("JSON import report")
}

fn assert_insert_counts(report: &Value, entities: u64, edges: u64) {
    for (field, expected) in [
        ("entities_imported", entities),
        ("entities_inserted", entities),
        ("entities_updated", 0),
        ("entities_skipped", 0),
        ("edges_imported", edges),
        ("edges_inserted", edges),
        ("edges_updated", 0),
        ("edges_skipped", 0),
    ] {
        assert_eq!(report[field], json!(expected), "{field}: {report}");
    }
}

#[cfg(feature = "pack-tool")]
#[derive(Debug, PartialEq)]
struct RowImage {
    table: &'static str,
    rows: Vec<Vec<SqlValue>>,
}

struct Fixture {
    directory: TempDir,
    db: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(packs: &[&str]) -> Self {
        let directory = TempDir::new().unwrap();
        let config = directory.path().join("strict.toml");
        std::fs::write(
            &config,
            format!("[runtime]\npacks = {packs:?}\n[schema]\nstrict = true\n"),
        )
        .unwrap();
        let db = directory.path().join("target/import.db");
        Self {
            directory,
            db,
            config,
        }
    }

    fn command(&self) -> Command {
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
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env(
                "KHIVE_VOLUME_LOCK_DIR",
                self.directory.path().join("volume-locks"),
            )
            .env("KHIVE_NO_DAEMON", "1")
            .args(["--log", "off"]);
        command
    }

    fn import(&self, input: &Value, flags: &[&str]) -> Output {
        let source = self.directory.path().join("input.json");
        std::fs::write(&source, serde_json::to_vec(input).unwrap()).unwrap();
        self.command()
            .args(["kg", "import"])
            .arg(source)
            .args(["--format", "archive", "--db"])
            .arg(&self.db)
            .arg("--config")
            .arg(&self.config)
            .args(flags)
            .output()
            .unwrap()
    }

    #[cfg(feature = "pack-tool")]
    fn register_tool(&self) -> String {
        std::fs::create_dir_all(self.db.parent().unwrap()).unwrap();
        let seed_config = self.directory.path().join("seed.toml");
        std::fs::write(
            &seed_config,
            format!(
                "engines = []\n{}",
                std::fs::read_to_string(&self.config).unwrap()
            ),
        )
        .unwrap();
        let operations = json!([{
            "tool": "tool.register",
            "args": {
                "name": TOOL_NAME, "kind": "tool", "description": "registered owner",
                "source": "fixture:registered", "side_effect": "write",
                "trust": "external", "tags": ["registration-fixture"]
            }
        }]);
        let output = self
            .command()
            .args(["exec", "--strict", "--output-format", "json"])
            .arg(operations.to_string())
            .arg("--db")
            .arg(&self.db)
            .arg("--config")
            .arg(&seed_config)
            .args(["--namespace", "local"])
            .output()
            .unwrap();
        assert_success(&output);
        self.connection()
            .query_row(
                "SELECT id FROM entities WHERE name = ?1",
                [TOOL_NAME],
                |row| row.get(0),
            )
            .expect("tool.register must persist its registry entity")
    }

    fn connection(&self) -> Connection {
        Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open existing destination read-only after the child exits")
    }

    fn entity(&self, id: &str) -> Value {
        self.connection()
            .query_row(
                "SELECT id, namespace, kind, entity_type, name, description, properties, tags,
                        created_at, updated_at, deleted_at FROM entities WHERE id = ?1",
                [id],
                |row| {
                    let properties: Option<String> = row.get(6)?;
                    let tags: String = row.get(7)?;
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "namespace": row.get::<_, String>(1)?,
                        "kind": row.get::<_, String>(2)?,
                        "entity_type": row.get::<_, Option<String>>(3)?,
                        "name": row.get::<_, String>(4)?,
                        "description": row.get::<_, Option<String>>(5)?,
                        "properties": properties.map(|text| serde_json::from_str::<Value>(&text).unwrap()),
                        "tags": serde_json::from_str::<Value>(&tags).unwrap(),
                        "created_at": row.get::<_, i64>(8)?,
                        "updated_at": row.get::<_, i64>(9)?,
                        "deleted_at": row.get::<_, Option<i64>>(10)?
                    }))
                },
            )
            .unwrap()
    }

    #[cfg(feature = "pack-tool")]
    fn rows(&self) -> Vec<RowImage> {
        let connection = self.connection();
        ["entities", "graph_edges", "events"]
            .into_iter()
            .map(|table| {
                let mut statement = connection
                    .prepare(&format!("SELECT * FROM {table}"))
                    .unwrap();
                let columns = statement.column_count();
                let mut rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get(column))
                            .collect::<rusqlite::Result<Vec<SqlValue>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                rows.sort_by_cached_key(|row| format!("{row:?}"));
                RowImage { table, rows }
            })
            .collect()
    }
}

#[cfg(feature = "pack-tool")]
#[test]
fn import_update_refuses_registered_tools_before_any_row_write() {
    for tags in [json!([]), json!(["ordinary-import-tag"])] {
        let fixture = Fixture::new(&["kg", "tool"]);
        let id = fixture.register_tool();
        let registered = fixture.entity(&id);
        assert_eq!(registered["kind"], "project");
        assert_eq!(registered["entity_type"], "tool");
        assert_eq!(registered["properties"]["source"], "fixture:registered");
        assert_eq!(registered["properties"]["side_effect"], "write");
        assert!(registered["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("tool-registry")));
        let before = fixture.rows();
        let input = archive(
            vec![
                entity(5_767_001, "FreshBeforeProtectedUpdate"),
                json!({
                    "id": id, "kind": "project", "entity_type": "tool",
                    "name": TOOL_NAME, "description": "must not replace registry metadata",
                    "properties": {"source": "fixture:replacement", "side_effect": "read"},
                    "tags": tags, "created_at": CREATED, "updated_at": EXPORTED
                }),
            ],
            vec![],
        );
        for flags in [
            &["--on-conflict", "update"][..],
            &["--on-conflict", "update", "--dry-run"][..],
        ] {
            let output = fixture.import(&input, flags);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "registry update must refuse: tags={tags}, flags={flags:?}, stdout={}, stderr={stderr}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(
                stderr.contains("update refuses registry tag \"tool-registry\"")
                    && stderr.contains("registry rows are written only by the owning pack"),
                "must reach the stored-row registry guard: tags={tags}, flags={flags:?}, stderr={stderr}"
            );
            assert_eq!(
                fixture.entity(&id),
                registered,
                "registry fields must remain unchanged: tags={tags}, flags={flags:?}"
            );
            assert_eq!(
                fixture.rows(),
                before,
                "no entity, edge, event, or version change: tags={tags}, flags={flags:?}"
            );
            let fresh_rows: i64 = fixture
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM entities WHERE id = ?1",
                    [uuid(5_767_001).to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(fresh_rows, 0, "the earlier fresh record must not commit");
        }
    }
}

#[cfg(feature = "pack-code")]
#[test]
fn import_accepts_code_pack_function_dependency_without_flags() {
    for existing_destination in [false, true] {
        let fixture = Fixture::new(&["kg", "code"]);
        let resident = if existing_destination {
            let seeded = report(&fixture.import(
                &archive(vec![code_entity(5_767_010, "Resident")], vec![]),
                &[],
            ));
            assert_insert_counts(&seeded, 1, 0);
            Some(fixture.entity(&uuid(5_767_010).to_string()))
        } else {
            assert!(!fixture.db.exists());
            None
        };
        let input = archive(
            vec![
                code_entity(5_767_011, "Caller"),
                code_entity(5_767_012, "Callee"),
            ],
            vec![json!({
                "edge_id": uuid(5_767_013), "source": uuid(5_767_011),
                "target": uuid(5_767_012), "relation": "depends_on", "weight": 0.5,
                "properties": {"fixture": "code-dependency"},
                "created_at": CREATED, "updated_at": CREATED
            })],
        );
        let imported = report(&fixture.import(&input, &[]));
        assert_insert_counts(&imported, 2, 1);
        for (id, name) in [(5_767_011, "Caller"), (5_767_012, "Callee")] {
            assert_eq!(
                fixture.entity(&uuid(id).to_string()),
                json!({
                    "id": uuid(id), "namespace": "local", "kind": "concept",
                    "entity_type": "function", "name": name, "description": "code endpoint",
                    "properties": {"fixture": "pack-rule"}, "tags": ["import-fixture"],
                    "created_at": CREATED_MICROS, "updated_at": CREATED_MICROS,
                    "deleted_at": null
                }),
                "existing_destination={existing_destination}"
            );
        }
        let connection = fixture.connection();
        let edge = connection
            .query_row(
                "SELECT id, namespace, source_id, target_id, relation, weight, metadata,
                        created_at, updated_at, deleted_at, target_backend
                 FROM graph_edges WHERE id = ?1",
                [uuid(5_767_013).to_string()],
                |row| {
                    let metadata: Option<String> = row.get(6)?;
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "namespace": row.get::<_, String>(1)?,
                        "source": row.get::<_, String>(2)?,
                        "target": row.get::<_, String>(3)?,
                        "relation": row.get::<_, String>(4)?,
                        "weight": row.get::<_, f64>(5)?,
                        "properties": metadata.map(|text| serde_json::from_str::<Value>(&text).unwrap()),
                        "created_at": row.get::<_, i64>(7)?,
                        "updated_at": row.get::<_, i64>(8)?,
                        "deleted_at": row.get::<_, Option<i64>>(9)?,
                        "target_backend": row.get::<_, Option<String>>(10)?
                    }))
                },
            )
            .unwrap();
        assert_eq!(
            edge,
            json!({
                "id": uuid(5_767_013), "namespace": "local", "source": uuid(5_767_011),
                "target": uuid(5_767_012), "relation": "depends_on", "weight": 0.5,
                "properties": {"fixture": "code-dependency"},
                "created_at": CREATED_MICROS, "updated_at": CREATED_MICROS,
                "deleted_at": null, "target_backend": null
            }),
            "existing_destination={existing_destination}"
        );
        let entity_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
            .unwrap();
        let edge_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(entity_count, if existing_destination { 3 } else { 2 });
        assert_eq!(edge_count, 1);
        if let Some(resident) = resident {
            assert_eq!(fixture.entity(&uuid(5_767_010).to_string()), resident);
        }
    }
}

impl Fixture {
    #[cfg(feature = "pack-tool")]
    fn export(&self) -> Value {
        let target = self.directory.path().join("export.json");
        let output = self
            .command()
            .args(["kg", "export", "--db"])
            .arg(&self.db)
            .arg(&target)
            .output()
            .unwrap();
        assert_success(&output);
        serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap()
    }
}

#[cfg(feature = "pack-tool")]
#[test]
fn exported_registry_rows_and_incident_edges_are_skipped_in_every_import_mode() {
    let source = Fixture::new(&["kg", "tool"]);
    let tool_id = source.register_tool();
    let ordinary = archive(
        vec![
            entity(5_767_201, "FirstOrdinary"),
            entity(5_767_202, "SecondOrdinary"),
        ],
        vec![],
    );
    assert_insert_counts(&report(&source.import(&ordinary, &[])), 2, 0);
    let mut input = source.export();
    let registered = input["entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["id"] == tool_id)
        .collect::<Vec<_>>();
    assert_eq!(registered.len(), 1, "export must retain the registry row");
    assert!(registered[0]["tags"]
        .as_array()
        .unwrap()
        .contains(&json!("tool-registry")));
    assert!(
        input["edges"].as_array().unwrap().is_empty(),
        "registration without capabilities creates no edges"
    );
    input["edges"] = json!([
        {"edge_id": uuid(5_767_203), "source": tool_id, "target": uuid(5_767_201),
         "relation": "supports", "weight": 1.0, "created_at": CREATED},
        {"edge_id": uuid(5_767_204), "source": uuid(5_767_202), "target": tool_id,
         "relation": "supports", "weight": 1.0, "created_at": CREATED},
        {"edge_id": uuid(5_767_205), "source": uuid(5_767_201), "target": uuid(5_767_202),
         "relation": "supports", "weight": 1.0, "created_at": CREATED}
    ]);
    for flags in [
        &[][..],
        &["--on-conflict", "skip"][..],
        &["--on-conflict", "update"][..],
    ] {
        let destination = Fixture::new(&["kg", "tool"]);
        let mut preview_flags = flags.to_vec();
        preview_flags.push("--dry-run");
        let preview = report(&destination.import(&input, &preview_flags));
        assert!(
            !destination.db.exists(),
            "preview must not create the destination"
        );
        let actual = report(&destination.import(&input, flags));
        assert_eq!(actual, preview, "preview mismatch with {flags:?}");
        for (field, expected) in [
            ("entities_imported", 2),
            ("entities_inserted", 2),
            ("entities_updated", 0),
            ("entities_skipped", 1),
            ("entities_registry_skipped", 1),
            ("edges_imported", 1),
            ("edges_inserted", 1),
            ("edges_updated", 0),
            ("edges_skipped", 2),
            ("edges_registry_skipped", 2),
        ] {
            assert_eq!(actual[field], expected, "{field} with {flags:?}: {actual}");
        }
        let skipped = actual["skipped_edges"].as_array().unwrap();
        assert_eq!(skipped.len(), 2);
        assert_eq!(skipped[0]["edge_id"], json!(uuid(5_767_203)));
        assert_eq!(skipped[1]["edge_id"], json!(uuid(5_767_204)));
        assert!(skipped.iter().all(|row| row["cause"] == "skipped_record"));
        let connection = destination.connection();
        let registry_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE id = ?1",
                [&tool_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registry_count, 0);
        let tags = connection
            .prepare("SELECT tags FROM entities")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(tags.len(), 2);
        assert!(tags
            .iter()
            .all(|tags| !serde_json::from_str::<Vec<String>>(tags)
                .unwrap()
                .iter()
                .any(|tag| tag == "tool-registry")));
        let edge_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM graph_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edge_count, 1);
        let retained: String = connection
            .query_row("SELECT id FROM graph_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, uuid(5_767_205).to_string());
    }
}
