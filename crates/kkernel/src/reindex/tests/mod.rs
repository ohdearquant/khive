mod fts_partition_sweep_tests;
mod record_repair_tests;

use super::*;
use crate::dbpath::resolve_db_override;
use clap::Parser;
use khive_storage::types::{SqlStatement, SqlValue};
use serial_test::serial;

mod cross_process_epoch_tests;
mod cursor_interleaving_tests;
use cross_process_epoch_tests::FixedReindexEmbedder;

// Empty TOML retains the normal default engine. FTS-only fixtures must
// explicitly remove every configured engine before the validated open.
async fn run_reindex_without_embeddings(args: ReindexArgs) -> Result<()> {
    run_reindex_with_setup(
        args,
        |mut cfg| {
            cfg.embedding_model = None;
            cfg.additional_embedding_models.clear();
            cfg
        },
        |runtime| {
            assert!(
                runtime.registered_embedding_model_names().is_empty(),
                "FTS-only reindex fixture must have zero configured models"
            );
            Ok(())
        },
    )
    .await
}

async fn run_reindex_offline(args: ReindexArgs) -> Result<()> {
    run_reindex_with_setup(
        args,
        |cfg| cfg,
        |runtime| {
            for name in runtime.registered_embedding_model_names() {
                let dimensions = runtime.resolve_embedding_model(Some(&name))?.dimensions();
                runtime.register_embedder(FixedReindexEmbedder { name, dimensions });
            }
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn reindex_respects_message_policy_and_explicit_model_override() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("reindex fixture");
    let db_path = dir.path().join("reindex.db");
    let config = dir.path().join("khive.toml");
    std::fs::write(
        &config,
        r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true

[[engines]]
name = "secondary"
model = "paraphrase"
default = false
"#,
    )
    .expect("write two-engine config");

    let resolve_config = |no_embed| {
        resolve_runtime_config(RuntimeConfigInputs {
            db: db_path.to_str(),
            config: Some(&config),
            namespace: Namespace::local(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve reindex config")
    };

    let (message_id, observation_id) = {
        let rt = KhiveRuntime::new(resolve_config(true)).expect("seed runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let notes = rt.notes(&token).expect("note store");
        let message = Note::new("local", "message", "message body for reindex");
        let observation = Note::new("local", "observation", "observation body for reindex");
        let ids = (message.id, observation.id);
        notes.upsert_note(message).await.expect("seed message");
        notes
            .upsert_note(observation)
            .await
            .expect("seed observation");
        ids
    };

    let args = |model: Option<String>| ReindexArgs {
        id: None,
        db: Some(db_path.to_str().unwrap().to_owned()),
        config: Some(config.clone()),
        model,
        batch_size: 100,
        keep_existing: true,
        namespace: Some("local".into()),
        knowledge_only: false,
        no_knowledge: true,
        best_effort: false,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    };

    run_reindex_offline(args(None))
        .await
        .expect("policy-based reindex");

    let rt = KhiveRuntime::new(resolve_config(false)).expect("verify runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let primary = rt.default_embedder_name().to_owned();
    let secondary = rt
        .registered_embedding_model_names()
        .into_iter()
        .find(|model| model != &primary)
        .expect("secondary model");
    let primary_rows = rt
        .vectors_for_model(&token, &primary)
        .expect("primary vectors")
        .batch_exists(&[message_id, observation_id], "local")
        .await
        .expect("primary rows");
    assert!(primary_rows.contains(&message_id));
    assert!(primary_rows.contains(&observation_id));
    let secondary_vectors = rt
        .vectors_for_model(&token, &secondary)
        .expect("secondary vectors");
    let secondary_rows = secondary_vectors
        .batch_exists(&[message_id, observation_id], "local")
        .await
        .expect("secondary rows");
    assert!(!secondary_rows.contains(&message_id));
    assert!(secondary_rows.contains(&observation_id));

    run_reindex_offline(args(Some(secondary.clone())))
        .await
        .expect("explicit model reindex");
    let overridden_rows = secondary_vectors
        .batch_exists(&[message_id], "local")
        .await
        .expect("secondary rows after override");
    assert!(overridden_rows.contains(&message_id));
}

fn write_empty_test_config(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("empty-khive-config.toml");
    std::fs::write(&path, "").expect("write isolated empty config");
    path
}

fn write_declared_backend_test_config(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let main = dir.join("main.db");
    let knowledge = dir.join("knowledge.db");
    let config = dir.join("khive.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "knowledge"
kind = "sqlite"
path = "{}"
"#,
            main.display(),
            knowledge.display(),
        ),
    )
    .expect("write declared-backend config");
    (config, main, knowledge)
}

/// One writable `main` backend plus one `read_only = true` `archive`
/// backend, so a reindex validator test can assert the read-only path is
/// refused while the writable path (the control) still passes.
fn write_read_only_backend_test_config(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let main = dir.join("main.db");
    let archive = dir.join("archive.db");
    let config = dir.join("khive.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
read_only = true
"#,
            main.display(),
            archive.display(),
        ),
    )
    .expect("write read-only-backend config");
    (config, main, archive)
}

#[test]
fn allocation_free_embedding_eligibility_matches_canonical_text() {
    let entities = [
        Entity::new("eligibility", "concept", "named"),
        Entity::new("eligibility", "concept", "").with_description("description only"),
        Entity::new("eligibility", "concept", "   ").with_description("\t"),
    ];
    for entity in &entities {
        assert_eq!(
            entity_has_embedding_text(entity),
            !entity_embedding_text(entity).trim().is_empty()
        );
    }

    let notes = [
        Note::new("eligibility", "observation", "content"),
        Note::new("eligibility", "observation", "  \n\t"),
    ];
    for note in &notes {
        assert_eq!(
            note_has_embedding_text(note),
            !note_embedding_text(note).trim().is_empty()
        );
    }
}

#[tokio::test]
async fn test_reindex_invalidates_vamana_snapshots() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    // Create retrieval_snapshots table and seed rows.
    let mut w = sql.writer().await.expect("writer");
    w.execute_script(include_str!("../../../sql/retrieval_snapshots_prepare.sql").into())
        .await
        .expect("create table");

    for (ns, idx_type) in &[
        ("local::vamana::model-a", "vamana"),
        ("local::vamana::model-b", "vamana"),
        ("other::vamana::model-a", "vamana"),
        ("local::hnsw::model-a", "hnsw"),
    ] {
        w.execute(SqlStatement {
            sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(idx_type.to_string()),
                SqlValue::Blob(b"{}".to_vec()),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    }
    drop(w);

    invalidate_vamana_snapshots(&rt, "local")
        .await
        .expect("invalidate");

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"other::vamana::model-a".to_string()),
        "other namespace must survive: {remaining:?}"
    );
    assert!(
        remaining.contains(&"local::hnsw::model-a".to_string()),
        "HNSW rows must survive: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"local::vamana::model-a".to_string()),
        "local vamana model-a must be deleted: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"local::vamana::model-b".to_string()),
        "local vamana model-b must be deleted: {remaining:?}"
    );
}

#[tokio::test]
async fn test_reindex_invalidate_does_not_cross_underscore_namespace() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    // "a_b" and "aXb" are distinct namespaces (the `_` in "a_b" is a
    // literal underscore, not a wildcard). Before #819's fix, invalidating
    // "a_b" also deleted "aXb"'s row because `_` is a single-character
    // LIKE wildcard.
    for ns in &["a_b::vamana::model-a", "aXb::vamana::model-a"] {
        w.execute(SqlStatement {
            sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text("vamana".to_string()),
                SqlValue::Blob(b"{}".to_vec()),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    }
    drop(w);

    invalidate_vamana_snapshots(&rt, "a_b")
        .await
        .expect("invalidate");

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"aXb::vamana::model-a".to_string()),
        "unrelated namespace 'aXb' must survive invalidating 'a_b': {remaining:?}"
    );
    assert!(
        !remaining.contains(&"a_b::vamana::model-a".to_string()),
        "'a_b' own snapshot must still be deleted: {remaining:?}"
    );
}

/// Regression test (#812): the active global memory Vamana snapshot row
/// must
/// be deleted by `invalidate_active_memory_vamana_snapshot`, since
/// `invalidate_vamana_snapshots`'s `{namespace}::vamana::%` pattern never
/// matches the memory pack's distinct `global::memory_vamana::*` key.
#[tokio::test]
async fn test_reindex_invalidates_active_memory_vamana_snapshot() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    for (ns, idx_type) in &[
        ("global::memory_vamana::model-a", "memory_vamana"),
        ("local::vamana::model-a", "vamana"),
        ("local::memory_vamana::model-a", "memory_vamana"),
    ] {
        w.execute(SqlStatement {
            sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(idx_type.to_string()),
                SqlValue::Blob(b"{}".to_vec()),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    }
    drop(w);

    invalidate_active_memory_vamana_snapshot(&rt).await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        !remaining.contains(&"global::memory_vamana::model-a".to_string()),
        "the active global memory Vamana snapshot must be deleted: {remaining:?}"
    );
    assert!(
        remaining.contains(&"local::vamana::model-a".to_string()),
        "unrelated knowledge Vamana rows must survive: {remaining:?}"
    );
    assert!(
        remaining.contains(&"local::memory_vamana::model-a".to_string()),
        "legacy per-namespace memory Vamana rows are purge_stale_memory_vamana_snapshots's \
             job, not this function's: {remaining:?}"
    );
}

/// Regression test (ADR-116 (PR #1080) condition 4): `purge_stale_memory_vamana_snapshots` must
/// keep the current, retained `global::memory_vamana::{model}` key (ADR-062) and purge
/// only legacy per-namespace `{ns}::memory_vamana::*` rows. The prior predicate
/// (`namespace != 'global'`) matched every row unconditionally, since the namespace
/// column stores the full composite key and is never the bare string `'global'`.
#[tokio::test]
async fn test_purge_stale_memory_vamana_snapshots_keeps_current_key() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    for (ns, idx_type) in &[
        ("global::memory_vamana::model-a", "memory_vamana"),
        ("local::memory_vamana::model-a", "memory_vamana"),
        ("tenant-a::memory_vamana::model-b", "memory_vamana"),
        ("local::vamana::model-a", "vamana"),
    ] {
        w.execute(SqlStatement {
            sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(idx_type.to_string()),
                SqlValue::Blob(b"{}".to_vec()),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    }
    drop(w);

    purge_stale_memory_vamana_snapshots(&rt).await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"global::memory_vamana::model-a".to_string()),
        "current-key global memory Vamana snapshot must be retained: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"local::memory_vamana::model-a".to_string()),
        "legacy per-namespace memory Vamana snapshot must be purged: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"tenant-a::memory_vamana::model-b".to_string()),
        "legacy per-namespace memory Vamana snapshot must be purged: {remaining:?}"
    );
    assert!(
        remaining.contains(&"local::vamana::model-a".to_string()),
        "unrelated knowledge Vamana rows must survive: {remaining:?}"
    );
}

/// Regression test (PR #1081 review): SQLite `LIKE` is ASCII case-insensitive, so
/// `NOT LIKE 'global::memory_vamana::%'` treated a legacy `GLOBAL::memory_vamana::*`
/// row (a valid namespace per namespace validation) as the retained lowercase key and
/// never purged it. `GLOB` is case-sensitive and must tell the two apart.
#[tokio::test]
async fn test_purge_stale_memory_vamana_snapshots_is_case_sensitive() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, \
             index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, \
             created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    for (ns, idx_type) in &[
        ("global::memory_vamana::model-a", "memory_vamana"),
        ("GLOBAL::memory_vamana::model-a", "memory_vamana"),
    ] {
        w.execute(SqlStatement {
            sql: "INSERT INTO retrieval_snapshots \
                      (namespace, index_type, snapshot, created_at) \
                      VALUES (?1, ?2, ?3, 0)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(idx_type.to_string()),
                SqlValue::Blob(b"{}".to_vec()),
            ],
            label: None,
        })
        .await
        .expect("insert row");
    }
    drop(w);

    purge_stale_memory_vamana_snapshots(&rt).await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"global::memory_vamana::model-a".to_string()),
        "current-key lowercase global memory Vamana snapshot must be retained: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"GLOBAL::memory_vamana::model-a".to_string()),
        "legacy uppercase GLOBAL::memory_vamana snapshot must be purged, not mistaken for \
             the retained lowercase key: {remaining:?}"
    );
}

