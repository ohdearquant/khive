use super::*;
use crate::pool::PoolConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::sync::atomic::{AtomicBool, Ordering};

fn deny_commit(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Unknown,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

pub(super) mod page_snapshot_seam {
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::Mutex;

    struct Barrier {
        operation: &'static str,
        namespace: String,
        reached_tx: SyncSender<()>,
        proceed_rx: Receiver<()>,
    }

    static BARRIER: Mutex<Option<Barrier>> = Mutex::new(None);

    pub(crate) fn install(
        operation: &'static str,
        namespace: String,
    ) -> (Receiver<()>, SyncSender<()>) {
        let (reached_tx, reached_rx) = sync_channel(0);
        let (proceed_tx, proceed_rx) = sync_channel(0);
        *BARRIER.lock().unwrap() = Some(Barrier {
            operation,
            namespace,
            reached_tx,
            proceed_rx,
        });
        (reached_rx, proceed_tx)
    }

    pub(crate) fn uninstall() {
        *BARRIER.lock().unwrap() = None;
    }

    pub(crate) fn hook(operation: &'static str, namespace: &str) {
        let barrier = {
            let mut guard = BARRIER.lock().unwrap();
            match guard.as_ref() {
                Some(barrier)
                    if barrier.operation == operation && barrier.namespace == namespace =>
                {
                    guard.take()
                }
                _ => None,
            }
        };
        let Some(barrier) = barrier else {
            return;
        };
        let _ = barrier.reached_tx.send(());
        let _ = barrier.proceed_rx.recv();
    }
}

fn setup_pool() -> Arc<ConnectionPool> {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }
    pool
}

fn setup_memory_store() -> SqlNoteStore {
    SqlNoteStore::new(setup_pool(), false)
}

fn make_note(namespace: &str, kind: &str, content: &str) -> Note {
    Note::new(namespace, kind, content)
}

#[tokio::test]
async fn test_upsert_and_get_note() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "Hello world");
    let id = note.id;

    store.upsert_note(note).await.unwrap();

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.content, "Hello world");
    assert_eq!(fetched.kind, "observation");
}

#[tokio::test]
async fn replace_note_cas_requires_new_revision_strictly_greater_than_snapshot() {
    let store = setup_memory_store();
    let mut original = make_note("default", "observation", "original");
    original.created_at = 100;
    original.updated_at = 100;
    let id = original.id;
    store.upsert_note(original.clone()).await.unwrap();

    for refused_revision in [99, 100] {
        let mut replacement = original.clone();
        replacement.content = format!("must-not-land-{refused_revision}");
        replacement.updated_at = refused_revision;
        assert!(
            !store
                .replace_note_if_unchanged(replacement, original.updated_at, original.deleted_at)
                .await
                .unwrap(),
            "CAS must refuse replacement revision {refused_revision} when the snapshot revision is {}",
            original.updated_at
        );
        assert_eq!(
            store.get_note(id).await.unwrap().unwrap().content,
            "original"
        );
    }

    let mut advanced = original.clone();
    advanced.content = "advanced".to_string();
    advanced.updated_at = 101;
    assert!(
        store
            .replace_note_if_unchanged(advanced, original.updated_at, original.deleted_at)
            .await
            .unwrap(),
        "a strictly newer revision must still satisfy the CAS"
    );
    let persisted = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(persisted.content, "advanced");
    assert_eq!(persisted.updated_at, 101);
}

#[tokio::test]
async fn test_kind_roundtrip_all_variants() {
    let store = setup_memory_store();
    for kind in [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ] {
        let note = make_note("default", kind, "content");
        let id = note.id;
        store.upsert_note(note).await.unwrap();
        let fetched = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(fetched.kind, kind);
    }
}

#[tokio::test]
async fn test_soft_delete() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "to be deleted");
    let id = note.id;
    store.upsert_note(note).await.unwrap();

    let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_hard_delete() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "to be hard deleted");
    let id = note.id;
    store.upsert_note(note).await.unwrap();

    let deleted = store.delete_note(id, DeleteMode::Hard).await.unwrap();
    assert!(deleted);

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_none());
}

/// Namespace isolation: one store, two namespaces — each query sees only its own.
#[tokio::test]
async fn test_namespace_isolation() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);

    for _ in 0..3 {
        store
            .upsert_note(make_note("ns1", "observation", "content"))
            .await
            .unwrap();
    }
    store
        .upsert_note(make_note("ns2", "observation", "other"))
        .await
        .unwrap();

    let count_ns1 = store.count_notes("ns1", None).await.unwrap();
    assert_eq!(count_ns1, 3);

    let count_ns2 = store.count_notes("ns2", None).await.unwrap();
    assert_eq!(count_ns2, 1);
}

