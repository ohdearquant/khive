//! ADR-118 end-to-end regression tests: fresh-tail exact leg visibility
//! through the real `memory.recall` / `memory.recall_candidates` verbs.
//!
//! Function-level coverage of the merge/registration/compaction-linearization
//! mechanics lives in `crate::ann`'s own test module, which has direct access
//! to its private helpers. This file exercises the same code paths end to
//! end, through the dispatch surface a real agent actually calls.

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;
use serial_test::serial;

use crate::test_support::HashVecProvider;
use crate::MemoryPack;

const MODEL: &str = "adr118-e2e-test-model";
const DIMS: usize = 16;

/// Guards a mutated `KHIVE_ANN_FRESH_TAIL` value, restoring the prior state
/// (unset, by default) on drop even if the test panics.
struct EnvGuard;

impl EnvGuard {
    fn disable_fresh_tail() -> Self {
        std::env::set_var("KHIVE_ANN_FRESH_TAIL", "0");
        Self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("KHIVE_ANN_FRESH_TAIL");
    }
}

async fn build_registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    builder.build().expect("registry")
}

fn new_runtime(tmp: &std::path::Path) -> KhiveRuntime {
    let db_path = tmp.join("khive-graph.db");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    rt
}

fn contains_id(hits: &[serde_json::Value], id: uuid::Uuid) -> bool {
    let target = id.to_string();
    hits.iter()
        .any(|h| h["id"].as_str() == Some(target.as_str()))
}

/// Same-process write-then-recall: a note written after the ANN cache is
/// warm must surface on the very next `memory.recall`, without waiting for
/// any background rebuild.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn same_process_write_then_recall_surfaces_without_rebuild() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-adr118-e2e-1-")
        .tempdir_in(std::env::temp_dir())
        .expect("tempdir");
    let rt = new_runtime(tmp.path());
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    for i in 0..5u32 {
        rt.create_note(
            &token,
            "memory",
            None,
            &format!("adr118 e2e seed note {i}"),
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create seed note");
    }

    let registry = build_registry(&rt).await;

    // First recall is a cache miss: it blocks on a synchronous warm build,
    // installing a bridge whose watermark covers exactly the 5 seed notes.
    registry
        .dispatch(
            "memory.recall",
            json!({"query": "adr118 e2e seed note", "limit": 10}),
        )
        .await
        .expect("warm recall");

    const MARKER: &str = "adr118 e2e distinctive fresh tail marker zzyx quux";
    let fresh = rt
        .create_note(&token, "memory", None, MARKER, Some(0.7), None, vec![])
        .await
        .expect("create fresh note");

    // Second call reuses the now-warm (and now one-write-stale) bridge —
    // exactly the regression path (#1143). Assert on `recall_candidates`'s
    // `vector_candidates`, sourced only from the vector leg: the FTS leg
    // would find a fresh note regardless of this fix (its own index updates
    // in the write transaction, per the ADR's own root-cause analysis), so a
    // plain end-to-end `memory.recall` check would pass even without the fix
    // and would not actually be testing the fresh-tail leg.
    let candidates = registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": MARKER, "limit": 10}),
        )
        .await
        .expect("recall_candidates after fresh write");
    let vector_candidates = candidates["vector_candidates"]
        .as_array()
        .expect("vector_candidates must be an array");
    assert!(
        contains_id(vector_candidates, fresh.id),
        "a note written after the ANN cache warmed must surface in the \
         vector leg's own candidates on the very next recall, without \
         waiting for a rebuild, got: {vector_candidates:?}"
    );

    // End-to-end sanity: the note is also visible through the full,
    // fusion-facing `memory.recall` verb (both legs now agree on it).
    let result = registry
        .dispatch("memory.recall", json!({"query": MARKER, "limit": 10}))
        .await
        .expect("recall after fresh write");
    let hits = result.as_array().expect("recall result must be an array");
    assert!(
        contains_id(hits, fresh.id),
        "the fresh note must also surface through full memory.recall, got: {hits:?}"
    );
}