#[tokio::test]
async fn stale_fts_sweep_quotes_malicious_table_name_and_preserves_entities() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    // Seed a base row so `distinct_base_namespaces` returns only `local`
    // and the sweep guard does not skip the drop loop.
    rt.create_entity_with_embedding_report(&token, "concept", None, "seed", None, None, vec![])
        .await
        .map(|(row, _report)| row)
        .expect("seed entity");

    let sql = rt.sql();
    let malicious = "fts_entities_x\"; DROP TABLE entities; --";
    {
        let mut w = sql.writer().await.expect("writer");
        let ddl = format!(
            "CREATE TABLE {} (rowid INTEGER)",
            quote_sqlite_identifier(malicious)
        );
        w.execute(SqlStatement {
            sql: ddl,
            params: vec![],
            label: None,
        })
        .await
        .expect("create malicious stale table");
    }

    sweep_stale_fts_partitions(&rt, "local").await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT COUNT(*) AS c FROM entities".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("entities table must still exist and be queryable");
    let count = rows
        .first()
        .and_then(|row| row.get("c"))
        .map(|v| matches!(v, SqlValue::Integer(n) if *n >= 1))
        .unwrap_or(false);
    assert!(
        count,
        "entities table must survive the sweep with its seeded row intact"
    );

    let survivors = r
        .query_all(SqlStatement {
            sql: "SELECT name FROM sqlite_master WHERE name = ?1".into(),
            params: vec![SqlValue::Text(malicious.to_string())],
            label: None,
        })
        .await
        .expect("query sqlite_master");
    assert!(
        survivors.is_empty(),
        "malicious stale table should have been dropped"
    );
}

fn report_with(errors: u64, k_failed: u64, k_errored: bool) -> ReindexReport {
    ReindexReport {
        entities_processed: 0,
        notes_processed: 0,
        knowledge_atoms_indexed: Some(0),
        knowledge_sections_indexed: None,
        knowledge_sections_superseded: None,
        knowledge_fts_rebuild: None,
        knowledge_atoms_failed: k_failed,
        knowledge_pass_errored: k_errored,
        knowledge_ann_failed: false,
        knowledge_sections_failed: 0,
        models_used: vec![],
        truncation_by_model: BTreeMap::new(),
        elapsed_ms: 0,
        errors_skipped: errors,
        entities_fts_failed: 0,
        notes_fts_failed: 0,
        epoch_bump_failed: false,
        vamana_snapshot_invalidation_failed: false,
    }
}

#[test]
fn report_serializes_per_model_truncation_accounting() {
    let mut report = report_with(0, 0, false);
    report.truncation_by_model.insert(
        "strict-model".to_string(),
        EmbeddingTruncationReport {
            truncated: 2,
            discarded_bytes: 17,
        },
    );
    let json = serde_json::to_value(report).expect("serialize report");
    assert_eq!(json["truncation_by_model"]["strict-model"]["truncated"], 2);
    assert_eq!(
        json["truncation_by_model"]["strict-model"]["discarded_bytes"],
        17
    );
}

#[test]
fn human_report_renders_per_model_truncation_in_sorted_order() {
    let mut report = report_with(0, 0, false);
    report.truncation_by_model.insert(
        "zeta-model".to_string(),
        EmbeddingTruncationReport {
            truncated: 3,
            discarded_bytes: 29,
        },
    );
    report.truncation_by_model.insert(
        "alpha-model".to_string(),
        EmbeddingTruncationReport {
            truncated: 1,
            discarded_bytes: 7,
        },
    );

    let rendered = render_human_report(&report);
    let truncation_lines: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("Embedding truncation"))
        .collect();
    assert_eq!(
        truncation_lines,
        [
            "Embedding truncation (alpha-model): 1 input truncated, 7 bytes discarded",
            "Embedding truncation (zeta-model): 3 inputs truncated, 29 bytes discarded",
        ]
    );
}

#[test]
fn has_failures_flags_each_failure_source() {
    assert!(!report_with(0, 0, false).has_failures());
    assert!(
        report_with(1, 0, false).has_failures(),
        "entity/note errors"
    );
    assert!(
        report_with(0, 1, false).has_failures(),
        "knowledge atom fails"
    );
    assert!(
        report_with(0, 0, true).has_failures(),
        "knowledge pass error"
    );
}

#[test]
fn has_failures_flags_knowledge_ann_failed() {
    let report = ReindexReport {
        entities_processed: 0,
        notes_processed: 0,
        knowledge_atoms_indexed: Some(10),
        knowledge_sections_indexed: None,
        knowledge_sections_superseded: None,
        knowledge_fts_rebuild: None,
        knowledge_atoms_failed: 0,
        knowledge_pass_errored: false,
        knowledge_ann_failed: true,
        knowledge_sections_failed: 0,
        models_used: vec![],
        truncation_by_model: BTreeMap::new(),
        elapsed_ms: 0,
        errors_skipped: 0,
        entities_fts_failed: 0,
        notes_fts_failed: 0,
        epoch_bump_failed: false,
        vamana_snapshot_invalidation_failed: false,
    };
    assert!(
        report.has_failures(),
        "knowledge_ann_failed alone must drive has_failures() = true"
    );
    assert!(
        decide_result(report.has_failures(), false).is_err(),
        "knowledge_ann_failed must fail closed (non-zero exit)"
    );
    assert!(
        decide_result(report.has_failures(), true).is_ok(),
        "best-effort downgrades knowledge_ann_failed to exit 0"
    );
}