#[tokio::test]
async fn batched_namespace_note_count_exceeds_sqlite_variable_limit() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);
    let live_a = make_note("stats-a", "observation", "live-a");
    let deleted_a = make_note("stats-a", "observation", "deleted-a");
    let deleted_a_id = deleted_a.id;
    let live_b = make_note("stats-b", "insight", "live-b");

    store.upsert_note(live_a).await.unwrap();
    store.upsert_note(deleted_a).await.unwrap();
    store.upsert_note(live_b).await.unwrap();
    assert!(store
        .delete_note(deleted_a_id, DeleteMode::Soft)
        .await
        .unwrap());

    let per_namespace_total = store.count_notes("stats-a", None).await.unwrap()
        + store.count_notes("stats-b", None).await.unwrap();
    pool.writer()
        .unwrap()
        .conn()
        .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 999)
        .unwrap();
    let mut namespaces = vec!["stats-a".to_string(), "stats-b".to_string()];
    namespaces.extend((0..999).map(|i| format!("empty-{i}")));
    assert_eq!(namespaces.len(), 1_001);

    assert_eq!(
        store
            .count_notes_in_namespaces(&namespaces, None)
            .await
            .unwrap(),
        per_namespace_total
    );
    assert_eq!(
        store
            .count_notes_in_namespaces(&namespaces, Some("observation"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(per_namespace_total, 2);
}

#[tokio::test]
async fn duplicate_namespace_across_chunk_boundary_is_not_double_counted() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);

    store
        .upsert_note(make_note("stats-a", "observation", "live-a-1"))
        .await
        .unwrap();
    store
        .upsert_note(make_note("stats-a", "observation", "live-a-2"))
        .await
        .unwrap();

    let per_namespace_total = store.count_notes("stats-a", None).await.unwrap();
    assert_eq!(per_namespace_total, 2);

    // 501 unique namespaces ("stats-a" + 500 empties), then "stats-a" repeats
    // once more past index 500 — the repeat lands in the second 500-entry
    // chunk, so a dedup bug double-counts "stats-a"'s rows.
    let mut namespaces = vec!["stats-a".to_string()];
    namespaces.extend((0..500).map(|i| format!("empty-{i}")));
    assert_eq!(namespaces.len(), 501);
    namespaces.push("stats-a".to_string());
    assert_eq!(namespaces.len(), 502);

    assert_eq!(
        store
            .count_notes_in_namespaces(&namespaces, None)
            .await
            .unwrap(),
        per_namespace_total
    );
}

/// query_notes and count_notes use the namespace parameter as passed.
#[tokio::test]
async fn test_query_and_count_use_caller_namespace() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);

    store
        .upsert_note(make_note("ns_a", "observation", "A"))
        .await
        .unwrap();
    store
        .upsert_note(make_note("ns_b", "insight", "B"))
        .await
        .unwrap();

    let page_a = store
        .query_notes("ns_a", None, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_a.items.len(), 1);
    assert_eq!(page_a.items[0].content, "A");
    assert_eq!(page_a.total, Some(1));

    let page_b = store
        .query_notes("ns_b", None, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_b.items.len(), 1);
    assert_eq!(page_b.items[0].content, "B");
    assert_eq!(page_b.total, Some(1));

    let count_a = store.count_notes("ns_a", None).await.unwrap();
    let count_b = store.count_notes("ns_b", None).await.unwrap();
    assert_eq!(count_a, 1);
    assert_eq!(count_b, 1);
}

#[derive(Clone, Copy, Debug)]
enum SnapshotPageQuery {
    Basic,
    Filtered,
}

impl SnapshotPageQuery {
    fn operation(self) -> &'static str {
        match self {
            Self::Basic => "query_notes",
            Self::Filtered => "query_notes_filtered",
        }
    }
}

async fn run_snapshot_page_query(
    store: &SqlNoteStore,
    query: SnapshotPageQuery,
    namespace: &str,
    filter: &NoteFilter,
) -> Result<Page<Note>, StorageError> {
    let page = PageRequest {
        offset: 0,
        limit: 10,
    };
    match query {
        SnapshotPageQuery::Basic => {
            store
                .query_notes(namespace, Some("observation"), page)
                .await
        }
        SnapshotPageQuery::Filtered => store.query_notes_filtered(namespace, filter, page).await,
    }
}

async fn assert_page_count_and_items_share_snapshot(query: SnapshotPageQuery) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("note-page-snapshot-{query:?}.db"));
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
        let journal_mode: String = writer
            .conn()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    }

    let store = Arc::new(SqlNoteStore::new(Arc::clone(&pool), true));
    let namespace = format!("snapshot-{}", Uuid::new_v4());
    let properties = serde_json::json!({"snapshot_case": true});

    let mut initial = make_note_with_props(
        &namespace,
        "observation",
        "present when count starts",
        properties.clone(),
    );
    initial.created_at = 1;
    let initial_id = initial.id;
    store.upsert_note(initial).await.unwrap();

    let filter = NoteFilter {
        kind: Some("observation".to_string()),
        property_filters: vec![khive_storage::note::PropertyFilter {
            json_path: "$.snapshot_case".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Bool(true),
        }],
        ..NoteFilter::default()
    };

    let (reached_rx, proceed_tx) =
        page_snapshot_seam::install(query.operation(), namespace.clone());
    let query_task = {
        let store = Arc::clone(&store);
        let namespace = namespace.clone();
        let filter = filter.clone();
        tokio::spawn(
            async move { run_snapshot_page_query(&store, query, &namespace, &filter).await },
        )
    };

    tokio::task::spawn_blocking(move || reached_rx.recv_timeout(std::time::Duration::from_secs(5)))
        .await
        .expect("waiting for the count-to-page seam must not panic")
        .expect("query must reach the seam after reading its count");

    let mut concurrent = make_note_with_props(
        &namespace,
        "observation",
        "committed between count and page",
        properties,
    );
    concurrent.created_at = 2;
    let concurrent_id = concurrent.id;
    store
        .upsert_note(concurrent)
        .await
        .expect("WAL writer must commit while the page reader is parked");
    assert!(
        store.get_note(concurrent_id).await.unwrap().is_some(),
        "a new reader must observe the committed row before the page reader resumes"
    );
    assert!(
        !query_task.is_finished(),
        "page query must remain parked until the test releases its production seam"
    );

    proceed_tx
        .send(())
        .expect("page query must still be waiting at the production seam");
    let page = query_task
        .await
        .expect("page query task must not panic")
        .expect("page query must succeed");
    page_snapshot_seam::uninstall();

    assert_eq!(page.total, Some(1));
    assert_eq!(
        page.items.iter().map(|note| note.id).collect::<Vec<_>>(),
        vec![initial_id]
    );

    let after = run_snapshot_page_query(&store, query, &namespace, &filter)
        .await
        .unwrap();
    assert_eq!(after.total, Some(2));
    assert!(after.items.iter().any(|note| note.id == concurrent_id));
}

