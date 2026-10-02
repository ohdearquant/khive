use super::*;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use khive_pack_kg::KgPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, Namespace, RuntimeConfig,
    VerbRegistryBuilder,
};
use khive_storage::note::Note;
use khive_storage::types::SqlRow;
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, NamespaceToken, MemoryPack) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory SQLite runtime");
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime.notes(&token).expect("real notes schema");
    let pack = MemoryPack::new(runtime.clone());
    (runtime, token, pack)
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, notes: &[Note]) {
    let store = runtime.notes(token).unwrap();
    for chunk in notes.chunks(256) {
        store.upsert_notes(chunk.to_vec()).await.unwrap();
    }
}

async fn write(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> u64 {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: sql.to_owned(),
            params,
            label: Some("test.prune_materialization.write".into()),
        })
        .await
        .unwrap()
}

async fn query(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> Vec<SqlRow> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: sql.to_owned(),
            params,
            label: Some("test.prune_materialization.read".into()),
        })
        .await
        .unwrap()
}

async fn observe<F, T>(future: F) -> (T, PruneSelectionWork)
where
    F: std::future::Future<Output = T>,
{
    PRUNE_SELECTION_WORK
        .scope(RefCell::new(PruneSelectionWork::default()), async {
            let result = future.await;
            let work = PRUNE_SELECTION_WORK.with(|work| work.borrow().clone());
            (result, work)
        })
        .await
}

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|name| (*name).to_owned()).collect()
}