#[test]
fn has_failures_flags_knowledge_sections_failed() {
    let report = ReindexReport {
        entities_processed: 0,
        notes_processed: 0,
        knowledge_atoms_indexed: None,
        knowledge_sections_indexed: Some(0),
        knowledge_sections_superseded: None,
        knowledge_fts_rebuild: None,
        knowledge_atoms_failed: 0,
        knowledge_pass_errored: false,
        knowledge_ann_failed: false,
        knowledge_sections_failed: 3,
        models_used: vec![],
        truncation_by_model: BTreeMap::new(),
        elapsed_ms: 0,
        errors_skipped: 0,
        entities_fts_failed: 0,
        notes_fts_failed: 0,
        epoch_bump_failed: false,
        vamana_snapshot_invalidation_failed: false,
    };
    assert!(
        report.has_failures(),
        "knowledge_sections_failed > 0 alone must drive has_failures() = true"
    );
    assert!(
        decide_result(report.has_failures(), false).is_err(),
        "knowledge_sections_failed must fail closed (non-zero exit)"
    );
    assert!(
        decide_result(report.has_failures(), true).is_ok(),
        "best-effort downgrades knowledge_sections_failed to exit 0"
    );
}

#[test]
fn superseded_knowledge_sections_are_reported_without_a_failure_exit() {
    let mut report = report_with(0, 0, false);
    report.knowledge_sections_indexed = Some(0);
    report.knowledge_sections_superseded = Some(2);
    assert!(!report.has_failures());
    assert_eq!(
        serde_json::to_value(&report).unwrap()["knowledge_sections_superseded"],
        2
    );
    assert!(render_human_report(&report).contains("2 changed during embedding"));
}

#[test]
fn decide_result_fails_closed_by_default() {
    assert!(decide_result(false, false).is_ok(), "clean run exits 0");
    assert!(
        decide_result(true, false).is_err(),
        "failures fail closed (non-zero exit)"
    );
}

#[test]
fn decide_result_best_effort_downgrades_to_ok() {
    assert!(
        decide_result(true, true).is_ok(),
        "best-effort downgrades failures to exit 0"
    );
    assert!(decide_result(false, true).is_ok());
}

#[test]
fn rebuild_fts_conflicts_with_no_knowledge() {
    // The rebuild lives inside the knowledge pass; skipping that pass
    // while asking for the rebuild must be refused at parse time instead
    // of accepted and ignored.
    let err = ReindexArgs::try_parse_from(["reindex", "--rebuild-fts", "--no-knowledge"])
        .expect_err("--rebuild-fts with --no-knowledge must be rejected");
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    let ok = ReindexArgs::try_parse_from(["reindex", "--rebuild-fts"])
        .expect("--rebuild-fts alone parses");
    assert!(ok.rebuild_fts);
}

#[test]
fn rebuild_fts_is_off_unless_requested() {
    // The FTS indexes are global while every run targets one namespace,
    // so no run shape implies the rebuild: only the explicit flag does.
    let default_run = ReindexArgs::try_parse_from(["reindex"]).expect("bare reindex parses");
    assert!(!default_run.rebuild_fts);
    let keep_existing_run = ReindexArgs::try_parse_from(["reindex", "--keep-existing"])
        .expect("keep-existing run parses");
    assert!(!keep_existing_run.rebuild_fts);
}

// DB resolution parity with `kkernel exec` / `kkernel mcp`. The shared
// helper is unit-tested in `dbpath`; here we assert reindex consumes it
// through clap (`--db` / `KHIVE_DB` / `:memory:`) the same way.
#[test]
fn db_memory_sentinel_resolves_to_none() {
    assert_eq!(resolve_db_override(Some(":memory:")), Some(None));
}

#[test]
fn db_explicit_path_resolves_to_some() {
    assert_eq!(
        resolve_db_override(Some("/tmp/kkernel-reindex-test.db")),
        Some(Some(PathBuf::from("/tmp/kkernel-reindex-test.db")))
    );
}

#[test]
fn db_absent_leaves_default() {
    assert_eq!(resolve_db_override(None), None);
}

#[test]
#[serial]
fn declared_backends_require_an_explicit_reindex_target() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (config, _, _) = write_declared_backend_test_config(dir.path());

    let error = validate_declared_reindex_target(None, Some(&config))
        .expect_err("a topology-backed reindex without a target must fail closed");
    let message = error.to_string();
    assert!(message.contains("requires an explicit persistent --db / KHIVE_DB target"));
    assert!(message.contains(&config.display().to_string()));
}

#[test]
#[serial]
fn declared_secondary_backend_is_a_valid_reindex_target() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (config, _, knowledge) = write_declared_backend_test_config(dir.path());

    validate_declared_reindex_target(knowledge.to_str(), Some(&config))
        .expect("reindex may target any explicitly declared SQLite backend");
}

#[test]
#[serial]
fn undeclared_reindex_target_is_rejected_with_config_source() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (config, _, _) = write_declared_backend_test_config(dir.path());
    let wrong = dir.path().join("typo.db");

    let error = validate_declared_reindex_target(wrong.to_str(), Some(&config))
        .expect_err("an undeclared target must never be reindexed");
    let message = error.to_string();
    assert!(message.contains("is not a path declared in [[backends]]"));
    assert!(message.contains(&config.display().to_string()));
}

#[test]
#[serial]
fn read_only_declared_backend_is_refused_as_reindex_target() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (config, main, archive) = write_read_only_backend_test_config(dir.path());

    // Expected before the fix: the read-only path passes the validator,
    // since it only matched declared paths and never inspected
    // `read_only` — reindex always writes, so that is wrong.
    let error = validate_declared_reindex_target(archive.to_str(), Some(&config))
        .expect_err("a backend declared read_only must never be reindexed");
    let message = error.to_string();
    assert!(message.contains(&archive.display().to_string()));
    assert!(message.contains("read_only"));

    // Control: the writable backend in the same config still passes.
    validate_declared_reindex_target(main.to_str(), Some(&config))
        .expect("a writable declared backend remains a valid reindex target");

    // Control: an undeclared path is still refused as before.
    let wrong = dir.path().join("typo.db");
    validate_declared_reindex_target(wrong.to_str(), Some(&config))
        .expect_err("an undeclared target must never be reindexed");
}

#[test]
#[serial]
fn reindex_refuses_conflicting_alias_modes_in_either_declaration_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shared = dir.path().join("shared.db");
    let config = dir.path().join("khive.toml");
    for declarations in [
        [("main", false), ("archive", true)],
        [("archive", true), ("main", false)],
    ] {
        let body = declarations
                .iter()
                .map(|(name, read_only)| {
                    format!(
                        "[[backends]]\nname = \"{name}\"\nkind = \"sqlite\"\npath = \"{}\"\nread_only = {read_only}\n",
                        shared.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        std::fs::write(&config, body).expect("write alias config");
        let loaded = KhiveConfig::load_with_home_fallback_and_source(Some(&config), None)
            .expect("parse alias config")
            .expect("explicit config exists");
        let expected = khive_mcp::serve::validate_effective_backend_alias_modes(&loaded.0.backends)
            .expect_err("boot rejects conflicting alias modes")
            .to_string();
        let actual = validate_declared_reindex_target(shared.to_str(), Some(&config))
            .expect_err("reindex must reject conflicting alias modes before opening")
            .to_string();
        assert_eq!(actual, expected, "declaration order: {declarations:?}");
    }
}

#[test]
#[serial]
fn reindex_preserves_consistent_aliases_and_distinct_writable_secondary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shared = dir.path().join("shared.db");
    let distinct = dir.path().join("secondary.db");
    let config = dir.path().join("khive.toml");
    for (main_read_only, archive_read_only, archive_path, admitted) in [
        (false, false, &shared, true),
        (true, true, &shared, false),
        (true, false, &distinct, true),
    ] {
        std::fs::write(
                &config,
                format!(
                    "[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = \"{}\"\nread_only = {main_read_only}\n\
                     \n[[backends]]\nname = \"archive\"\nkind = \"sqlite\"\npath = \"{}\"\nread_only = {archive_read_only}\n",
                    shared.display(),
                    archive_path.display(),
                ),
            )
            .expect("write consistent alias config");
        let result = validate_declared_reindex_target(archive_path.to_str(), Some(&config));
        if admitted {
            assert!(result.unwrap().is_some(), "writable target must be bound");
        } else {
            let error = result.expect_err("read-only alias cannot be reindexed");
            assert!(error.to_string().contains("read_only"), "{error}");
        }
    }
}

#[test]
#[serial]
fn single_backend_reindex_keeps_ordinary_db_override_behavior() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = write_empty_test_config(dir.path());
    let target = dir.path().join("ordinary.db");

    let validated = validate_declared_reindex_target(target.to_str(), Some(&config))
        .expect("without [[backends]], --db remains an ordinary target");
    assert!(
        validated.is_none(),
        "no [[backends]] declared: there is no validated identity to bind, so the \
             ordinary single-backend --db path must be used unchanged"
    );
}

