//! Executable witnesses for archive conflict modes and the narrow force policy.
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;
use uuid::Uuid;

const CREATED: &str = "2025-01-02T03:04:05.123456Z";
const LATER: &str = "2026-01-02T03:04:05.654321Z";

fn uuid(number: u128) -> Uuid {
    Uuid::from_u128(number)
}

fn entity(id: u128, name: &str) -> Value {
    json!({
        "id":uuid(id),"kind":"concept","entity_type":"algorithm","name":name,
        "description":"stored description","properties":{"keep":1,"replace":0,"nested":{"stored":true}},
        "tags":["stored","tag"],"created_at":CREATED,"updated_at":CREATED
    })
}

fn edge(id: u128, source: u128, target: u128, relation: &str) -> Value {
    json!({
        "edge_id":uuid(id),"source":uuid(source),"target":uuid(target),"relation":relation,
        "weight":0.2,"properties":{"stored":true},"created_at":CREATED,"updated_at":CREATED
    })
}

fn archive(entities: Vec<Value>, edges: Vec<Value>) -> Value {
    json!({
        "format":"khive-kg","version":"0.1","namespace":"local",
        "exported_at":LATER,"entities":entities,"edges":edges
    })
}

fn skipped(id: u128, source: u128, target: u128, relation: &str, cause: &str) -> Value {
    json!({"edge_id":uuid(id),"source":uuid(source),"target":uuid(target),"relation":relation,"cause":cause})
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_refused(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn report(output: &Output) -> Value {
    assert_success(output);
    serde_json::from_slice(&output.stdout).expect("JSON import report")
}

fn assert_counts(report: &Value, entities: [u64; 3], edges: [u64; 3]) {
    for (field, expected) in [
        ("entities_imported", entities[0] + entities[1]),
        ("entities_inserted", entities[0]),
        ("entities_updated", entities[1]),
        ("entities_skipped", entities[2]),
        ("edges_imported", edges[0] + edges[1]),
        ("edges_inserted", edges[0]),
        ("edges_updated", edges[1]),
        ("edges_skipped", edges[2]),
    ] {
        assert_eq!(report[field], json!(expected), "{field}: {report}");
    }
}

fn entity_at(data: &Value, id: u128) -> &Value {
    data["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == json!(uuid(id)))
        .expect("entity UUID must be retained")
}

fn edge_at(data: &Value, id: u128) -> &Value {
    data["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["edge_id"] == json!(uuid(id)))
        .expect("edge UUID must be retained")
}

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
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let config = directory.path().join("strict.toml");
        std::fs::write(&config, "[schema]\nstrict=true\n").unwrap();
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
        self.import_as(input, "archive", flags)
    }

    fn import_as(&self, input: &Value, format: &str, flags: &[&str]) -> Output {
        let source = self.directory.path().join("input.json");
        std::fs::write(&source, serde_json::to_vec(input).unwrap()).unwrap();
        self.command()
            .args(["kg", "import"])
            .arg(source)
            .args(["--format", format, "--db"])
            .arg(&self.db)
            .arg("--config")
            .arg(&self.config)
            .args(flags)
            .output()
            .unwrap()
    }

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

    fn seed(&self, input: &Value) {
        report(&self.import(input, &[]));
    }

    fn rows(&self) -> Vec<RowImage> {
        // Every child has exited before observation; this connection closes before the next child.
        let connection = Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open existing destination read-only");
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

    fn assert_no_destination(&self) {
        assert!(!self.db.exists(), "dry-run/refusal created a destination");
        for suffix in ["-wal", "-shm", ".sync.lock"] {
            assert!(!PathBuf::from(format!("{}{suffix}", self.db.display())).exists());
        }
        assert!(
            !self.db.parent().unwrap().exists(),
            "target parent was created"
        );
    }
}

#[test]
fn default_and_explicit_error_refuse_entity_collisions_before_any_row_write() {
    for flags in [
        &[][..],
        &["--on-conflict", "error"][..],
        &["--dry-run"][..],
        &["--on-conflict", "error", "--dry-run"][..],
    ] {
        let fixture = Fixture::new();
        fixture.seed(&archive(vec![entity(1, "StoredName")], vec![]));
        let before = fixture.rows();
        let input = archive(
            vec![entity(3, "FreshFirst"), entity(1, "MustNotReplace")],
            vec![],
        );
        let output = fixture.import(&input, flags);
        assert_refused(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("archive entity UUID {} already exists", uuid(1))),
            "collision must be refused during preflight: {stderr}"
        );
        assert_eq!(
            fixture.rows(),
            before,
            "a later collision must not commit the fresh first row"
        );
        let exported = fixture.export();
        assert_eq!(exported["entities"].as_array().unwrap().len(), 1);
        assert_eq!(entity_at(&exported, 1)["name"], "StoredName");
    }
    let fixture = Fixture::new();
    assert_refused(&fixture.import(
        &archive(vec![entity(3, "ArchiveContinueMustRefuse")], vec![]),
        &["--continue"],
    ));
    fixture.assert_no_destination();
}

#[test]
fn skip_keeps_existing_records_and_omits_only_their_incident_edges() {
    let fixture = Fixture::new();
    fixture.seed(&archive(
        vec![entity(1, "StoredName"), entity(2, "ExistingEndpoint")],
        vec![edge(100, 1, 2, "supports")],
    ));
    let before = fixture.export();
    let input = archive(
        vec![entity(1, "MustNotReplace"), entity(3, "FreshRecord")],
        vec![
            edge(101, 1, 3, "supports"),
            edge(102, 2, 1, "extends"),
            edge(103, 3, 2, "supports"),
        ],
    );
    let summary = report(&fixture.import(&input, &["--on-conflict", "skip"]));
    assert_counts(&summary, [1, 0, 1], [1, 0, 2]);
    assert_eq!(
        summary["skipped_edges"],
        json!([
            skipped(101, 1, 3, "supports", "skipped_record"),
            skipped(102, 2, 1, "extends", "skipped_record")
        ])
    );
    let after = fixture.export();
    assert_eq!(entity_at(&after, 1), entity_at(&before, 1));
    assert_eq!(entity_at(&after, 2), entity_at(&before, 2));
    assert_eq!(entity_at(&after, 3)["name"], "FreshRecord");
    assert_eq!(after["entities"].as_array().unwrap().len(), 3);
    assert_eq!(after["edges"].as_array().unwrap().len(), 2);
    assert_eq!(edge_at(&after, 100), edge_at(&before, 100));
    assert_eq!(edge_at(&after, 103)["source"], json!(uuid(3)));

    let adapter = json!([
        entity(1, "AdapterMustNotReplace"),
        entity(4, "AdapterFresh")
    ]);
    let summary = report(&fixture.import_as(&adapter, "json", &["--continue"]));
    assert_counts(&summary, [1, 0, 1], [0, 0, 0]);
    let after_adapter = fixture.export();
    assert_eq!(entity_at(&after_adapter, 1), entity_at(&before, 1));
    assert_eq!(entity_at(&after_adapter, 4)["name"], "AdapterFresh");
}

#[test]
fn update_projects_optional_fields_properties_tags_and_immutable_identity() {
    let fixture = Fixture::new();
    fixture.seed(&archive(vec![entity(1, "StoredName")], vec![]));
    let original = entity_at(&fixture.export(), 1).clone();
    let mut incoming = entity(1, "ImportedName");
    incoming.as_object_mut().unwrap().remove("description");
    incoming.as_object_mut().unwrap().remove("entity_type");
    incoming["created_at"] = json!(LATER);
    incoming["properties"] = json!({"replace":2,"added":true,"nested":{"archive":true}});
    incoming["tags"] = json!(["replacement"]);
    let summary = report(&fixture.import(
        &archive(vec![incoming.clone()], vec![]),
        &["--on-conflict", "update"],
    ));
    assert_counts(&summary, [0, 1, 0], [0, 0, 0]);
    let changed = entity_at(&fixture.export(), 1).clone();
    assert_eq!(changed["id"], original["id"]);
    assert_eq!(changed["kind"], original["kind"]);
    assert_eq!(changed["created_at"], original["created_at"]);
    assert_eq!(changed["description"], original["description"]);
    assert_eq!(changed["entity_type"], original["entity_type"]);
    assert_eq!(changed["name"], "ImportedName");
    assert_eq!(
        changed["properties"],
        json!({"keep":1,"replace":2,"added":true,"nested":{"archive":true}})
    );
    assert_eq!(changed["tags"], json!(["replacement"]));

    incoming["description"] = Value::Null;
    incoming["entity_type"] = Value::Null;
    incoming["properties"] = Value::Null;
    incoming["tags"] = json!([]);
    let summary = report(&fixture.import(
        &archive(vec![incoming], vec![]),
        &["--on-conflict", "update"],
    ));
    assert_counts(&summary, [0, 1, 0], [0, 0, 0]);
    let unchanged_fields = entity_at(&fixture.export(), 1).clone();
    for field in [
        "id",
        "kind",
        "created_at",
        "name",
        "description",
        "entity_type",
        "properties",
        "tags",
    ] {
        assert_eq!(
            unchanged_fields[field], changed[field],
            "null/empty must preserve {field}"
        );
    }

    let before = fixture.rows();
    let mut wrong_kind = entity(1, "KindMustNotChange");
    wrong_kind["kind"] = json!("document");
    assert_refused(&fixture.import(
        &archive(vec![entity(3, "FreshFirst"), wrong_kind], vec![]),
        &["--on-conflict", "update"],
    ));
    assert_eq!(fixture.rows(), before);
}

#[test]
fn edge_uuid_modes_refuse_skip_or_update_in_place_with_creation_time_preserved() {
    for mode in ["error", "skip", "update"] {
        let fixture = Fixture::new();
        fixture.seed(&archive(
            vec![entity(1, "A"), entity(2, "B")],
            vec![edge(100, 1, 2, "supports")],
        ));
        let original = edge_at(&fixture.export(), 100).clone();
        let before = fixture.rows();
        let mut incoming = edge(100, 1, 2, "supports");
        incoming["created_at"] = json!(LATER);
        incoming["weight"] = json!(0.8);
        incoming["properties"] = json!({"replacement":true});
        let input = archive(vec![entity(3, "FreshFirst")], vec![incoming]);
        let output = fixture.import(&input, &["--on-conflict", mode]);
        if mode == "error" {
            assert_refused(&output);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("archive edge UUID {} already exists", uuid(100))),
                "collision must be refused during preflight: {stderr}"
            );
            assert_eq!(fixture.rows(), before);
            continue;
        }
        let summary = report(&output);
        let after = fixture.export();
        assert_eq!(after["entities"].as_array().unwrap().len(), 3);
        assert_eq!(after["edges"].as_array().unwrap().len(), 1);
        let stored = edge_at(&after, 100);
        if mode == "skip" {
            assert_counts(&summary, [1, 0, 0], [0, 0, 1]);
            assert_eq!(stored, &original);
            assert_eq!(
                summary["skipped_edges"],
                json!([skipped(100, 1, 2, "supports", "existing_edge")])
            );
        } else {
            assert_counts(&summary, [1, 0, 0], [0, 1, 0]);
            for field in ["edge_id", "source", "target", "relation", "created_at"] {
                assert_eq!(stored[field], original[field], "stable edge {field}");
            }
            assert_eq!(stored["weight"], json!(0.8));
            assert_eq!(
                stored["properties"],
                json!({"replacement":true}),
                "edge properties replace rather than merge"
            );
        }
    }
}

