//! Exercise vector maintenance through the admin binary with private databases.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_types::SubstrateKind;
use lattice_embed::EmbeddingModel;
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

const PRIMARY_MODEL: &str = "all-minilm-l6-v2";
const SECONDARY_MODEL: &str = "bge-small-en-v1.5";

struct Fixture {
    root: TempDir,
    db: PathBuf,
    runtime: KhiveRuntime,
}

fn id(index: u128) -> Uuid {
    Uuid::from_u128(index)
}

fn model_key(model: &str) -> String {
    model
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
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
    async fn new(engines: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().expect("private vector fixture");
        let db = root.path().join("vectors.db");
        let mut config = String::from("[runtime]\npacks = [\"kg\"]\n");
        for (index, (name, model)) in engines.iter().enumerate() {
            config.push_str(&format!(
                "\n[[engines]]\nname = {name:?}\nmodel = {model:?}\ndefault = {}\n",
                index == 0
            ));
        }
        std::fs::write(root.path().join("khive.toml"), config)
            .expect("write private engine configuration");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("migrate private vector fixture");
        {
            let writer = runtime.backend().pool().writer().expect("substrate writer");
            for (index, deleted_at) in [(1, None), (3, Some(2_i64))] {
                writer
                    .execute(
                        "INSERT INTO entities \
                         (id, namespace, kind, name, created_at, updated_at, deleted_at) \
                         VALUES (?1, 'a', 'concept', 'Vector fixture entity', 1, 1, ?2)",
                        (id(index).to_string(), deleted_at),
                    )
                    .expect("seed live and soft-deleted entities");
            }
            writer
                .execute(
                    "INSERT INTO notes \
                     (id, namespace, kind, content, created_at, updated_at) \
                     VALUES (?1, 'a', 'observation', 'Live vector fixture note', 1, 1)",
                    [id(2).to_string()],
                )
                .expect("seed live note");
        }

        let models: BTreeSet<&str> = engines.iter().map(|(_, model)| *model).collect();
        for name in models {
            let model: EmbeddingModel = name.parse().expect("supported fixture model");
            let canonical = model.to_string();
            let store = runtime
                .backend()
                .vectors(&model_key(&canonical), &canonical, model.dimensions())
                .expect("create private model vector table");
            for (index, kind, namespace) in [
                (1, SubstrateKind::Entity, "a"),
                (2, SubstrateKind::Note, "a"),
                (3, SubstrateKind::Entity, "a"),
                (4, SubstrateKind::Entity, "a"),
                (5, SubstrateKind::Entity, "b"),
            ] {
                store
                    .insert(
                        id(index),
                        kind,
                        namespace,
                        "content",
                        vec![vec![0.25_f32; model.dimensions()]],
                    )
                    .await
                    .expect("seed vector without embedding inference");
            }
        }
        Self { root, db, runtime }
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.run_with_db(&self.db, extra)
    }

    fn run_with_db(&self, db: &Path, extra: &[&str]) -> Output {
        isolated_command(self.root.path())
            .args(["vector", "sweep", "--db"])
            .arg(db)
            .args(extra)
            .output()
            .expect("run vector sweep against private fixture")
    }

    fn rows(&self, model: &str) -> BTreeMap<String, String> {
        let writer = self
            .runtime
            .backend()
            .pool()
            .writer()
            .expect("vector snapshot connection");
        let mut statement = writer
            .conn()
            .prepare(&format!(
                "SELECT subject_id, namespace FROM vec_{} ORDER BY subject_id",
                model_key(model)
            ))
            .expect("prepare vector row snapshot");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("read vector rows")
            .collect::<Result<_, _>>()
            .expect("collect vector row snapshot")
    }

    fn row_count(&self, model: &str) -> u64 {
        let count: i64 = self
            .runtime
            .backend()
            .pool()
            .reader()
            .expect("count reader")
            .query_row(
                &format!("SELECT COUNT(*) FROM vec_{}", model_key(model)),
                [],
                |row| row.get(0),
            )
            .expect("count stored vector rows");
        u64::try_from(count).expect("vector row count is nonnegative")
    }
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "vector sweep failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON sweep report")
}

fn assert_counts(value: &Value, scanned: u64, deleted: u64, would_delete: u64, hit: bool) {
    assert_eq!(value["scanned"], scanned, "scanned rows");
    assert_eq!(value["deleted"], deleted, "deleted rows");
    assert_eq!(value["would_delete"], would_delete, "uncapped orphan count");
    assert_eq!(value["max_delete_hit"], hit, "deletion budget result");
}

fn store_report<'a>(value: &'a Value, model: &str) -> &'a Value {
    value["stores"]
        .as_array()
        .expect("per-model reports")
        .iter()
        .find(|store| store["model"] == model)
        .expect("selected model report")
}