#[tokio::test]
async fn note_page_count_and_items_share_one_snapshot_during_concurrent_insert() {
    for query in [SnapshotPageQuery::Basic, SnapshotPageQuery::Filtered] {
        assert_page_count_and_items_share_snapshot(query).await;
    }
}

#[tokio::test]
async fn test_soft_delete_sets_status_deleted() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);
    let note = make_note("default", "observation", "to delete");
    let id = note.id;
    store.upsert_note(note).await.unwrap();
    let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);
    // Verify directly via raw SQL
    let writer = pool.writer().unwrap();
    let status: String = writer
        .conn()
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "deleted");
}

#[tokio::test]
async fn test_note_status_field_roundtrip() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "status test");
    let id = note.id;
    store.upsert_note(note).await.unwrap();
    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(fetched.status, "active");
}

#[tokio::test]
async fn set_note_property_initializes_null_and_preserves_json_type() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "atomic property set");
    let id = note.id;
    let updated_at = note.updated_at + 1;
    store.upsert_note(note).await.unwrap();

    assert!(store
        .set_note_property(
            id,
            "delivery.stamp",
            serde_json::json!({ "channel": "email", "attempt": 1 }),
            updated_at,
        )
        .await
        .unwrap());
    assert!(store
        .set_note_property(id, "explicit_null", serde_json::Value::Null, updated_at + 1)
        .await
        .unwrap());

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.properties,
        Some(serde_json::json!({
            "delivery.stamp": { "channel": "email", "attempt": 1 },
            "explicit_null": null
        })),
        "keys must be literal top-level segments and values must keep their JSON types"
    );
    assert_eq!(fetched.updated_at, updated_at + 1);
}

#[tokio::test]
async fn concurrent_distinct_note_property_sets_both_survive() {
    let store = Arc::new(setup_memory_store());
    let note = make_note("default", "message", "concurrent atomic properties")
        .with_properties(serde_json::json!({ "existing": "preserved" }));
    let id = note.id;
    let updated_at = note.updated_at;
    store.upsert_note(note).await.unwrap();

    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let left = {
        let store = Arc::clone(&store);
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            store
                .set_note_property(
                    id,
                    "delivery_stamp",
                    serde_json::json!("email"),
                    updated_at + 1,
                )
                .await
        })
    };
    let right = {
        let store = Arc::clone(&store);
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            store
                .set_note_property(id, "ingest_marker", serde_json::json!(true), updated_at + 2)
                .await
        })
    };

    gate.wait().await;
    assert!(left.await.unwrap().unwrap());
    assert!(right.await.unwrap().unwrap());

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.properties,
        Some(serde_json::json!({
            "existing": "preserved",
            "delivery_stamp": "email",
            "ingest_marker": true
        })),
        "one-statement property sets must not lose a different concurrent key"
    );
}

#[tokio::test]
async fn set_note_property_refuses_non_object_document() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "scalar properties")
        .with_properties(serde_json::json!(["not", "an", "object"]));
    let id = note.id;
    let original_updated_at = note.updated_at;
    store.upsert_note(note).await.unwrap();

    assert!(!store
        .set_note_property(id, "read", serde_json::json!(true), original_updated_at + 1)
        .await
        .unwrap());
    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.properties,
        Some(serde_json::json!(["not", "an", "object"]))
    );
    assert_eq!(fetched.updated_at, original_updated_at);
}

#[tokio::test]
async fn try_patch_note_property_refuses_scalar_document() {
    use khive_storage::note::NoteFilter;

    let store = setup_memory_store();
    let note =
        make_note("default", "message", "scalar properties").with_properties(serde_json::json!(1));
    let id = note.id;
    let original_updated_at = note.updated_at;
    store.upsert_note(note).await.unwrap();

    let matched = store
        .try_patch_note_property(
            id,
            "default",
            &NoteFilter::default(),
            "$.read",
            serde_json::json!(true),
            original_updated_at + 1,
        )
        .await
        .unwrap();
    assert!(!matched, "a scalar properties document must not be patched");

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(fetched.properties, Some(serde_json::json!(1)));
    assert_eq!(fetched.updated_at, original_updated_at);
}

#[tokio::test]
async fn try_patch_note_property_refuses_array_document() {
    use khive_storage::note::NoteFilter;

    let store = setup_memory_store();
    let note = make_note("default", "message", "array properties")
        .with_properties(serde_json::json!(["not", "an", "object"]));
    let id = note.id;
    let original_updated_at = note.updated_at;
    store.upsert_note(note).await.unwrap();

    let matched = store
        .try_patch_note_property(
            id,
            "default",
            &NoteFilter::default(),
            "$.read",
            serde_json::json!(true),
            original_updated_at + 1,
        )
        .await
        .unwrap();
    assert!(!matched, "an array properties document must not be patched");

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.properties,
        Some(serde_json::json!(["not", "an", "object"]))
    );
    assert_eq!(fetched.updated_at, original_updated_at);
}