include!("volume_open_tests.rs");

#[test]
#[serial]
fn khive_db_env_binds_to_db_arg() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::set_var("KHIVE_DB", "/tmp/kkernel-reindex-env.db");
    let args = ReindexArgs::parse_from(["reindex"]);
    std::env::remove_var("KHIVE_DB");
    assert_eq!(args.db.as_deref(), Some("/tmp/kkernel-reindex-env.db"));
}

#[test]
#[serial]
fn khive_config_env_binds_to_config_arg() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::set_var("KHIVE_CONFIG", "/tmp/kkernel-reindex.toml");
    let args = ReindexArgs::parse_from(["reindex"]);
    std::env::remove_var("KHIVE_CONFIG");
    assert_eq!(
        args.config.as_deref(),
        Some(std::path::Path::new("/tmp/kkernel-reindex.toml"))
    );
}

// Namespace resolution parity with `kkernel mcp` under ADR-007 Rev 4 Rule 0:
// when --namespace is omitted, the config file `[actor] id` does NOT set
// default_namespace — it stays `local` (writes pin to local). A non-`'local'`
// actor.id IS folded into the default READ visible-set (Rule 3b), but that
// does not affect default_namespace. When --namespace is explicit, it routes
// storage (Rule 1 / reindex's explicit namespace channel) and overrides local.
#[test]
#[serial]
fn namespace_absent_defers_to_local_not_config_actor_id() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write;
    std::env::remove_var("KHIVE_NAMESPACE");
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("khive.toml");
    let mut f = std::fs::File::create(&config_path).expect("create config");
    f.write_all(b"[actor]\nid = \"lambda:prod\"\n")
        .expect("write config");

    // No --namespace: config [actor] id is attribution only (Rule 0), so the
    // effective namespace stays `local` — it must NOT become lambda:prod.
    let resolved = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config");
    assert_eq!(
        resolved.default_namespace.as_str(),
        "local",
        "omitted --namespace must stay local; config [actor] id does NOT set \
             default_namespace (ADR-007 Rev 4 Rule 0)"
    );

    // Explicit --namespace must override [actor] id.
    let resolved_explicit = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::parse("explicit-ns").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config explicit");
    assert_eq!(
        resolved_explicit.default_namespace.as_str(),
        "explicit-ns",
        "explicit --namespace must override config [actor] id"
    );
}

#[test]
#[serial]
fn namespace_env_var_sets_explicit_flag() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::set_var("KHIVE_NAMESPACE", "env-ns");
    let args = ReindexArgs::parse_from(["reindex"]);
    std::env::remove_var("KHIVE_NAMESPACE");
    assert_eq!(
        args.namespace.as_deref(),
        Some("env-ns"),
        "KHIVE_NAMESPACE env var must bind to --namespace"
    );
    assert!(
        args.namespace.is_some(),
        "env var binding must make namespace Some (explicit)"
    );
}

#[test]
#[serial]
fn namespace_absent_defaults_to_none() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_NAMESPACE");
    let args = ReindexArgs::parse_from(["reindex"]);
    assert!(
        args.namespace.is_none(),
        "omitted --namespace must be None (not a String default)"
    );
}

