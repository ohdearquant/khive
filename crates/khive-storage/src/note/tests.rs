use super::*;

fn base_note() -> Note {
    Note::new("ns:test", "memory", "hello world")
}

#[test]
fn note_key_is_optional_and_unkeyed_wire_shape_is_unchanged() {
    let note = base_note();
    assert_eq!(note.key, None);
    let legacy = serde_json::to_value(&note).unwrap();
    assert!(legacy.get("key").is_none());
    assert_eq!(serde_json::from_value::<Note>(legacy).unwrap(), note);

    let mut keyed = note;
    keyed.key = Some("operation-1".to_string());
    let encoded = serde_json::to_value(&keyed).unwrap();
    assert_eq!(encoded["key"], "operation-1");
    assert_eq!(serde_json::from_value::<Note>(encoded).unwrap(), keyed);
}

// -- with_salience --

#[test]
fn with_salience_clamps_to_range() {
    let n = base_note().with_salience(1.5);
    assert_eq!(n.salience, Some(1.0));
    let n = base_note().with_salience(-0.1);
    assert_eq!(n.salience, Some(0.0));
    let n = base_note().with_salience(0.7);
    assert_eq!(n.salience, Some(0.7));
}

#[test]
fn with_salience_ignores_nan() {
    let n = base_note().with_salience(f64::NAN);
    assert_eq!(n.salience, None, "NaN must not set salience");
}

#[test]
fn with_salience_ignores_inf() {
    let n = base_note().with_salience(f64::INFINITY);
    assert_eq!(n.salience, None, "+Inf must not set salience");
    let n = base_note().with_salience(f64::NEG_INFINITY);
    assert_eq!(n.salience, None, "-Inf must not set salience");
}

// -- with_decay --

#[test]
fn with_decay_floors_at_zero() {
    let n = base_note().with_decay(-1.0);
    assert_eq!(n.decay_factor, Some(0.0));
    let n = base_note().with_decay(0.5);
    assert_eq!(n.decay_factor, Some(0.5));
}

#[test]
fn with_decay_ignores_nan() {
    let n = base_note().with_decay(f64::NAN);
    assert_eq!(n.decay_factor, None, "NaN must not set decay_factor");
}

#[test]
fn with_decay_ignores_inf() {
    let n = base_note().with_decay(f64::INFINITY);
    assert_eq!(n.decay_factor, None, "+Inf must not set decay_factor");
}

// -- try_with_salience --

#[test]
fn try_with_salience_accepts_valid_range() {
    let n = base_note().try_with_salience(0.0).unwrap();
    assert_eq!(n.salience, Some(0.0));
    let n = base_note().try_with_salience(1.0).unwrap();
    assert_eq!(n.salience, Some(1.0));
    let n = base_note().try_with_salience(0.85).unwrap();
    assert_eq!(n.salience, Some(0.85));
}

#[test]
fn try_with_salience_rejects_nan() {
    let err = base_note().try_with_salience(f64::NAN).unwrap_err();
    assert!(err.contains("finite"), "error must mention finite: {err}");
}

#[test]
fn try_with_salience_rejects_out_of_range() {
    let err = base_note().try_with_salience(1.1).unwrap_err();
    assert!(err.contains("1.0"), "error must mention bound: {err}");
    let err = base_note().try_with_salience(-0.01).unwrap_err();
    assert!(err.contains("0.0"), "error must mention bound: {err}");
}

// -- try_with_decay --

#[test]
fn try_with_decay_accepts_valid_values() {
    let n = base_note().try_with_decay(0.0).unwrap();
    assert_eq!(n.decay_factor, Some(0.0));
    let n = base_note().try_with_decay(2.5).unwrap();
    assert_eq!(n.decay_factor, Some(2.5));
}

#[test]
fn try_with_decay_rejects_nan() {
    let err = base_note().try_with_decay(f64::NAN).unwrap_err();
    assert!(err.contains("finite"), "error must mention finite: {err}");
}

#[test]
fn try_with_decay_rejects_negative() {
    let err = base_note().try_with_decay(-0.1).unwrap_err();
    assert!(err.contains("0.0"), "error must mention bound: {err}");
}

#[derive(Default)]
struct DefaultOnlyNoteStore {
    query_calls: std::sync::atomic::AtomicUsize,
    record_calls: std::sync::atomic::AtomicUsize,
}