/// Cross-process visibility: an external writer (a second runtime handle
/// against the same database file) commits a note; the first runtime's warm
/// daemon must surface it on its next recall, even though its in-process
/// generation counters were never touched by the external write.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn cross_process_external_writer_surfaces_on_next_recall() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-adr118-e2e-2-")
        .tempdir_in(std::env::temp_dir())
        .expect("tempdir");
    let rt1 = new_runtime(tmp.path());
    let ns = Namespace::parse("local").expect("local namespace");
    let token1 = rt1.authorize(ns.clone()).expect("authorize local rt1");

    for i in 0..5u32 {
        rt1.create_note(
            &token1,
            "memory",
            None,
            &format!("adr118 e2e cross-process seed note {i}"),
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create seed note");
    }

    let registry1 = build_registry(&rt1).await;
    registry1
        .dispatch(
            "memory.recall_candidates",
            json!({"query": "adr118 e2e cross-process seed note", "limit": 10}),
        )
        .await
        .expect("warm recall_candidates on rt1");

    // A second runtime handle against the SAME database file — simulates an
    // external writer (`kkernel --atomic`, another daemon process) whose
    // write rt1's in-process generation counter never observes.
    let rt2 = new_runtime(tmp.path());
    let token2 = rt2.authorize(ns).expect("authorize local rt2");
    const MARKER: &str = "adr118 e2e cross-process distinctive marker plugh";
    let fresh = rt2
        .create_note(&token2, "memory", None, MARKER, Some(0.7), None, vec![])
        .await
        .expect("external writer creates fresh note");

    // Assert on the vector leg specifically (see the same-process test's
    // comment): FTS would find this note regardless of the fix.
    let candidates = registry1
        .dispatch(
            "memory.recall_candidates",
            json!({"query": MARKER, "limit": 10}),
        )
        .await
        .expect("recall_candidates on rt1 after external write");
    let vector_candidates = candidates["vector_candidates"]
        .as_array()
        .expect("vector_candidates must be an array");
    assert!(
        contains_id(vector_candidates, fresh.id),
        "the warm daemon (rt1) must surface, in its vector leg's own \
         candidates, a note committed by an external writer (rt2) against \
         the same database file on its very next recall, got: {vector_candidates:?}"
    );

    let result = registry1
        .dispatch("memory.recall", json!({"query": MARKER, "limit": 10}))
        .await
        .expect("recall on rt1 after external write");
    let hits = result.as_array().expect("recall result must be an array");
    assert!(
        contains_id(hits, fresh.id),
        "the fresh note must also surface through full memory.recall, got: {hits:?}"
    );
}

/// Empty tail: with nothing written since the bridge warmed, the merged
/// vector candidates are unchanged — fusion is byte-identical whenever there
/// is nothing to merge.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn empty_tail_leaves_vector_candidates_unchanged() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-adr118-e2e-5-")
        .tempdir_in(std::env::temp_dir())
        .expect("tempdir");
    let rt = new_runtime(tmp.path());
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    for i in 0..5u32 {
        rt.create_note(
            &token,
            "memory",
            None,
            &format!("adr118 e2e empty-tail seed note {i}"),
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create seed note");
    }

    let registry = build_registry(&rt).await;
    let query = "adr118 e2e empty-tail seed note";

    let first = registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": query, "limit": 10}),
        )
        .await
        .expect("first recall_candidates (warms the cache)");
    let second = registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": query, "limit": 10}),
        )
        .await
        .expect("second recall_candidates (empty tail)");

    assert_eq!(
        first["vector_candidates"], second["vector_candidates"],
        "with no writes between two recalls, the fresh-tail leg's empty tail \
         must leave the merged vector candidates byte-identical to the \
         ANN-only list"
    );
}

/// `KHIVE_ANN_FRESH_TAIL=0` disables the exact leg: a note written after the
/// ANN cache is warm must NOT appear in the vector leg's own candidates
/// (`memory.recall_candidates`'s `vector_candidates`, sourced only from the
/// vector leg — unlike full `memory.recall`, unaffected by FTS finding it).
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn env_var_disables_fresh_tail_restoring_pre_adr_behavior() {
    let tmp = tempfile::Builder::new()
        .prefix("khive-adr118-e2e-6-")
        .tempdir_in(std::env::temp_dir())
        .expect("tempdir");
    let rt = new_runtime(tmp.path());
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    for i in 0..5u32 {
        rt.create_note(
            &token,
            "memory",
            None,
            &format!("adr118 e2e env-disable seed note {i}"),
            Some(0.7),
            None,
            vec![],
        )
        .await
        .expect("create seed note");
    }

    let registry = build_registry(&rt).await;
    registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": "adr118 e2e env-disable seed note", "limit": 10}),
        )
        .await
        .expect("warm recall_candidates");

    let _guard = EnvGuard::disable_fresh_tail();

    const MARKER: &str = "adr118 e2e env-disable distinctive marker xyzzy";
    let fresh = rt
        .create_note(&token, "memory", None, MARKER, Some(0.7), None, vec![])
        .await
        .expect("create fresh note");

    let result = registry
        .dispatch(
            "memory.recall_candidates",
            json!({"query": MARKER, "limit": 10}),
        )
        .await
        .expect("recall_candidates with fresh-tail disabled");
    let vector_candidates = result["vector_candidates"]
        .as_array()
        .expect("vector_candidates must be an array");
    assert!(
        !contains_id(vector_candidates, fresh.id),
        "KHIVE_ANN_FRESH_TAIL=0 must restore pre-ADR-118 behavior: a note \
         written after the cache warmed must stay invisible to the vector \
         leg until a rebuild, got: {vector_candidates:?}"
    );
}
