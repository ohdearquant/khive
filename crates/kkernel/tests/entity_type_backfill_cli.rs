//! Exercise the admin boundary against private legacy-entity fixtures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use khive_runtime::{KhiveRuntime, RuntimeConfig};
use serde_json::{json, Value};
use tempfile::TempDir;

const NAMESPACE: &str = "fixture";

struct Fixture {
    root: TempDir,
    db: PathBuf,
    config: PathBuf,
    runtime: KhiveRuntime,
}

fn id(index: usize) -> String {
    format!("00000000-0000-0000-0000-{index:012x}")
}

fn isolated_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(root)
        .env("HOME", root)
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_NO_EMBED", "true")
        .env("KHIVE_EVENTS_SPLIT", "0");
    command
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("private backfill fixture");
        let db = root.path().join("entities.db");
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            "[runtime]\npacks = [\"kg\", \"git\"]\n[actor]\nid = \"fixture\"\n",
        )
        .expect("write isolated pack configuration");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("migrate private fixture");
        let rows = [
            (
                "concept",
                Some("algorithm"),
                json!({"type":"not_a_registered_subtype", "keep":"served"}),
            ),
            ("document", None, json!({"type":"paper"})),
            ("concept", None, json!({"type":" ALGO "})),
            (
                "document",
                None,
                json!({"type":" Architecture--Decision__Record "}),
            ),
            (
                "concept",
                None,
                json!({
                    "type":" __CoNcEpT-- ",
                    "nested":{"type":"keep", "number":12},
                    "items":[null, true, {"label":" unchanged "}]
                }),
            ),
            ("concept", None, json!({"type":"Article", "keep":true})),
            ("document", None, json!({"type":"not_a_registered_subtype"})),
            ("concept", None, json!({"other":"missing type"})),
            ("concept", None, json!({"type":null})),
            ("concept", None, json!({"type":42})),
        ];
        {
            let writer = runtime.backend().pool().writer().expect("seed writer");
            for (index, (kind, entity_type, properties)) in rows.into_iter().enumerate() {
                writer
                    .execute(
                        "INSERT INTO entities \
                         (id, namespace, kind, entity_type, name, description, properties, tags, \
                          created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                        (
                            id(index + 1),
                            NAMESPACE,
                            kind,
                            entity_type,
                            format!("Legacy fixture {}", index + 1),
                            "Preserve the description",
                            properties.to_string(),
                            "[\"keep\"]",
                            (index + 1) as i64,
                        ),
                    )
                    .expect("seed legacy entity without write-time normalization");
            }
            for (index, namespace, deleted_at) in
                [(11, "other", None), (12, NAMESPACE, Some(99_i64))]
            {
                writer
                    .execute(
                        "INSERT INTO entities \
                         (id, namespace, kind, name, properties, created_at, updated_at, deleted_at) \
                         VALUES (?1, ?2, 'document', 'Excluded entity', '{\"type\":\"paper\"}', 99, 99, ?3)",
                        (id(index), namespace, deleted_at),
                    )
                    .expect("seed namespace and tombstone controls");
            }
        }
        Self {
            root,
            db,
            config,
            runtime,
        }
    }

    fn run(&self, extra: &[&str]) -> Output {
        isolated_command(self.root.path())
            .arg("entity-type-backfill")
            .arg("--db")
            .arg(&self.db)
            .arg("--config")
            .arg(&self.config)
            .args(["--namespace", NAMESPACE])
            .args(extra)
            .output()
            .expect("run admin command against private fixture")
    }

    fn entities(&self) -> BTreeMap<String, Value> {
        let reader = self
            .runtime
            .backend()
            .pool()
            .reader()
            .expect("fixture reader");
        let mut statement = reader
            .prepare(
                "SELECT id, json_object( \
                 'namespace', namespace, 'kind', kind, 'entity_type', entity_type, \
                 'name', name, 'description', description, 'properties', json(properties), \
                 'tags', json(tags), 'created_at', created_at, 'updated_at', updated_at, \
                 'deleted_at', deleted_at, 'merged_into', merged_into, 'merge_event_id', merge_event_id) \
                 FROM entities ORDER BY id",
            )
            .expect("prepare exact entity snapshot");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query exact entity snapshot");
        rows.map(|row| {
            let (id, encoded) = row.expect("entity row");
            (id, serde_json::from_str(&encoded).expect("entity JSON"))
        })
        .collect()
    }

    fn freeze_sidecars(&self) -> Vec<(PathBuf, std::fs::Permissions)> {
        ["-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| {
                let mut name = self.db.as_os_str().to_os_string();
                name.push(suffix);
                let path = PathBuf::from(name);
                let original = std::fs::metadata(&path).ok()?.permissions();
                let mut frozen = original.clone();
                frozen.set_readonly(true);
                std::fs::set_permissions(&path, frozen).expect("freeze private snapshot sidecar");
                Some((path, original))
            })
            .collect()
    }
}