// The old reindex path committed a
// subject-scoped DELETE (`drop_vectors_for_subjects`) before embedding, so a
// transient embed OR insert failure left the prior vector permanently ABSENT
// instead of merely stale. `embed_and_store_batch` no longer pre-deletes —
// it hands the runtime straight to `VectorStore::insert_batch`, which
// replaces each subject's row atomically (DELETE+INSERT under one
// per-record SAVEPOINT, see `replace_vector_row_dml` /
// `insert_batch_rollback_restores_deleted_stale_after_post_delete_insert_failure`
// in khive-db). This test proves the guarantee at the `embed_and_store_batch`
// call boundary: force the embed step (not the storage layer) to fail after a
// stale vector already exists, and assert the stale vector SURVIVES —
// no-worse-than-stale, never absent.
#[tokio::test]
async fn embed_and_store_batch_preserves_stale_vector_on_embed_failure() {
    use async_trait::async_trait;
    use khive_runtime::{EmbedderProvider, RuntimeConfig, RuntimeError};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    struct FailingStubService;

    #[async_trait]
    impl EmbeddingService for FailingStubService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Err(EmbedError::ModelInitialization(
                "simulated transient embed failure".into(),
            ))
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "stub-failing-embed"
        }
    }

    struct StubProvider {
        model_name: &'static str,
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for StubProvider {
        fn name(&self) -> &str {
            self.model_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(FailingStubService))
        }
    }

    const MODEL: &str = "stub-model-embed-fail";
    const DIMS: usize = 4;
    const NS: &str = "local";

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(StubProvider {
        model_name: MODEL,
        dims: DIMS,
    });

    let ns = Namespace::parse(NS).expect("ns");
    let token = rt.authorize(ns).expect("authorize");
    let store = rt.vectors_for_model(&token, MODEL).expect("store");

    // Stale row already present before the reindex pass runs.
    let subject_id = Uuid::new_v4();
    let stale_vec = vec![0.1_f32, 0.2, 0.3, 0.4];
    store
        .insert_batch(vec![VectorRecord {
            subject_id,
            kind: SubstrateKind::Note,
            namespace: NS.to_string(),
            field: "note.content".to_string(),
            embedding_model: Some(MODEL.to_string()),
            vectors: vec![stale_vec.clone()],
            text_fingerprint: None,
            updated_at: chrono::Utc::now(),
        }])
        .await
        .expect("stale insert_batch");
    assert_eq!(store.count().await.expect("count before"), 1);

    // drop_existing = true — this is exactly the code path that used to
    // commit a pre-delete before the (now-failing) embed call.
    let staged = vec![(subject_id, "some note content".to_string())];
    let errors = embed_and_store_batch(
        &rt,
        &token,
        &[MODEL.to_string()],
        NS,
        &staged,
        SubstrateKind::Note,
        "note.content",
        true,
        &mut BTreeMap::new(),
    )
    .await;

    assert_eq!(
        errors, 1,
        "the forced embed failure must count as one error"
    );

    // The stale vector must still be present and unchanged: no pre-delete
    // ran, and embed failing means insert_batch was never even called.
    let after = store.count().await.expect("count after");
    assert_eq!(
        after, 1,
        "an embed failure must leave the prior vector in place, not absent"
    );
    assert!(
        store
            .batch_exists(&[subject_id], NS)
            .await
            .expect("batch_exists after failure")
            .contains(&subject_id),
        "stale subject must still resolve to a row after the embed failure"
    );

    let hits = store
        .search(khive_storage::types::VectorSearchRequest {
            query_vectors: vec![stale_vec],
            top_k: 1,
            namespace: Some(NS.to_string()),
            kind: Some(SubstrateKind::Note),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("search after failure");
    assert_eq!(hits.len(), 1, "stale vector must still be searchable");
    assert_eq!(hits[0].subject_id, subject_id);
    assert!(
        hits[0].score.to_f64() > 0.999,
        "surviving row must be the original stale vector, not a partial write"
    );
}

#[tokio::test]
async fn reindex_builtin_preparation_persists_exact_bounded_prefixed_note_content() {
    use async_trait::async_trait;
    use khive_runtime::{EmbedderProvider, RuntimeConfig, RuntimeError};
    use khive_storage::ContentRef;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
    use std::sync::{Arc, Mutex};

    struct CapturingService(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl EmbeddingService for CapturingService {
        async fn embed(
            &self,
            texts: &[String],
            model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.lock().unwrap().extend_from_slice(texts);
            Ok(texts
                .iter()
                .map(|_| vec![0.5; model.dimensions()])
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "capturing-audited-reindex"
        }
    }

    struct CapturingProvider(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl EmbedderProvider for CapturingProvider {
        fn name(&self) -> &str {
            "multilingual-e5-small"
        }

        fn dimensions(&self) -> usize {
            EmbeddingModel::MultilingualE5Small.dimensions()
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(CapturingService(Arc::clone(&self.0))))
        }
    }

    let model = EmbeddingModel::MultilingualE5Small;
    let model_name = model.to_string();
    assert_eq!(model_name, "multilingual-e5-small");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(model),
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime with built-in model registration");
    runtime.register_test_audited_embedder(model, CapturingProvider(Arc::clone(&captured)));
    let token = runtime
        .authorize(Namespace::parse("local").unwrap())
        .unwrap();

    // Note creation embeds the supplied short override while preserving the
    // full content. Reindex later renders the stored content instead.
    let full_content = "a".repeat(MAX_TEXT_BYTES + 17);
    let creation_input = &full_content[..23];
    let note = runtime
        .create_note_with_embedding_content(
            &token,
            "observation",
            None,
            &full_content,
            Some(creation_input),
            None,
            None,
            vec![],
        )
        .await
        .expect("create with a distinct embedding override");
    assert_eq!(note.content, full_content);
    let creation_prepared = format!("passage: {creation_input}");
    assert_eq!(captured.lock().unwrap().as_slice(), &[creation_prepared]);

    let vectors = runtime.vectors_for_model(&token, &model_name).unwrap();
    let created = vectors.provenance(note.id).await.unwrap().unwrap();
    assert_eq!(
        created.text_fingerprint, None,
        "raw create route is unknown"
    );

    let staged = vec![(note.id, note_embedding_text(&note))];
    let expected_prepared = format!(
        "passage: {}",
        "a".repeat(MAX_TEXT_BYTES - "passage: ".len())
    );
    let expected_fingerprint = ContentRef::from_hex(
        blake3::hash(expected_prepared.as_bytes())
            .to_hex()
            .to_string(),
    )
    .unwrap();
    let mut truncation = BTreeMap::new();
    assert_eq!(
        embed_and_store_batch(
            &runtime,
            &token,
            std::slice::from_ref(&model_name),
            "local",
            &staged,
            SubstrateKind::Note,
            "note.content",
            true,
            &mut truncation,
        )
        .await,
        0
    );
    assert_eq!(captured.lock().unwrap().last(), Some(&expected_prepared));
    let first = vectors.provenance(note.id).await.unwrap().unwrap();
    assert_eq!(first.text_fingerprint, Some(expected_fingerprint.clone()));
    assert!(first.updated_at.is_some());

    // Re-embedding the same source has the same fingerprint; changing a
    // byte inside the bounded input changes it even with identical vectors.
    assert_eq!(
        embed_and_store_batch(
            &runtime,
            &token,
            std::slice::from_ref(&model_name),
            "local",
            &staged,
            SubstrateKind::Note,
            "note.content",
            true,
            &mut truncation,
        )
        .await,
        0
    );
    assert_eq!(
        vectors
            .provenance(note.id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint,
        Some(expected_fingerprint.clone())
    );
    let mut changed = full_content.clone();
    changed.replace_range(..1, "z");
    let changed_staged = vec![(note.id, changed)];
    assert_eq!(
        embed_and_store_batch(
            &runtime,
            &token,
            &[model_name],
            "local",
            &changed_staged,
            SubstrateKind::Note,
            "note.content",
            true,
            &mut truncation,
        )
        .await,
        0
    );
    let changed_prepared = format!(
        "passage: z{}",
        "a".repeat(MAX_TEXT_BYTES - "passage: ".len() - 1)
    );
    assert_eq!(captured.lock().unwrap().last(), Some(&changed_prepared));
    let changed_fingerprint = ContentRef::from_hex(
        blake3::hash(changed_prepared.as_bytes())
            .to_hex()
            .to_string(),
    )
    .unwrap();
    assert_ne!(changed_fingerprint, expected_fingerprint);
    assert_eq!(
        vectors
            .provenance(note.id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint,
        Some(changed_fingerprint)
    );
}

#[tokio::test]
async fn reindex_then_runtime_note_update_clears_same_blob_provenance() {
    use async_trait::async_trait;
    use khive_runtime::{EmbedderProvider, NotePatch, RuntimeConfig, RuntimeError};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    struct ConstantService;

    #[async_trait]
    impl EmbeddingService for ConstantService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.3, 0.4]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "constant-provenance-test"
        }
    }

    struct ConstantProvider;

    #[async_trait]
    impl EmbedderProvider for ConstantProvider {
        fn name(&self) -> &str {
            "provenance-test-model"
        }

        fn dimensions(&self) -> usize {
            2
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(ConstantService))
        }
    }

    const MODEL: &str = "provenance-test-model";
    const NS: &str = "local";
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .unwrap();
    rt.register_embedder(ConstantProvider);
    let token = rt.authorize(Namespace::parse(NS).unwrap()).unwrap();
    let note = rt
        .create_note(
            &token,
            "observation",
            None,
            "first body",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let staged = vec![(note.id, note_embedding_text(&note))];
    assert_eq!(
        embed_and_store_batch(
            &rt,
            &token,
            &[MODEL.to_string()],
            NS,
            &staged,
            SubstrateKind::Note,
            "note.content",
            true,
            &mut BTreeMap::new(),
        )
        .await,
        0
    );
    let vectors = rt.vectors_for_model(&token, MODEL).unwrap();
    let indexed = vectors.provenance(note.id).await.unwrap().unwrap();
    assert_eq!(indexed.text_fingerprint, None);

    // A custom provider does not attest the prepared bytes. Seed a
    // previously attributed row with the same constant vector so this
    // checks that a later raw replacement clears known historical metadata.
    let historical_fingerprint =
        VectorRecord::fingerprint_text("previously attested prepared bytes");
    let seed = vectors
        .insert_batch(vec![VectorRecord {
            subject_id: note.id,
            kind: SubstrateKind::Note,
            namespace: NS.to_string(),
            field: "note.content".to_string(),
            embedding_model: Some(MODEL.to_string()),
            vectors: vec![vec![0.3, 0.4]],
            text_fingerprint: Some(historical_fingerprint.clone()),
            updated_at: chrono::Utc::now(),
        }])
        .await
        .unwrap();
    assert_eq!(seed.affected, 1);
    assert_eq!(seed.failed, 0);
    let attributed = vectors.provenance(note.id).await.unwrap().unwrap();
    assert_eq!(attributed.text_fingerprint, Some(historical_fingerprint));
    assert!(attributed.updated_at.is_some());
    let blob_before = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT hex(embedding) FROM vec_provenance_test_model \
                      WHERE namespace=?1 AND subject_id=?2"
                .into(),
            params: vec![
                SqlValue::Text(NS.into()),
                SqlValue::Text(note.id.to_string()),
            ],
            label: Some("test.note_provenance_blob_before".into()),
        })
        .await
        .unwrap();
    let Some(SqlValue::Text(blob_before)) = blob_before else {
        panic!("attributed vector blob missing")
    };

    let mut patch = NotePatch::default();
    patch.content = Some("second body with different source text".into());
    rt.update_note_with_embedding_report(&token, note.id, patch)
        .await
        .map(|(row, _report)| row)
        .unwrap();
    let blob_after = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT hex(embedding) FROM vec_provenance_test_model \
                      WHERE namespace=?1 AND subject_id=?2"
                .into(),
            params: vec![
                SqlValue::Text(NS.into()),
                SqlValue::Text(note.id.to_string()),
            ],
            label: Some("test.note_provenance_blob_after".into()),
        })
        .await
        .unwrap();
    assert!(matches!(blob_after, Some(SqlValue::Text(blob)) if blob == blob_before));
    let changed = vectors.provenance(note.id).await.unwrap().unwrap();
    assert_eq!(changed.text_fingerprint, None);
    assert_eq!(changed.updated_at, None);
    let sidecar_rows = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM vector_provenance \
                      WHERE model_key=?1 AND namespace=?2 AND subject_id=?3"
                .into(),
            params: vec![
                SqlValue::Text("provenance_test_model".into()),
                SqlValue::Text(NS.into()),
                SqlValue::Text(note.id.to_string()),
            ],
            label: Some("test.note_provenance_sidecar_after".into()),
        })
        .await
        .unwrap();
    assert!(matches!(sidecar_rows, Some(SqlValue::Integer(0))));
}

#[test]
fn has_failures_flags_notes_fts_failed() {
    let report = ReindexReport {
        entities_processed: 0,
        notes_processed: 0,
        knowledge_atoms_indexed: None,
        knowledge_sections_indexed: None,
        knowledge_sections_superseded: None,
        knowledge_fts_rebuild: None,
        knowledge_atoms_failed: 0,
        knowledge_pass_errored: false,
        knowledge_ann_failed: false,
        knowledge_sections_failed: 0,
        models_used: vec![],
        truncation_by_model: BTreeMap::new(),
        elapsed_ms: 0,
        errors_skipped: 0,
        entities_fts_failed: 0,
        notes_fts_failed: 1,
        epoch_bump_failed: false,
        vamana_snapshot_invalidation_failed: false,
    };
    assert!(
        report.has_failures(),
        "notes_fts_failed > 0 alone must drive has_failures() = true"
    );
    assert!(
        decide_result(report.has_failures(), false).is_err(),
        "notes_fts_failed must fail closed (non-zero exit)"
    );
    assert!(
        decide_result(report.has_failures(), true).is_ok(),
        "best-effort downgrades notes_fts_failed to exit 0"
    );
}

// Parity: note_fts_document must produce the same body/title as operations.rs.
#[test]
fn note_fts_document_parity_with_name() {
    let mut note = Note::new("local", "memory", "the content body");
    note.name = Some("my title".to_string());
    let doc = note_fts_document(&note);
    assert_eq!(doc.subject_id, note.id);
    assert_eq!(doc.namespace, "local");
    assert_eq!(doc.title.as_deref(), Some("my title"));
    assert_eq!(doc.body, "my title the content body");
    assert_eq!(doc.kind, SubstrateKind::Note);
}

