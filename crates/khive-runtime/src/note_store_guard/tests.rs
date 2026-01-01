use super::*;
use crate::{KhiveRuntime, NamespaceToken};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn assert_secret_gate_refusal(error: StorageError) {
    assert!(
        matches!(error, StorageError::InvalidInput { ref message, .. }
            if message.contains("khive:secret_gate")),
        "{error}"
    );
}

#[tokio::test]
async fn public_note_store_refuses_reserved_property_on_every_whole_object_route() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let raw = runtime.raw_notes(&token).unwrap();
    let mut existing = Note::new("local", "observation", "seeded by privileged store");
    existing.properties = Some(json!({"khive:secret_gate": "legacy", "safe": 1}));
    raw.upsert_note(existing.clone()).await.unwrap();
    let before = raw.get_note(existing.id).await.unwrap().unwrap();

    let mut fresh = Note::new("local", "observation", "new row");
    fresh.properties = before.properties.clone();
    assert_secret_gate_refusal(
        store
            .insert_note_if_absent(fresh.clone())
            .await
            .unwrap_err(),
    );
    assert_eq!(raw.get_note(fresh.id).await.unwrap(), None);
    assert_secret_gate_refusal(store.try_insert_note(fresh.clone()).await.unwrap_err());
    assert_eq!(raw.get_note(fresh.id).await.unwrap(), None);

    let mut replacement = before.clone();
    replacement.content = "unrelated edit".into();
    replacement.properties = Some(json!({"safe": 2}));
    assert_secret_gate_refusal(store.upsert_note(replacement.clone()).await.unwrap_err());
    assert_secret_gate_refusal(
        store
            .replace_note_if_unchanged(replacement.clone(), before.updated_at, before.deleted_at)
            .await
            .unwrap_err(),
    );
    let ordinary = Note::new("local", "observation", "batch sibling");
    assert_secret_gate_refusal(
        store
            .upsert_notes(vec![ordinary.clone(), replacement])
            .await
            .unwrap_err(),
    );
    assert_eq!(raw.get_note(ordinary.id).await.unwrap(), None);
    assert_secret_gate_refusal(
        store
            .update_note_properties(before.id, Some(json!({"safe": 2})), before.updated_at + 1)
            .await
            .unwrap_err(),
    );
    assert_secret_gate_refusal(
        store
            .set_note_property(before.id, "safe", json!(2), before.updated_at + 1)
            .await
            .unwrap_err(),
    );
    assert_secret_gate_refusal(
        store
            .try_patch_note_property(
                before.id,
                "local",
                &NoteFilter::default(),
                "$.safe",
                json!(2),
                before.updated_at + 1,
            )
            .await
            .unwrap_err(),
    );
    assert_secret_gate_refusal(
        store
            .patch_note_property_atomic(
                vec![before.id],
                "local",
                &NoteFilter::default(),
                "$.safe",
                json!(2),
                before.updated_at + 1,
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(raw.get_note(before.id).await.unwrap(), Some(before));
}

#[tokio::test]
async fn public_note_store_cannot_forge_or_rewrite_web_receipt() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let forged = Note::new("local", "observation", "forged").with_properties(json!({
        "tags": ["web.receipt"],
        "khive:web_receipt": "v1",
        "request": {"verb": "web.fetch"},
    }));
    assert!(matches!(
        store.upsert_note(forged).await,
        Err(StorageError::InvalidInput { .. })
    ));

    let trusted = runtime
        .create_web_receipt_note(&token, "web.fetch", json!({"verb": "web.fetch"}), vec![])
        .await
        .unwrap();
    let mut rewritten = trusted.clone();
    rewritten.properties.as_mut().unwrap()["request"]["verb"] = json!("web.refresh");
    rewritten.updated_at += 1;
    assert!(matches!(
        store.upsert_note(rewritten).await,
        Err(StorageError::InvalidInput { .. })
    ));
    assert!(matches!(
        store
            .set_note_property(
                trusted.id,
                "request",
                json!({"verb": "web.refresh"}),
                trusted.updated_at + 1,
            )
            .await,
        Err(StorageError::InvalidInput { .. })
    ));
    assert_eq!(store.get_note(trusted.id).await.unwrap(), Some(trusted));
}

async fn seed_health(runtime: &KhiveRuntime, deleted: bool) -> Note {
    let token = NamespaceToken::local();
    let mut note = Note::new("local", "channel_health", "heartbeat state");
    note.properties = Some(json!({
        "channel_kind": "email",
        "channel_slug": "original@example.com",
        "operator_note": "original",
    }));
    note.created_at = 100;
    note.updated_at = 100;
    note.deleted_at = deleted.then_some(101);
    let trusted = runtime.raw_notes(&token).unwrap();
    trusted.upsert_note(note.clone()).await.unwrap();
    trusted
        .get_note_including_deleted(note.id)
        .await
        .unwrap()
        .unwrap()
}