#[test]
fn fresh_edge_uuid_with_a_stored_triple_refuses_every_mode_before_writes() {
    for mode in ["error", "skip", "update"] {
        let fixture = Fixture::new();
        fixture.seed(&archive(
            vec![entity(1, "A"), entity(2, "B")],
            vec![edge(100, 1, 2, "supports")],
        ));
        let before = fixture.rows();
        let input = archive(
            vec![entity(3, "FreshFirst")],
            vec![edge(101, 1, 2, "supports")],
        );
        assert_refused(&fixture.import(&input, &["--on-conflict", mode]));
        assert_eq!(
            fixture.rows(),
            before,
            "{mode} must not absorb a new edge UUID by triple"
        );
    }
}

#[test]
fn force_skips_only_missing_or_contract_invalid_edges_and_reports_each_cause() {
    let fixture = Fixture::new();
    let input = archive(
        vec![entity(1, "A"), entity(2, "B")],
        vec![
            edge(100, 1, 2, "supports"),
            edge(101, 9, 1, "supports"),
            edge(102, 1, 9, "supports"),
            edge(103, 1, 2, "annotates"),
        ],
    );
    assert_refused(&fixture.import(&input, &[]));
    fixture.assert_no_destination();
    let summary = report(&fixture.import(&input, &["--force"]));
    assert_counts(&summary, [2, 0, 0], [1, 0, 3]);
    let mut contract = skipped(103, 1, 2, "annotates", "endpoint_contract");
    contract["error"] = json!(format!(
        "invalid input: annotates source {} must be a note",
        uuid(1)
    ));
    assert_eq!(
        summary["skipped_edges"],
        json!([
            skipped(101, 9, 1, "supports", "missing_source"),
            skipped(102, 1, 9, "supports", "missing_target"),
            contract
        ])
    );
    let exported = fixture.export();
    assert_eq!(exported["entities"].as_array().unwrap().len(), 2);
    assert_eq!(exported["edges"], json!([edge(100, 1, 2, "supports")]));

    let adapter = json!([edge(104, 2, 1, "extends")]);
    let summary = report(&fixture.import_as(&adapter, "json", &[]));
    assert_counts(&summary, [0, 0, 0], [1, 0, 0]);
    assert_eq!(
        edge_at(&fixture.export(), 104),
        &edge(104, 2, 1, "extends"),
        "adapter endpoints may resolve from the existing destination"
    );
    let unresolved = json!([edge(105, 1, 9, "supports")]);
    let before = fixture.rows();
    assert_refused(&fixture.import_as(&unresolved, "json", &[]));
    assert_eq!(fixture.rows(), before);
    let summary = report(&fixture.import_as(&unresolved, "json", &["--force"]));
    assert_counts(&summary, [0, 0, 0], [0, 0, 1]);
    assert_eq!(
        summary["skipped_edges"],
        json!([skipped(105, 1, 9, "supports", "missing_target")])
    );
    assert_eq!(fixture.export()["edges"].as_array().unwrap().len(), 2);
}