fn atomic_mark_read_filter() -> khive_storage::note::NoteFilter {
    use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
    use khive_storage::types::SqlValue;

    NoteFilter {
        kind: Some("message".to_string()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".to_string(),
                op: FilterOp::NotInOrMissing(vec![SqlValue::Text("outbound".to_string())]),
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op: FilterOp::EqOrMissing,
                value: SqlValue::Text("lambda:reader".to_string()),
            },
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn atomic_note_property_patch_rolls_back_when_one_target_is_ineligible() {
    let store = setup_memory_store();
    let eligible = make_note("local", "message", "eligible").with_properties(serde_json::json!({
        "direction": "inbound",
        "to_actor": "lambda:reader",
        "read": false,
        "preserve": "eligible",
    }));
    let ineligible =
        make_note("local", "message", "ineligible").with_properties(serde_json::json!({
            "direction": "outbound",
            "to_actor": "lambda:reader",
            "read": false,
            "preserve": "ineligible",
        }));
    let eligible_id = eligible.id;
    let ineligible_id = ineligible.id;
    let updated_at = eligible.updated_at.max(ineligible.updated_at) + 1;
    store.upsert_note(eligible).await.unwrap();
    store.upsert_note(ineligible).await.unwrap();

    let filter = atomic_mark_read_filter();

    let error = store
        .patch_note_property_atomic(
            vec![eligible_id, ineligible_id],
            "local",
            &filter,
            "$.read",
            serde_json::json!(true),
            updated_at,
        )
        .await
        .expect_err("an ineligible target must abort the atomic patch");
    assert!(
        matches!(&error, StorageError::Conflict { message, .. }
            if message.contains(&ineligible_id.to_string())),
        "the conflict must name the first failing id {ineligible_id}; got {error:?}"
    );
    assert!(!error.is_retryable(), "a precondition conflict is terminal");

    for (id, preserved) in [(eligible_id, "eligible"), (ineligible_id, "ineligible")] {
        let stored = store.get_note(id).await.unwrap().unwrap();
        let properties = stored.properties.unwrap();
        assert_eq!(properties["read"], false);
        assert_eq!(properties["preserve"], preserved);
    }

    store
        .patch_note_property_atomic(
            vec![eligible_id, eligible_id],
            "local",
            &filter,
            "$.read",
            serde_json::json!(true),
            updated_at,
        )
        .await
        .expect("deduplicated eligible targets commit together");
    let stored = store.get_note(eligible_id).await.unwrap().unwrap();
    let properties = stored.properties.unwrap();
    assert_eq!(properties["read"], true);
    assert_eq!(properties["preserve"], "eligible");
}

#[tokio::test]
async fn atomic_note_property_patch_writer_task_commits_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("atomic-note-property-writer-task.db")),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }
    let store = SqlNoteStore::new(Arc::clone(&pool), true);
    let eligible_properties = serde_json::json!({
        "direction": "inbound",
        "to_actor": "lambda:reader",
        "read": false,
    });
    let first = make_note("local", "message", "first eligible")
        .with_properties(eligible_properties.clone());
    let second = make_note("local", "message", "second eligible")
        .with_properties(eligible_properties.clone());
    let ineligible =
        make_note("local", "message", "ineligible").with_properties(serde_json::json!({
            "direction": "outbound",
            "to_actor": "lambda:reader",
            "read": false,
        }));
    let first_id = first.id;
    let second_id = second.id;
    let ineligible_id = ineligible.id;
    let updated_at = first
        .updated_at
        .max(second.updated_at)
        .max(ineligible.updated_at)
        + 1;
    for note in [first, second, ineligible] {
        store.upsert_note(note).await.unwrap();
    }

    let filter = atomic_mark_read_filter();
    store
        .patch_note_property_atomic(
            vec![first_id, second_id],
            "local",
            &filter,
            "$.read",
            serde_json::json!(true),
            updated_at,
        )
        .await
        .expect("all eligible rows commit through the writer task");
    for id in [first_id, second_id] {
        let stored = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(stored.properties.unwrap()["read"], true);
    }

    store
        .update_note_properties(first_id, Some(eligible_properties), updated_at + 1)
        .await
        .unwrap();
    let error = store
        .patch_note_property_atomic(
            vec![first_id, ineligible_id],
            "local",
            &filter,
            "$.read",
            serde_json::json!(true),
            updated_at + 2,
        )
        .await
        .expect_err("a later ineligible row must abort the writer-task transaction");
    assert!(
        matches!(&error, StorageError::Conflict { message, .. }
            if message.contains(&ineligible_id.to_string())),
        "the conflict must name the first failing id {ineligible_id}; got {error:?}"
    );
    assert_eq!(
        store
            .get_note(first_id)
            .await
            .unwrap()
            .unwrap()
            .properties
            .unwrap()["read"],
        false,
        "the earlier eligible update must roll back"
    );
    assert_eq!(pool.writer_task_spawn_count(), 1);
}

#[tokio::test]
async fn set_note_property_rejects_nul_key_without_mutation() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "nul property key").with_properties(
        serde_json::json!({
            "a": 0,
            "a\u{0000}b": 9,
        }),
    );
    let id = note.id;
    let original_updated_at = note.updated_at;
    let original_properties = note.properties.clone();
    store.upsert_note(note).await.unwrap();

    let result = store
        .set_note_property(
            id,
            "a\u{0000}b",
            serde_json::json!(1),
            original_updated_at + 1,
        )
        .await;
    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(fetched.properties, original_properties);
    assert_eq!(fetched.updated_at, original_updated_at);
}

// -- query_notes_filtered tests --

fn make_note_with_props(
    namespace: &str,
    kind: &str,
    content: &str,
    props: serde_json::Value,
) -> Note {
    Note::new(namespace, kind, content).with_properties(props)
}

#[tokio::test]
async fn test_filtered_namespace_and_kind_isolation() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    // Insert "scheduled_event" with status=pending in ns1
    let n1 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "event1",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
    );
    let n2 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "event2",
        serde_json::json!({"status": "done", "trigger_at": "2027-01-02T00:00:00Z"}),
    );
    let n3 = make_note_with_props(
        "ns2",
        "scheduled_event",
        "event3",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
    );
    store.upsert_note(n1).await.unwrap();
    store.upsert_note(n2).await.unwrap();
    store.upsert_note(n3).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "only the pending ns1 event should appear"
    );
    assert_eq!(page.items[0].content, "event1");
    assert_eq!(page.total, Some(1));
}

#[tokio::test]
async fn test_filtered_order_by_json_path_asc() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter, SortDir};
    use khive_storage::types::{PageRequest, SqlValue};

    // Insert in reverse order — filter should return ascending by trigger_at.
    let n3 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "third",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
    );
    let n1 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "first",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
    );
    let n2 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "second",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-02T00:00:00Z"}),
    );
    store.upsert_note(n3).await.unwrap();
    store.upsert_note(n1).await.unwrap();
    store.upsert_note(n2).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items[0].content, "first");
    assert_eq!(page.items[1].content, "second");
    assert_eq!(page.items[2].content, "third");
}