#[test]
fn note_fts_document_parity_without_name() {
    let note = Note::new("local", "memory", "body only content");
    let doc = note_fts_document(&note);
    assert!(doc.title.is_none());
    assert_eq!(doc.body, "body only content");
}

// Regression: insert N notes via NoteStore (bypassing FTS), run
// fts_backfill_notes_batch, assert FTS count == N and a keyword hit works.
#[tokio::test]
async fn fts_backfill_populates_pre_existing_notes() {
    use khive_storage::types::TextFilter;
    use khive_types::SubstrateKind;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    let notes: Vec<Note> = (0..5)
        .map(|i| {
            Note::new(
                "local",
                "memory",
                format!("zxqsentinel{i} backfill content"),
            )
        })
        .collect();

    let note_store = rt.notes(&token).expect("note store");
    for note in &notes {
        note_store
            .upsert_note(note.clone())
            .await
            .expect("upsert note");
    }

    // FTS should be empty before backfill (notes inserted via store, not runtime).
    let fts = rt.text_for_notes(&token).expect("FTS store");
    let before = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count before");
    assert_eq!(before, 0, "FTS must be empty before backfill");

    // Run the backfill.
    let errors = fts_backfill_notes_batch(&rt, &token, &notes).await;
    assert_eq!(errors, 0, "backfill must produce zero errors");

    // FTS must now contain one row per note.
    let after = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count after");
    assert_eq!(after, 5, "FTS must contain exactly N docs after backfill");

    // A keyword from the first note must be retrievable.
    let hits = fts
        .search(khive_storage::types::TextSearchRequest {
            query: "zxqsentinel0".to_string(),
            mode: khive_storage::types::TextQueryMode::Plain,
            filter: None,
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .expect("FTS search");
    assert!(
        hits.iter().any(|h| h.subject_id == notes[0].id),
        "pre-existing note must be findable by FTS after backfill"
    );
}

// Cross-path equality: a note created through the runtime (operations.rs path)
// must produce a stored FTS document that is field-identical to calling
// note_fts_document() on the same Note. Catches drift between the shared
// constructor and any caller that previously built documents inline.
// Properties are included so that metadata and updated_at are also under test.
#[tokio::test]
async fn note_fts_document_matches_runtime_create_path() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    // Create with a name AND properties so metadata, title+body composition,
    // and updated_at derivation are all exercised.
    let props = serde_json::json!({"key": "value", "score": 42});
    let note = rt
        .create_note(
            &token,
            "observation",
            Some("cross path title"),
            "cross path content body",
            None,
            Some(props),
            vec![],
        )
        .await
        .expect("create_note");

    // Retrieve the stored FTS document written by the create path.
    let fts = rt.text_for_notes(&token).expect("FTS store");
    let stored = fts
        .get_document("local", note.id)
        .await
        .expect("get_document")
        .expect("document must exist after create");

    // Build the expected document using the shared constructor on the same note.
    let expected = note_fts_document(&note);

    assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
    assert_eq!(stored.kind, expected.kind, "kind");
    assert_eq!(stored.title, expected.title, "title");
    assert_eq!(stored.body, expected.body, "body");
    assert_eq!(stored.namespace, expected.namespace, "namespace");
    assert_eq!(stored.tags, expected.tags, "tags");
    assert_eq!(stored.metadata, expected.metadata, "metadata");
    // Compare at microsecond resolution — DateTime<Utc> round-trips through i64.
    assert_eq!(
        stored.updated_at.timestamp_micros(),
        note.updated_at,
        "updated_at must be derived from the note, not Utc::now()"
    );
}

// Regression: run_reindex with no embedding model must still populate FTS for
// pre-existing notes. Guards against reintroduction of the early-return that
// skipped the FTS pass when model_names was empty.
#[tokio::test]
async fn run_reindex_populates_fts_without_embedding_model() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_storage::types::TextFilter;
    use khive_types::SubstrateKind;

    // Use a temp-file db so run_reindex (which builds its own runtime) and our
    // verification pass share the same on-disk state.
    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let db_path = db_file.path().to_str().expect("utf8 path").to_string();
    let config_dir = tempfile::tempdir().expect("config temp dir");
    let config = write_empty_test_config(config_dir.path());

    // Seed notes via a runtime opened on the same file BEFORE calling run_reindex.
    {
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: Some(&config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for seed");
        let rt = KhiveRuntime::new(cfg).expect("seed runtime");
        let token = rt
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let note_store = rt.notes(&token).expect("note store");
        for i in 0..3usize {
            note_store
                .upsert_note(Note::new(
                    "local",
                    "observation",
                    format!("run-reindex-sentinel{i} body"),
                ))
                .await
                .expect("upsert seed note");
        }
    }

    // run_reindex with no embedding model and --no-knowledge.
    let args = ReindexArgs {
        id: None,
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: Some("local".to_string()),
        knowledge_only: false,
        no_knowledge: true,
        best_effort: true,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    };
    run_reindex_without_embeddings(args)
        .await
        .expect("run_reindex must succeed");

    // Verify FTS was populated by re-opening the db.
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_path),
        config: Some(&config),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config for verify");
    let rt = KhiveRuntime::new(cfg).expect("verify runtime");
    let token = rt
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let fts = rt.text_for_notes(&token).expect("FTS store");
    let count = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("fts count");
    assert_eq!(
        count, 3,
        "run_reindex must populate FTS even when no embedding model is configured"
    );
}

// No-embedding-model FTS: when no embedding model is registered, the note
// loop and FTS backfill must still execute — FTS needs no embedder.
#[tokio::test]
async fn fts_backfill_runs_without_embedding_model() {
    use khive_storage::types::TextFilter;
    use khive_types::SubstrateKind;

    // KhiveRuntime::memory() has no embedding model configured.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    let notes: Vec<Note> = (0..3)
        .map(|i| {
            Note::new(
                "local",
                "observation",
                format!("nomodel-sentinel{i} content"),
            )
        })
        .collect();

    let note_store = rt.notes(&token).expect("note store");
    for note in &notes {
        note_store.upsert_note(note.clone()).await.expect("upsert");
    }

    // With no embedding model, embed_and_store_batch is a no-op but
    // fts_backfill_notes_batch must still populate the FTS index.
    let errors = fts_backfill_notes_batch(&rt, &token, &notes).await;
    assert_eq!(
        errors, 0,
        "FTS backfill must succeed with no embedding model"
    );

    let fts = rt.text_for_notes(&token).expect("FTS store");
    let count = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count");
    assert_eq!(
        count, 3,
        "FTS must be populated even when no embedding model is configured"
    );
}

// Idempotency: running backfill twice must not duplicate rows.
#[tokio::test]
async fn fts_backfill_is_idempotent() {
    use khive_storage::types::TextFilter;
    use khive_types::SubstrateKind;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    let notes: Vec<Note> = (0..3)
        .map(|i| Note::new("local", "memory", format!("idemnote{i} content")))
        .collect();

    let note_store = rt.notes(&token).expect("note store");
    for note in &notes {
        note_store
            .upsert_note(note.clone())
            .await
            .expect("upsert note");
    }

    let errors1 = fts_backfill_notes_batch(&rt, &token, &notes).await;
    let errors2 = fts_backfill_notes_batch(&rt, &token, &notes).await;
    assert_eq!(errors1, 0);
    assert_eq!(errors2, 0);

    let fts = rt.text_for_notes(&token).expect("FTS store");
    let count = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Note],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count");
    assert_eq!(count, 3, "second backfill pass must not duplicate rows");
}

// Parity: entity_fts_document must produce the same body/title as
// operations.rs create_entity.
#[test]
fn entity_fts_document_parity_with_description() {
    use khive_storage::entity::Entity;
    let mut entity = Entity::new("local", "concept", "TestEntity");
    entity = entity.with_description("detail text");
    let doc = entity_fts_document(&entity);
    assert_eq!(doc.subject_id, entity.id);
    assert_eq!(doc.namespace, "local");
    assert_eq!(doc.title.as_deref(), Some("TestEntity"));
    assert_eq!(doc.body, "TestEntity detail text");
    assert_eq!(doc.kind, SubstrateKind::Entity);
}

#[test]
fn entity_fts_document_parity_without_description() {
    use khive_storage::entity::Entity;
    let entity = Entity::new("local", "concept", "NameOnly");
    let doc = entity_fts_document(&entity);
    assert_eq!(doc.title.as_deref(), Some("NameOnly"));
    assert_eq!(doc.body, "NameOnly");
}