fn live_rows() -> BTreeMap<String, String> {
    [
        (id(1).to_string(), "a".into()),
        (id(2).to_string(), "a".into()),
    ]
    .into_iter()
    .collect()
}

#[tokio::test]
async fn vector_sweep_dry_run_counts_all_namespaces_without_deleting() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    let before = fixture.rows(PRIMARY_MODEL);
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);

    let output = fixture.run(&["--dry-run"]);
    assert_eq!(
        fixture.row_count(PRIMARY_MODEL),
        5,
        "VECTOR_SWEEP_DRY_RUN_ROWS"
    );
    assert_eq!(fixture.rows(PRIMARY_MODEL), before);
    let value = report(&output);
    assert_counts(&value, 5, 0, 3, false);
    assert_eq!(value["dry_run"], true);
    assert_eq!(value["max_delete"], 1000);
    assert_eq!(value["namespaces"], json!([]));
    assert_eq!(value["stores"].as_array().unwrap().len(), 1);
    let store = store_report(&value, PRIMARY_MODEL);
    assert_eq!(store["engine_names"], json!(["primary"]));
    assert_eq!(store["namespaces"], json!([]));
    assert_counts(store, 5, 0, 3, false);
}

#[tokio::test]
async fn vector_sweep_deletes_orphans_and_keeps_live_entities_and_notes() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);

    let value = report(&fixture.run(&[]));
    assert_counts(&value, 5, 3, 3, false);
    assert_eq!(value["dry_run"], false);
    assert_counts(store_report(&value, PRIMARY_MODEL), 5, 3, 3, false);
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 2);
    assert_eq!(fixture.rows(PRIMARY_MODEL), live_rows());
}

#[tokio::test]
async fn vector_sweep_limit_caps_deletions_and_a_second_run_removes_the_rest() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);

    let first = report(&fixture.run(&["--max-delete", "1"]));
    assert_counts(&first, 5, 1, 3, true);
    assert_eq!(first["max_delete"], 1);
    assert_counts(store_report(&first, PRIMARY_MODEL), 5, 1, 3, true);
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 4);
    let remaining = fixture.rows(PRIMARY_MODEL);
    for (subject, namespace) in live_rows() {
        assert_eq!(remaining.get(&subject), Some(&namespace));
    }

    let second = report(&fixture.run(&[]));
    assert_counts(&second, 4, 2, 2, false);
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 2);
    assert_eq!(fixture.rows(PRIMARY_MODEL), live_rows());
}

#[tokio::test]
async fn vector_sweep_namespace_restricts_deletions() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);

    let output = fixture.run(&["--namespace", "a"]);
    let mut expected = live_rows();
    expected.insert(id(5).to_string(), "b".into());
    assert_eq!(
        fixture.rows(PRIMARY_MODEL),
        expected,
        "VECTOR_SWEEP_NAMESPACE_SCOPE"
    );
    let value = report(&output);
    assert_counts(&value, 4, 2, 2, false);
    assert_eq!(value["namespaces"], json!(["a"]));
    let store = store_report(&value, PRIMARY_MODEL);
    assert_eq!(store["namespaces"], json!(["a"]));
    assert_counts(store, 4, 2, 2, false);
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 3);
}

#[tokio::test]
async fn vector_sweep_rejects_oversized_limit_before_opening_database() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    let before = fixture.rows(PRIMARY_MODEL);
    let missing = fixture.root.path().join("uncreated/vectors.db");
    for db in [&fixture.db, &missing] {
        let output = fixture.run_with_db(db, &["--max-delete", "4294967296"]);
        assert!(!output.status.success(), "oversized limit must be refused");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("--max-delete"), "wrong refusal: {error}");
        assert!(error.contains("4294967295"), "limit must be named: {error}");
        assert!(
            !missing.parent().unwrap().exists(),
            "store open created a directory"
        );
        assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);
        assert_eq!(fixture.rows(PRIMARY_MODEL), before);
    }
}

#[tokio::test]
async fn vector_sweep_zero_limit_counts_without_deleting() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL)]).await;
    let before = fixture.rows(PRIMARY_MODEL);
    for extra in [
        vec!["--max-delete", "0"],
        vec!["--max-delete", "0", "--dry-run"],
    ] {
        let value = report(&fixture.run(&extra));
        assert_counts(&value, 5, 0, 3, true);
        assert_eq!(value["max_delete"], 0);
        assert_counts(store_report(&value, PRIMARY_MODEL), 5, 0, 3, true);
        assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);
        assert_eq!(fixture.rows(PRIMARY_MODEL), before);
    }
}