#[tokio::test]
async fn filtered_default_order_is_stable_across_equal_timestamp_pages() {
    let store = setup_memory_store();
    let created_at = 1_750_000_000_000_000_i64;
    let mut expected_ids = Vec::new();

    for index in 0..317 {
        let mut note = make_note("ns1", "observation", &format!("note-{index}"));
        note.created_at = created_at;
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable();

    let mut actual_ids = Vec::new();
    let page_size = 29_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes_filtered(
                "ns1",
                &NoteFilter::default(),
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
}

/// #1671: `query_notes` (the single-namespace `list` path) had no `id`
/// tiebreak at all. Rows sharing `created_at` swept via offset pages must come
/// back exactly once — no duplicates, no misses across page boundaries.
#[tokio::test]
async fn query_notes_offset_sweep_covers_equal_created_at_exactly_once() {
    let store = setup_memory_store();
    let created_at = 1_750_000_000_000_000_i64;
    let mut expected_ids = Vec::new();

    for index in 0..211 {
        let mut note = make_note("ns1", "observation", &format!("note-{index}"));
        note.created_at = created_at;
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable();

    let mut actual_ids = Vec::new();
    let page_size = 37_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes(
                "ns1",
                None,
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
}

/// #1671: a custom JSON `order_by` whose extracted value repeats must also get
/// an `id` tiebreak in the primary key's direction, or offset pages over equal
/// values are unsound.
#[tokio::test]
async fn query_notes_filtered_custom_order_offset_sweep_is_total() {
    let store = setup_memory_store();
    let mut expected_ids = Vec::new();

    for index in 0..113 {
        let note = make_note_with_props(
            "ns1",
            "observation",
            &format!("note-{index}"),
            serde_json::json!({"rank": 7}),
        );
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable_by(|a, b| b.cmp(a));

    let filter = NoteFilter {
        order_by: Some(("$.rank".to_string(), SortDir::Desc)),
        ..Default::default()
    };
    let mut actual_ids = Vec::new();
    let page_size = 23_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes_filtered(
                "ns1",
                &filter,
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
}

#[tokio::test]
async fn test_filtered_soft_deleted_excluded() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{DeleteMode, PageRequest, SqlValue};

    let n = make_note_with_props(
        "ns1",
        "scheduled_event",
        "to_delete",
        serde_json::json!({"status": "pending"}),
    );
    let id = n.id;
    store.upsert_note(n).await.unwrap();
    store.delete_note(id, DeleteMode::Soft).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 0, "soft-deleted rows must not appear");
}

#[tokio::test]
async fn test_filtered_invalid_json_path_rejected() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    let filter = NoteFilter {
        kind: None,
        property_filters: vec![NotePropFilter {
            json_path: "DROP TABLE notes".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("x".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let result = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await;
    assert!(
        result.is_err(),
        "invalid json_path must be rejected before SQL"
    );
}

// ── try_insert_note dedup-vs-error discrimination ───────────────────────────

/// A PRIMARY KEY collision on a note without external_id must surface as a
/// StorageError, not be silently misreported as a dedup hit.
#[tokio::test]
async fn test_try_insert_note_pk_collision_returns_error_not_dedup() {
    let store = setup_memory_store();

    let mut note = make_note("ns1", "message", "original content");
    let fixed_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000099").unwrap();
    note.id = fixed_id;
    // No external_id on this note.

    let inserted = store
        .try_insert_note(note.clone())
        .await
        .expect("first insert must succeed");
    assert!(inserted, "first insert must return true");

    // Second attempt with the same UUID triggers a PK collision.
    // With no external_id to verify against, this must not be reported as dedup.
    let result = store.try_insert_note(note).await;
    assert!(
        result.is_err(),
        "PK collision without external_id must return StorageError, not Ok(false)"
    );
}

// ── #827: single-note insert + notes_seq assignment atomicity ────────────

/// Regression for #827: on the default flag-off (pool-mutex) path,
/// `upsert_note` used to issue its INSERT into `notes` and `assign_note_seq`
/// as two separate autocommit statements, so a crash or interleaving
/// between them could strand a note with no `notes_seq` row -- permanently
/// invisible to `comm.probe`'s `INNER JOIN notes_seq`. A `BEFORE INSERT`
/// trigger on `notes_seq` injects a failure for one specific note id,
/// simulating exactly that crash point (the `notes` INSERT has already run
/// by the time this fires); the note row must not survive either, proving
/// both statements now land in one transaction.
#[tokio::test]
async fn test_upsert_note_insert_and_seq_assignment_are_atomic() {
    let store = setup_memory_store();

    let fail_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000aa").unwrap();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER inject_seq_failure_upsert BEFORE INSERT ON notes_seq \
                 WHEN NEW.note_id = '{fail_id}' \
                 BEGIN SELECT RAISE(ABORT, 'injected failure for #827 atomicity test'); END;"
            ))
            .unwrap();
    }

    let mut note = make_note("ns1", "message", "atomic test upsert_note");
    note.id = fail_id;

    let result = store.upsert_note(note).await;
    assert!(
        result.is_err(),
        "the injected notes_seq trigger failure must surface as an error"
    );

    let fetched = store.get_note(fail_id).await.unwrap();
    assert!(
        fetched.is_none(),
        "the note insert must roll back together with the failed sequence \
         assignment, not strand the note without a notes_seq row: {fetched:?}"
    );
}

/// ADR-116 prerequisite: `upsert_note` must be a true UPSERT (`INSERT ...
/// ON CONFLICT(id) DO UPDATE`), not `INSERT OR REPLACE` — the latter is a
/// SQLite DELETE-then-INSERT that would spuriously fire ANN-generation
/// DELETE triggers and discard the row's original `created_at`. This
/// installs a stand-in DELETE trigger (the shape ADR-116 lands on `notes`)
/// to prove upserting an existing row never fires it, while a real delete
/// still does — confirming the probe itself is live.
#[tokio::test]
async fn test_upsert_note_is_true_upsert_no_delete_semantics() {
    let store = setup_memory_store();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(
                // `recursive_triggers` defaults OFF, under which SQLite does
                // NOT fire AFTER DELETE triggers for the delete half of an
                // `INSERT OR REPLACE` conflict resolution — so without this
                // pragma the probe below would read 0 even on the old
                // INSERT-OR-REPLACE path, and the test would prove nothing.
                "PRAGMA recursive_triggers = ON;
                 CREATE TABLE delete_fires (n INTEGER);
                 CREATE TRIGGER notes_delete_probe AFTER DELETE ON notes \
                 BEGIN INSERT INTO delete_fires VALUES (1); END;",
            )
            .unwrap();
    }

    let mut note = make_note("default", "observation", "v1");
    let id = note.id;
    let original_created_at = note.created_at;

    store.upsert_note(note.clone()).await.unwrap();

    // Re-upsert the same id with mutated fields and a later created_at, as a
    // careless caller might pass — the store must still preserve the original.
    note.content = "v2".to_string();
    note.salience = Some(0.9);
    note.updated_at += 1_000;
    note.created_at += 1_000;
    store.upsert_note(note).await.unwrap();

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.content, "v2",
        "mutable fields must reflect the second upsert"
    );
    assert_eq!(fetched.salience, Some(0.9));
    assert_eq!(
        fetched.created_at, original_created_at,
        "created_at must be preserved across an upsert of an existing row"
    );

    let (row_count, delete_fires): (i64, i64) = {
        let writer = store.pool.try_writer().unwrap();
        let conn = writer.conn();
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE id = ?1",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let delete_fires: i64 = conn
            .query_row("SELECT COUNT(*) FROM delete_fires", [], |row| row.get(0))
            .unwrap();
        (row_count, delete_fires)
    };
    assert_eq!(
        row_count, 1,
        "upsert must update in place, never duplicate rows"
    );
    assert_eq!(
        delete_fires, 0,
        "upserting an existing row must not fire DELETE-path triggers"
    );

    // Sanity: a real delete does fire the probe, proving it would have
    // caught the old INSERT OR REPLACE delete+insert behavior.
    store.delete_note(id, DeleteMode::Hard).await.unwrap();
    let delete_fires_after: i64 = {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .query_row("SELECT COUNT(*) FROM delete_fires", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(
        delete_fires_after, 1,
        "the probe trigger must fire on a genuine delete"
    );
}

/// Same regression as `test_upsert_note_insert_and_seq_assignment_are_atomic`
/// for `try_insert_note`'s flag-off path.
#[tokio::test]
async fn test_try_insert_note_insert_and_seq_assignment_are_atomic() {
    let store = setup_memory_store();

    let fail_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000bb").unwrap();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER inject_seq_failure_try_insert BEFORE INSERT ON notes_seq \
                 WHEN NEW.note_id = '{fail_id}' \
                 BEGIN SELECT RAISE(ABORT, 'injected failure for #827 atomicity test'); END;"
            ))
            .unwrap();
    }

    let mut note = make_note("ns1", "message", "atomic test try_insert_note");
    note.id = fail_id;

    let result = store.try_insert_note(note).await;
    assert!(
        result.is_err(),
        "the injected notes_seq trigger failure must surface as an error"
    );

    let fetched = store.get_note(fail_id).await.unwrap();
    assert!(
        fetched.is_none(),
        "the note insert must roll back together with the failed sequence \
         assignment, not strand the note without a notes_seq row: {fetched:?}"
    );
}

#[tokio::test]
async fn pooled_transaction_commit_failure_with_verified_rollback_keeps_writer_usable() {
    let store = setup_memory_store();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE tx_finalize (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    let result = store
        .with_writer_tx("test_pooled_commit", |conn| {
            conn.execute("INSERT INTO tx_finalize (id) VALUES (1)", [])?;
            conn.authorizer(Some(deny_commit))?;
            Ok(())
        })
        .await;
    assert!(
        matches!(
            &result,
            Err(StorageError::Pool { operation, .. }) if operation == "test_pooled_commit"
        ),
        "a denied COMMIT followed by a verified rollback must report the commit error: {result:?}"
    );

    let writer = store
        .pool
        .try_writer()
        .expect("verified rollback must leave the pooled writer usable");
    let conn = writer.conn();
    assert!(conn.is_autocommit());
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tx_finalize", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn pooled_transaction_rollback_failure_reports_unknown_and_retires_writer() {
    let store = setup_memory_store();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE tx_rollback_fault (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    let result = store
        .with_writer_tx(
            "test_pooled_rollback",
            |conn| -> Result<(), rusqlite::Error> {
                conn.execute("INSERT INTO tx_rollback_fault (id) VALUES (1)", [])?;
                conn.authorizer(Some(deny_rollback))?;
                Err(rusqlite::Error::InvalidQuery)
            },
        )
        .await;
    assert!(
        matches!(
            result,
            Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            })
        ),
        "a failed rollback cannot claim that the attempted write did not land"
    );

    let checkout = store.pool.try_writer();
    assert!(
        matches!(checkout, Err(SqliteError::InvalidData(message)) if message.contains("retired")),
        "a connection with an unverified rollback must never be checked out again"
    );

    let legacy = store.pool.legacy_conn();
    let legacy_guard = legacy.lock();
    let direct_probe = legacy_guard.query_row("SELECT 1", [], |row| row.get::<_, i64>(0));
    assert!(
        direct_probe.is_err(),
        "the compatibility raw-connection handle must not bypass retirement quarantine"
    );
}

#[tokio::test]
async fn pooled_transaction_panic_with_failed_rollback_reports_unknown_and_retires_writer() {
    let store = setup_memory_store();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE tx_panic_fault (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    let result = store
        .with_writer_tx("test_pooled_panic", |conn| -> Result<(), rusqlite::Error> {
            conn.execute("INSERT INTO tx_panic_fault (id) VALUES (1)", [])?;
            conn.authorizer(Some(deny_rollback))?;
            panic!("intentional pooled transaction panic");
        })
        .await;
    assert!(
        matches!(
            result,
            Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            })
        ),
        "a panic whose rollback cannot be verified must report unknown side effects"
    );

    let checkout = store.pool.try_writer();
    assert!(
        matches!(checkout, Err(SqliteError::InvalidData(message)) if message.contains("retired")),
        "the pooled writer must retire after a transaction-body panic"
    );
}