impl DefaultOnlyNoteStore {
    fn query_call_count(&self) -> usize {
        self.query_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn default_key_assignment_refuses_instead_of_emulating_a_write() {
    let store = DefaultOnlyNoteStore::default();
    let result = store
        .assign_note_key_if_unset(Uuid::new_v4(), "key", "local")
        .await;
    assert!(matches!(
        result,
        Err(crate::StorageError::Unsupported { .. })
    ));
    assert_eq!(
        store.record_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

fn unsupported<T>(operation: &'static str) -> StorageResult<T> {
    Err(crate::StorageError::Unsupported {
        capability: crate::StorageCapability::Notes,
        operation: operation.into(),
        message: "test double does not implement this operation".into(),
    })
}

#[async_trait]
impl NoteStore for DefaultOnlyNoteStore {
    async fn upsert_note(&self, _note: Note) -> StorageResult<()> {
        self.record_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        unsupported("upsert_note")
    }

    async fn upsert_notes(&self, _notes: Vec<Note>) -> StorageResult<BatchWriteSummary> {
        self.record_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        unsupported("upsert_notes")
    }

    async fn get_note(&self, _id: Uuid) -> StorageResult<Option<Note>> {
        self.record_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        unsupported("get_note")
    }

    async fn get_note_including_deleted(&self, _id: Uuid) -> StorageResult<Option<Note>> {
        self.record_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        unsupported("get_note_including_deleted")
    }

    async fn delete_note(&self, _id: Uuid, _mode: DeleteMode) -> StorageResult<bool> {
        unsupported("delete_note")
    }

    async fn update_note_properties(
        &self,
        _id: Uuid,
        _properties: Option<Value>,
        _updated_at: i64,
    ) -> StorageResult<bool> {
        unsupported("update_note_properties")
    }

    async fn set_note_property(
        &self,
        _id: Uuid,
        _key: &str,
        _value: Value,
        _updated_at: i64,
    ) -> StorageResult<bool> {
        unsupported("set_note_property")
    }

    async fn try_patch_note_property(
        &self,
        _id: Uuid,
        _namespace: &str,
        _filter: &NoteFilter,
        _json_path: &str,
        _value: Value,
        _updated_at: i64,
    ) -> StorageResult<bool> {
        unsupported("try_patch_note_property")
    }

    async fn query_notes(
        &self,
        _namespace: &str,
        _kind: Option<&str>,
        _page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.query_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Page {
            items: Vec::new(),
            total: Some(0),
        })
    }

    async fn query_notes_filtered(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _page: PageRequest,
    ) -> StorageResult<Page<Note>> {
        self.query_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Page {
            items: Vec::new(),
            total: Some(0),
        })
    }

    async fn query_notes_filtered_bounded(
        &self,
        _namespace: &str,
        _filter: &NoteFilter,
        _max_rows: u32,
    ) -> StorageResult<Vec<Note>> {
        unsupported("query_notes_filtered_bounded")
    }

    async fn count_notes(&self, _namespace: &str, _kind: Option<&str>) -> StorageResult<u64> {
        unsupported("count_notes")
    }

    async fn try_insert_note(&self, _note: Note) -> StorageResult<bool> {
        unsupported("try_insert_note")
    }
}

#[tokio::test]
async fn default_partial_property_patch_is_fail_closed_without_read_or_upsert() {
    let store = DefaultOnlyNoteStore::default();
    let patch = NotePropertyPatch {
        preconditions: Vec::new(),
        set: [("delivered".into(), Value::Bool(true))].into(),
        extend_expires_at: Some(123),
        updated_at: 42,
    };
    let result = store
        .try_patch_note_properties(Uuid::new_v4(), "ns:test", "message", &patch)
        .await;
    assert!(
        matches!(
            result,
            Err(crate::StorageError::Unsupported {
                capability: crate::StorageCapability::Notes,
                ref operation,
                ..
            }) if operation == "try_patch_note_properties"
        ),
        "inherited partial patch must fail closed, got {result:?}"
    );
    assert_eq!(
        store.record_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(store.query_call_count(), 0);
}

#[tokio::test]
async fn default_snapshot_count_is_fail_closed_without_query_fallback() {
    let store = DefaultOnlyNoteStore::default();
    let result = store
        .count_notes_filtered_in_snapshot(
            "ns:test",
            &[NoteFilter::default(), NoteFilter::default()],
        )
        .await;

    assert!(
        matches!(
            result,
            Err(crate::StorageError::Unsupported { ref operation, .. })
                if operation == "count_notes_filtered_in_snapshot"
        ),
        "inherited snapshot count must fail closed, got {result:?}"
    );
    assert_eq!(
        store.query_call_count(),
        0,
        "inherited snapshot count must not fall back to independent queries"
    );
}

#[tokio::test]
async fn count_free_page_default_is_fail_closed_without_exact_query_fallback() {
    let store = DefaultOnlyNoteStore::default();
    let result = store
        .query_notes_filtered_count_free("ns:test", &NoteFilter::default(), PageRequest::default())
        .await;

    assert!(
        matches!(
            result,
            Err(crate::StorageError::Unsupported { ref operation, .. })
                if operation == "query_notes_filtered_count_free"
        ),
        "inherited count-free query must fail closed, got {result:?}"
    );
    assert_eq!(
        store.query_call_count(),
        0,
        "count-free default must not invoke the exact-count page"
    );
}

#[tokio::test]
async fn unfiltered_count_free_page_default_is_fail_closed_without_exact_query_fallback() {
    let store = DefaultOnlyNoteStore::default();
    let result = store
        .query_notes_count_free("ns:test", Some("message"), PageRequest::default())
        .await;

    assert!(
        matches!(
            result,
            Err(crate::StorageError::Unsupported { ref operation, .. })
                if operation == "query_notes_count_free"
        ),
        "inherited count-free query must fail closed, got {result:?}"
    );
    assert_eq!(
        store.query_call_count(),
        0,
        "count-free default must not invoke the exact-count page"
    );
}

#[tokio::test]
async fn bounded_snapshot_count_default_is_fail_closed_without_query_fallback() {
    let store = DefaultOnlyNoteStore::default();
    let result = store
        .count_notes_filtered_bounded_in_snapshot(
            "ns:test",
            &[NoteFilter::default(), NoteFilter::default()],
            1_000,
        )
        .await;

    assert!(
        matches!(
            result,
            Err(crate::StorageError::Unsupported { ref operation, .. })
                if operation == "count_notes_filtered_bounded_in_snapshot"
        ),
        "inherited bounded snapshot count must fail closed, got {result:?}"
    );
    assert_eq!(
        store.query_call_count(),
        0,
        "bounded snapshot count must not compose independent page queries"
    );
}