fn restore_permissions(sidecars: Vec<(PathBuf, std::fs::Permissions)>) {
    for (path, permissions) in sidecars {
        std::fs::set_permissions(path, permissions).expect("restore private sidecar permissions");
    }
}

fn files(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, current: &Path, result: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(current).expect("read fixture directory") {
            let path = entry.expect("fixture entry").path();
            let relative = path
                .strip_prefix(root)
                .expect("private fixture path")
                .to_path_buf();
            if path.is_dir() {
                result.insert(relative, None);
                visit(root, &path, result);
            } else {
                result.insert(
                    relative,
                    Some(std::fs::read(path).expect("snapshot fixture file")),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "admin command failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("admin stdout must be UTF-8");
    let (target_line, encoded) = stdout.split_once('\n').expect("target must precede counts");
    let target = target_line
        .strip_prefix("target: ")
        .expect("resolved target line");
    assert!(
        Path::new(target).is_absolute(),
        "target must be absolute: {target}"
    );
    let value: Value = serde_json::from_str(encoded).expect("JSON report after target line");
    assert_eq!(value["target"], target, "target line and report must agree");
    value
}

fn assert_classification(value: &Value, mode: &str) {
    assert_eq!(value["namespace"], NAMESPACE);
    assert_eq!(value["mode"], mode);
    assert_eq!(value["effective_limit"], 1_000_000);
    assert_eq!(value["scanned"], 10);
    assert_eq!(value["eligible"], 6);
    assert_eq!(value["promote"], 3);
    assert_eq!(value["echo"], 1);
    assert_eq!(value["untouched"], 2);
    assert_eq!(value["wrong_kind"], 1);
    assert_eq!(value["nullable_before"], 9);
    assert_eq!(value["projected_nullable_after"], 6);
    assert_eq!(value["complete"], true);
    assert_eq!(value["failures"], json!([]));
    let packs = value["loaded_packs"]
        .as_array()
        .expect("reported composed packs");
    assert!(packs.contains(&json!("kg")));
    assert!(packs.contains(&json!("git")));
    assert!(value["registry_types"]["document"]
        .as_array()
        .expect("reported document subtypes")
        .contains(&json!("adr")));
    assert!(!value["source_revision"]
        .as_str()
        .expect("source revision")
        .is_empty());
}

#[test]
fn entity_type_backfill_dry_run_classifies_without_changing_db_or_sidecars() {
    let fixture = Fixture::new();
    let entities_before = fixture.entities();
    let frozen = fixture.freeze_sidecars();
    assert_eq!(frozen.len(), 2, "fixture must retain both WAL and SHM");
    let before = files(fixture.root.path());
    let output = fixture.run(&["--dry-run"]);
    let after = files(fixture.root.path());
    restore_permissions(frozen);
    assert_eq!(
        before, after,
        "dry-run must leave all fixture files byte-identical"
    );
    let value = report(&output);
    assert_classification(&value, "dry_run");
    assert_eq!(value["nullable_after"], 9);
    assert_eq!(value["promoted"], 0);
    assert_eq!(value["echo_removed"], 0);
    assert_eq!(fixture.entities(), entities_before);
}

#[test]
fn entity_type_backfill_apply_canonicalizes_preserves_controls_and_is_idempotent() {
    let fixture = Fixture::new();
    let before = fixture.entities();
    let value = report(&fixture.run(&["--apply"]));
    assert_classification(&value, "apply");
    assert_eq!(value["nullable_after"], 6);
    assert_eq!(value["promoted"], 3);
    assert_eq!(value["echo_removed"], 1);

    let after = fixture.entities();
    assert_eq!(
        after.len(),
        before.len(),
        "backfill must not delete entities"
    );
    for (index, canonical) in [(2, "paper"), (3, "algorithm"), (4, "adr")] {
        let mut expected = before[&id(index)].clone();
        expected["entity_type"] = json!(canonical);
        expected["updated_at"] = after[&id(index)]["updated_at"].clone();
        assert_eq!(
            after[&id(index)],
            expected,
            "promotion changes only subtype and revision"
        );
    }
    let mut expected_echo = before[&id(5)].clone();
    expected_echo["properties"]
        .as_object_mut()
        .unwrap()
        .remove("type");
    expected_echo["updated_at"] = after[&id(5)]["updated_at"].clone();
    assert_eq!(
        after[&id(5)],
        expected_echo,
        "echo removes only the top-level type key"
    );
    for index in [1, 6, 7, 8, 9, 10, 11, 12] {
        assert_eq!(
            after[&id(index)],
            before[&id(index)],
            "excluded entity {index} changed"
        );
    }

    let frozen = fixture.freeze_sidecars();
    let output = fixture.run(&["--dry-run"]);
    restore_permissions(frozen);
    let again = report(&output);
    assert_eq!(again["promote"], 0);
    assert_eq!(again["echo"], 0);
    assert_eq!(again["eligible"], 2);
    assert_eq!(again["untouched"], 2);
    assert_eq!(again["wrong_kind"], 1);
    assert_eq!(again["nullable_before"], 6);
    assert_eq!(again["nullable_after"], 6);
    assert_eq!(again["projected_nullable_after"], 6);
    assert_eq!(again["complete"], true);
    assert_eq!(fixture.entities(), after);
}

#[test]
fn entity_type_backfill_limit_counts_live_scanned_entities_not_only_candidates() {
    let fixture = Fixture::new();
    let before = fixture.entities();
    let value = report(&fixture.run(&["--apply", "--limit", "1"]));
    assert_eq!(value["scanned"], 1);
    assert_eq!(value["effective_limit"], 1);
    assert_eq!(value["eligible"], 0);
    for field in [
        "promote",
        "echo",
        "untouched",
        "wrong_kind",
        "promoted",
        "echo_removed",
    ] {
        assert_eq!(value[field], 0, "{field}");
    }
    assert_eq!(value["nullable_before"], 9);
    assert_eq!(value["nullable_after"], 9);
    assert_eq!(value["projected_nullable_after"], 9);
    assert_eq!(value["complete"], false);
    assert_eq!(value["failures"], json!([]));
    assert_eq!(fixture.entities(), before);
}

#[test]
fn entity_type_backfill_applies_across_pages_and_second_scan_is_idempotent() {
    let fixture = Fixture::new();
    {
        let writer = fixture
            .runtime
            .backend()
            .pool()
            .writer()
            .expect("page fixture writer");
        for offset in 0..260 {
            let index = offset + 13;
            let (kind, legacy_type) = if offset % 2 == 0 {
                ("document", " Article ")
            } else {
                ("concept", " CONCEPT ")
            };
            writer
                .execute(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, properties, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, 'Page boundary entity', ?4, ?5, ?5)",
                    (
                        id(index),
                        NAMESPACE,
                        kind,
                        json!({"type":legacy_type, "keep":{"index":index}}).to_string(),
                        index as i64,
                    ),
                )
                .expect("seed bounded multi-page legacy population");
        }
    }
    let value = report(&fixture.run(&["--apply"]));
    assert_eq!(value["scanned"], 270);
    assert_eq!(value["eligible"], 266);
    assert_eq!(value["promote"], 133);
    assert_eq!(value["echo"], 131);
    assert_eq!(value["promoted"], 133);
    assert_eq!(value["echo_removed"], 131);
    assert_eq!(value["untouched"], 2);
    assert_eq!(value["wrong_kind"], 1);
    assert_eq!(value["nullable_before"], 269);
    assert_eq!(value["nullable_after"], 136);
    assert_eq!(value["projected_nullable_after"], 136);
    assert_eq!(value["complete"], true);
    assert_eq!(value["failures"], json!([]));
    let after = fixture.entities();
    assert_eq!(after.len(), 272);
    for offset in 0..260 {
        let index = offset + 13;
        let entity = &after[&id(index)];
        assert_eq!(entity["properties"]["keep"], json!({"index":index}));
        if offset % 2 == 0 {
            assert_eq!(
                entity["entity_type"], "paper",
                "promotion at entity {index}"
            );
        } else {
            assert!(entity["entity_type"].is_null());
            assert!(
                entity["properties"].get("type").is_none(),
                "echo at entity {index}"
            );
        }
    }
    let frozen = fixture.freeze_sidecars();
    let output = fixture.run(&["--dry-run"]);
    restore_permissions(frozen);
    let again = report(&output);
    assert_eq!(again["scanned"], 270);
    assert_eq!(again["eligible"], 2);
    assert_eq!(again["promote"], 0);
    assert_eq!(again["echo"], 0);
    assert_eq!(again["nullable_before"], 136);
    assert_eq!(again["nullable_after"], 136);
    assert_eq!(again["complete"], true);
    assert_eq!(fixture.entities(), after);
}

#[test]
fn entity_type_backfill_rejects_missing_or_conflicting_modes_and_invalid_limits() {
    let root = tempfile::tempdir().expect("private argument fixture");
    let db = root.path().join("missing-parent/missing.db");
    for args in [
        vec![],
        vec!["--dry-run", "--apply"],
        vec!["--dry-run", "--limit", "0"],
        vec!["--apply", "--limit", "1000001"],
    ] {
        let output = isolated_command(root.path())
            .arg("entity-type-backfill")
            .arg("--db")
            .arg(&db)
            .args(&args)
            .output()
            .expect("run invalid CLI arguments");
        assert_eq!(
            output.status.code(),
            Some(2),
            "clap must reject {args:?}: {output:?}"
        );
        assert!(
            !db.parent().unwrap().exists(),
            "invalid args must not create a database directory"
        );
    }
}

#[test]
fn entity_type_backfill_dry_run_missing_database_does_not_create_files() {
    let root = tempfile::tempdir().expect("private missing database fixture");
    let config = root.path().join("config.toml");
    std::fs::write(&config, "[runtime]\npacks = [\"kg\", \"git\"]\n").unwrap();
    let db = root.path().join("missing-parent/missing.db");
    let before = files(root.path());
    let output = isolated_command(root.path())
        .arg("entity-type-backfill")
        .args(["--dry-run", "--db"])
        .arg(&db)
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run missing database dry-run");
    assert!(!output.status.success(), "missing source must be refused");
    assert_eq!(
        files(root.path()),
        before,
        "refusal must not create files or parents"
    );
    assert!(!db.parent().unwrap().exists());
}

#[test]
fn entity_type_backfill_routes_only_to_the_configured_kg_backend() {
    for route in ["main", "other"] {
        let main = Fixture::new();
        let other = Fixture::new();
        let config = main.root.path().join("multi.toml");
        let encoded = toml::to_string(&json!({
            "runtime":{"packs":["kg", "git"]},
            "packs":{"kg":{"backend":route}, "git":{"backend":"other"}},
            "backends":[
                {"name":"main", "path":main.db},
                {"name":"other", "path":other.db}
            ]
        }))
        .expect("serialize isolated backend config");
        std::fs::write(&config, encoded).expect("write multi-backend config");
        let (target, untouched) = if route == "main" {
            (&main, &other)
        } else {
            (&other, &main)
        };
        let untouched_entities = untouched.entities();
        let untouched_before = files(untouched.root.path());
        let discovery_parent = main.root.path().join(".khive");
        let discovery_events = discovery_parent.join("khive.db.events.db");
        assert!(!discovery_parent.exists());
        let output = isolated_command(main.root.path())
            .env_remove("KHIVE_EVENTS_SPLIT")
            .args(["entity-type-backfill", "--apply", "--config"])
            .arg(&config)
            .args(["--namespace", NAMESPACE])
            .output()
            .expect("run configured-backend apply");
        let untouched_after = files(untouched.root.path());
        let value = report(&output);
        assert_classification(&value, "apply");
        assert_eq!(value["nullable_after"], 6);
        assert_eq!(value["promoted"], 3);
        assert_eq!(value["echo_removed"], 1);
        assert_eq!(
            std::fs::canonicalize(value["target"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&target.db).unwrap()
        );
        assert_eq!(target.entities()[&id(4)]["entity_type"], "adr");
        assert!(
            !discovery_events.exists() && !discovery_parent.exists(),
            "default-on events split must not materialize the discovery store ({route})"
        );
        for fixture in [&main, &other] {
            assert!(
                !fixture.root.path().join("entities.db.events.db").exists(),
                "backfill must keep mutation events in the selected database ({route})"
            );
        }
        assert_eq!(
            untouched_before, untouched_after,
            "unrelated backend files must remain byte-identical ({route})"
        );
        assert_eq!(untouched.entities(), untouched_entities);
    }
}

#[test]
fn entity_type_backfill_refuses_conflicting_alias_access_modes_before_writes() {
    let fixture = Fixture::new();
    let config = fixture.root.path().join("conflicting-aliases.toml");
    let encoded = toml::to_string(&json!({
        "runtime":{"packs":["kg", "git"]},
        "packs":{"kg":{"backend":"other"}},
        "backends":[
            {"name":"main", "path":fixture.db, "read_only":true},
            {"name":"other", "path":fixture.db, "read_only":false}
        ]
    }))
    .expect("serialize conflicting aliases");
    std::fs::write(&config, encoded).expect("write conflicting aliases");
    let entities_before = fixture.entities();
    let before = files(fixture.root.path());
    #[cfg(unix)]
    let identity_before = {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&fixture.db).expect("fixture identity");
        (metadata.dev(), metadata.ino())
    };
    for mode in ["--dry-run", "--apply"] {
        let output = isolated_command(fixture.root.path())
            .env_remove("KHIVE_EVENTS_SPLIT")
            .args(["entity-type-backfill", mode, "--config"])
            .arg(&config)
            .args(["--namespace", NAMESPACE])
            .output()
            .expect("run conflicting alias refusal");
        assert!(
            !output.status.success(),
            "contradictory access aliases must refuse {mode}: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("every alias of one database must use the same access mode"),
            "refusal must use the ordinary server alias validation: {stderr}"
        );
        assert_eq!(
            files(fixture.root.path()),
            before,
            "alias refusal must preserve all files and directories ({mode})"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&fixture.db).expect("preserved fixture identity");
            assert_eq!(
                (metadata.dev(), metadata.ino()),
                identity_before,
                "alias refusal must preserve the database identity ({mode})"
            );
        }
    }
    assert_eq!(fixture.entities(), entities_before);
}

#[test]
fn entity_type_backfill_refuses_missing_backend_reference_before_writes() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let config = fixture.root.path().join("missing-backend.toml");
    let encoded = toml::to_string(&json!({
        "runtime":{"packs":["kg", "git"]},
        "packs":{"kg":{"backend":"missing"}},
        "backends":[
            {"name":"main", "path":fixture.db},
            {"name":"other", "path":other.db}
        ]
    }))
    .expect("serialize missing-reference config");
    std::fs::write(&config, encoded).expect("write missing-reference config");
    let before = files(fixture.root.path());
    let other_before = files(other.root.path());
    let output = isolated_command(fixture.root.path())
        .args(["entity-type-backfill", "--apply", "--config"])
        .arg(&config)
        .args(["--namespace", NAMESPACE])
        .output()
        .expect("run missing-backend refusal");
    assert!(
        !output.status.success(),
        "missing KG target must be refused"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing"));
    assert_eq!(
        files(fixture.root.path()),
        before,
        "refusal must not modify main"
    );
    assert_eq!(
        files(other.root.path()),
        other_before,
        "refusal must not modify another backend"
    );
}