// Regression: insert N entities via EntityStore (bypassing FTS), run
// fts_backfill_entities_batch, assert FTS count == N and a keyword hit works.
#[tokio::test]
async fn fts_backfill_populates_pre_existing_entities() {
    use khive_storage::entity::Entity;
    use khive_storage::types::TextFilter;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    let entities: Vec<Entity> = (0..5)
        .map(|i| {
            Entity::new("local", "concept", format!("zxqentitysentinel{i}"))
                .with_description(format!("backfill entity description {i}"))
        })
        .collect();

    let entity_store = rt.entities(&token).expect("entity store");
    for entity in &entities {
        entity_store
            .upsert_entity(entity.clone())
            .await
            .expect("upsert entity");
    }

    // FTS should be empty before backfill (entities inserted via store, not runtime).
    let fts = rt.text(&token).expect("FTS store");
    let before = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Entity],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count before");
    assert_eq!(before, 0, "FTS must be empty before backfill");

    // Run the backfill.
    let errors = fts_backfill_entities_batch(&rt, &token, &entities).await;
    assert_eq!(errors, 0, "backfill must produce zero errors");

    // FTS must now contain one row per entity.
    let after = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Entity],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count after");
    assert_eq!(after, 5, "FTS must contain exactly N docs after backfill");

    // A keyword from the first entity must be retrievable.
    let hits = fts
        .search(khive_storage::types::TextSearchRequest {
            query: "zxqentitysentinel0".to_string(),
            mode: khive_storage::types::TextQueryMode::Plain,
            filter: None,
            top_k: 10,
            snippet_chars: 0,
        })
        .await
        .expect("FTS search");
    assert!(
        hits.iter().any(|h| h.subject_id == entities[0].id),
        "pre-existing entity must be findable by FTS after backfill"
    );
}

// run_reindex with no embedding model must populate entity FTS for pre-existing
// entities. Guards the entity FTS path running independently of embedding.
#[tokio::test]
async fn run_reindex_populates_entity_fts_without_embedding_model() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_storage::entity::Entity;
    use khive_storage::types::TextFilter;

    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let db_path = db_file.path().to_str().expect("utf8 path").to_string();
    let config_dir = tempfile::tempdir().expect("config temp dir");
    let config = write_empty_test_config(config_dir.path());

    // Seed entities via EntityStore (bypassing runtime FTS write).
    {
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_path),
            config: Some(&config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config for seed");
        let rt = KhiveRuntime::new(cfg).expect("seed runtime");
        let token = rt
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entity_store = rt.entities(&token).expect("entity store");
        for i in 0..3usize {
            entity_store
                .upsert_entity(Entity::new(
                    "local",
                    "concept",
                    format!("reindex-entity-sentinel{i}"),
                ))
                .await
                .expect("upsert seed entity");
        }
    }

    let args = ReindexArgs {
        id: None,
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: Some("local".to_string()),
        knowledge_only: false,
        no_knowledge: true,
        best_effort: true,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    };
    run_reindex_without_embeddings(args)
        .await
        .expect("run_reindex must succeed");

    // Verify entity FTS was populated.
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_path),
        config: Some(&config),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config for verify");
    let rt = KhiveRuntime::new(cfg).expect("verify runtime");
    let token = rt
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let fts = rt.text(&token).expect("entity FTS store");
    let count = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Entity],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("fts count");
    assert_eq!(
        count, 3,
        "run_reindex must populate entity FTS even when no embedding model is configured"
    );
}

/// Seeds one knowledge atom and deliberately desynchronizes `fts_knowledge`
/// against it (same technique as the pack-level FTS-repair regression),
/// so a caller can observe whether a later `run_reindex` call repaired it.
async fn seed_desynced_knowledge_fts(db_path: &str, config: &std::path::Path) {
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(db_path),
        config: Some(config),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config for seed");
    let rt = KhiveRuntime::new(cfg).expect("seed runtime");
    let mut writer = rt.sql().writer().await.expect("knowledge writer");
    writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                          (id, namespace, slug, name, content, created_at, updated_at) \
                          VALUES ('9de50000-0000-4000-8000-000000000001', 'local', \
                                  'reindex-fts-scope', 'Reindex FTS Scope', \
                                  'scopeable lexical atom document', 1, 1)"
                    .into(),
                params: vec![],
                label: Some("test.reindex_fts_scope.atom".into()),
            },
            SqlStatement {
                sql: "INSERT INTO fts_knowledge \
                          (fts_knowledge, rowid, id, namespace, slug, name, content) \
                          SELECT 'delete', rowid, id, namespace, slug, name, content \
                          FROM knowledge_atoms \
                          WHERE id = '9de50000-0000-4000-8000-000000000001'"
                    .into(),
                params: vec![],
                label: Some("test.reindex_fts_scope.desync".into()),
            },
        ])
        .await
        .expect("seed and desynchronize fts_knowledge");
}

/// True once `fts_knowledge` again matches the seeded atom's content —
/// i.e. the desync `seed_desynced_knowledge_fts` created was repaired.
async fn knowledge_fts_repaired(db_path: &str, config: &std::path::Path) -> bool {
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(db_path),
        config: Some(config),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve config for verify");
    let rt = KhiveRuntime::new(cfg).expect("verify runtime");
    let mut reader = rt.sql().reader().await.expect("knowledge reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT count(*) AS n FROM fts_knowledge \
                      WHERE fts_knowledge MATCH 'scopeable'"
                .into(),
            params: vec![],
            label: Some("test.reindex_fts_scope.verify".into()),
        })
        .await
        .expect("query fts_knowledge")
        .expect("count row");
    matches!(row.get("n"), Some(SqlValue::Integer(1)))
}

// Regression for the FTS-rebuild scoping fix: an explicit `--namespace`
// makes the run scoped, and `fts_knowledge`/`fts_sections` are global —
// rebuilding them on a scoped run is exactly the wasted writer work this
// fix removes. Before the fix, `rebuild_fts` was unconditionally `true`
// and this desync would have been repaired regardless of scope.
#[tokio::test]
async fn run_reindex_scoped_run_does_not_rebuild_fts() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let db_path = db_file.path().to_str().expect("utf8 path").to_string();
    let config_dir = tempfile::tempdir().expect("config temp dir");
    let config = write_empty_test_config(config_dir.path());

    seed_desynced_knowledge_fts(&db_path, &config).await;

    let args = ReindexArgs {
        id: None,
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: Some("local".to_string()), // explicit → scoped run
        knowledge_only: false,
        no_knowledge: false,
        best_effort: true,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    };
    run_reindex_offline(args)
        .await
        .expect("run_reindex must succeed");

    assert!(
        !knowledge_fts_repaired(&db_path, &config).await,
        "a namespace-scoped run must NOT rebuild the global knowledge FTS indexes"
    );
}

// Companion to the scoped-run test above: a run with no explicit
// --namespace still targets one namespace (the configured one), so it
// does not imply the global rebuild either. Only the flag does.
#[tokio::test]
async fn run_reindex_without_the_flag_does_not_rebuild_fts() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let db_path = db_file.path().to_str().expect("utf8 path").to_string();
    let config_dir = tempfile::tempdir().expect("config temp dir");
    let config = write_empty_test_config(config_dir.path());

    seed_desynced_knowledge_fts(&db_path, &config).await;

    let args = ReindexArgs {
        id: None,
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: None, // omitted namespace resolves to the configured one
        knowledge_only: false,
        no_knowledge: false,
        best_effort: true,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    };
    run_reindex_offline(args)
        .await
        .expect("run_reindex must succeed");

    assert!(
        !knowledge_fts_repaired(&db_path, &config).await,
        "a run without --rebuild-fts must not rebuild the global knowledge FTS indexes"
    );
}

// The explicit flag routes through the operator entry point and repairs
// the desync end to end.
#[tokio::test]
async fn run_reindex_with_the_flag_rebuilds_fts() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let db_path = db_file.path().to_str().expect("utf8 path").to_string();
    let config_dir = tempfile::tempdir().expect("config temp dir");
    let config = write_empty_test_config(config_dir.path());

    seed_desynced_knowledge_fts(&db_path, &config).await;

    let args = ReindexArgs {
        id: None,
        db: Some(db_path.clone()),
        config: Some(config.clone()),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: None,
        knowledge_only: false,
        no_knowledge: false,
        best_effort: true,
        no_sections: false,
        sections_only: false,
        rebuild_fts: true,
        human: false,
    };
    run_reindex_offline(args)
        .await
        .expect("run_reindex must succeed");

    assert!(
        knowledge_fts_repaired(&db_path, &config).await,
        "--rebuild-fts must rebuild and repair the global knowledge FTS indexes"
    );
}

// Idempotency: running entity FTS backfill twice must not duplicate rows.
#[tokio::test]
async fn fts_backfill_entities_is_idempotent() {
    use khive_storage::entity::Entity;
    use khive_storage::types::TextFilter;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = Namespace::parse("local").expect("ns");
    let token = rt.authorize(ns).expect("authorize");

    let entities: Vec<Entity> = (0..3)
        .map(|i| Entity::new("local", "concept", format!("idem-entity{i}")))
        .collect();

    let entity_store = rt.entities(&token).expect("entity store");
    for entity in &entities {
        entity_store
            .upsert_entity(entity.clone())
            .await
            .expect("upsert entity");
    }

    let errors1 = fts_backfill_entities_batch(&rt, &token, &entities).await;
    let errors2 = fts_backfill_entities_batch(&rt, &token, &entities).await;
    assert_eq!(errors1, 0);
    assert_eq!(errors2, 0);

    let fts = rt.text(&token).expect("FTS store");
    let count = fts
        .count(TextFilter {
            kinds: vec![SubstrateKind::Entity],
            record_kinds: vec![],
            namespaces: vec!["local".to_string()],
            ids: vec![],
        })
        .await
        .expect("count");
    assert_eq!(
        count, 3,
        "second backfill pass must not duplicate entity rows"
    );
}

