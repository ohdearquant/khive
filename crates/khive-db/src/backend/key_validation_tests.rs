#[test]
fn invalid_model_key_rejected() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.vectors("bad key!", "bad key!", 3).is_err());
    assert!(backend.vectors("", "", 3).is_err());
}

#[test]
fn invalid_table_key_rejected() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.text("bad key!").is_err());
    assert!(backend.text("").is_err());
}

/// A `table_key` ending in `_rowids` must be rejected outright — it
/// would otherwise resolve to the exact sidecar
/// table name another key's own rowid map already reserves (e.g.
/// `"entities_rowids"` -> `fts_entities_rowids`, colliding with
/// `"entities"`'s own map).
#[test]
fn table_key_ending_in_rowids_suffix_rejected() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.text("entities_rowids").is_err());
    assert!(backend.text("notes_rowids").is_err());
    assert!(backend.text("anything_rowids").is_err());
}

/// The accepted case: a key that merely contains, but does not end in,
/// the reserved suffix must still work normally.
#[test]
fn table_key_containing_but_not_ending_in_rowids_suffix_accepted() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.text("rowids_but_not_at_the_end").is_ok());
}

/// A `table_key` ending in `_rowids_state` must be rejected outright too
/// — it would otherwise resolve to the exact sidecar completion-marker
/// table name another key's own rowid map already reserves (e.g.
/// `"entities_rowids_state"` -> `fts_entities_rowids_state`, colliding
/// with `"entities"`'s own map-state table). This suffix does not end in
/// `_rowids`, so it needs its own check separate from the one above.
#[test]
fn table_key_ending_in_rowids_state_suffix_rejected() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.text("entities_rowids_state").is_err());
    assert!(backend.text("notes_rowids_state").is_err());
    assert!(backend.text("anything_rowids_state").is_err());
}

/// The accepted case for the `_rowids_state` suffix: a key that merely
/// contains, but does not end in, the reserved suffix must still work.
#[test]
fn table_key_containing_but_not_ending_in_rowids_state_suffix_accepted() {
    let backend = StorageBackend::memory().unwrap();
    assert!(backend.text("rowids_state_but_not_at_the_end").is_ok());
}
