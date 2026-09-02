//! ADR-133 D6 proof: `comm.mark_read`'s atomic mode commits its whole batch
//! in one writer acquisition and is all-or-none; `comm.read` bulk and
//! `comm.mark_read`'s non-atomic mode keep independent per-unique-id results.
//! Production comm handlers and note-store APIs are read-only for this proof
//! (`final_file_ownership_r2.md` §6).

use std::sync::Arc;

use khive_pack_comm::CommPack;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder,
};

fn file_backed_registry(
    db_path: std::path::PathBuf,
) -> (khive_runtime::VerbRegistry, KhiveRuntime) {
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(db_path),
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
    })
    .expect("file-backed runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(CommPack::new(rt.clone()));
    // ADR-133: wire the real EventStore so this fixture's dispatches
    // actually route through the audit-batch seam. Without this, `event_store`
    // stays `None`, `audit_batch` is never constructed (`pack.rs`'s
    // `build()`), and every writer_acquisitions delta measured below is pure
    // note-store cost with no deferred audit row riding along — silently
    // contradicting the "includes the deferred audit-batch row" claim this
    // file's comments make about what the delta contains.
    builder
        .with_runtime_event_store(&rt)
        .expect("configure trusted runtime audit store");
    let registry = builder.build().expect("registry builds");
    (registry, rt)
}

/// Number of persisted `Audit` events naming `verb` — proof that a dispatch
/// actually rode the ADR-133 batch seam into the store, not just that the
/// dispatch itself succeeded.
async fn audit_row_count_for_verb(rt: &KhiveRuntime, verb: &str) -> u64 {
    let token = rt.authorize(Namespace::local()).expect("local token");
    let store = rt.events(&token).expect("event store");
    store
        .count_events(khive_storage::EventFilter {
            verbs: vec![verb.to_string()],
            ..Default::default()
        })
        .await
        .expect("count_events")
}