async fn write_full_row(
    store: &dyn NoteStore,
    before: &Note,
    replacement: Note,
    cas: bool,
) -> StorageResult<bool> {
    if cas {
        store
            .replace_note_if_unchanged(replacement, before.updated_at, before.deleted_at)
            .await
    } else {
        store.upsert_note(replacement).await.map(|()| true)
    }
}

async fn exercise_full_row_identity(cas: bool) {
    for deleted in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let store = runtime.notes(&token).unwrap();
        let before = seed_health(&runtime, deleted).await;
        for properties in [
            Some(json!({"channel_kind":"email", "channel_slug":"changed@example.com"})),
            Some(json!({"channel_kind":"telegram", "channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":null, "channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":"email", "channel_slug":null})),
            Some(json!({"channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":"email"})),
            Some(json!({})),
            Some(json!(null)),
            Some(json!([])),
            None,
        ] {
            let mut replacement = before.clone();
            replacement.properties = properties;
            replacement.updated_at += 1;
            let error = write_full_row(store.as_ref(), &before, replacement, cas)
                .await
                .expect_err("a public full-row write must not alter health identity");
            assert!(
                matches!(error, StorageError::InvalidInput { ref message, .. }
                    if message.contains("channel_health") && message.contains("kind-owned")),
                "{error}"
            );
            assert_eq!(
                store.get_note_including_deleted(before.id).await.unwrap(),
                Some(before.clone()),
                "refusal must leave the complete row unchanged"
            );
        }

        // Dropping the owning kind would let a second call bypass its identity
        // policy, even when the first call leaves both coordinates untouched.
        let mut demoted = before.clone();
        demoted.kind = "observation".into();
        demoted.updated_at += 1;
        write_full_row(store.as_ref(), &before, demoted, cas)
            .await
            .expect_err("the owning kind cannot be removed to evade the next write");
        assert_eq!(
            store.get_note_including_deleted(before.id).await.unwrap(),
            Some(before.clone())
        );

        let mut metadata = before.clone();
        metadata.properties.as_mut().unwrap()["operator_note"] = json!("changed");
        metadata.updated_at += 1;
        assert!(write_full_row(store.as_ref(), &before, metadata, cas)
            .await
            .unwrap());
        let after = store
            .get_note_including_deleted(before.id)
            .await
            .unwrap()
            .unwrap();
        let properties = after.properties.as_ref().unwrap();
        assert_eq!(properties["channel_kind"], "email");
        assert_eq!(properties["channel_slug"], "original@example.com");
        assert_eq!(properties["operator_note"], "changed");
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.deleted_at, before.deleted_at);
        assert_eq!(after.updated_at, before.updated_at + 1);
        assert!(after.version > before.version);
    }
}

// Must fail with only the existing-row identity guard reverted: the first
// replacement succeeds and changes channel_slug instead of returning refusal.
#[tokio::test]
async fn channel_health_identity_public_store_upsert_refuses_changes() {
    exercise_full_row_identity(false).await;
}

// Separate from the upsert arm so the reverted control must demonstrate that
// the same coordinate change is also refused by the public compare-and-swap.
#[tokio::test]
async fn channel_health_identity_public_store_cas_refuses_changes() {
    exercise_full_row_identity(true).await;
}

#[tokio::test]
async fn channel_health_identity_public_store_batch_validates_before_any_write() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let before = seed_health(&runtime, false).await;
    let ordinary = Note::new("local", "observation", "unrelated sibling");
    let mut changed = before.clone();
    changed.properties.as_mut().unwrap()["channel_slug"] = json!("changed@example.com");
    changed.updated_at += 1;
    store
        .upsert_notes(vec![ordinary.clone(), changed])
        .await
        .expect_err("the complete batch must be admitted before its first write");
    assert_eq!(store.get_note(ordinary.id).await.unwrap(), None);
    assert_eq!(store.get_note(before.id).await.unwrap(), Some(before));

    let mut first = Note::new("local", "channel_health", "new health row");
    first.properties = Some(json!({"channel_kind":"email", "channel_slug":"first@example.com"}));
    let mut second = first.clone();
    second.properties.as_mut().unwrap()["channel_slug"] = json!("second@example.com");
    store
        .upsert_notes(vec![first.clone(), second])
        .await
        .expect_err("a duplicate ID cannot alter identity established earlier in its batch");
    assert_eq!(store.get_note(first.id).await.unwrap(), None);

    let mut metadata = first.clone();
    metadata.properties.as_mut().unwrap()["operator_note"] = json!("allowed");
    store
        .upsert_notes(vec![first.clone(), metadata])
        .await
        .unwrap();
    let stored = store.get_note(first.id).await.unwrap().unwrap();
    assert_eq!(
        stored.properties.as_ref().unwrap()["channel_slug"],
        "first@example.com"
    );
    assert_eq!(
        stored.properties.as_ref().unwrap()["operator_note"],
        "allowed"
    );
}