#[test]
fn force_does_not_relax_validation_collisions_or_existing_edge_endpoints() {
    for case in [
        "relation",
        "weight",
        "entity_reserved",
        "edge_reserved",
        "strict_kind",
        "kind_mismatch",
        "collision",
        "endpoints",
        "triple",
    ] {
        let fixture = Fixture::new();
        fixture.seed(&archive(
            vec![entity(1, "A"), entity(2, "B")],
            vec![edge(100, 1, 2, "supports")],
        ));
        let before = fixture.rows();
        let mut records = vec![entity(3, "FreshFirst")];
        let mut edges = vec![edge(101, 3, 1, "supports")];
        let mode = match case {
            "relation" => {
                edges[0]["relation"] = json!("unknown_relation");
                "error"
            }
            "weight" => {
                edges[0]["weight"] = json!(1.5);
                "error"
            }
            "entity_reserved" => {
                records[0]["properties"] = json!({"khive:secret_gate":"forged"});
                "error"
            }
            "edge_reserved" => {
                edges[0]["properties"] = json!({"khive:secret_gate":"forged"});
                "error"
            }
            "strict_kind" => {
                records[0]["kind"] = json!("unregistered_import_kind");
                "error"
            }
            "kind_mismatch" => {
                let mut wrong_kind = entity(1, "MustNotChangeKind");
                wrong_kind["kind"] = json!("document");
                records.push(wrong_kind);
                "update"
            }
            "collision" => {
                records.push(entity(1, "MustNotReplace"));
                "error"
            }
            "endpoints" => {
                edges = vec![edge(100, 3, 2, "supports")];
                "update"
            }
            "triple" => {
                edges = vec![edge(101, 1, 2, "supports")];
                "skip"
            }
            _ => unreachable!(),
        };
        assert_refused(&fixture.import(
            &archive(records, edges),
            &["--force", "--on-conflict", mode],
        ));
        assert_eq!(fixture.rows(), before, "--force changed rows for {case}");
    }
}