/// Self-send `n` messages and return their inbound message ids (full UUIDs),
/// in send order, via `comm.inbox`.
async fn seed_inbound_ids(registry: &khive_runtime::VerbRegistry, n: usize) -> Vec<String> {
    for i in 0..n {
        registry
            .dispatch(
                "comm.send",
                serde_json::json!({ "to": "local", "content": format!("adr133-d6-seed-{i}") }),
            )
            .await
            .expect("self-send seeds an inbound/outbound pair");
    }
    let inbox = registry
        .dispatch(
            "comm.inbox",
            serde_json::json!({ "status": "unread", "limit": n }),
        )
        .await
        .expect("inbox lists the seeded unread messages");
    let mut ids: Vec<String> = inbox["messages"]
        .as_array()
        .expect("inbox messages array")
        .iter()
        .map(|m| m["full_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids.len(), n, "expected {n} seeded unread inbound messages");
    ids
}

/// P4 D6 selector: `comm_mark_read_atomic_uses_one_transaction`
/// (`final_verification_plan_r2.md` §8). `patch_note_property_atomic`
/// (khive-db `stores/note.rs`) wraps every target of an atomic batch in a
/// single `with_writer_tx_storage` call, so the mutation itself is one
/// transaction regardless of batch size — but the measured
/// `writer_acquisitions` delta for a whole `comm.mark_read(atomic=true)`
/// dispatch also includes dispatch-level overhead outside that store call
/// (the deferred audit-batch row for the dispatch itself, plus a
/// `NoteStore::get_note` re-fetch per target to build the response), so the
/// end-to-end delta is not literally 1. The claim this test actually proves:
/// atomic mode's end-to-end writer cost for a same-size batch is strictly
/// lower than non-atomic mode's, because non-atomic mode pays one
/// `patch_note_property` call per target instead of one shared transaction
/// for the whole batch. A batch containing one target that fails the
/// recheck filter (an outbound-only copy the caller never received) commits
/// none of the batch — the whole dispatch errors rather than partially
/// marking the valid targets.
#[tokio::test]
async fn comm_mark_read_atomic_uses_one_transaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, rt) = file_backed_registry(dir.path().join("adr133-d6-atomic.db"));
    let ids = seed_inbound_ids(&registry, 3).await;

    let audit_rows_before = audit_row_count_for_verb(&rt, "comm.mark_read").await;
    let before = rt.db_diagnostics().await.expect("diagnostics before");
    let atomic = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": ids, "atomic": true }),
        )
        .await
        .expect("atomic mark_read commits every target in one transaction");
    let after = rt.db_diagnostics().await.expect("diagnostics after");

    // The batch-seam claim in this file's comments must hold for this
    // fixture: the dispatch persisted a deferred audit row through
    // `AuditBatch`, not just through the note store.
    let audit_rows_after = audit_row_count_for_verb(&rt, "comm.mark_read").await;
    assert_eq!(
        audit_rows_after,
        audit_rows_before + 1,
        "the atomic mark_read dispatch must persist exactly one audit row through the \
         ADR-133 batch seam"
    );
    let atomic_delta = after
        .writer_contention
        .writer_acquisitions
        .checked_sub(before.writer_contention.writer_acquisitions)
        .expect("monotonic writer counter");

    let dir2 = tempfile::tempdir().expect("tempdir2");
    let (registry2, rt2) = file_backed_registry(dir2.path().join("adr133-d6-nonatomic-cmp.db"));
    let ids2 = seed_inbound_ids(&registry2, 3).await;
    let before2 = rt2.db_diagnostics().await.expect("diagnostics before2");
    registry2
        .dispatch(
            "comm.mark_read",
            serde_json::json!({ "ids": ids2, "atomic": false }),
        )
        .await
        .expect("non-atomic mark_read of the same 3 targets");
    let after2 = rt2.db_diagnostics().await.expect("diagnostics after2");
    let non_atomic_delta = after2
        .writer_contention
        .writer_acquisitions
        .checked_sub(before2.writer_contention.writer_acquisitions)
        .expect("monotonic writer counter");

    assert!(
        atomic_delta < non_atomic_delta,
        "a 3-target atomic batch (one shared transaction) must cost fewer writer \
         acquisitions than the same 3 targets marked non-atomically (one \
         patch_note_property call per target); atomic_delta={atomic_delta} \
         non_atomic_delta={non_atomic_delta}"
    );
    assert_eq!(atomic["requested_count"], 3);
    assert_eq!(atomic["marked_count"], 3);
    assert!(atomic["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["read"] == true));

    // All-or-none: mix a valid inbound id with the OUTBOUND copy of a fresh
    // send (fails `read_recheck_filter`'s direction != outbound check). The
    // whole atomic dispatch must error, and the valid id must remain unread.
    let sent = registry
        .dispatch(
            "comm.send",
            serde_json::json!({ "to": "local", "content": "adr133-d6-poison" }),
        )
        .await
        .expect("second self-send");
    let outbound_id = sent["full_id"].as_str().unwrap().to_string();
    let fresh_inbound = seed_inbound_ids(&registry, 1).await;

    let before_poisoned = rt
        .db_diagnostics()
        .await
        .expect("diagnostics before poisoned");
    let poisoned = registry
        .dispatch(
            "comm.mark_read",
            serde_json::json!({
                "ids": [fresh_inbound[0].clone(), outbound_id],
                "atomic": true,
            }),
        )
        .await;
    assert!(
        poisoned.is_err(),
        "a batch containing a target that fails the recheck filter must not partially commit"
    );

    let token = rt.authorize(Namespace::local()).expect("local token");
    let still_unread = rt
        .notes(&token)
        .unwrap()
        .get_note(fresh_inbound[0].parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        still_unread.properties.unwrap()["read"],
        false,
        "all-or-none: the valid target in a poisoned atomic batch must not be marked read"
    );
    let after_poisoned = rt
        .db_diagnostics()
        .await
        .expect("diagnostics after poisoned");
    let _ = after_poisoned; // no acquisition-count claim is made for the rejected/errored path
    let _ = before_poisoned;
}

/// P4 D6 selector: `comm_read_bulk_preserves_independent_results`
/// (`final_verification_plan_r2.md` §8). `comm.read(ids=...)` reports one
/// result per unique id with correct requested/unique/marked counts —
/// duplicate ids in the same call collapse to one result, not one per
/// occurrence — and makes no one-acquisition claim.
#[tokio::test]
async fn comm_read_bulk_preserves_independent_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _rt) = file_backed_registry(dir.path().join("adr133-d6-read-bulk.db"));
    let ids = seed_inbound_ids(&registry, 3).await;

    let mut raw_ids = ids.clone();
    raw_ids.push(ids[0].clone()); // duplicate

    let result = registry
        .dispatch("comm.read", serde_json::json!({ "ids": raw_ids }))
        .await
        .expect("bulk read succeeds");
    assert_eq!(result["requested_count"], 4);
    assert_eq!(result["unique_count"], 3);
    assert_eq!(result["marked_count"], 3);
    assert_eq!(result["failed_count"], 0);
    let results = result["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        3,
        "one result per unique id, not per occurrence"
    );
    assert!(results.iter().all(|r| r["read"] == true));
}

/// P4 D6 selector: `comm_mark_read_non_atomic_preserves_independent_results`
/// (`final_verification_plan_r2.md` §8). Non-atomic `comm.mark_read` reports
/// one result per unique id, deduplicating repeats in the same call, and
/// makes no one-acquisition claim (unlike the atomic path).
#[tokio::test]
async fn comm_mark_read_non_atomic_preserves_independent_results() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (registry, _rt) = file_backed_registry(dir.path().join("adr133-d6-nonatomic.db"));
    let ids = seed_inbound_ids(&registry, 2).await;

    let mut raw_ids = ids.clone();
    raw_ids.push(ids[1].clone()); // duplicate of the second id

    let result = registry
        .dispatch("comm.mark_read", serde_json::json!({ "ids": raw_ids }))
        .await
        .expect("non-atomic (default) mark_read succeeds");
    assert_eq!(result["requested_count"], 3);
    assert_eq!(result["unique_count"], 2);
    assert_eq!(result["marked_count"], 2);
    assert_eq!(result["failed_count"], 0);
    let results = result["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        2,
        "one result per unique id, not per occurrence"
    );
    for r in results {
        assert_eq!(r["read"], true);
        assert!(r.get("mark_error").is_none());
    }
}