#[tokio::test]
async fn pooled_transaction_refuses_preexisting_non_autocommit_connection() {
    let store = setup_memory_store();
    {
        let writer = store.pool.try_writer().unwrap();
        writer.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
    }
    let operation_ran = Arc::new(AtomicBool::new(false));
    let operation_ran_in_closure = Arc::clone(&operation_ran);

    let result = store
        .with_writer_tx("test_pooled_preexisting_transaction", move |_conn| {
            operation_ran_in_closure.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert!(
        matches!(
            result,
            Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            })
        ),
        "an inherited transaction has an unknown prior outcome and must fail closed"
    );
    assert!(!operation_ran.load(Ordering::SeqCst));

    let checkout = store.pool.try_writer();
    assert!(
        matches!(checkout, Err(SqliteError::InvalidData(message)) if message.contains("retired")),
        "the inherited non-autocommit connection must not be reused"
    );
}

/// Regression for #827: on the flag-off (pool-mutex)
/// path, `upsert_notes` used to run `batch_upsert_notes` inside a hand-rolled
/// `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK`, but only wired `ROLLBACK` to a
/// failed `COMMIT` -- an error from `batch_upsert_notes` itself (e.g. a
/// failed `assign_note_seq` mid-batch) propagated via `?` straight out of
/// the closure, skipping `ROLLBACK` and leaving `BEGIN IMMEDIATE` open on
/// the shared pool-mutex connection. A `BEFORE INSERT` trigger fails the
/// redundant explicit `assign_note_seq` safeguard for the SECOND note in a
/// two-note batch, after V13's `AFTER INSERT ON notes` trigger has made the
/// first, atomic sequence assignment. This keeps the injected error outside
/// the per-row UPSERT error path (which intentionally reports partial
/// success): the first note's insert and sequence assignment must already
/// have succeeded by the time this fires. The whole batch must roll back --
/// neither note nor sequence row may survive -- and the connection must not
/// be left poisoned: a subsequent write through the same pool must still
/// succeed.
#[tokio::test]
async fn test_upsert_notes_batch_rolls_back_fully_on_mid_batch_seq_failure() {
    let store = setup_memory_store();

    let fail_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-0000000000cc").unwrap();
    {
        let writer = store.pool.try_writer().unwrap();
        writer
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER inject_seq_failure_batch BEFORE INSERT ON notes_seq \
                 WHEN NEW.note_id = '{fail_id}' \
                   AND EXISTS (SELECT 1 FROM notes_seq WHERE note_id = NEW.note_id) \
                 BEGIN SELECT RAISE(ABORT, 'injected mid-batch failure for #827 test'); END;"
            ))
            .unwrap();
    }

    let mut note_ok = make_note(
        "ns1",
        "message",
        "first note in batch, seq assignment succeeds",
    );
    let ok_id = note_ok.id;
    let mut note_fail = make_note(
        "ns1",
        "message",
        "second note in batch, seq assignment fails",
    );
    note_fail.id = fail_id;
    // Ensure iteration order is deterministic: note_ok first, note_fail second.
    note_ok.created_at = 1_000_000;
    note_fail.created_at = 1_000_001;

    let result = store.upsert_notes(vec![note_ok, note_fail]).await;
    assert!(
        result.is_err(),
        "the injected mid-batch notes_seq trigger failure must surface as an error, not a \
         partial BatchWriteSummary: {result:?}"
    );

    let fetched_ok = store.get_note(ok_id).await.unwrap();
    assert!(
        fetched_ok.is_none(),
        "the whole batch must roll back -- the first note (whose own insert and seq \
         assignment succeeded) must not survive a later note's failure in the same batch: \
         {fetched_ok:?}"
    );
    let fetched_fail = store.get_note(fail_id).await.unwrap();
    assert!(
        fetched_fail.is_none(),
        "the failed note must not survive either: {fetched_fail:?}"
    );

    // The pool-mutex connection must not be left with an open transaction --
    // a subsequent write on the same pool must succeed.
    let next_note = make_note("ns1", "message", "write after rolled-back batch");
    let next_id = next_note.id;
    store
        .upsert_note(next_note)
        .await
        .expect("a write after the rolled-back batch must succeed, not hang on an open BEGIN");
    let fetched_next = store.get_note(next_id).await.unwrap();
    assert!(
        fetched_next.is_some(),
        "the post-rollback write must actually land"
    );
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput for both list paths instead of silently narrowing to a
/// negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store();
    store
        .upsert_note(make_note("ns1", "observation", "Hello world"))
        .await
        .unwrap();

    let oversized = PageRequest {
        offset: (i64::MAX as u64) + 1,
        limit: 10,
    };

    let result = store.query_notes("ns1", None, oversized.clone()).await;
    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "query_notes: expected InvalidInput, got {result:?}"
    );

    let filtered_result = store
        .query_notes_filtered("ns1", &NoteFilter::default(), oversized)
        .await;
    assert!(
        matches!(filtered_result, Err(StorageError::InvalidInput { .. })),
        "query_notes_filtered: expected InvalidInput, got {filtered_result:?}"
    );
}