// has_failures must flag entities_fts_failed alone.
#[test]
fn has_failures_flags_entities_fts_failed() {
    let report = ReindexReport {
        entities_processed: 0,
        notes_processed: 0,
        knowledge_atoms_indexed: None,
        knowledge_sections_indexed: None,
        knowledge_sections_superseded: None,
        knowledge_fts_rebuild: None,
        knowledge_atoms_failed: 0,
        knowledge_pass_errored: false,
        knowledge_ann_failed: false,
        knowledge_sections_failed: 0,
        models_used: vec![],
        truncation_by_model: BTreeMap::new(),
        elapsed_ms: 0,
        errors_skipped: 0,
        entities_fts_failed: 1,
        notes_fts_failed: 0,
        epoch_bump_failed: false,
        vamana_snapshot_invalidation_failed: false,
    };
    assert!(
        report.has_failures(),
        "entities_fts_failed > 0 alone must drive has_failures() = true"
    );
    assert!(
        decide_result(report.has_failures(), false).is_err(),
        "entities_fts_failed must fail closed (non-zero exit)"
    );
    assert!(
        decide_result(report.has_failures(), true).is_ok(),
        "best-effort downgrades entities_fts_failed to exit 0"
    );
}

#[test]
fn snapshot_invalidation_failure_is_reported_and_controls_exit() {
    let mut report = report_with(0, 0, false);
    assert!(finish(&report, false).is_ok());
    assert_eq!(
        serde_json::to_value(&report).unwrap()["vamana_snapshot_invalidation_failed"],
        false
    );
    report.vamana_snapshot_invalidation_failed = true;
    assert!(finish(&report, false).is_err());
    assert!(finish(&report, true).is_ok());
    assert_eq!(
        serde_json::to_value(&report).unwrap()["vamana_snapshot_invalidation_failed"],
        true
    );
    let human = render_human_report(&report);
    assert!(human.contains("Reindex completed WITH FAILURES"));
    assert!(human.contains("Vamana snapshot invalidation: FAILED"));
    assert!(human.contains("prior writes remain committed"));
}

fn snapshot_reindex_args(dir: &std::path::Path, best_effort: bool) -> ReindexArgs {
    ReindexArgs {
        id: None,
        db: Some(dir.join("reindex.db").to_str().unwrap().to_owned()),
        config: Some(write_empty_test_config(dir)),
        model: None,
        batch_size: 100,
        keep_existing: false,
        namespace: Some("local".into()),
        knowledge_only: false,
        no_knowledge: true,
        best_effort,
        no_sections: false,
        sections_only: false,
        rebuild_fts: false,
        human: false,
    }
}

fn snapshot_test_runtime(args: &ReindexArgs) -> KhiveRuntime {
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve owned test database");
    KhiveRuntime::new(cfg).expect("owned test runtime")
}

#[tokio::test]
async fn reindex_fts_fixture_clears_primary_and_additional_models() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("reindex fixture");
    let args = snapshot_reindex_args(dir.path(), false);
    std::fs::write(
        args.config.as_ref().unwrap(),
        r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true

[[engines]]
name = "additional"
model = "paraphrase"
default = false
"#,
    )
    .expect("write two-engine fixture");
    let configured = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve fixture engine precondition");
    assert!(configured.embedding_model.is_some());
    assert_eq!(configured.additional_embedding_models.len(), 1);
    run_reindex_without_embeddings(args)
        .await
        .expect("FTS-only fixture must clear both configured engines");
}

async fn snapshot_test_count(rt: &KhiveRuntime, query: &str) -> i64 {
    let mut reader = rt.sql().reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: query.into(),
            params: vec![],
            label: Some("test.reindex_snapshot.count".into()),
        })
        .await
        .expect("query")
        .expect("count row");
    match row.get("n") {
        Some(SqlValue::Integer(n)) => *n,
        other => panic!("expected integer count, got {other:?}"),
    }
}

async fn seed_snapshot_test_note(rt: &KhiveRuntime) {
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // Empty embedding text still requires an FTS backfill write. The command
    // fixture installs offline providers independently of its seed text.
    rt.notes(&token)
        .expect("notes")
        .upsert_note(Note::new("local", "observation", ""))
        .await
        .expect("seed note without indexing");
    assert_eq!(
        snapshot_test_count(rt, "SELECT count(*) AS n FROM fts_notes").await,
        0
    );
}

#[tokio::test]
async fn run_reindex_snapshot_failure_fails_closed_and_preserves_committed_work() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("owned database directory");
    let args = snapshot_reindex_args(dir.path(), false);
    let rt = snapshot_test_runtime(&args);
    seed_snapshot_test_note(&rt).await;
    {
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute_script(
                "CREATE TABLE retrieval_snapshots (
                        namespace TEXT NOT NULL, index_type TEXT NOT NULL,
                        snapshot BLOB NOT NULL, created_at INTEGER NOT NULL,
                        PRIMARY KEY(namespace, index_type));
                     INSERT INTO retrieval_snapshots VALUES
                        ('local::vamana::test-model', 'vamana', X'00', 0),
                        ('other::vamana::test-model', 'vamana', X'00', 0),
                        ('local::hnsw::test-model', 'hnsw', X'00', 0),
                        ('global::memory_vamana::test-model', 'memory_vamana', X'00', 0);
                     CREATE TRIGGER refuse_namespace_snapshot_delete
                     BEFORE DELETE ON retrieval_snapshots
                     WHEN OLD.namespace = 'local::vamana::test-model'
                     BEGIN SELECT RAISE(ABORT, 'injected namespace snapshot failure'); END;"
                    .into(),
            )
            .await
            .expect("seed snapshots and rejecting trigger");
    }

    let injected = invalidate_vamana_snapshots(&rt, "local")
        .await
        .expect_err("the owned trigger must reject the real invalidation statement");
    assert!(injected
        .to_string()
        .contains("injected namespace snapshot failure"));
    let error = run_reindex_offline(args)
        .await
        .expect_err("snapshot failure must fail the command");
    assert!(error
        .to_string()
        .contains("reindex completed with failures"));
    assert_eq!(
        snapshot_test_count(&rt, "SELECT count(*) AS n FROM fts_notes").await,
        1,
        "the earlier FTS write remains committed"
    );
    assert_eq!(
        snapshot_test_count(&rt, "SELECT epoch AS n FROM memory_ann_epoch").await,
        2,
        "snapshot failure must not suppress the completion epoch"
    );
    assert_eq!(
        snapshot_test_count(&rt, "SELECT count(*) AS n FROM retrieval_snapshots").await,
        3,
        "only the independent active-memory snapshot was removed"
    );

    run_reindex_offline(snapshot_reindex_args(dir.path(), true))
        .await
        .expect("explicit best effort allows partial completion");
    assert_eq!(
        snapshot_test_count(&rt, "SELECT count(*) AS n FROM retrieval_snapshots").await,
        3,
        "best effort must not bypass the injected DELETE refusal"
    );
    {
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute_script("DROP TRIGGER refuse_namespace_snapshot_delete;".into())
            .await
            .expect("remove injected failure");
    }
    run_reindex_offline(snapshot_reindex_args(dir.path(), false))
        .await
        .expect("same fixture succeeds once invalidation can commit");
    assert_eq!(
        snapshot_test_count(&rt, "SELECT count(*) AS n FROM retrieval_snapshots").await,
        2,
        "unrelated namespace and HNSW snapshots survive"
    );
    assert_eq!(
            snapshot_test_count(
                &rt,
                "SELECT count(*) AS n FROM retrieval_snapshots WHERE namespace = 'local::vamana::test-model'"
            )
            .await,
            0
        );
}

#[tokio::test]
async fn run_reindex_snapshot_missing_table_remains_successful() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("owned database directory");
    let args = snapshot_reindex_args(dir.path(), false);
    let rt = snapshot_test_runtime(&args);
    seed_snapshot_test_note(&rt).await;
    let table_count = "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = 'retrieval_snapshots'";
    assert_eq!(snapshot_test_count(&rt, table_count).await, 0);
    run_reindex_offline(args)
        .await
        .expect("missing snapshots are a successful no-op");
    assert_eq!(snapshot_test_count(&rt, table_count).await, 0);
    assert_eq!(
        snapshot_test_count(&rt, "SELECT count(*) AS n FROM fts_notes").await,
        1
    );
    assert_eq!(
        snapshot_test_count(&rt, "SELECT epoch AS n FROM memory_ann_epoch").await,
        2
    );
}

mod degraded_ingest_repair_tests;