#[tokio::test]
async fn channel_health_identity_public_property_replacement_cannot_erase_coordinates() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let before = seed_health(&runtime, false).await;
    for properties in [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!({"operator_note":"changed"})),
    ] {
        store
            .update_note_properties(before.id, properties, before.updated_at + 1)
            .await
            .expect_err("whole-property replacement cannot erase health coordinates");
        assert_eq!(
            store.get_note(before.id).await.unwrap(),
            Some(before.clone())
        );
    }
    assert!(store
        .set_note_property(
            before.id,
            "operator_note",
            json!("allowed"),
            before.updated_at + 1
        )
        .await
        .unwrap());
    let after = store.get_note(before.id).await.unwrap().unwrap();
    assert_eq!(
        after.properties.as_ref().unwrap()["channel_slug"],
        "original@example.com"
    );
    assert_eq!(
        after.properties.as_ref().unwrap()["operator_note"],
        "allowed"
    );
}

#[tokio::test]
async fn channel_health_identity_public_store_preserves_first_insert_and_ordinary_metadata() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let mut first = Note::new("local", "channel_health", "first heartbeat");
    first.properties = Some(json!({"channel_kind":"email", "channel_slug":"new@example.com"}));
    assert!(store.insert_note_if_absent(first.clone()).await.unwrap());
    let mut collision = first.clone();
    collision.properties.as_mut().unwrap()["channel_slug"] = json!("other@example.com");
    assert!(!store.insert_note_if_absent(collision).await.unwrap());
    assert_eq!(store.get_note(first.id).await.unwrap(), Some(first));

    let mut ordinary = Note::new("local", "observation", "unowned metadata");
    ordinary.properties = Some(json!({"channel_kind":"one", "channel_slug":"before"}));
    store.upsert_note(ordinary.clone()).await.unwrap();
    for cas in [false, true] {
        let before = store.get_note(ordinary.id).await.unwrap().unwrap();
        let mut changed = before.clone();
        changed.properties =
            Some(json!({"channel_kind":cas, "channel_slug":format!("after-{cas}")}));
        changed.updated_at += 1;
        assert!(
            write_full_row(store.as_ref(), &before, changed.clone(), cas)
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get_note(ordinary.id)
                .await
                .unwrap()
                .unwrap()
                .properties,
            changed.properties
        );
    }
}

/// Forwards every operation the property-patch seams reach to the real store
/// and counts the whole-note reads the guard issues before it writes.
struct ReadCountingStore {
    inner: Arc<dyn NoteStore>,
    reads: Arc<AtomicUsize>,
}

#[async_trait]
impl NoteStore for ReadCountingStore {
    async fn upsert_note(&self, note: Note) -> StorageResult<()> {
        self.inner.upsert_note(note).await
    }

    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary> {
        self.inner.upsert_notes(notes).await
    }

    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.inner.get_note(id).await
    }

    async fn get_note_including_deleted(&self, id: Uuid) -> StorageResult<Option<Note>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_note_including_deleted(id).await
    }

    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool> {
        self.inner.delete_note(id, mode).await
    }

    async fn update_note_properties(
        &self,
        id: Uuid,
        properties: Option<Value>,
        updated_at: i64,
    ) -> StorageResult<bool> {
        self.inner
            .update_note_properties(id, properties, updated_at)
            .await
    }

    async fn set_note_property(
        &self,
        id: Uuid,
        key: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool> {
        self.inner
            .set_note_property(id, key, value, updated_at)
            .await
    }

    async fn try_patch_note_property(
        &self,
        id: Uuid,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<bool> {
        self.inner
            .try_patch_note_property(id, namespace, filter, json_path, value, updated_at)
            .await
    }

    async fn patch_note_property_atomic(
        &self,
        ids: Vec<Uuid>,
        namespace: &str,
        filter: &NoteFilter,
        json_path: &str,
        value: Value,
        updated_at: i64,
    ) -> StorageResult<()> {
        self.inner
            .patch_note_property_atomic(ids, namespace, filter, json_path, value, updated_at)
            .await
    }

    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<&str>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.inner.query_notes(namespace, kind, page).await
    }

    async fn query_notes_filtered(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.inner
            .query_notes_filtered(namespace, filter, page)
            .await
    }

    async fn query_notes_filtered_bounded(
        &self,
        namespace: &str,
        filter: &NoteFilter,
        max_rows: u32,
    ) -> StorageResult<Vec<Note>> {
        self.inner
            .query_notes_filtered_bounded(namespace, filter, max_rows)
            .await
    }

    async fn count_notes(&self, namespace: &str, kind: Option<&str>) -> StorageResult<u64> {
        self.inner.count_notes(namespace, kind).await
    }

    async fn try_insert_note(&self, note: Note) -> StorageResult<bool> {
        self.inner.try_insert_note(note).await
    }
}