fn memory(salience: f64, expires_at: Option<i64>) -> Note {
    let mut note = Note::new("local", "memory", "prune projection contract")
        .with_salience(salience)
        .with_decay(0.02)
        .with_properties(json!({"memory_type": "semantic"}));
    note.expires_at = expires_at;
    note
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn raw_prune_materialization_excludes_effective_columns() {
    const ROWS: usize = 2048;
    const PADDING: usize = 4096;
    let (runtime, token, pack) = fixture();
    let payload = json!({"memory_type": "semantic", "padding": "x".repeat(PADDING)});
    let notes: Vec<Note> = (0..ROWS)
        .map(|_| memory(0.9, None).with_properties(payload.clone()))
        .collect();
    seed(&runtime, &token, &notes).await;
    let premise = query(
        &runtime,
        "SELECT count(*) AS n, min(length(properties)) AS bytes FROM notes \
         WHERE kind='memory' AND namespace='local' AND deleted_at IS NULL",
        vec![],
    )
    .await;
    assert!(matches!(premise[0].get("n"), Some(SqlValue::Integer(n)) if *n == ROWS as i64));
    assert!(matches!(premise[0].get("bytes"), Some(SqlValue::Integer(n)) if *n >= PADDING as i64));

    let params = json!({"min_salience": 0.5, "before": 0, "dry_run": true});
    let (empty, work) = observe(pack.handle_prune(&token, params.clone())).await;
    assert_eq!(empty.unwrap()["would_prune"], 0);
    assert_eq!(work.queries, 1);
    assert_eq!(
        work.rows, ROWS,
        "this scope still reads every live candidate row"
    );
    assert_eq!(work.columns, 3 * ROWS);
    assert_eq!(work.column_names, names(&["id", "salience", "expires_at"]));
    assert_eq!(work.properties_bytes, 0);
    assert_eq!(work.owned_blob_bytes, 0);
    assert_eq!(
        work.owned_text_bytes,
        36 * ROWS,
        "only canonical UUID text is copied"
    );

    for note in &notes[..3] {
        assert_eq!(
            write(
                &runtime,
                "UPDATE notes SET salience=? WHERE id=?",
                vec![SqlValue::Float(0.2), SqlValue::Text(note.id.to_string())],
            )
            .await,
            1
        );
    }
    let (selected, selected_work) = observe(pack.handle_prune(&token, params)).await;
    assert_eq!(selected.unwrap()["would_prune"], 3);
    assert_eq!(selected_work.rows, ROWS);
    assert_eq!(selected_work.columns, 3 * ROWS);
    assert_eq!(selected_work.properties_bytes, 0);

    let result = pack
        .handle_prune(&token, json!({"min_salience": 0.5, "before": 0}))
        .await
        .unwrap();
    assert_eq!(result["pruned"], 3);
    let store = runtime.notes(&token).unwrap();
    for note in &notes[..3] {
        let deleted = store
            .get_note_including_deleted(note.id)
            .await
            .unwrap()
            .unwrap();
        assert!(deleted.deleted_at.is_some());
        assert_eq!(deleted.content, note.content);
        assert_eq!(
            deleted.properties, note.properties,
            "soft deletion retains the actual payload"
        );
    }
    assert_eq!(
        store
            .get_note(notes[3].id)
            .await
            .unwrap()
            .unwrap()
            .properties,
        Some(payload)
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prune_retains_strict_raw_inclusive_expiry_union_and_disabled_selection() {
    let (runtime, token, pack) = fixture();
    let notes = [
        memory(0.2, None),
        memory(0.9, Some(500)),
        memory(0.2, Some(500)),
        memory(0.5, Some(501)),
        memory(0.9, Some(501)),
    ];
    seed(&runtime, &token, &notes).await;
    for (params, expected) in [
        (
            json!({"min_salience": 0.5, "before": 500, "dry_run": true}),
            3,
        ),
        (
            json!({"min_salience": 0.5, "before": 0, "dry_run": true}),
            2,
        ),
        (json!({"before": 500, "dry_run": true}), 2),
        (json!({"before": 499, "dry_run": true}), 0),
        (json!({"before": 0, "dry_run": true}), 0),
    ] {
        let (result, work) = observe(pack.handle_prune(&token, params)).await;
        assert_eq!(result.unwrap()["would_prune"], expected);
        assert_eq!(
            work.queries, 1,
            "disabled criteria still retain query admission"
        );
        assert_eq!(work.rows, notes.len());
    }
    for note in notes {
        assert!(runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .is_some());
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn raw_prune_retains_legacy_defaults_and_sqlite_affinity() {
    let (runtime, token, pack) = fixture();
    let notes: Vec<Note> = (0..5).map(|_| memory(0.5, None)).collect();
    seed(&runtime, &token, &notes).await;
    let values = [
        SqlValue::Null,
        SqlValue::Text("legacy salience".into()),
        SqlValue::Blob(vec![0x30, 0x2e, 0x30, 0x35]),
        SqlValue::Text("0.05".into()),
        SqlValue::Integer(1),
    ];
    for (note, value) in notes.iter().zip(values) {
        assert_eq!(
            write(
                &runtime,
                "UPDATE notes SET salience=? WHERE id=?",
                vec![value, SqlValue::Text(note.id.to_string())]
            )
            .await,
            1
        );
    }
    for (note, expected_type) in notes.iter().zip(["null", "text", "blob", "real", "real"]) {
        let rows = query(
            &runtime,
            "SELECT typeof(salience) AS t FROM notes WHERE id=?",
            vec![SqlValue::Text(note.id.to_string())],
        )
        .await;
        assert!(matches!(rows[0].get("t"), Some(SqlValue::Text(t)) if t == expected_type));
    }
    let result = pack
        .handle_prune(
            &token,
            json!({"min_salience": 0.1, "before": 0, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        result["would_prune"], 4,
        "NULL/TEXT/BLOB default0 plus REAL0.05; REAL1 spared"
    );
    let result = pack
        .handle_prune(
            &token,
            json!({"min_salience": 0.0, "before": 0, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        result["would_prune"], 0,
        "default0 retains strict comparison"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn expiry_prune_ignores_noninteger_stored_values() {
    let (runtime, token, pack) = fixture();
    let notes: Vec<Note> = (0..6).map(|_| memory(0.9, None)).collect();
    seed(&runtime, &token, &notes).await;
    let values = [
        SqlValue::Integer(500),
        SqlValue::Float(499.5),
        SqlValue::Text("legacy expiry".into()),
        SqlValue::Blob(vec![0x31]),
        SqlValue::Null,
        SqlValue::Text("500".into()),
    ];
    for (note, value) in notes.iter().zip(values) {
        assert_eq!(
            write(
                &runtime,
                "UPDATE notes SET expires_at=? WHERE id=?",
                vec![value, SqlValue::Text(note.id.to_string())]
            )
            .await,
            1
        );
    }
    for (note, expected_type) in notes
        .iter()
        .zip(["integer", "real", "text", "blob", "null", "integer"])
    {
        let rows = query(
            &runtime,
            "SELECT typeof(expires_at) AS t FROM notes WHERE id=?",
            vec![SqlValue::Text(note.id.to_string())],
        )
        .await;
        assert!(matches!(rows[0].get("t"), Some(SqlValue::Text(t)) if t == expected_type));
    }
    let result = pack
        .handle_prune(&token, json!({"before": 500, "dry_run": true}))
        .await
        .unwrap();
    assert_eq!(
        result["would_prune"], 2,
        "INTEGER affinity accepts integral numeric text; REAL499.5 is not coerced by prune"
    );
    let result = pack
        .handle_prune(&token, json!({"before": 499, "dry_run": true}))
        .await
        .unwrap();
    assert_eq!(result["would_prune"], 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prune_retains_uuid_occurrences_and_live_kind_namespace_scope() {
    let (runtime, token, pack) = fixture();
    let id: Uuid = "abcdefab-cdef-4abc-8def-abcdefabcdef".parse().unwrap();
    let mut canonical = memory(0.2, None);
    canonical.id = id;
    seed(&runtime, &token, std::slice::from_ref(&canonical)).await;
    for stored_id in [
        id.to_string().to_ascii_uppercase(),
        id.simple().to_string(),
        "not-a-uuid".into(),
    ] {
        assert_eq!(write(
            &runtime,
            "INSERT INTO notes (id,namespace,kind,status,content,salience,properties,created_at,updated_at) \
             VALUES (?,'local','memory','active','historical UUID fixture',0.2,'{}',1,1)",
            vec![SqlValue::Text(stored_id)],
        ).await, 1);
    }
    let mut foreign = memory(0.2, None);
    foreign.namespace = "other".into();
    let mut other_kind = memory(0.2, None);
    other_kind.kind = "observation".into();
    let mut tombstone = memory(0.2, None);
    tombstone.deleted_at = Some(1);
    seed(
        &runtime,
        &token,
        &[foreign.clone(), other_kind.clone(), tombstone],
    )
    .await;
    let premise = query(
        &runtime,
        "SELECT id FROM notes WHERE namespace='local' AND kind='memory' AND deleted_at IS NULL",
        vec![],
    )
    .await;
    assert_eq!(premise.len(), 4);
    let parsed: Vec<Uuid> = premise
        .iter()
        .filter_map(|row| match row.get("id") {
            Some(SqlValue::Text(s)) => s.parse().ok(),
            _ => None,
        })
        .collect();
    assert_eq!(parsed.len(), 3);
    assert!(parsed.iter().all(|parsed| *parsed == id));
    let dry = pack
        .handle_prune(
            &token,
            json!({"min_salience": 0.5, "before": 0, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        dry["would_prune"], 3,
        "accepted UUID occurrences are not deduplicated or COUNT(*)"
    );
    let real = pack
        .handle_prune(&token, json!({"min_salience": 0.5, "before": 0}))
        .await
        .unwrap();
    assert_eq!(
        real["pruned"], 1,
        "fresh canonical by-ID lookup sees one live note then tolerates misses"
    );
    let store = runtime.notes(&token).unwrap();
    assert!(store
        .get_note_including_deleted(id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert!(store.get_note(foreign.id).await.unwrap().is_some());
    assert!(store.get_note(other_kind.id).await.unwrap().is_some());
    let remaining = query(
        &runtime,
        "SELECT id FROM notes WHERE namespace='local' AND kind='memory' AND deleted_at IS NULL",
        vec![],
    )
    .await;
    assert_eq!(
        remaining.len(),
        3,
        "noncanonical text and malformed ID remain live"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prune_preserves_later_full_note_decode_failure() {
    let (runtime, token, pack) = fixture();
    let note = memory(0.9, None);
    seed(&runtime, &token, std::slice::from_ref(&note)).await;
    assert_eq!(
        write(
            &runtime,
            "UPDATE notes SET salience=? WHERE id=?",
            vec![
                SqlValue::Text("legacy salience".into()),
                SqlValue::Text(note.id.to_string())
            ]
        )
        .await,
        1
    );
    let premise = query(
        &runtime,
        "SELECT typeof(salience) AS t, deleted_at FROM notes WHERE id=?",
        vec![SqlValue::Text(note.id.to_string())],
    )
    .await;
    assert!(matches!(premise[0].get("t"), Some(SqlValue::Text(t)) if t == "text"));
    let unselected = pack
        .handle_prune(
            &token,
            json!({"min_salience": 0.0, "before": 0, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(unselected["would_prune"], 0);
    let selected = pack
        .handle_prune(
            &token,
            json!({"min_salience": 0.1, "before": 0, "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        selected["would_prune"], 1,
        "initial SqlRow selection does not hydrate a Note"
    );
    let error = pack
        .handle_prune(&token, json!({"min_salience": 0.1, "before": 0}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Storage(_)),
        "later real note decode fails: {error:?}"
    );
    let error = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("get_note"),
        "same full Note read refuses: {error}"
    );
    let row = query(
        &runtime,
        "SELECT deleted_at, properties FROM notes WHERE id=?",
        vec![SqlValue::Text(note.id.to_string())],
    )
    .await;
    assert!(matches!(row[0].get("deleted_at"), Some(SqlValue::Null)));
    assert!(
        matches!(row[0].get("properties"), Some(SqlValue::Text(text)) if Some(&serde_json::from_str::<Value>(text).unwrap()) == note.properties.as_ref())
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn effective_prune_retains_full_projection_and_decay_selection() {
    let (runtime, token, pack) = fixture();
    let mut note = memory(0.6, None)
        .with_properties(json!({"memory_type": "semantic", "padding": "y".repeat(4096)}));
    note.created_at = chrono::Utc::now().timestamp_micros() - 150 * 86_400 * 1_000_000;
    seed(&runtime, &token, std::slice::from_ref(&note)).await;
    let (raw, raw_work) = observe(pack.handle_prune(
        &token,
        json!({"min_salience": 0.3, "before": 0, "dry_run": true}),
    ))
    .await;
    assert_eq!(raw.unwrap()["would_prune"], 0);
    assert_eq!(raw_work.rows, 1);
    let (effective, work) = observe(pack.handle_prune(
        &token,
        json!({"min_effective_salience": 0.05, "before": 0, "dry_run": true}),
    ))
    .await;
    assert_eq!(
        effective.unwrap()["would_prune"],
        1,
        "old semantic row selected only through effective decay"
    );
    assert_eq!(work.queries, 1);
    assert_eq!(work.rows, 1);
    assert_eq!(work.columns, 6);
    assert_eq!(
        work.column_names,
        names(&[
            "id",
            "salience",
            "decay_factor",
            "created_at",
            "expires_at",
            "properties"
        ])
    );
    assert!(
        work.properties_bytes >= 4096,
        "actual effective SqlRow still carries full properties"
    );
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn disabled_prune_retains_query_and_schema_failure_boundary() {
    let (runtime, token, pack) = fixture();
    seed(&runtime, &token, &[memory(0.9, None)]).await;
    let params = json!({"before": 0, "dry_run": true});
    let (healthy, work) = observe(pack.handle_prune(&token, params.clone())).await;
    assert_eq!(healthy.unwrap()["would_prune"], 0);
    let healthy_work = work;
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("ALTER TABLE notes RENAME TO prune_missing_notes;".into())
        .await
        .unwrap();
    let (missing, work) = observe(pack.handle_prune(&token, params)).await;
    assert!(
        matches!(missing, Err(RuntimeError::Storage(_))),
        "real candidate statement must refuse missing notes table: {missing:?}"
    );
    assert_eq!(
        work.queries, 0,
        "observer never sees a fabricated row after query failure"
    );
    assert_eq!(healthy_work.queries, 1);
    assert_eq!(healthy_work.rows, 1);
}

#[derive(Debug, Default)]
struct PruneGate(Mutex<Vec<(String, String)>>);

impl Gate for PruneGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        self.0
            .lock()
            .unwrap()
            .push((request.verb.clone(), request.namespace.to_string()));
        Ok(if request.namespace.as_str() == "blocked" {
            GateDecision::deny("blocked prune namespace")
        } else {
            GateDecision::allow()
        })
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn public_prune_retains_exact_namespace_and_gate_denial() {
    let (runtime, token, _) = fixture();
    let local = memory(0.2, None);
    let mut foreign = memory(0.2, None);
    foreign.namespace = "other".into();
    let mut blocked = memory(0.2, None);
    blocked.namespace = "blocked".into();
    seed(&runtime, &token, &[local, foreign, blocked.clone()]).await;
    let gate = Arc::new(PruneGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate.clone());
    builder.with_visible_namespaces(vec![Namespace::parse("other").unwrap()]);
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    for params in [
        json!({"min_salience": 0.5, "before": 0, "dry_run": true}),
        json!({"namespace": "other", "min_salience": 0.5, "before": 0, "dry_run": true}),
    ] {
        let (result, work) = observe(registry.dispatch("memory.prune", params)).await;
        assert_eq!(
            result.unwrap()["would_prune"],
            1,
            "extra-visible namespace never broadens prune's exact filter"
        );
        assert_eq!(work.rows, 1);
    }
    let (denied, work) = observe(registry.dispatch(
        "memory.prune",
        json!({"namespace": "blocked", "min_salience": 0.5, "before": 0}),
    ))
    .await;
    assert!(
        matches!(denied, Err(RuntimeError::PermissionDenied { .. })),
        "real public dispatch gate refusal: {denied:?}"
    );
    assert_eq!(work.queries, 0);
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(blocked.id)
        .await
        .unwrap()
        .is_some());
    let checks = gate.0.lock().unwrap();
    assert_eq!(
        checks.as_slice(),
        &[
            ("memory.prune".to_owned(), "local".to_owned()),
            ("memory.prune".to_owned(), "other".to_owned()),
            ("memory.prune".to_owned(), "blocked".to_owned())
        ]
    );
}