/// ADR-067 Component A entry 2: with `KHIVE_WRITE_QUEUE=1`, `upsert_notes`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both rows are actually committed and independently readable back.
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: Some(true)`), not
/// the `KHIVE_WRITE_QUEUE` env var — that env var is process-global and this
/// crate's other tests are NOT `#[serial]` against it, so a window where it
/// is set here could leak into a concurrently-scheduled test's own pool
/// construction (ADR-067 Fork C slice 2).
#[tokio::test]
async fn upsert_notes_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_notes.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }

    let store = SqlNoteStore::new(Arc::clone(&pool), true);

    let n1 = make_note("default", "observation", "first");
    let n2 = make_note("default", "observation", "second");
    let id1 = n1.id;
    let id2 = n2.id;

    let summary = store.upsert_notes(vec![n1, n2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    assert!(store.get_note(id1).await.unwrap().is_some());
    assert!(store.get_note(id2).await.unwrap().is_some());
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}

/// Fork C slice 2: proves the SINGLE-row `upsert_note` (via `with_writer`,
/// distinct from the already-migrated batch `upsert_notes` above) is actually
/// enqueued on the pool's shared `WriterTaskHandle` channel when
/// `KHIVE_WRITE_QUEUE=1`.
///
/// Deliberately NOT a wall-clock/occupier-timing test: real SQLite
/// file-level locking would serialize a second writer connection against an
/// occupier's open transaction regardless of which Rust-level path issued
/// it, making elapsed time alone vacuous here (confirmed empirically while
/// designing khive-db's entity.rs sibling test in this same PR). Instead
/// this reads `WriterTaskHandle::queue_depth` directly — the live gauge over
/// the exact `mpsc` channel `with_writer`'s writer-task branch must call
/// `send` on — while an occupier deterministically holds the writer task's
/// one drain slot open (parked on a oneshot via `blocking_recv`, valid
/// because it runs inside the writer task's own `spawn_blocking`, not a
/// sleep/timing race).
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: Some(true)`), not
/// the `KHIVE_WRITE_QUEUE` env var — see
/// `upsert_notes_routes_through_writer_task_when_flag_enabled`'s doc comment
/// for the race this avoids.
#[tokio::test]
async fn upsert_note_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_note_single.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }

    let store = Arc::new(SqlNoteStore::new(Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let note = make_note("default", "observation", "single-row write-queue routing");
    let note_id = note.id;

    let store_task = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.upsert_note(note).await })
    };

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "upsert_note's write request never appeared in the writer task's \
         channel while the occupier held the single drain slot — with_writer \
         is not routing this single-row write through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    store_task
        .await
        .expect("store task must not panic")
        .expect("upsert_note must succeed once unblocked");

    let fetched = store.get_note(note_id).await.unwrap();
    assert!(
        fetched.is_some(),
        "note must be committed and readable after queuing behind the occupier"
    );
}

/// #1803: the note store's transaction-wrapped write path must preserve the
/// writer task's bounded admission contract. With one request executing and
/// the sole queue slot occupied, `upsert_note` must return the configured
/// `WriteQueueFull` error without accepting or later executing the note write.
#[tokio::test]
async fn upsert_note_reports_configured_write_queue_admission_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note_store_bounded_admission.db");
    let pool_cfg = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_queue_capacity: 1,
        write_admission_deadline_ms: 100,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }

    let store = SqlNoteStore::new(Arc::clone(&pool), true);
    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be enabled for the file-backed pool");

    // A is dequeued and blocks inside the real writer-task transaction,
    // leaving the bounded channel empty but preventing the drain loop from
    // accepting another request.
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let a_task = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    release_rx.recv().expect("test must release request A");
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    let started = tokio::time::timeout(std::time::Duration::from_secs(5), started_rx).await;
    if !matches!(started, Ok(Ok(()))) {
        let _ = release_tx.send(());
        panic!("request A did not start inside the writer task: {started:?}");
    }

    // B occupies the only pending channel slot while A remains blocked.
    let b_task = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move { writer_task.send(|_conn| Ok::<(), StorageError>(())).await })
    };
    let b_enqueued = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while writer_task.queue_depth() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if b_enqueued.is_err() {
        let _ = release_tx.send(());
        let _ = a_task.await;
        let _ = b_task.await;
        panic!("request B did not occupy the writer task's sole queue slot");
    }

    // C enters through SqlNoteStore::with_writer_tx_storage. The outer
    // timeout is only a test failure bound: the live contract must return
    // WriteQueueFull after the configured 100 ms admission deadline.
    let note = make_note(
        "default",
        "observation",
        "store-level bounded write admission regression",
    );
    let note_id = note.id;
    let c_result =
        tokio::time::timeout(std::time::Duration::from_secs(2), store.upsert_note(note)).await;

    // Always unblock the writer before making assertions, so a failing
    // bounded-admission implementation cannot strand the blocking task.
    let release_result = release_tx.send(());
    let a_result = tokio::time::timeout(std::time::Duration::from_secs(5), a_task).await;
    let b_result = tokio::time::timeout(std::time::Duration::from_secs(5), b_task).await;

    release_result.expect("request A must still be waiting for release");
    a_result
        .expect("request A did not complete after release")
        .expect("request A task must not panic")
        .expect("request A must complete successfully");
    b_result
        .expect("request B did not complete after request A")
        .expect("request B task must not panic")
        .expect("request B must complete successfully");

    match c_result.expect("note-store admission waited beyond its bounded deadline") {
        Err(StorageError::WriteQueueFull { timeout_ms }) => assert_eq!(timeout_ms, 100),
        other => panic!("expected configured WriteQueueFull from SqlNoteStore, got {other:?}"),
    }
    assert!(
        store.get_note(note_id).await.unwrap().is_none(),
        "a queue-rejected note write must never execute after capacity returns"
    );
}