/// The guard under test, wrapped around a store that counts the guard's
/// `get_note_including_deleted` calls.
fn guarded_store_counting_reads(runtime: &KhiveRuntime) -> (Arc<dyn NoteStore>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let counting = ReadCountingStore {
        inner: runtime.raw_notes(&NamespaceToken::local()).unwrap(),
        reads: Arc::clone(&reads),
    };
    (PolicyEnforcingNoteStore::wrap(Arc::new(counting)), reads)
}

async fn seed_patch_target(runtime: &KhiveRuntime, properties: Value) -> Note {
    let note = Note::new("local", "observation", "patch target").with_properties(properties);
    runtime
        .raw_notes(&NamespaceToken::local())
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

// Fails on the two-pass guard: it reads every target once for the web-receipt
// check and again for the secret-gate check, so four ids cost eight reads and
// the read-count assertion sees 8 where 4 is expected.
#[tokio::test]
async fn atomic_property_patch_reads_each_target_once() {
    let runtime = KhiveRuntime::memory().unwrap();
    let (store, reads) = guarded_store_counting_reads(&runtime);
    let mut targets = Vec::new();
    for _ in 0..4 {
        targets.push(seed_patch_target(&runtime, json!({"read": false})).await);
    }
    let ids: Vec<Uuid> = targets.iter().map(|note| note.id).collect();

    store
        .patch_note_property_atomic(
            ids.clone(),
            "local",
            &NoteFilter::default(),
            "$.read",
            json!(true),
            targets[0].updated_at + 1,
        )
        .await
        .unwrap();

    assert_eq!(
        reads.load(Ordering::SeqCst),
        ids.len(),
        "the guard must read each target of the batch once"
    );
    for id in ids {
        let stored = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(stored.properties.unwrap()["read"], true);
    }
}

// Fails on the two-pass guard for the same reason: each single-target patch
// seam reads its note twice (counter 2 where 1 is expected).
#[tokio::test]
async fn single_property_patch_seams_read_the_target_once() {
    let runtime = KhiveRuntime::memory().unwrap();
    let (store, reads) = guarded_store_counting_reads(&runtime);
    let target = seed_patch_target(&runtime, json!({"read": false})).await;

    assert!(store
        .try_patch_note_property(
            target.id,
            "local",
            &NoteFilter::default(),
            "$.read",
            json!(true),
            target.updated_at + 1,
        )
        .await
        .unwrap());
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "try_patch_note_property must read its target once"
    );

    assert!(store
        .set_note_property(target.id, "read", json!(false), target.updated_at + 2)
        .await
        .unwrap());
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "set_note_property must read its target once"
    );
}

// The web-receipt refusal outranks a secret-gate refusal on an earlier target
// of the same batch, and a missing target still passes both checks. The refusal
// order holds before and after the single-read change; the read-count assertion
// on the refused batch fails on the two-pass guard (5 reads where 3 are expected).
#[tokio::test]
async fn atomic_property_patch_refuses_web_receipt_before_secret_gate_across_the_batch() {
    let runtime = KhiveRuntime::memory().unwrap();
    let (store, reads) = guarded_store_counting_reads(&runtime);
    let reserved = seed_patch_target(&runtime, json!({"khive:secret_gate": "legacy"})).await;
    let receipt = seed_patch_target(&runtime, json!({"khive:web_receipt": "v1"})).await;
    let ordinary = seed_patch_target(&runtime, json!({"read": false})).await;
    let missing = Uuid::new_v4();

    let error = store
        .patch_note_property_atomic(
            vec![reserved.id, missing, receipt.id, ordinary.id],
            "local",
            &NoteFilter::default(),
            "$.read",
            json!(true),
            ordinary.updated_at + 1,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, StorageError::InvalidInput { ref message, .. }
            if message.contains("web receipt provenance")),
        "{error}"
    );

    reads.store(0, Ordering::SeqCst);
    let error = store
        .patch_note_property_atomic(
            vec![missing, reserved.id, ordinary.id],
            "local",
            &NoteFilter::default(),
            "$.read",
            json!(true),
            ordinary.updated_at + 1,
        )
        .await
        .unwrap_err();
    assert_secret_gate_refusal(error);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        3,
        "a refused batch still reads each target once"
    );

    let raw = runtime.raw_notes(&NamespaceToken::local()).unwrap();
    let stored = raw.get_note(ordinary.id).await.unwrap().unwrap();
    assert_eq!(stored.properties.unwrap()["read"], false);
}