/// Like code_ingest_cli's live-WAL dry-run fixture, the test process keeps a
/// writable connection open while a separate kkernel process plans the import.
#[test]
fn dry_run_reads_a_live_wal_destination_without_changing_durable_state() {
    let fixture = Fixture::new();
    fixture.seed(&archive(
        vec![entity(1, "StoredA"), entity(2, "StoredB")],
        vec![edge(100, 1, 2, "supports")],
    ));
    let holder = Connection::open_with_flags(&fixture.db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .expect("hold destination open in the test process");
    holder
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; \
             CREATE TABLE import_live_wal_probe(value INTEGER); \
             INSERT INTO import_live_wal_probe VALUES (1);",
        )
        .expect("leave committed WAL frames and a live shared-memory sidecar");
    let wal = PathBuf::from(format!("{}-wal", fixture.db.display()));
    let shm = PathBuf::from(format!("{}-shm", fixture.db.display()));
    assert!(wal.exists(), "fixture must have a live WAL");
    assert!(
        shm.exists(),
        "fixture must have a live writable SHM sidecar"
    );
    let entries = || {
        let mut names = std::fs::read_dir(fixture.db.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let before = fixture.rows();
    let db_before = std::fs::read(&fixture.db).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    assert!(!wal_before.is_empty(), "fixture needs committed WAL data");
    let entries_before = entries();
    let assert_unchanged = |mode: &str| {
        assert_eq!(fixture.rows(), before, "live dry-run wrote rows: {mode}");
        assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
        assert_eq!(entries(), entries_before, "dry-run created target files");
        assert!(shm.exists(), "dry-run must retain the live WAL sidecar");
    };
    let mut updated = entity(1, "UpdatedA");
    updated["properties"] = json!({"new":true});
    let input = archive(
        vec![entity(3, "FreshC"), updated],
        vec![edge(101, 2, 3, "supports"), edge(102, 1, 3, "supports")],
    );

    let refused = fixture.import(&input, &["--dry-run"]);
    assert_refused(&refused);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("archive entity UUID {} already exists", uuid(1))),
        "live destination must reach collision planning: {stderr}"
    );
    assert_unchanged("error");
    for (mode, entities, edges) in [
        ("skip", [1, 0, 1], [1, 0, 1]),
        ("update", [1, 1, 0], [2, 0, 0]),
    ] {
        let dry = report(&fixture.import(&input, &["--dry-run", "--on-conflict", mode]));
        assert_counts(&dry, entities, edges);
        if mode == "skip" {
            assert_eq!(
                dry["skipped_edges"],
                json!([skipped(102, 1, 3, "supports", "skipped_record")])
            );
        } else {
            assert!(dry.get("skipped_edges").is_none());
        }
        assert_unchanged(mode);
    }
    let held_value: i64 = holder
        .query_row("SELECT value FROM import_live_wal_probe", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(held_value, 1);
    drop(holder);
}

#[test]
fn dry_run_matches_real_outcomes_preserves_all_row_images_and_creates_no_missing_db() {
    let fixture = Fixture::new();
    fixture.seed(&archive(
        vec![entity(1, "StoredA"), entity(2, "StoredB")],
        vec![edge(100, 1, 2, "supports")],
    ));
    let skip_input = archive(
        vec![entity(1, "MustNotReplace"), entity(3, "FreshC")],
        vec![edge(101, 1, 3, "supports"), edge(102, 9, 3, "supports")],
    );
    let before = fixture.rows();
    let dry = report(&fixture.import(
        &skip_input,
        &["--on-conflict", "skip", "--force", "--dry-run"],
    ));
    assert_counts(&dry, [1, 0, 1], [0, 0, 2]);
    assert_eq!(
        dry["skipped_edges"],
        json!([
            skipped(101, 1, 3, "supports", "skipped_record"),
            skipped(102, 9, 3, "supports", "missing_source")
        ])
    );
    assert_eq!(fixture.rows(), before);
    let real = report(&fixture.import(&skip_input, &["--on-conflict", "skip", "--force"]));
    assert_counts(&real, [1, 0, 1], [0, 0, 2]);
    assert_eq!(dry["skipped_edges"], real["skipped_edges"]);
    let mut updated = entity(1, "UpdatedA");
    updated["properties"] = json!({"new":true});
    let mut updated_edge = edge(100, 1, 2, "supports");
    updated_edge["weight"] = json!(0.9);
    let input = archive(
        vec![updated, entity(4, "FreshD")],
        vec![
            updated_edge,
            edge(101, 1, 4, "supports"),
            edge(102, 9, 4, "supports"),
        ],
    );
    let before = fixture.rows();
    let dry = report(&fixture.import(&input, &["--on-conflict", "update", "--force", "--dry-run"]));
    assert_counts(&dry, [1, 1, 0], [1, 1, 1]);
    assert_eq!(
        dry["skipped_edges"],
        json!([skipped(102, 9, 4, "supports", "missing_source")])
    );
    assert_eq!(
        fixture.rows(),
        before,
        "dry-run must preserve every entity/edge/event column"
    );
    let real = report(&fixture.import(&input, &["--on-conflict", "update", "--force"]));
    assert_counts(&real, [1, 1, 0], [1, 1, 1]);
    assert_eq!(dry["skipped_edges"], real["skipped_edges"]);
    let exported = fixture.export();
    assert_eq!(entity_at(&exported, 1)["name"], "UpdatedA");
    assert_eq!(entity_at(&exported, 3)["name"], "FreshC");
    assert_eq!(entity_at(&exported, 4)["name"], "FreshD");
    assert_eq!(exported["edges"].as_array().unwrap().len(), 2);
    assert_eq!(edge_at(&exported, 100)["weight"], json!(0.9));
    assert_eq!(edge_at(&exported, 101)["target"], json!(uuid(4)));

    let fresh = Fixture::new();
    let fresh_input = archive(
        vec![entity(4, "FreshD"), entity(5, "FreshE")],
        vec![edge(200, 4, 5, "supports")],
    );
    let dry = report(&fresh.import(&fresh_input, &["--dry-run"]));
    assert_counts(&dry, [2, 0, 0], [1, 0, 0]);
    fresh.assert_no_destination();
    let real = report(&fresh.import(&fresh_input, &[]));
    assert_counts(&real, [2, 0, 0], [1, 0, 0]);
    assert!(fresh.db.exists());
    assert_eq!(
        fresh.export()["edges"],
        json!([edge(200, 4, 5, "supports")])
    );
}

impl Fixture {
    fn seed_collision_note(&self) -> Uuid {
        self.seed(&archive(vec![], vec![]));
        let seed_config = self.directory.path().join("seed.toml");
        std::fs::write(
            &seed_config,
            format!(
                "engines = []\n{}",
                std::fs::read_to_string(&self.config).unwrap()
            ),
        )
        .unwrap();
        let output = self
            .command()
            .args(["exec", "--strict", "--output-format", "json"])
            .arg(
                json!([{
                    "tool": "create",
                    "args": {"kind": "observation", "content": "import identity collision fixture"}
                }])
                .to_string(),
            )
            .arg("--db")
            .arg(&self.db)
            .arg("--config")
            .arg(&seed_config)
            .output()
            .unwrap();
        assert_success(&output);
        let connection =
            Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let id: String = connection
            .query_row(
                "SELECT id FROM notes WHERE content = ?1",
                ["import identity collision fixture"],
                |row| row.get(0),
            )
            .unwrap();
        Uuid::parse_str(&id).unwrap()
    }

    fn collision_note_image(&self, id: Uuid) -> Vec<SqlValue> {
        let connection =
            Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut statement = connection
            .prepare("SELECT * FROM notes WHERE id = ?1")
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_row([id.to_string()], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap()
    }

    fn collision_identity_rows(&self, table: &str, id: Uuid) -> i64 {
        assert!(matches!(table, "entities" | "graph_edges"));
        Connection::open_with_flags(&self.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE id = ?1"),
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }
}

#[test]
fn import_entity_refuses_note_uuid_before_any_row_write() {
    let fixture = Fixture::new();
    let note_id = fixture.seed_collision_note();
    let note_before = fixture.collision_note_image(note_id);
    let before = fixture.rows();
    let input = archive(
        vec![
            entity(5_767_101, "FreshBeforeNoteCollision"),
            entity(note_id.as_u128(), "MustNotShadowNote"),
        ],
        vec![],
    );
    for flags in [
        &[][..],
        &["--on-conflict", "error"][..],
        &["--on-conflict", "update"][..],
        &["--dry-run"][..],
        &["--on-conflict", "update", "--dry-run"][..],
    ] {
        let output = fixture.import(&input, flags);
        assert_refused(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&note_id.to_string()),
            "must refuse the colliding identity: {output:?}"
        );
        assert_eq!(fixture.collision_identity_rows("entities", note_id), 0);
        assert_eq!(fixture.collision_note_image(note_id), note_before);
        assert_eq!(fixture.rows(), before, "partial import with {flags:?}");
    }
}

#[test]
fn import_skip_omits_note_uuid_entity_and_incident_edges() {
    let fixture = Fixture::new();
    let note_id = fixture.seed_collision_note();
    let note_before = fixture.collision_note_image(note_id);
    let before = fixture.rows();
    let input = archive(
        vec![
            entity(note_id.as_u128(), "MustNotShadowNote"),
            entity(5_767_102, "ImportedBesideNote"),
        ],
        vec![edge(5_767_103, note_id.as_u128(), 5_767_102, "supports")],
    );
    let preview = report(&fixture.import(&input, &["--on-conflict", "skip", "--dry-run"]));
    assert_counts(&preview, [1, 0, 1], [0, 0, 1]);
    assert_eq!(fixture.rows(), before);
    let actual = report(&fixture.import(&input, &["--on-conflict", "skip"]));
    assert_eq!(actual, preview);
    assert_eq!(
        actual["skipped_edges"],
        json!([skipped(
            5_767_103,
            note_id.as_u128(),
            5_767_102,
            "supports",
            "skipped_record"
        )])
    );
    assert_eq!(fixture.collision_identity_rows("entities", note_id), 0);
    assert_eq!(
        fixture.collision_identity_rows("entities", uuid(5_767_102)),
        1
    );
    assert_eq!(
        fixture.collision_identity_rows("graph_edges", uuid(5_767_103)),
        0
    );
    assert_eq!(fixture.collision_note_image(note_id), note_before);
}

#[test]
fn import_edge_refuses_note_uuid_before_any_row_write() {
    let fixture = Fixture::new();
    let note_id = fixture.seed_collision_note();
    fixture.seed(&archive(
        vec![entity(5_767_104, "Source"), entity(5_767_105, "Target")],
        vec![],
    ));
    let note_before = fixture.collision_note_image(note_id);
    let before = fixture.rows();
    let input = archive(
        vec![entity(5_767_106, "FreshBeforeEdgeCollision")],
        vec![edge(note_id.as_u128(), 5_767_104, 5_767_105, "supports")],
    );
    for flags in [
        &[][..],
        &["--on-conflict", "update"][..],
        &["--dry-run"][..],
        &["--on-conflict", "update", "--dry-run"][..],
    ] {
        let output = fixture.import(&input, flags);
        assert_refused(&output);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&note_id.to_string()),
            "must refuse the colliding identity: {output:?}"
        );
        assert_eq!(fixture.collision_identity_rows("graph_edges", note_id), 0);
        assert_eq!(fixture.collision_note_image(note_id), note_before);
        assert_eq!(fixture.rows(), before, "partial import with {flags:?}");
    }
    let preview = report(&fixture.import(&input, &["--on-conflict", "skip", "--dry-run"]));
    assert_counts(&preview, [1, 0, 0], [0, 0, 1]);
    assert_eq!(fixture.rows(), before);
    let actual = report(&fixture.import(&input, &["--on-conflict", "skip"]));
    assert_eq!(actual, preview);
    assert_eq!(fixture.collision_identity_rows("graph_edges", note_id), 0);
    assert_eq!(fixture.collision_note_image(note_id), note_before);
}