#[tokio::test]
async fn vector_sweep_shares_the_deletion_budget_across_models() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL), ("secondary", SECONDARY_MODEL)]).await;
    for model in [PRIMARY_MODEL, SECONDARY_MODEL] {
        assert_eq!(fixture.row_count(model), 5);
    }

    let value = report(&fixture.run(&["--max-delete", "1"]));
    assert_counts(&value, 10, 1, 6, true);
    let stores = value["stores"].as_array().expect("two model reports");
    assert_eq!(stores.len(), 2);
    assert_counts(&stores[0], 5, 1, 3, true);
    assert_counts(&stores[1], 5, 0, 3, true);
    assert_eq!(
        fixture.row_count(PRIMARY_MODEL) + fixture.row_count(SECONDARY_MODEL),
        9
    );
    for model in [PRIMARY_MODEL, SECONDARY_MODEL] {
        let store = store_report(&value, model);
        assert_eq!(
            fixture.row_count(model),
            5 - store["deleted"].as_u64().unwrap()
        );
        let remaining = fixture.rows(model);
        for (subject, namespace) in live_rows() {
            assert_eq!(remaining.get(&subject), Some(&namespace));
        }
    }

    let second = report(&fixture.run(&[]));
    assert_counts(&second, 9, 5, 5, false);
    for model in [PRIMARY_MODEL, SECONDARY_MODEL] {
        assert_eq!(fixture.row_count(model), 2);
        assert_eq!(fixture.rows(model), live_rows());
    }
}

#[tokio::test]
async fn vector_sweep_dry_run_accounts_for_the_shared_budget_across_models() {
    let fixture = Fixture::new(&[("primary", PRIMARY_MODEL), ("secondary", SECONDARY_MODEL)]).await;
    let before: Vec<_> = [PRIMARY_MODEL, SECONDARY_MODEL]
        .into_iter()
        .map(|model| fixture.rows(model))
        .collect();

    let value = report(&fixture.run(&["--dry-run", "--max-delete", "4"]));
    assert_counts(&value, 10, 0, 6, true);
    let stores = value["stores"].as_array().expect("two model reports");
    assert_eq!(stores.len(), 2);
    assert_counts(&stores[0], 5, 0, 3, false);
    assert_counts(&stores[1], 5, 0, 3, true);
    for (model, expected) in [PRIMARY_MODEL, SECONDARY_MODEL].into_iter().zip(before) {
        assert_eq!(fixture.row_count(model), 5);
        assert_eq!(fixture.rows(model), expected);
    }
}

#[tokio::test]
async fn vector_sweep_selects_a_configured_engine_name() {
    let fixture = Fixture::new(&[
        ("primary", PRIMARY_MODEL),
        ("primary-alias", PRIMARY_MODEL),
        ("secondary", SECONDARY_MODEL),
    ])
    .await;
    let untouched = fixture.rows(PRIMARY_MODEL);

    let value = report(&fixture.run(&["--engine", "secondary"]));
    assert_counts(&value, 5, 3, 3, false);
    assert_eq!(value["stores"].as_array().unwrap().len(), 1);
    let store = store_report(&value, SECONDARY_MODEL);
    assert_eq!(store["engine_names"], json!(["secondary"]));
    assert_counts(store, 5, 3, 3, false);
    assert_eq!(fixture.row_count(SECONDARY_MODEL), 2);
    assert_eq!(fixture.rows(SECONDARY_MODEL), live_rows());
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 5);
    assert_eq!(fixture.rows(PRIMARY_MODEL), untouched);

    let alias = report(&fixture.run(&["--engine", "primary-alias"]));
    assert_counts(&alias, 5, 3, 3, false);
    assert_eq!(alias["stores"].as_array().unwrap().len(), 1);
    assert_eq!(
        store_report(&alias, PRIMARY_MODEL)["engine_names"],
        json!(["primary-alias"])
    );
    assert_eq!(fixture.row_count(PRIMARY_MODEL), 2);
    assert_eq!(fixture.rows(PRIMARY_MODEL), live_rows());
}

#[tokio::test]
async fn vector_sweep_groups_engine_aliases_and_sweeps_each_model_once() {
    let fixture = Fixture::new(&[
        ("primary", PRIMARY_MODEL),
        ("primary-alias", PRIMARY_MODEL),
        ("secondary", SECONDARY_MODEL),
    ])
    .await;

    let value = report(&fixture.run(&[]));
    assert_counts(&value, 10, 6, 6, false);
    assert_eq!(value["stores"].as_array().unwrap().len(), 2);
    let primary = store_report(&value, PRIMARY_MODEL);
    assert_eq!(primary["engine_names"], json!(["primary", "primary-alias"]));
    assert_counts(primary, 5, 3, 3, false);
    let secondary = store_report(&value, SECONDARY_MODEL);
    assert_eq!(secondary["engine_names"], json!(["secondary"]));
    assert_counts(secondary, 5, 3, 3, false);
    for model in [PRIMARY_MODEL, SECONDARY_MODEL] {
        assert_eq!(fixture.row_count(model), 2);
        assert_eq!(fixture.rows(model), live_rows());
    }
}
