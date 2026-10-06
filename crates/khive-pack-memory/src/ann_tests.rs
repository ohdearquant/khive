use super::*;
#[path = "ann/tests/final_tail_tests.rs"]
mod final_tail_tests;
#[path = "ann/tests/generation_pin_tests.rs"]
mod generation_pin_tests;
#[path = "ann/tests/incremental_tests.rs"]
mod incremental_tests;
#[path = "ann/tests/maintenance_lock_tests.rs"]
mod maintenance_lock_tests;
use serial_test::serial;

include!("ann/rotation_watcher_lifecycle_tests.rs");

/// Owns a file-backed runtime and removes its database directory after shutdown.
struct TestRuntime {
    runtime: KhiveRuntime,
    _temp_dir: tempfile::TempDir,
}

impl std::ops::Deref for TestRuntime {
    type Target = KhiveRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

struct InterleavingTailReader {
    subject: Uuid,
    calls: Vec<String>,
}

impl InterleavingTailReader {
    fn row(columns: Vec<(&str, SqlValue)>) -> khive_storage::SqlRow {
        khive_storage::SqlRow {
            columns: columns
                .into_iter()
                .map(|(name, value)| khive_storage::types::SqlColumn {
                    name: name.to_owned(),
                    value,
                })
                .collect(),
        }
    }

    fn embedding(values: &[f32]) -> SqlValue {
        SqlValue::Blob(
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        )
    }
}

#[async_trait::async_trait]
impl khive_storage::SqlReader for InterleavingTailReader {
    async fn query_row(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Option<khive_storage::SqlRow>> {
        panic!("fresh-tail replay must use query_all")
    }

    async fn query_all(
        &mut self,
        statement: SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
        let label = statement.label.unwrap_or_default();
        self.calls.push(label.clone());
        let id = SqlValue::Text(self.subject.to_string());
        Ok(match label.as_str() {
            // The coherent snapshot: the suffix and its matching current
            // vector are both the pre-commit state.
            "memory_ann_fresh_tail_snapshot" => {
                assert!(
                    statement.sql.contains("COUNT(*) AS live_count"),
                    "the snapshot statement must derive the corpus-relative cap"
                );
                assert!(
                    statement.sql.contains("LIMIT (SELECT"),
                    "the snapshot statement must apply the derived newest-suffix cap"
                );
                assert!(
                    statement.sql.contains("LEFT JOIN vec_snapshot_race_model"),
                    "the snapshot statement must hydrate the selected suffix's vectors"
                );
                assert!(
                    statement.sql.contains("LEFT JOIN notes"),
                    "the snapshot statement must evaluate note liveness"
                );
                vec![Self::row(vec![
                    ("seq", SqlValue::Integer(1)),
                    ("subject_id", id),
                    ("op", SqlValue::Text("upsert".into())),
                    ("vector_model", SqlValue::Text("snapshot-race-model".into())),
                    ("vector_kind", SqlValue::Text("note".into())),
                    ("vector_field", SqlValue::Text("note.content".into())),
                    ("embedding", Self::embedding(&[1.0, 0.0])),
                    ("live_note_id", SqlValue::Text(self.subject.to_string())),
                ])]
            }
            // Legacy multi-query behavior: a writer commits immediately
            // after suffix selection, so the next pooled read observes a
            // newer vector than the selected log row described.
            "memory_ann_fetch_tail" => vec![Self::row(vec![
                ("seq", SqlValue::Integer(1)),
                ("subject_id", id),
                ("op", SqlValue::Text("upsert".into())),
            ])],
            "memory_ann_tail_point_read" => vec![Self::row(vec![
                (
                    "embedding_model",
                    SqlValue::Text("snapshot-race-model".into()),
                ),
                ("kind", SqlValue::Text("note".into())),
                ("field", SqlValue::Text("note.content".into())),
                ("embedding", Self::embedding(&[0.0, 1.0])),
            ])],
            "memory_ann_tail_live_notes" => vec![Self::row(vec![(
                "id",
                SqlValue::Text(self.subject.to_string()),
            )])],
            other => panic!("unexpected fresh-tail query label: {other}"),
        })
    }

    async fn query_scalar(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Option<SqlValue>> {
        panic!("fresh-tail replay must use query_all")
    }

    async fn explain(
        &mut self,
        _statement: SqlStatement,
    ) -> khive_storage::StorageResult<Vec<khive_storage::SqlRow>> {
        panic!("fresh-tail replay must not issue EXPLAIN")
    }
}

/// A pool-backed reader must see a racing commit as entirely visible or entirely invisible, never mixed.
#[tokio::test]
async fn fresh_tail_snapshot_cannot_return_a_torn_log_vector_pair() {
    let subject = Uuid::new_v4();
    let mut reader = InterleavingTailReader {
        subject,
        calls: Vec::new(),
    };

    let (ops, watermark) = fetch_final_tail_on(&mut reader, "snapshot-race-model", 0, Some(0.20))
        .await
        .expect("fresh-tail snapshot");

    assert_eq!(watermark, 1);
    assert_eq!(ops, vec![(subject, Some(vec![1.0, 0.0]))]);
    assert_eq!(
        reader.calls,
        vec!["memory_ann_fresh_tail_snapshot"],
        "one logical no-index replay must execute exactly one SQLite statement"
    );
}

#[test]
fn outcome_into_candidates_replace_with_reason_discloses_degradation() {
    // A reasoned Replace must surface its failure-site reason, not report healthy.
    let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
    let replaced = vec![(Uuid::from_u128(2), 0.8_f64)];
    let (candidates, disclosure) = outcome_into_candidates(
        FreshTailOutcome::Replace(
            replaced.clone(),
            Some("fresh-tail: re-resolved tail fetch failed"),
        ),
        prior,
        &[1.0, 0.0],
    );
    assert_eq!(
        candidates,
        vec![(Uuid::from_u128(2), 0.8_f64 as f32)],
        "Replace must swap the candidate set"
    );
    assert_eq!(
        disclosure.as_deref(),
        Some("fresh-tail: re-resolved tail fetch failed"),
        "a reasoned Replace must disclose its failure site"
    );
}

#[test]
fn outcome_into_candidates_healthy_replace_and_ops_do_not_disclose() {
    let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
    let replaced = vec![(Uuid::from_u128(2), 0.8_f64)];
    let (candidates, disclosure) = outcome_into_candidates(
        FreshTailOutcome::Replace(replaced.clone(), None),
        prior.clone(),
        &[1.0, 0.0],
    );
    assert_eq!(candidates, vec![(Uuid::from_u128(2), 0.8_f64 as f32)]);
    assert!(
        disclosure.is_none(),
        "a fully assembled re-resolution is not degraded"
    );

    let (candidates, disclosure) = outcome_into_candidates(
        FreshTailOutcome::Ops(Vec::new()),
        prior.clone(),
        &[1.0, 0.0],
    );
    assert_eq!(candidates, prior);
    assert!(disclosure.is_none(), "an empty tail merge is not degraded");
}

#[test]
fn outcome_into_candidates_skipped_keeps_prior_and_discloses() {
    let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
    let (candidates, disclosure) = outcome_into_candidates(
        FreshTailOutcome::Skipped(SkipReason::with_error(
            "fresh-tail: reader open failed",
            "pool exhausted after 5s",
        )),
        prior.clone(),
        &[1.0, 0.0],
    );
    assert_eq!(candidates, prior, "Skipped must leave candidates untouched");
    assert_eq!(
        disclosure.as_deref(),
        Some("fresh-tail: reader open failed: pool exhausted after 5s"),
        "the disclosure must carry the error that caused the skip, not \
             only the label shared by every failure at that site"
    );
}

/// A site holding no error keeps emitting the bare label: the enriched
/// rendering must not smuggle a separator or a placeholder onto a skip
/// that genuinely has nothing further to say.
#[test]
fn skip_reason_without_an_error_renders_the_bare_label() {
    const LABEL: &str = "note-search ANN consumer is not active in tail snapshot";
    let reason = SkipReason::bare(LABEL);
    assert_eq!(reason.detail(), None);
    assert_eq!(reason.label(), LABEL);
    assert_eq!(reason.to_string(), LABEL);

    let prior = vec![(Uuid::from_u128(1), 0.9_f32)];
    let (candidates, disclosure) = outcome_into_candidates(
        FreshTailOutcome::Skipped(SkipReason::bare(LABEL)),
        prior.clone(),
        &[1.0, 0.0],
    );
    assert_eq!(candidates, prior);
    assert_eq!(
        disclosure.as_deref(),
        Some(LABEL),
        "an error-free skip must disclose exactly the label it always did"
    );
}

/// Two skips that share a label are told apart by the error each carries:
/// an exhausted reader pool is retryable, an unopenable database file is
/// not, and the label alone cannot separate them.
#[test]
fn skip_reason_separates_two_causes_that_share_a_label() {
    const LABEL: &str = "fresh-tail: reader open failed";
    let exhausted = SkipReason::with_error(LABEL, "pool exhausted after 5s");
    let unopenable = SkipReason::with_error(LABEL, "unable to open database file");

    assert_eq!(exhausted.label(), unopenable.label());
    assert_ne!(
        exhausted.to_string(),
        unopenable.to_string(),
        "the two causes must be distinguishable in the served reason"
    );
    for (reason, expected_error) in [
        (&exhausted, "pool exhausted after 5s"),
        (&unopenable, "unable to open database file"),
    ] {
        let rendered = reason.to_string();
        assert!(
            rendered.starts_with(LABEL),
            "the label must stay at the front of the reason, got: {rendered:?}"
        );
        assert!(
            rendered.contains(expected_error),
            "the reason must carry the error text, got: {rendered:?}"
        );
    }
}

/// An unbounded error (a driver message quoting a whole statement) is cut
/// to the documented character bound and marked as cut, so one degraded
/// response cannot be bloated by the error it discloses.
#[test]
fn skip_reason_detail_is_bounded_and_marks_the_cut() {
    let long_error = "x".repeat(SKIP_DETAIL_MAX_CHARS * 10);
    let reason = SkipReason::with_error("fresh-tail: tail fetch failed", &long_error);

    let detail = reason
        .detail()
        .expect("an error-bearing skip carries detail");
    assert_eq!(
        detail.chars().count(),
        SKIP_DETAIL_MAX_CHARS,
        "a cut detail must land exactly on the bound, marker included"
    );
    assert!(
        detail.ends_with(SKIP_DETAIL_TRUNCATION_MARKER),
        "a cut must be marked so the reader knows the error continues, got: {detail:?}"
    );
    assert!(
        reason
            .to_string()
            .starts_with("fresh-tail: tail fetch failed"),
        "truncating the error must not disturb the label"
    );

    // The control arm: an error that fits is carried whole, with no
    // marker — otherwise the assertion above passes on a function that
    // simply truncates everything.
    let short_error = "y".repeat(SKIP_DETAIL_MAX_CHARS);
    let short = SkipReason::with_error("fresh-tail: tail fetch failed", &short_error);
    assert_eq!(
        short.detail(),
        Some(short_error.as_str()),
        "an error within the bound must be carried unchanged"
    );
}

/// The bound counts characters, not bytes: a multi-byte error message
/// must never be cut mid-character (which would not even be a `String`).
#[test]
fn skip_reason_detail_bound_never_splits_a_character() {
    let multibyte = "\u{00e9}".repeat(SKIP_DETAIL_MAX_CHARS * 2);
    let bounded = bound_skip_detail(&multibyte);
    assert_eq!(bounded.chars().count(), SKIP_DETAIL_MAX_CHARS);
    assert!(bounded.ends_with(SKIP_DETAIL_TRUNCATION_MARKER));
    assert!(
        bounded
            .trim_end_matches(SKIP_DETAIL_TRUNCATION_MARKER)
            .chars()
            .all(|c| c == '\u{00e9}'),
        "the kept prefix must be whole characters, got: {bounded:?}"
    );
}

#[test]
fn ann_key_is_model_only() {
    // After FTS+ANN consolidation AnnKey is model-only; namespace is ignored.
    let k1 = AnnKey::new("model-x");
    let k2 = AnnKey::new("model-x"); // same model, different ns → same key
    let k3 = AnnKey::new("model-y"); // different model → different key
    assert_eq!(
        k1, k2,
        "same model, different namespace must produce the same key"
    );
    assert_ne!(k1, k3, "different models must produce different keys");
}

#[test]
fn ann_bridge_maps_vamana_ids_to_uuids() {
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let id_c = Uuid::new_v4();

    // 3 orthogonal unit vectors in 3D
    let vectors = vec![
        1.0f32, 0.0, 0.0, // id_a
        0.0, 1.0, 0.0, // id_b
        0.0, 0.0, 1.0, // id_c
    ];
    let bridge =
        AnnBridge::build(vectors, 3, vec![id_a, id_b, id_c], HashSet::new()).expect("build");

    // query close to id_a
    let hits = bridge.search(&[1.0, 0.0, 0.0], 1).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, id_a, "nearest to [1,0,0] must be id_a");
    assert!(hits[0].1 > 0.9, "cosine must be close to 1.0");
}

#[test]
fn ann_search_dimension_error_returns_err() {
    let id = Uuid::new_v4();
    let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0], 3, vec![id], HashSet::new())
        .expect("build 3-dim bridge");
    // query with wrong dimension (2 instead of 3)
    let result = bridge.search(&[1.0, 0.0], 1);
    assert!(result.is_err(), "wrong dimension must return Err");
}

/// A stale id-map entry must not let a delete replay tombstone a slot a same-batch upsert reused (#1150).
#[test]
fn replay_does_not_tombstone_slot_reused_by_same_batch_upsert() {
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let id_c = Uuid::new_v4();

    let vectors = vec![
        1.0f32, 0.0, 0.0, // id_a, ordinal 0
        0.0, 1.0, 0.0, // id_b, ordinal 1
    ];
    let mut bridge = AnnBridge::build(vectors, 3, vec![id_a, id_b], HashSet::new()).expect("build");

    // Simulate a PRIOR tombstone of id_a that left the id-map entry
    // stale (tombstoning never clears it) — exactly the persisted state
    // #1150 describes, without going through a save/load round trip.
    bridge.index.tombstone(0).expect("tombstone id_a");
    assert_eq!(
        bridge.id_map[0], id_a,
        "id-map entry stays stale after tombstone"
    );

    // Coalesced final tail: id_c's upsert (which recycles id_a's freed
    // ordinal 0) is processed BEFORE id_a's own final delete — a legal
    // op order since coalescing only guarantees per-subject dedup, not
    // cross-subject sequencing.
    let ops = vec![(id_c, Some(vec![0.0f32, 0.0, 1.0])), (id_a, None)];
    bridge.apply_final_ops(ops, 1).expect("apply replay tail");

    assert_eq!(
        bridge.id_map[0], id_c,
        "ordinal 0 must be owned by id_c after the replay"
    );
    assert!(
        !bridge.index.is_tombstoned(0),
        "id_a's stale delete must not tombstone the slot id_c now owns"
    );
    let hits = bridge.search(&[0.0, 0.0, 1.0], 2).expect("search");
    assert!(
        hits.iter().any(|(id, score)| *id == id_c && *score > 0.9),
        "id_c must remain live and searchable, got: {hits:?}"
    );
    assert!(
        !hits.iter().any(|(id, _)| *id == id_a),
        "id_a must not resurface as a search hit, got: {hits:?}"
    );
}

#[test]
fn snapshot_key_does_not_collide_with_knowledge_vamana() {
    let mem_key = snapshot_key("local", "all-minilm-l6-v2");
    assert!(
        mem_key.contains("::memory_vamana::"),
        "memory key must contain ::memory_vamana:: but got: {mem_key}"
    );
    assert!(
        !mem_key.contains("::vamana::"),
        "memory key must not match knowledge pattern ::vamana:: but got: {mem_key}"
    );
}

// Writes preserve the installed graph and persisted segment until a fresher
// build replaces them.

#[tokio::test]
async fn bump_generation_does_not_evict_installed_index_or_segment() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    let id = Uuid::new_v4();

    install_replacing(&ann, &key, tiny_bridge(id, 1)).await;
    let seg_dir = tempfile::Builder::new()
        .prefix("khive-memory-ann-seg-")
        .tempdir_in(std::env::temp_dir())
        .expect("segment tempdir");
    {
        let idxs = ann.indexes.read().await;
        let bridge = idxs.get(&key).expect("installed above");
        bridge.save_atomic(seg_dir.path()).expect("persist segment");
    }

    // A write lands: it bumps the generation but must not clear anything.
    bump_generation(&ann, &key).await;

    assert!(
        ann.indexes.read().await.contains_key(&key),
        "a write must not evict the previously-installed in-memory index"
    );
    assert!(
        AnnBridge::load(seg_dir.path()).is_ok(),
        "a write must not invalidate the previously-persisted segment before \
             a fresher checkpoint has durably replaced it"
    );
}

/// `search_loaded` serves an installed stale graph rather than forcing an inline rebuild.
#[tokio::test]
async fn search_loaded_serves_stale_installed_entry_without_rebuild() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    let id = Uuid::new_v4();

    install_replacing(&ann, &key, tiny_bridge(id, 1)).await;
    bump_generation(&ann, &key).await; // counter -> 1
    bump_generation(&ann, &key).await; // counter -> 2, ahead of installed gen 1

    assert!(
        !is_current(&ann, &key).await,
        "sanity: the installed entry must now be behind the write-generation counter"
    );

    let hits = search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
        .await
        .expect("search_loaded must not error on a stale-but-installed entry");
    assert!(
        hits.is_some(),
        "a stale-but-installed entry must still be served by search_loaded, \
             not treated the same as a genuine cache miss"
    );
}

// These deterministic tests pin generation compare-and-replace semantics directly.

fn tiny_bridge(id: Uuid, generation: u64) -> AnnBridge {
    AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![id], HashSet::new())
        .expect("build tiny bridge")
        .with_generation(generation)
}

/// A peer can rotate an otherwise byte-identical checkpoint while this
/// process is completely idle. One watcher tick must adopt its new UUID
/// sidecar and drop the bridge that owns the unlinked predecessor mmaps.
#[tokio::test]
async fn rotation_tick_releases_predecessor_and_adopts_identical_peer_checkpoint() {
    const MODEL: &str = "memory-rotation-release-test-model";
    let rt = test_runtime_with_hash_embedder(MODEL, 4);
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
    let old_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();

    tiny_bridge(old_id, 7)
        .save_atomic(&dir)
        .expect("persist first generation");
    let mut loaded = AnnBridge::load(&dir)
        .expect("load first mmap generation")
        .with_generation(7);
    let probe = Arc::new(());
    let dropped = Arc::downgrade(&probe);
    loaded.drop_probe = Some(probe);
    assert!(install_replacing(&ann, &key, loaded).await);
    let first_digest = ann
        .indexes
        .read()
        .await
        .get(&key)
        .and_then(|bridge| bridge.commit_digest)
        .expect("loaded bridge identity");

    // The vector bytes and Vamana lifecycle are identical. Only the ID
    // sidecar denotes the peer's newly published logical mapping.
    tiny_bridge(new_id, 7)
        .save_atomic(&dir)
        .expect("rotate identical checkpoint");

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        dropped.upgrade().is_none(),
        "replacing the cache entry must drop the predecessor mmap owner"
    );
    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("rotated bridge installed");
    assert_ne!(bridge.commit_digest, Some(first_digest));
    let hits = bridge
        .search(&[1.0, 0.0, 0.0, 0.0], 1)
        .expect("search replacement");
    assert_eq!(hits.first().map(|hit| hit.0), Some(new_id));
    assert_eq!(bridge.generation, 7, "local generation fence is preserved");
}

/// A peer's rotated checkpoint can cover namespaces this process never
/// queried. Adopting it must not inherit the incumbent's narrower
/// namespace_set — recall's over-fetch decision trusts an empty set as
/// "assume non-visible namespaces exist" and a stale narrow set as
/// "corpus fully accounted for", so carrying the incumbent's set forward
/// would hide eligible memories from namespaces the peer's checkpoint
/// added.
#[tokio::test]
async fn rotation_tick_resets_namespace_set_instead_of_inheriting_incumbent() {
    const MODEL: &str = "memory-rotation-namespace-reset-test-model";
    let rt = test_runtime_with_hash_embedder(MODEL, 4);
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
    let old_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();

    tiny_bridge(old_id, 7)
        .save_atomic(&dir)
        .expect("persist first generation");
    let mut incumbent = AnnBridge::load(&dir)
        .expect("load first mmap generation")
        .with_generation(7);
    // Simulate a bridge this process actually built: it only ever
    // observed namespace "ns-a".
    incumbent.set_namespace_set(HashSet::from(["ns-a".to_string()]));
    assert!(install_replacing(&ann, &key, incumbent).await);

    // The peer's rotated checkpoint carries data from a namespace this
    // process never saw.
    tiny_bridge(new_id, 7)
        .save_atomic(&dir)
        .expect("rotate peer checkpoint covering an unseen namespace");

    refresh_rotated_segments_once(&rt, &ann).await;

    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("rotated bridge installed");
    assert!(
        bridge.namespace_set.is_empty(),
        "rotation must not carry the incumbent's namespace_set {:?} onto a peer \
             checkpoint of unknown namespace coverage; recall requires the conservative \
             empty set to keep over-fetching for eligible visible memories",
        bridge.namespace_set
    );
}

/// A process without corpus-build authority declines instead of scanning and
/// publishing. The second half is the control: the same corpus, the same
/// runtime, with the authority, builds — so the decline is caused by the role
/// and not by a fixture that could not have built anyway.
#[tokio::test]
async fn a_process_that_does_not_build_declines_instead_of_scanning_the_corpus() {
    const MODEL: &str = "memory-non-building-process-declines-test-model";
    const DIMS: usize = 4;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "a note the daemon will index",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create note");

    let key = AnnKey::new(MODEL);
    let client = new_shared_for_role(false);
    let declined = ensure_ann_for_model(&rt, &token, &client, MODEL)
        .await
        .expect("ensure must not error, it must decline");
    assert!(
        matches!(declined, AnnEnsureStatus::DeclinedNotWarmHost),
        "a process without corpus-build authority must decline, got {declined:?}"
    );
    assert!(
        !client.indexes.read().await.contains_key(&key),
        "a decline must install nothing"
    );
    if let Some(seg_dir) = ann_segment_dir(&rt, MODEL) {
        assert!(
            !seg_dir.join("metadata.bin").exists(),
            "a decline must publish no segment"
        );
    }

    let host = new_shared();
    let built = ensure_ann_for_model(&rt, &token, &host, MODEL)
        .await
        .expect("control build");
    assert!(
        matches!(built, AnnEnsureStatus::Built { vectors: 1 }),
        "control: with the authority the same corpus builds, got {built:?}"
    );
}

/// The Stale-tail path replays in memory and then checkpoints the delta.
/// A process without corpus-build
/// authority must serve the replayed bridge and publish nothing, or every
/// client warming after any write republishes the segment. The search for
/// the tail note is the witness that the replay path ran rather than a Hot
/// load of the seeded segment. The control is the same state warmed with
/// the authority, which does checkpoint.
#[tokio::test]
async fn a_process_that_does_not_build_replays_the_tail_without_publishing() {
    const MODEL: &str = "memory-non-building-process-stale-tail-test-model";
    const DIMS: usize = 4;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..4 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("seeded note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seeded note");
    }
    let seed = new_shared();
    let built = ensure_ann_for_model(&rt, &token, &seed, MODEL)
        .await
        .expect("seed build");
    assert!(
        matches!(built, AnnEnsureStatus::Built { vectors: 4 }),
        "seed: expected a build over 4 vectors, got {built:?}"
    );
    let seg_dir = ann_segment_dir(&rt, MODEL).expect("segment dir");
    let metadata = seg_dir.join("metadata.bin");
    let vectors = seg_dir.join("vectors.bin");
    let before_metadata = std::fs::read(&metadata).expect("seeded metadata.bin");
    let before_vectors = std::fs::read(&vectors).expect("seeded vectors.bin");

    // One more note: live = 5, tail = 1 ≤ ceil(0.20 × 5) → Stale-tail.
    let tail_note = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "the note only a tail replay can find",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create tail note");

    let key = AnnKey::new(MODEL);
    let client = new_shared_for_role(false);
    let status = ensure_ann_for_model(&rt, &token, &client, MODEL)
        .await
        .expect("client warm");
    assert!(
        matches!(status, AnnEnsureStatus::LoadedSnapshot),
        "a client must adopt the segment through Stale-tail replay, got {status:?}"
    );
    let query = fnv_to_vec("the note only a tail replay can find", DIMS);
    let hits = search_loaded(&client, &key, &query, 5)
        .await
        .expect("search must succeed")
        .expect("the replayed bridge must be installed");
    assert!(
        hits.iter()
            .any(|(id, score)| *id == tail_note.id && *score > 0.99),
        "the tail note must be served from the replayed bridge, got {hits:?}"
    );
    assert_eq!(
        std::fs::read(&metadata).expect("metadata.bin after client warm"),
        before_metadata,
        "a client must not checkpoint: metadata.bin changed"
    );
    assert_eq!(
        std::fs::read(&vectors).expect("vectors.bin after client warm"),
        before_vectors,
        "a client must not checkpoint: vectors.bin changed"
    );

    let host = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &host, MODEL)
        .await
        .expect("host warm");
    assert!(
        matches!(status, AnnEnsureStatus::LoadedSnapshot),
        "control: the host adopts the same segment, got {status:?}"
    );
    assert_eq!(
        std::fs::read(&metadata).expect("metadata.bin after host warm"),
        before_metadata,
        "the host checkpoint must preserve the base segment nonce"
    );
    assert!(
        seg_dir.join(delta::HEAD_FILE).exists(),
        "the host must publish a delta HEAD after replay"
    );
}

/// The chain debounce is what decides how many writes one rebuild absorbs.
/// One second absorbed nothing against a fleet writing continuously, which is
/// how a coalescing window became a rebuild cadence.
#[test]
fn rebuild_chain_debounce_policy() {
    let default = std::time::Duration::from_secs(30);
    assert_eq!(resolve_rebuild_chain_debounce(None, default), default);
    assert_eq!(
        resolve_rebuild_chain_debounce(Some(" 2500 "), default),
        std::time::Duration::from_millis(2500)
    );
    // Zero is a real answer: it means no coalescing was asked for.
    assert_eq!(
        resolve_rebuild_chain_debounce(Some("0"), default),
        std::time::Duration::ZERO
    );
    // A malformed value must not silently become zero, which would restore
    // the behaviour this default exists to fix.
    assert_eq!(
        resolve_rebuild_chain_debounce(Some("soon"), default),
        default
    );
    assert_eq!(resolve_rebuild_chain_debounce(Some("-1"), default), default);
    assert_eq!(resolve_rebuild_chain_debounce(Some(""), default), default);
}

/// Mirrors the knowledge-pack invalid-rotation tests (issue #2340): a
/// peer's rotated checkpoint that fails validation must evict the
/// predecessor here too, and a later warm must recover. Unlike the
/// knowledge pack, memory has no separate warm-lifecycle map to race —
/// `refresh_rotated_segment` and `ensure_ann_for_model` both take
/// `model_warm_lock` for the same key before touching `indexes`, so the
/// watcher and an in-flight rebuild are
/// already mutually exclusive; this test pins the recovery behavior that
/// serialization is relied on to make safe.
#[tokio::test]
async fn invalid_rotation_evicts_predecessor_and_next_warm_recovers() {
    const MODEL: &str = "memory-invalid-rotation-recovery-test-model";
    const DIMS: usize = 4;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "invalid rotation recovery note",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create note");

    let ann = new_shared();
    let key = AnnKey::new(MODEL);

    let first = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("initial full checkpoint");
    assert!(
        matches!(first, AnnEnsureStatus::Built { vectors: 1 }),
        "sanity: the initial warm must build and persist the corpus, got {first:?}"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "sanity: the initial build must be installed"
    );

    // A peer publishes a changed commit whose UUID sidecar is missing, so
    // the rotated generation fails validation and the incumbent is evicted.
    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
    let incumbent_seq = ann
        .indexes
        .read()
        .await
        .get(&key)
        .and_then(|bridge| bridge.index.last_applied_seq())
        .expect("initial build carries an applied watermark");
    let mut rotated = tiny_bridge(Uuid::new_v4(), 1);
    rotated.set_applied_seq(incumbent_seq);
    rotated.save_atomic(&dir).expect("rotate checkpoint");
    std::fs::remove_file(dir.join("external_ids.bin")).expect("remove sidecar");

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        ann.indexes.read().await.get(&key).is_none(),
        "an invalid rotated generation must evict the incumbent"
    );

    // The next warm must recover: `model_warm_lock` is the same lock the
    // watcher just released, so no stale ownership blocks the rebuild.
    let recovered = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("recovery checkpoint after eviction");
    assert!(
        matches!(recovered, AnnEnsureStatus::Built { vectors: 1 }),
        "the next warm must rebuild after the watcher's eviction, got {recovered:?}"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "the recovery build must reinstall the index"
    );
}

/// A strictly older generation than the installed entry must never replace it (pre-#750 bug shape).
#[tokio::test]
async fn install_replacing_rejects_older_generation_candidate() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    let newer_id = Uuid::new_v4();
    let older_id = Uuid::new_v4();

    assert!(install_replacing(&ann, &key, tiny_bridge(newer_id, 5)).await);
    assert!(!install_replacing(&ann, &key, tiny_bridge(older_id, 2)).await);

    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("an entry must be installed");
    assert_eq!(bridge.generation, 5, "the newer generation must survive");
    assert_eq!(
        bridge.id_map,
        vec![newer_id],
        "the older-generation candidate must not have replaced it"
    );
}

/// A pathless build rejected by a newer post-scan generation must not advance the watermark or compact the tail.
#[tokio::test]
async fn pathless_rejected_candidate_does_not_raise_or_compact() {
    let rt = KhiveRuntime::memory().expect("runtime");
    let ann = new_shared();
    let model = "pathless-rejected-checkpoint";
    let key = AnnKey::new(model);
    register_consumer(&rt, model)
        .await
        .expect("register pending consumer");

    let newer_id = Uuid::new_v4();
    assert!(install_replacing(&ann, &key, tiny_bridge(newer_id, 5)).await);
    let mut rejected = tiny_bridge(Uuid::new_v4(), 2);
    rejected.set_applied_seq(2);

    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    for seq in 1..=2 {
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                          (seq, namespace, embedding_model, kind, field, subject_id, op) \
                          VALUES (?1, 'local', ?2, 'note', 'note.content', ?3, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Integer(seq),
                    SqlValue::Text(model.into()),
                    SqlValue::Text(format!("subject-{seq}")),
                ],
                label: Some("test_pathless_checkpoint_tail".into()),
            })
            .await
            .expect("insert tail row");
    }
    drop(writer);

    assert!(
        !checkpoint_raise_compact_readopt(
            &rt,
            &ann,
            &key,
            model,
            rejected,
            CheckpointPublication {
                generation: 2,
                epoch: 0,
                authority: WatermarkAuthority::PendingOrActive,
            },
        )
        .await,
        "a rejected generation must abort pathless publication"
    );
    assert_eq!(
        read_own_watermark(&rt, model)
            .await
            .expect("read watermark"),
        Some(PENDING_WATERMARK),
        "rejection must preserve the closed pending watermark"
    );
    let mut reader = sql.reader().await.expect("reader");
    let retained = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log WHERE embedding_model = ?1".into(),
            params: vec![SqlValue::Text(model.into())],
            label: Some("test_pathless_checkpoint_retained_tail".into()),
        })
        .await
        .expect("count retained tail");
    match retained {
        Some(SqlValue::Integer(2)) => {}
        other => panic!("rejected publication must retain both tail rows, got {other:?}"),
    }
    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("newer bridge remains installed");
    assert_eq!(bridge.generation, 5);
    assert_eq!(bridge.id_map, vec![newer_id]);
}

/// A pathless full scan after compaction must retain the active floor even though the log's MAX(seq) reset to zero.
#[tokio::test]
async fn pathless_full_checkpoint_inherits_compacted_active_floor() {
    const MODEL: &str = "pathless-compacted-active-floor";
    let rt = KhiveRuntime::memory().expect("runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 4,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    for seq in 1..=2 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("pathless compacted-floor note {seq}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
        bump_generation(&ann, &key).await;
    }

    let first = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("first full checkpoint");
    assert!(
        matches!(first, AnnEnsureStatus::Built { vectors: 2 }),
        "the first checkpoint must cover both writes, got {first:?}"
    );
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read active watermark"),
        Some(2)
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let retained = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log WHERE embedding_model = ?1".into(),
            params: vec![SqlValue::Text(MODEL.into())],
            label: Some("test_pathless_compacted_floor_empty_log".into()),
        })
        .await
        .expect("count retained tail");
    assert!(
        matches!(retained, Some(SqlValue::Integer(0))),
        "the first checkpoint must compact its retained log, got {retained:?}"
    );
    drop(reader);

    // Remove the ephemeral cache to force a full scan without appending
    // a log row; a generation-only bump now uses incremental maintenance.
    clear_key(&ann, &key).await;
    bump_generation(&ann, &key).await;
    let second = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("cache-miss full checkpoint");
    assert!(
        matches!(second, AnnEnsureStatus::Built { vectors: 2 }),
        "the later full scan must remain publishable, got {second:?}"
    );
    assert!(is_current(&ann, &key).await);

    let (_hits, applied) = search_loaded_with_seq(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
        .await
        .expect("search installed bridge")
        .expect("bridge remains installed");
    assert_eq!(applied, 2, "the bridge must advertise the inherited floor");
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read retained active watermark"),
        Some(2)
    );
}

/// A recall in the pathless install-before-activation window must wait and revalidate, not evict the pending candidate.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn pathless_pending_reader_waits_for_checkpoint_activation() {
    const MODEL: &str = "pathless-pending-publication-wait";
    let rt = KhiveRuntime::memory().expect("runtime");
    provision_test_vector_store(&rt, MODEL, 4);
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    register_consumer(&rt, MODEL)
        .await
        .expect("register pending consumer");

    let candidate_id = Uuid::new_v4();
    let mut candidate = tiny_bridge(candidate_id, 1);
    candidate.set_applied_seq(2);
    assert!(install_replacing(&ann, &key, candidate).await);

    let publication_lock = model_warm_lock(&ann, &key).await;
    let publication_guard = publication_lock.lock().await;
    let waiting = ann.pathless_pending_publication_wait.notified();
    let task_rt = rt.clone();
    let task_ann = ann.clone();
    let task_key = key.clone();
    let reader = tokio::spawn(async move {
        fresh_tail_leg(
            &task_rt,
            &task_ann,
            &task_key,
            MODEL,
            &[1.0, 0.0, 0.0, 0.0],
            1,
            Some(2),
        )
        .await
    });
    waiting.await;
    assert!(
        !reader.is_finished(),
        "the pending reader must be blocked behind checkpoint publication"
    );

    raise_watermark_with_authority(&rt, MODEL, 2, WatermarkAuthority::PendingOrActive)
        .await
        .expect("activate checkpoint");
    drop(publication_guard);

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), reader)
        .await
        .expect("reader must resume after activation")
        .expect("reader task must not panic");
    assert!(matches!(outcome, FreshTailOutcome::Ops(_)));
    let installed = ann.indexes.read().await;
    let bridge = installed
        .get(&key)
        .expect("activation must preserve the installed candidate");
    assert_eq!(bridge.id_map, vec![candidate_id]);
}

/// If publication loses its registration while a reader waits, revalidation must still evict and return empty.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn pathless_pending_reader_evicts_after_registration_loss() {
    const MODEL: &str = "pathless-pending-publication-loss";
    let rt = KhiveRuntime::memory().expect("runtime");
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    register_consumer(&rt, MODEL)
        .await
        .expect("register pending consumer");

    let mut candidate = tiny_bridge(Uuid::new_v4(), 1);
    candidate.set_applied_seq(2);
    assert!(install_replacing(&ann, &key, candidate).await);

    let publication_lock = model_warm_lock(&ann, &key).await;
    let publication_guard = publication_lock.lock().await;
    let waiting = ann.pathless_pending_publication_wait.notified();
    let task_rt = rt.clone();
    let task_ann = ann.clone();
    let task_key = key.clone();
    let reader = tokio::spawn(async move {
        fresh_tail_leg(
            &task_rt,
            &task_ann,
            &task_key,
            MODEL,
            &[1.0, 0.0, 0.0, 0.0],
            1,
            Some(2),
        )
        .await
    });
    waiting.await;
    assert!(!reader.is_finished());

    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                .into(),
            params: vec![
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
                SqlValue::Text(MODEL.into()),
            ],
            label: Some("test_pathless_pending_registration_loss".into()),
        })
        .await
        .expect("simulate pending registration retirement");
    drop(writer);
    drop(publication_guard);

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), reader)
        .await
        .expect("reader must resume after publication ends")
        .expect("reader task must not panic");
    match outcome {
        FreshTailOutcome::Replace(hits, _) => assert!(hits.is_empty()),
        FreshTailOutcome::Ops(_) | FreshTailOutcome::Skipped(_) => {
            panic!("registration loss must replace captured candidates")
        }
    }
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "registration loss must evict the unprotected candidate"
    );
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read re-registration"),
        Some(PENDING_WATERMARK),
        "the returning consumer must re-register closed"
    );
}

/// A slower process must check the durable watermark under the segment lock before writing files, or it can overwrite a newer commit.
#[tokio::test]
async fn file_backed_stale_checkpoint_does_not_overwrite_newer_segment() {
    const MODEL: &str = "file-backed-stale-checkpoint";
    let rt = test_runtime_with_hash_embedder(MODEL, 4);
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    register_consumer(&rt, MODEL)
        .await
        .expect("register pending consumer");

    let winner_id = Uuid::new_v4();
    let mut winner = tiny_bridge(winner_id, 9);
    winner.set_applied_seq(9);
    assert!(
        checkpoint_raise_compact_readopt(
            &rt,
            &ann,
            &key,
            MODEL,
            winner,
            CheckpointPublication {
                generation: 9,
                epoch: 0,
                authority: WatermarkAuthority::PendingOrActive,
            },
        )
        .await,
        "the first checkpoint must activate the pending registration"
    );

    let mut stale = tiny_bridge(Uuid::new_v4(), 3);
    stale.set_applied_seq(3);
    assert!(
        !checkpoint_raise_compact_readopt(
            &rt,
            &ann,
            &key,
            MODEL,
            stale,
            CheckpointPublication {
                generation: 3,
                epoch: 0,
                authority: WatermarkAuthority::Active,
            },
        )
        .await,
        "a checkpoint behind the durable watermark must lose before persistence"
    );

    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
    let commit = read_commit_info(&dir)
        .expect("read commit")
        .expect("persisted commit");
    assert_eq!(commit.last_applied_seq, Some(9));
    let persisted = AnnBridge::load(&dir).expect("load winner segment");
    assert_eq!(persisted.id_map, vec![winner_id]);
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read durable watermark"),
        Some(9)
    );
}

/// A failed replacement persist must not discard the still-protected incumbent while the retained tail remains.
#[tokio::test]
async fn file_backed_persist_failure_preserves_active_incumbent() {
    const MODEL: &str = "file-backed-persist-failure-fallback";
    let rt = test_runtime_with_hash_embedder(MODEL, 4);
    let ann = new_shared();
    let key = AnnKey::new(MODEL);
    register_consumer(&rt, MODEL)
        .await
        .expect("register pending consumer");
    raise_watermark_with_authority(&rt, MODEL, 1, WatermarkAuthority::PendingOrActive)
        .await
        .expect("activate incumbent checkpoint");

    let incumbent_id = Uuid::new_v4();
    let mut incumbent = tiny_bridge(incumbent_id, 1);
    incumbent.set_applied_seq(1);
    assert!(install_replacing(&ann, &key, incumbent).await);

    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");
    std::fs::create_dir_all(&dir).expect("create segment directory");
    std::fs::create_dir(dir.join("metadata.bin"))
        .expect("block metadata file publication with a directory");

    let mut replacement = tiny_bridge(Uuid::new_v4(), 2);
    replacement.set_applied_seq(2);
    assert!(
        !checkpoint_raise_compact_readopt(
            &rt,
            &ann,
            &key,
            MODEL,
            replacement,
            CheckpointPublication {
                generation: 2,
                epoch: 0,
                authority: WatermarkAuthority::Active,
            },
        )
        .await,
        "the deliberately blocked persist must fail publication"
    );

    let installed = ann.indexes.read().await;
    let bridge = installed
        .get(&key)
        .expect("active incumbent must survive replacement persistence failure");
    assert_eq!(bridge.generation, 1);
    assert_eq!(bridge.id_map, vec![incumbent_id]);
}

/// A strictly newer candidate replaces the installed older generation.
#[tokio::test]
async fn install_replacing_replaces_older_installed_entry() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    let older_id = Uuid::new_v4();
    let newer_id = Uuid::new_v4();

    install_replacing(&ann, &key, tiny_bridge(older_id, 1)).await;
    install_replacing(&ann, &key, tiny_bridge(newer_id, 9)).await;

    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("an entry must be installed");
    assert_eq!(bridge.generation, 9);
    assert_eq!(bridge.id_map, vec![newer_id]);
}

/// Equal generations replace: under the single-flight model lock a tie is an ordered later step of the same warm task.
#[tokio::test]
async fn install_replacing_replaces_on_equal_generation() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    let first_id = Uuid::new_v4();
    let second_id = Uuid::new_v4();

    install_replacing(&ann, &key, tiny_bridge(first_id, 3)).await;
    install_replacing(&ann, &key, tiny_bridge(second_id, 3)).await;

    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("an entry must be installed");
    assert_eq!(
        bridge.id_map,
        vec![second_id],
        "on an equal generation, the later ordered install must replace"
    );
}

/// An installed generation behind the write counter is not current.
#[tokio::test]
async fn is_current_false_when_installed_generation_behind_counter() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");

    // Install a bridge stamped with generation 1 (as if built before any
    // write bumped the counter further).
    install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 1)).await;
    assert!(
        is_current(&ann, &key).await,
        "with no bumps yet, generation-1 must be considered current (counter starts at 0)"
    );

    // A write lands and bumps the counter past the installed generation.
    bump_generation(&ann, &key).await; // -> 1
    bump_generation(&ann, &key).await; // -> 2
    assert!(
        !is_current(&ann, &key).await,
        "installed generation (1) is now behind the write-generation counter (2)"
    );

    // Once a fresher build (generation >= 2) installs, it is current again.
    install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 2)).await;
    assert!(
        is_current(&ann, &key).await,
        "installed generation (2) now matches the write-generation counter (2)"
    );
}

/// `is_current` on an absent key is false, a genuine cache miss that falls through to the ensure/build path.
#[tokio::test]
async fn is_current_false_when_absent() {
    let ann = new_shared();
    let key = AnnKey::new("model-x");
    assert!(!is_current(&ann, &key).await);
}

// Even an empty-corpus attempt must emit one complete phase pair.
#[tokio::test]
async fn ensure_ann_for_model_emits_phase_started_and_completed_events() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let ann = new_shared();
    let model = "ann-warm-phase-event-test-model";

    let status = ensure_ann_for_model(&rt, &token, &ann, model)
        .await
        .expect("ensure_ann_for_model must succeed on an empty corpus");
    assert!(matches!(status, AnnEnsureStatus::EmptyCorpus));

    let store = rt.events(&token).expect("event store for local namespace");
    let page = store
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::types::PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("query_events");

    let started = page
        .items
        .iter()
        .filter(|e| e.kind == khive_types::EventKind::PhaseStarted)
        .count();
    let completed = page
        .items
        .iter()
        .filter(|e| e.kind == khive_types::EventKind::PhaseCompleted)
        .count();
    let cancelled = page
        .items
        .iter()
        .filter(|e| e.kind == khive_types::EventKind::PhaseCancelled)
        .count();
    assert_eq!(started, 1, "exactly one PhaseStarted row, got: {page:?}");
    assert_eq!(
        completed, 1,
        "exactly one PhaseCompleted row, got: {page:?}"
    );
    assert_eq!(cancelled, 0, "no PhaseCancelled row on a normal completion");
}

// Concurrent callers share one model warm and therefore one phase pair.
#[tokio::test]
#[serial(background_tasks)]
async fn ensure_ann_for_model_concurrent_callers_emit_one_phase_pair() {
    use async_trait::async_trait;
    use khive_runtime::{EmbedderProvider, RuntimeConfig};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    struct HashVecService {
        dims: usize,
    }

    fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0001_0000_01b3);
        }
        let mut v = Vec::with_capacity(dims);
        let mut s = h;
        for _ in 0..dims {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
        }
        v
    }

    #[async_trait]
    impl EmbeddingService for HashVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "hash-vec"
        }
    }

    struct HashVecProvider {
        model_name: String,
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for HashVecProvider {
        fn name(&self) -> &str {
            &self.model_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(HashVecService { dims: self.dims }))
        }
    }

    let tmp = tempfile::Builder::new()
        .prefix("khive-memory-ann-single-flight-")
        .tempdir_in(std::env::temp_dir())
        .expect("temp db dir");
    let db_path = tmp.path().join("khive-graph.db");

    const MODEL: &str = "ann-warm-single-flight-test-model";
    const DIMS: usize = 16;

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

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..16u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("ann single-flight note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    let ann = new_shared();

    // Two concurrent callers warming the same model, mirroring boot warm
    // racing a recall-miss warm for the same key.
    let (r1, r2) = tokio::join!(
        ensure_ann_for_model(&rt, &token, &ann, MODEL),
        ensure_ann_for_model(&rt, &token, &ann, MODEL)
    );
    r1.expect("first caller must succeed");
    r2.expect("second caller must succeed");

    assert!(
        ann.indexes
            .read()
            .await
            .contains_key(&AnnKey::from_token(MODEL)),
        "the model must end up warm regardless of which caller built it"
    );

    let store = rt.events(&token).expect("event store for local namespace");
    let page = store
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::types::PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("query_events");

    let started = page
        .items
        .iter()
        .filter(|e| e.kind == khive_types::EventKind::PhaseStarted)
        .count();
    let completed = page
        .items
        .iter()
        .filter(|e| e.kind == khive_types::EventKind::PhaseCompleted)
        .count();
    assert_eq!(
        started, 1,
        "exactly one caller must emit PhaseStarted for the same model, got: {page:?}"
    );
    assert_eq!(
        completed, 1,
        "exactly one caller must emit PhaseCompleted for the same model, got: {page:?}"
    );
}

// The process-wide counter proves daemon shutdown can drain the tracked warm.
#[tokio::test]
#[serial(background_tasks)]
async fn ensure_ann_background_registers_a_tracked_task_not_a_bare_spawn() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let ann = new_shared();
    let model = "ann-warm-tracked-test-model";

    let before = khive_runtime::background_task_count();
    let started = ensure_ann_background(&rt, &token, &ann, model).await;
    assert!(
        started,
        "first call for a fresh key must start a background warm"
    );
    assert!(
        khive_runtime::background_task_count() > before,
        "track_background_task's counter must reflect the new warm \
             immediately after enqueue (the increment is synchronous), \
             proving ensure_ann_background is tracked rather than a bare \
             tokio::spawn invisible to drain()"
    );

    // Let the tracked task finish so it doesn't leak into another test's
    // counter snapshot.
    for _ in 0..200 {
        if khive_runtime::background_task_count() <= before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[test]
fn cancelled_store_join_emits_phase_cancelled() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("single-worker runtime");
    executor.block_on(async {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).expect("worker started");
            release_rx.recv().expect("release blocker");
        });
        started_rx.await.expect("blocking slot is occupied");
        let queued = tokio::task::spawn_blocking(|| Ok(AnnEnsureStatus::EmptyCorpus));
        queued.abort();
        release_tx.send(()).expect("release blocking slot");
        blocker.await.expect("blocker joined");
        let result = crate::store_access::join_store_task("memory.ann.vector_store", queued).await;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        emit_ann_warm_terminal_phase(
            &rt,
            &token,
            "cancelled-store-join",
            &result,
            1,
            None,
            AnnWarmDetails::default(),
        )
        .await;
        let page = rt
            .events(&token)
            .expect("event store")
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("terminal events");
        assert_eq!(page.items.len(), 1, "exactly one terminal event: {page:?}");
        assert_eq!(
            page.items[0].kind,
            khive_types::EventKind::PhaseCancelled,
            "a cancelled acquisition join must emit PhaseCancelled, not PhaseCompleted: {result:?}"
        );
    });
}

#[tokio::test]
async fn is_benign_shutdown_cancellation_accepts_cancelled_join_error() {
    // A real cancelled JoinError, produced the same way tokio produces
    // one internally when spawn_blocking's task is aborted at runtime
    // teardown — not a synthetic stand-in.
    let handle = tokio::spawn(std::future::pending::<()>());
    handle.abort();
    let join_err = handle
        .await
        .expect_err("aborted task must yield a JoinError");
    assert!(
        join_err.is_cancelled(),
        "sanity: abort() must produce a cancelled JoinError"
    );

    let err = RuntimeError::Storage(StorageError::driver(
        khive_storage::StorageCapability::Vectors,
        "vec_count",
        join_err,
    ));
    assert!(
        is_benign_shutdown_cancellation(&err),
        "a cancelled JoinError boxed inside a Driver error must classify as benign"
    );
}

#[tokio::test]
async fn is_benign_shutdown_cancellation_rejects_panicked_join_error() {
    // A JoinError from a genuine panic is a different failure mode than
    // cancellation (`is_cancelled()` is false for panics) and must not be
    // swallowed as benign.
    let handle = tokio::spawn(async { panic!("intentional panic for classification test") });
    let join_err = handle
        .await
        .expect_err("panicked task must yield a JoinError");
    assert!(
        join_err.is_panic(),
        "sanity: this JoinError must be a panic, not a cancellation"
    );

    let err = RuntimeError::Storage(StorageError::driver(
        khive_storage::StorageCapability::Vectors,
        "vec_count",
        join_err,
    ));
    assert!(
        !is_benign_shutdown_cancellation(&err),
        "a panicked (not cancelled) JoinError must not be classified as benign"
    );
}

#[test]
fn is_benign_shutdown_cancellation_rejects_genuine_driver_error() {
    // A real backend failure (not a JoinError at all) must still WARN —
    // the predicate must not treat every Driver error as benign.
    let io_err = std::io::Error::other("disk full");
    let err = RuntimeError::Storage(StorageError::driver(
        khive_storage::StorageCapability::Vectors,
        "vec_count",
        io_err,
    ));
    assert!(
        !is_benign_shutdown_cancellation(&err),
        "a genuine driver error must never be classified as benign shutdown cancellation"
    );
}

#[test]
fn is_benign_shutdown_cancellation_rejects_non_storage_error() {
    // Guards the outer match arm: a RuntimeError variant unrelated to
    // storage must never be misclassified as a benign cancellation.
    let err = RuntimeError::Internal("unrelated internal error".into());
    assert!(!is_benign_shutdown_cancellation(&err));
}

// ── #812: warming guard must release on every exit ─────────────────────

struct HashVecService {
    dims: usize,
}

fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0001_0000_01b3);
    }
    let mut v = Vec::with_capacity(dims);
    let mut s = h;
    for _ in 0..dims {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
    }
    v
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for HashVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "hash-vec"
    }
}

struct HashVecProvider {
    model_name: String,
    dims: usize,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for HashVecProvider {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(Arc::new(HashVecService { dims: self.dims }))
    }
}

/// Provision the real sqlite-vec store that backs a manually installed
/// test bridge. Production bridges are built from an existing store; tests
/// that install one directly must preserve that schema invariant.
fn provision_test_vector_store(rt: &KhiveRuntime, model: &str, dims: usize) {
    rt.register_embedder(HashVecProvider {
        model_name: model.to_owned(),
        dims,
    });
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize vector-store fixture");
    drop(
        rt.vectors_for_model(&token, model)
            .expect("provision vector-store fixture"),
    );
}

fn test_runtime_with_hash_embedder(model: &str, dims: usize) -> TestRuntime {
    let tmp = tempfile::Builder::new()
        .prefix("khive-memory-ann-test-")
        .tempdir_in(std::env::temp_dir())
        .expect("temp db dir");
    let db_path = tmp.path().join("khive-graph.db");
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..khive_runtime::RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(HashVecProvider {
        model_name: model.to_owned(),
        dims,
    });
    TestRuntime {
        runtime: rt,
        _temp_dir: tmp,
    }
}

#[tokio::test]
async fn session_exact_snapshot_proves_receipt_with_candidates_and_zero_hits() {
    const MODEL: &str = "session-exact-proof-model";
    const CONTENT: &str = "distinctive session exact proof memory";
    let rt = test_runtime_with_hash_embedder(MODEL, 8);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let (note, fences) = rt
        .create_note_with_decay_for_embedding_model_with_visibility(
            &token,
            "memory",
            None,
            CONTENT,
            Some(0.8),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("write note and exact visibility receipt");
    let seq = fences
        .iter()
        .find(|(model, _)| model == MODEL)
        .map(|(_, seq)| *seq)
        .expect("model fence");
    let query = fnv_to_vec(CONTENT, 8);
    let candidates =
        session_exact_candidates(&rt, MODEL, &query, &["local".into()], "local", seq, 10)
            .await
            .expect("one-statement exact read")
            .expect("matching receipt is proven");
    assert!(
        candidates
            .iter()
            .any(|(id, score)| *id == note.id && *score > 0.95),
        "candidate-producing read must include the recent matching note: {candidates:?}"
    );

    let empty = session_exact_candidates(&rt, MODEL, &query, &[], "local", seq, 10)
        .await
        .expect("empty-visible-set read")
        .expect("the same statement still proves the receipt");
    assert!(empty.is_empty(), "proof must survive zero candidates");
    let zero_limit =
        session_exact_candidates(&rt, MODEL, &query, &["local".into()], "local", seq, 0)
            .await
            .expect("zero-limit read")
            .expect("zero limit still proves the receipt");
    assert!(zero_limit.is_empty());
}

#[tokio::test]
async fn session_exact_snapshot_refuses_wrong_namespace_model_and_future_fence() {
    const MODEL: &str = "session-exact-wrong-receipt-model";
    let rt = test_runtime_with_hash_embedder(MODEL, 8);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let (_note, fences) = rt
        .create_note_with_decay_for_embedding_model_with_visibility(
            &token,
            "memory",
            None,
            "session exact wrong receipt",
            Some(0.8),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("write note and exact visibility receipt");
    let seq = fences[0].1;
    let query = fnv_to_vec("session exact wrong receipt", 8);
    let visible = ["local".into()];
    assert!(
        session_exact_candidates(&rt, MODEL, &query, &visible, "other", seq, 10)
            .await
            .expect("namespace mismatch is not a read error")
            .is_none()
    );
    assert!(
        session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq + 1, 10)
            .await
            .expect("future fence is not a read error")
            .is_none()
    );

    let mut writer = rt.sql().writer().await.expect("SQL writer");
    writer
        .execute(SqlStatement {
            sql: "UPDATE ann_write_log SET op = 'delete' WHERE seq = ?1".into(),
            params: vec![SqlValue::Integer(seq as i64)],
            label: Some("session_exact_wrong_op_fixture".into()),
        })
        .await
        .expect("alter fixture receipt operation");
    drop(writer);
    assert!(
        session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq, 10)
            .await
            .expect("delete receipt is not a read error")
            .is_none()
    );

    let mut writer = rt.sql().writer().await.expect("SQL writer");
    writer
            .execute(SqlStatement {
                sql: "UPDATE ann_write_log SET op = 'upsert', embedding_model = 'other-model' WHERE seq = ?1"
                    .into(),
                params: vec![SqlValue::Integer(seq as i64)],
                label: Some("session_exact_wrong_model_fixture".into()),
            })
            .await
            .expect("alter fixture receipt model");
    drop(writer);
    assert!(
        session_exact_candidates(&rt, MODEL, &query, &visible, "local", seq, 10)
            .await
            .expect("model mismatch is not a read error")
            .is_none()
    );
}

/// A completed warm releases its guard so a later write can trigger another rebuild.
#[tokio::test]
#[serial(background_tasks)]
async fn ensure_ann_background_releases_warming_guard_after_success_and_allows_later_rebuild() {
    const MODEL: &str = "ann-warm-guard-release-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..4u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("warming guard note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);

    assert!(
        ensure_ann_background(&rt, &token, &ann, MODEL).await,
        "first call for a fresh key must start a background warm"
    );
    // Wait for the tracked task to fully exit (guard dropped), not merely
    // for the index to appear — the task still does async phase-event
    // bookkeeping after `install_if_fresher` and before returning, so
    // polling on index presence alone races the guard's release.
    for _ in 0..300 {
        if !ann.warming.lock().unwrap().contains(&key) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !ann.warming.lock().unwrap().contains(&key),
        "the warming guard must be released once the first background warm \
             finishes, not left set forever after a success (#812)"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "the first background warm must install an index"
    );

    // A second write lands: bump the generation exactly like
    // `memory.remember` does, then request another background warm.
    bump_generation(&ann, &key).await;
    assert!(
        ensure_ann_background(&rt, &token, &ann, MODEL).await,
        "a write landing after a completed warm must be able to schedule a \
             new background rebuild — if the guard were still set from the \
             first warm this would wrongly return false, and every later \
             recall would keep serving the now-stale index forever"
    );

    for _ in 0..300 {
        if is_current(&ann, &key).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        is_current(&ann, &key).await,
        "the second background warm must eventually install a fresh entry"
    );
}

// ── ADR-079 Amendment 1: write-log restart classification ──────────────

/// A same-cardinality replacement must classify Stale-tail and replay, never trust the segment Hot.
#[tokio::test]
async fn ensure_ann_for_model_restart_same_cardinality_replacement_replays_tail() {
    const MODEL: &str = "ann-warm-restart-signal-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut note_ids = Vec::new();
    for i in 0..4u32 {
        let note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("restart signal note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        note_ids.push(note.id);
    }

    // First "process": warm and persist a snapshot over the initial
    // 4-note corpus.
    let ann1 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
        .await
        .expect("first warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected a fresh build over 4 vectors, got: {status:?}"
    );

    // Delete one note and add a fresh one: vector count and dimensions
    // both come back unchanged (still 4, still DIMS), but the corpus
    // content has moved on.
    assert!(
        rt.delete_note(&token, note_ids[0], false)
            .await
            .expect("soft delete"),
        "soft delete must succeed"
    );
    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "restart signal note REPLACEMENT",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create replacement note");

    // "Restart": a fresh `AnnState` with generations reset to 0, exactly
    // like a process restart — write-generation tracking (#750) cannot
    // see this corpus change at all, so only restart validation against
    // the persisted snapshot can catch it.
    let ann2 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
        .await
        .expect("post-restart warm");
    assert!(
        matches!(
            status,
            AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
        ),
        "a same-cardinality corpus content change must be detected via the \
             write-log tail (Stale-tail replay, or Stale-rebuild when the tail \
             exceeds the threshold) rather than silently classifying Hot, got: {status:?}"
    );
    // The replacement note's write left a tail row, so a Hot adoption of
    // the pre-change segment (which would report LoadedSnapshot WITHOUT
    // containing the replacement) is ruled out by searching for it.
    let key = AnnKey::from_token(MODEL);
    let query = fnv_to_vec("restart signal note REPLACEMENT", DIMS);
    let hits = search_loaded(&ann2, &key, &query, 5)
        .await
        .expect("search must succeed")
        .expect("index must be installed");
    assert!(
        hits.iter().any(|(_, score)| *score > 0.99),
        "the replayed index must contain the replacement note's vector, got: {hits:?}"
    );
}

/// A vector-only re-embed's log row (ADR-107) must make a restart replay the new bytes, not classify Hot on stale ones.
#[tokio::test]
async fn ensure_ann_for_model_restart_detects_vector_only_reindex() {
    const MODEL: &str = "ann-warm-restart-vector-only-reindex-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut note_ids = Vec::new();
    for i in 0..4u32 {
        let note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("vector-only reindex note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        note_ids.push(note.id);
    }

    let ann1 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
        .await
        .expect("first warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected a fresh build over 4 vectors, got: {status:?}"
    );

    // Match reindex behavior by changing vector bytes without touching note metadata.
    {
        let table_name = format!("vec_{}", sanitize_model_key(MODEL));
        let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
        let bytes: Vec<u8> = replacement.iter().flat_map(|f| f.to_le_bytes()).collect();
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: format!(
                "UPDATE {table_name} SET embedding = ?1 \
                     WHERE subject_id = ?2 AND embedding_model = ?3"
            ),
            params: vec![
                SqlValue::Blob(bytes),
                SqlValue::Text(note_ids[0].to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_vector_only_reindex".into()),
        })
        .await
        .expect("overwrite embedding");
        // The write-path contract requires every vector mutation to append
        // a log row; a reindexer that bypassed it would classify Hot on
        // stale bytes at the next restart.
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                .into(),
            params: vec![
                SqlValue::Text(note_ids[0].to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_vector_only_reindex_log".into()),
        })
        .await
        .expect("append reindex log row");
    }

    // "Restart": a fresh `AnnState`, generations reset to 0 — matches a
    // real restart exactly, and also matches `kkernel reindex` running
    // as a separate process from the daemon, which shares no in-memory
    // generation state with it at all.
    let ann2 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
        .await
        .expect("post-reindex warm");
    assert!(
        matches!(
            status,
            AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
        ),
        "a logged vector-only re-embed must classify as Stale (tail replay \
             or rebuild), never Hot on the pre-reindex bytes, got: {status:?}"
    );
    // The replayed index must serve the NEW embedding for the re-embedded
    // note — a Hot adoption of the stale segment would miss it.
    let key = AnnKey::from_token(MODEL);
    let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
    let hits = search_loaded(&ann2, &key, &replacement, 1)
        .await
        .expect("search must succeed")
        .expect("index must be installed");
    assert_eq!(
        hits.first().map(|(id, _)| *id),
        Some(note_ids[0]),
        "the re-embedded note must be nearest to its new vector, got: {hits:?}"
    );
    assert!(
        hits[0].1 > 0.99,
        "the served vector must be the re-embedded bytes, got score {}",
        hits[0].1
    );
}

/// A final tail upsert whose note fails the join predicate is not a contradiction; replay tombstones it instead of going Cold.
#[tokio::test]
async fn restart_tail_upsert_for_soft_deleted_note_replays_as_delete() {
    const MODEL: &str = "ann-warm-restart-join-predicate-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut note_ids = Vec::new();
    for i in 0..4u32 {
        let note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("join predicate note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        note_ids.push(note.id);
    }

    let ann1 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
        .await
        .expect("first warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected a fresh build over 4 vectors, got: {status:?}"
    );

    // A re-embed logs its upsert, then the note is soft-deleted by a path
    // that never cleans the vector row: the vec row and the final upsert
    // both survive while the join predicate now excludes the note.
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                .into(),
            params: vec![
                SqlValue::Text(note_ids[0].to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_join_predicate_log".into()),
        })
        .await
        .expect("append upsert log row");
        w.execute(SqlStatement {
            sql: "UPDATE notes SET deleted_at = created_at WHERE id = ?1".into(),
            params: vec![SqlValue::Text(note_ids[0].to_string())],
            label: Some("test_join_predicate_soft_delete".into()),
        })
        .await
        .expect("soft-delete note row without vector cleanup");
    }

    // Restart: live = 3, tail = 1 ≤ ceil(0.20 × 3) → Stale-tail replay.
    let ann2 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
        .await
        .expect("post-restart warm");
    assert!(
        matches!(status, AnnEnsureStatus::LoadedSnapshot),
        "a predicate-failing final upsert must replay as a delete within \
             Stale-tail adoption, not force a Cold rebuild, got: {status:?}"
    );
    let key = AnnKey::from_token(MODEL);
    let query = fnv_to_vec("join predicate note 0", DIMS);
    let hits = search_loaded(&ann2, &key, &query, 4)
        .await
        .expect("search must succeed")
        .expect("index must be installed");
    assert!(
        !hits
            .iter()
            .any(|(id, score)| *id == note_ids[0] && *score > 0.99),
        "the soft-deleted note must be tombstoned by the replayed delete, got: {hits:?}"
    );
}

/// A final tail upsert with no vector row at all contradicts the committed log, so replay must fall through to a Cold rebuild.
#[tokio::test]
async fn restart_tail_upsert_with_absent_vector_row_goes_cold() {
    const MODEL: &str = "ann-warm-restart-contradiction-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..4u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("contradiction note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    let ann1 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann1, MODEL)
        .await
        .expect("first warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected a fresh build over 4 vectors, got: {status:?}"
    );

    // A committed final upsert for a subject with no vector row anywhere —
    // impossible under the same-transaction write contract, so it can only
    // mean corruption. Replay must not fabricate or skip it.
    {
        let phantom = Uuid::new_v4();
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?2, 'note', 'note.content', ?1, 'upsert')"
                .into(),
            params: vec![
                SqlValue::Text(phantom.to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_contradiction_log".into()),
        })
        .await
        .expect("append phantom upsert log row");
    }

    // Restart: live = 4, tail = 1 ≤ ceil(0.20 × 4) → Stale-tail is
    // attempted, the point read finds no vector row, replay errs, and the
    // classifier falls through Cold to a full rebuild.
    let ann2 = new_shared();
    let status = ensure_ann_for_model(&rt, &token, &ann2, MODEL)
        .await
        .expect("post-restart warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "a log/corpus contradiction must force a Cold rebuild, never a \
             segment adoption, got: {status:?}"
    );
}

// ── #812: durable epoch vs. warm daemon ────────────────────────────────

/// A durable epoch exposes cross-process reindexing to a daemon's warm graph.
#[tokio::test]
async fn maybe_check_durable_epoch_detects_reindex_from_a_separate_warm_daemon() {
    const MODEL: &str = "ann-warm-durable-epoch-test-model";
    const DIMS: usize = 8;

    let tmp = tempfile::Builder::new()
        .prefix("khive-memory-ann-durable-epoch-")
        .tempdir_in(std::env::temp_dir())
        .expect("temp db dir");
    let db_path = tmp.path().join("khive-graph.db");

    // "Daemon": first runtime, warms the ANN index and stays resident —
    // exactly like a long-lived `kkernel mcp --daemon` process.
    let rt1 = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(db_path.clone()),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..khive_runtime::RuntimeConfig::default()
    })
    .expect("runtime 1");
    rt1.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token1 = rt1.authorize(Namespace::local()).expect("authorize local");

    let mut note_ids = Vec::new();
    for i in 0..4u32 {
        let note = rt1
            .create_note_with_decay_for_embedding_model(
                &token1,
                "memory",
                None,
                &format!("durable epoch note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        note_ids.push(note.id);
    }

    let ann1 = new_shared();
    let key = AnnKey::from_token(MODEL);
    let status = ensure_ann_for_model(&rt1, &token1, &ann1, MODEL)
        .await
        .expect("first warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected initial build, got: {status:?}"
    );

    // "Reindexer": a SEPARATE runtime pointed at the same DB file, like
    // `kkernel reindex` invoked while the daemon above stays warm.
    let rt2 = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..khive_runtime::RuntimeConfig::default()
    })
    .expect("runtime 2");
    rt2.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });

    // Vector-only re-embed, bypassing the notes table entirely — same
    // shape as `reindex.rs`'s `embed_and_store_batch`.
    {
        let table_name = format!("vec_{}", sanitize_model_key(MODEL));
        let replacement: Vec<f32> = (0..DIMS).map(|i| (i as f32 + 100.0) / 7.0).collect();
        let bytes: Vec<u8> = replacement.iter().flat_map(|f| f.to_le_bytes()).collect();
        let sql = rt2.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: format!(
                "UPDATE {table_name} SET embedding = ?1 \
                     WHERE subject_id = ?2 AND embedding_model = ?3"
            ),
            params: vec![
                SqlValue::Blob(bytes),
                SqlValue::Text(note_ids[0].to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_durable_epoch_vector_reindex".into()),
        })
        .await
        .expect("overwrite embedding");
        // Contract-mandated log append for the vector overwrite (ADR-107
        // supersession note) — the classifier replays this row after the
        // epoch bump invalidates the warm cache.
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      SELECT n.namespace, ?2, 'note', 'note.content', ?1, 'upsert' \
                      FROM notes n WHERE n.id = ?1"
                .into(),
            params: vec![
                SqlValue::Text(note_ids[0].to_string()),
                SqlValue::Text(MODEL.to_string()),
            ],
            label: Some("test_durable_epoch_vector_reindex_log".into()),
        })
        .await
        .expect("append reindex log row");
    }
    // Reindex schema setup is explicit because no pack registry boot runs in that process.
    ensure_epoch_schema(&rt2)
        .await
        .expect("ensure epoch schema");
    bump_durable_epoch(&rt2).await.expect("bump durable epoch");

    // Sanity: before the epoch check runs, the daemon's cache still
    // (wrongly) considers itself fresh — its in-memory generation was
    // never touched by `rt2`'s write.
    assert!(
        is_current(&ann1, &key).await,
        "sanity: the daemon's cache must still consider itself fresh before \
             the durable-epoch check runs"
    );
    maybe_check_durable_epoch(&rt1, &ann1, &key).await;
    assert!(
        !is_current(&ann1, &key).await,
        "the amortized durable-epoch check must detect a cross-process \
             reindex and mark the warm daemon's cached entry stale (#812)"
    );

    let status = ensure_ann_for_model(&rt1, &token1, &ann1, MODEL)
        .await
        .expect("rebuild after epoch mismatch");
    assert!(
        matches!(
            status,
            AnnEnsureStatus::LoadedSnapshot | AnnEnsureStatus::Built { .. }
        ),
        "the warm daemon must re-adopt (tail replay or rebuild) once its \
             durable-epoch check detects the out-of-process reindex, got: {status:?}"
    );
}

// ── #812: high-water re-enqueue on drop ────────────────────────────────

/// An in-flight warm re-enqueues itself when a later write advances its generation floor.
/// See `crates/khive-pack-memory/docs/recall-reliability.md`.
#[tokio::test]
#[serial(background_tasks)]
async fn ensure_ann_background_converges_on_write_during_warm_with_no_further_recalls() {
    const MODEL: &str = "ann-warm-medium-reenqueue-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..8u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("medium re-enqueue note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);

    // The two-way barrier orders the write after the task captures its first floor.
    ann.attempt_floor_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let notified = ann.attempt_floor_notify.notified();
    assert!(
        ensure_ann_background(&rt, &token, &ann, MODEL).await,
        "first call for a fresh key must start a background warm"
    );
    // Wait for the tracked task to actually commit to its first
    // attempt's generation floor before bumping — this is the barrier
    // that replaces the old "300 notes should be slow enough" gamble.
    notified.await;
    // Simulate a write racing in while the warm above is still building —
    // bump the generation exactly like `memory.remember` does, but
    // deliberately do NOT call `ensure_ann_background` again: the whole
    // point is that no second caller ever arrives to notice or retrigger.
    bump_generation(&ann, &key).await;
    // Disarm BEFORE releasing so later attempts (attempt 2, 3, ...) in
    // this same task's loop don't also block waiting for a release this
    // test never sends again. `Notify::notify_one` synchronizes with
    // the waiter's wakeup, so the task observes `barrier == false` by
    // the time it re-checks on its next attempt.
    ann.attempt_floor_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.attempt_floor_release.notify_one();

    for _ in 0..500 {
        if is_current(&ann, &key).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        is_current(&ann, &key).await,
        "a write racing in during an in-flight warm must eventually be \
             picked up and converge on its own, with zero further recalls or \
             writes to retrigger it (#812)"
    );
}

// ── ADR-118: fresh-tail exact leg ───────────────────────────────────────

/// The no-index fallback follows ADR-118's ceil(threshold × live corpus) ceiling, not a flat row cap (#1161).
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_no_index_cap_tracks_live_corpus_fraction() {
    const MODEL: &str = "adr118-corpus-relative-cap-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut ids = Vec::new();

    for i in 0..6u32 {
        let note = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("corpus-relative cap note {i}"),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create note");
        ids.push(note.id);
    }

    let ops = match fresh_tail_capped_at_threshold(&rt, MODEL, 0.20).await {
        FreshTailOutcome::Ops(ops) => ops,
        FreshTailOutcome::Replace(..) => {
            panic!("no-index capped leg must not produce replacement candidates")
        }
        FreshTailOutcome::Skipped(reason) => {
            panic!("no-index capped leg unexpectedly skipped: {reason}")
        }
    };
    let selected: Vec<Uuid> = ops.into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        selected,
        ids[4..].to_vec(),
        "ceil(0.20 × 6) must select the two newest raw log rows"
    );
}

/// A skip that discards an error must carry it out to the caller: the
/// label alone cannot say whether the store lost its write log or was
/// merely busy, and retry-vs-rebuild turns on that difference. The
/// control arm is the same call before the fault, which must not skip —
/// otherwise this passes on a leg that sits out unconditionally.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_capped_leg_carries_its_read_error_into_the_skip_reason() {
    const MODEL: &str = "adr118-skip-reason-carries-error-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("skip reason fixture note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create note");
    }

    match fresh_tail_capped_at_threshold(&rt, MODEL, 1.0).await {
        FreshTailOutcome::Ops(ops) => assert!(
            !ops.is_empty(),
            "control: a healthy store must replay the seeded tail"
        ),
        FreshTailOutcome::Replace(..) => {
            panic!("the no-index capped leg never replaces candidates")
        }
        FreshTailOutcome::Skipped(reason) => {
            panic!("control: a healthy store must not skip, got: {reason}")
        }
    }

    // Fault injection: remove the write-log table the leg's existence
    // probe reads, so its next call fails for a nameable reason. (Test
    // fixture database, mirroring the DROP TABLE fault pattern already
    // used in this module and in handlers/recall.rs.)
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("fault injection writer");
        w.execute(SqlStatement {
            sql: "DROP TABLE ann_write_log".into(),
            params: vec![],
            label: Some("test_drop_write_log_for_skip_reason".into()),
        })
        .await
        .expect("drop the write-log table");
    }

    match fresh_tail_capped_at_threshold(&rt, MODEL, 1.0).await {
        FreshTailOutcome::Skipped(reason) => {
            assert_eq!(
                reason.label(),
                "fresh-tail: tail-existence read failed",
                "the failure-site label must be unchanged by the enrichment"
            );
            let detail = reason
                .detail()
                .expect("a skip constructed while holding an error must carry it");
            assert!(
                detail.contains("ann_write_log"),
                "the carried error must name what actually failed, got: {detail:?}"
            );
            let rendered = reason.to_string();
            assert!(
                rendered.starts_with(reason.label()) && rendered.contains(detail),
                "the served reason must carry the label and the error, got: {rendered:?}"
            );
        }
        FreshTailOutcome::Ops(_) | FreshTailOutcome::Replace(..) => {
            panic!("a missing write-log table must make the capped leg sit out")
        }
    }
}

/// The policy-disabled arm holds no error, and must keep emitting exactly
/// the bare label it always did — through the real leg, not only through
/// a hand-built value. The control arm is the same fixture with the leg
/// enabled, which must not skip at all.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_disabled_by_policy_skips_with_a_bare_label() {
    const MODEL: &str = "adr118-disabled-policy-bare-label-test-model";
    const DIMS: usize = 8;

    async fn run_leg(fresh_tail_enabled: bool) -> FreshTailOutcome {
        let rt = KhiveRuntime::memory()
            .expect("in-memory runtime")
            .with_ann_fresh_tail_enabled(fresh_tail_enabled);
        provision_test_vector_store(&rt, MODEL, DIMS);
        register_consumer(&rt, MODEL)
            .await
            .expect("register this consumer");
        raise_watermark_with_authority(&rt, MODEL, 0, WatermarkAuthority::PendingOrActive)
            .await
            .expect("activate this consumer");
        let ann = new_shared();
        let key = AnnKey::new(MODEL);
        fresh_tail_leg(&rt, &ann, &key, MODEL, &[0.0_f32; DIMS], 10, Some(0)).await
    }

    match run_leg(false).await {
        FreshTailOutcome::Skipped(reason) => {
            assert!(
                reason
                    .label()
                    .starts_with("fresh-tail leg disabled by runtime policy"),
                "got: {}",
                reason.label()
            );
            assert_eq!(
                reason.detail(),
                None,
                "this site holds no error, so it must carry none"
            );
            assert_eq!(
                reason.to_string(),
                reason.label(),
                "an error-free skip must render as the bare label, with no \
                     separator and no placeholder"
            );
        }
        FreshTailOutcome::Ops(_) | FreshTailOutcome::Replace(..) => {
            panic!("a leg disabled by runtime policy must sit the query out")
        }
    }

    match run_leg(true).await {
        FreshTailOutcome::Ops(_) => {}
        FreshTailOutcome::Replace(..) => {
            panic!("control: an enabled leg over an empty log must not replace")
        }
        FreshTailOutcome::Skipped(reason) => {
            panic!("control: an enabled leg must not skip, got: {reason}")
        }
    }
}

/// A subject in the stale warm index whose final tail op is delete must be dropped from the merged list (#1828).
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_subject_whose_final_tail_op_is_delete() {
    const MODEL: &str = "adr118-tail-delete-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let target = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "tail delete target note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create target note");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("tail delete filler note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create filler note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    let status = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected initial build of 4 vectors, got: {status:?}"
    );

    // Delete AFTER the bridge is warm — a write only bumps generation, it
    // never evicts the served bridge, so its cached graph still nominates
    // the now-deleted subject.
    rt.delete_note(&token, target.id, false)
        .await
        .expect("soft delete target");

    let query = fnv_to_vec("tail delete target note", DIMS);
    let raw = search_loaded(&ann, &key, &query, 10)
        .await
        .expect("search")
        .expect("bridge still warm");
    assert!(
        raw.iter().any(|(id, _)| *id == target.id),
        "sanity: the stale warm bridge must still nominate the deleted \
             subject from its cached graph before the fresh-tail leg runs"
    );

    let s = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark");
    let ops = match fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await {
        FreshTailOutcome::Ops(ops) => ops,
        FreshTailOutcome::Replace(..) => panic!("fresh-tail leg unexpectedly re-resolved"),
        FreshTailOutcome::Skipped(reason) => {
            panic!("fresh-tail leg unexpectedly skipped: {reason}")
        }
    };
    let merged = merge_fresh_tail(raw, &query, ops);
    assert!(
        !merged.iter().any(|(id, _)| *id == target.id),
        "a subject whose final tail op is delete must be dropped from the \
             merged candidate list even though the stale ANN index still \
             nominates it, got: {merged:?}"
    );
}

/// A subject in both the stale candidates and the tail must appear once, carrying the tail's exact score.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_dedups_with_tail_winning() {
    const MODEL: &str = "adr118-tail-dedup-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let target = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "tail dedup original content",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create target note");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("tail dedup filler note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create filler note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    let status = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    assert!(
        matches!(status, AnnEnsureStatus::Built { vectors: 4 }),
        "expected initial build of 4 vectors, got: {status:?}"
    );

    const UPDATED_TEXT: &str = "tail dedup UPDATED content, unrelated to the original";
    rt.update_note_with_embedding_report(
        &token,
        target.id,
        khive_runtime::NotePatch::new(None, Some(UPDATED_TEXT.to_string()), None, None, None),
    )
    .await
    .map(|(row, _report)| row)
    .expect("update target note");

    // Query the segment's stale embedding of the ORIGINAL content: the
    // stale ANN index nominates the subject at its old (now-superseded)
    // score, while the tail carries the exact score against the updated
    // embedding — they must collapse to one entry, tail winning.
    let query = fnv_to_vec("tail dedup original content", DIMS);
    let raw = search_loaded(&ann, &key, &query, 10)
        .await
        .expect("search")
        .expect("bridge still warm");
    let stale_score = raw
        .iter()
        .find(|(id, _)| *id == target.id)
        .map(|(_, score)| *score)
        .expect("sanity: stale ANN index must still nominate the pre-update subject");

    let s = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark");
    let ops = match fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await {
        FreshTailOutcome::Ops(ops) => ops,
        FreshTailOutcome::Replace(..) => panic!("fresh-tail leg unexpectedly re-resolved"),
        FreshTailOutcome::Skipped(_) => panic!("fresh-tail leg unexpectedly skipped"),
    };
    let merged = merge_fresh_tail(raw, &query, ops);

    let matches: Vec<&(Uuid, f32)> = merged.iter().filter(|(id, _)| *id == target.id).collect();
    assert_eq!(
        matches.len(),
        1,
        "the subject must appear exactly once in the merged list, got: {merged:?}"
    );
    let exact_score = exact_cosine(&query, &fnv_to_vec(UPDATED_TEXT, DIMS));
    assert!(
        (matches[0].1 - exact_score).abs() < 1e-6,
        "the merged entry must carry the tail's exact score ({exact_score}) \
             rather than the stale segment's score ({stale_score}), got {}",
        matches[0].1
    );
}

/// A pathless recall racing a checkpoint must re-search the installed replacement, not merge a floored tail into stale candidates.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_pathless_mismatch_replaces_pre_checkpoint_candidates() {
    const MODEL: &str = "adr118-pathless-reresolve-model";
    const DIMS: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("pathless baseline note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create baseline note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("initial warm");

    const FRESH_TEXT: &str = "pathless checkpoint distinctive fresh note";
    let fresh = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            FRESH_TEXT,
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create post-checkpoint note");
    bump_generation(&ann, &key).await;

    let query = fnv_to_vec(FRESH_TEXT, DIMS);
    let (captured, s1) = search_loaded_with_seq(&ann, &key, &query, 10)
        .await
        .expect("search old bridge")
        .expect("old bridge installed");
    assert!(
        captured.iter().all(|(id, _)| *id != fresh.id),
        "the pre-checkpoint bridge must not contain the fresh note"
    );

    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("publish replacement");
    let s2 = bridge_applied_seq(&ann, &key)
        .await
        .expect("replacement watermark");
    assert!(s2 > s1, "replacement must advance the applied watermark");
    assert!(
        !tail_exists(&rt, MODEL, s1)
            .await
            .expect("read compacted tail"),
        "checkpoint compaction must remove the old bridge's intervening tail"
    );

    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
    let replacement = match outcome {
        FreshTailOutcome::Replace(hits, _) => hits,
        _ => panic!("pathless mismatch must replace stale candidates"),
    };
    assert!(
        replacement.iter().any(|(id, _)| *id == fresh.id),
        "re-resolution must recover the fresh note from the installed replacement"
    );
}

/// A negative (pending/recovering) registry minimum must not wrap to `u64::MAX` and suppress a real, fully-retained tail.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_negative_peer_minimum_keeps_real_tail_visible() {
    const MODEL: &str = "adr118-negative-minimum-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("negative minimum baseline note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create baseline note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark");
    let fresh = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "negative minimum distinctive fresh note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create tail note");
    let sql = rt.sql();
    ann_registry::register_pending(sql.as_ref(), "pending-peer", ANN_WILDCARD_NS, MODEL)
        .await
        .expect("register pending peer");

    let query = fnv_to_vec("negative minimum distinctive fresh note", DIMS);
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await;
    let ops = match outcome {
        FreshTailOutcome::Ops(ops) => ops,
        _ => panic!("negative minimum must preserve the ordinary tail scan"),
    };
    assert!(
        ops.iter()
            .any(|(id, embedding)| *id == fresh.id && embedding.is_some()),
        "the fully retained tail must include the fresh note"
    );
}

/// A pending peer's negative minimum is coherent with the installed
/// pathless bridge; re-resolution must retain its candidates and tail.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_pathless_negative_peer_minimum_merges_candidates_and_tail() {
    const MODEL: &str = "adr118-pathless-negative-minimum-model";
    const DIMS: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("pathless negative minimum baseline note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create baseline note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm pathless bridge");
    const FRESH_TEXT: &str = "pathless negative minimum distinctive fresh note";
    let fresh = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            FRESH_TEXT,
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create tail note");
    ann_registry::register_pending(rt.sql().as_ref(), "pending-peer", ANN_WILDCARD_NS, MODEL)
        .await
        .expect("register pending peer");

    let query = fnv_to_vec(FRESH_TEXT, DIMS);
    let (candidates, _) = search_loaded_with_seq(&ann, &key, &query, 10)
        .await
        .expect("search installed bridge")
        .expect("pathless bridge installed");
    assert!(!candidates.is_empty(), "bridge must supply candidates");
    assert!(
        candidates.iter().all(|(id, _)| *id != fresh.id),
        "fresh note must exist only in the tail"
    );

    let outcome = fresh_tail_pathless_reresolve(
        &rt,
        &ann,
        &key,
        MODEL,
        FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
        None,
    )
    .await;
    let merged = match outcome {
        FreshTailOutcome::Replace(hits, _) => hits,
        _ => panic!("pathless re-resolution must replace stale candidates"),
    };
    assert!(
        candidates
            .iter()
            .all(|(id, _)| merged.iter().any(|(served, _)| served == id)),
        "negative minimum must retain re-resolved bridge candidates: {merged:?}"
    );
    assert!(
        merged.iter().any(|(id, _)| *id == fresh.id),
        "negative minimum must merge the final fresh tail: {merged:?}"
    );
}

/// An active registry ahead of the only persisted base cannot establish coverage of compacted writes.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_registry_minimum_exceeds_persisted_base() {
    const MODEL: &str = "adr118-compaction-guard-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("compaction guard seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    // A write lands after the checkpoint: the log advances, but the
    // served bridge is never re-persisted (only `ensure_ann_for_model`
    // does that), so its own watermark stays pinned at `s1`.
    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "compaction guard post-checkpoint write",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create post-checkpoint note");

    // Simulate a peer process's checkpoint: raise the shared durable
    // registry watermark past `s1` and compact the log through it —
    // exactly `checkpoint_raise_compact_readopt`'s raise+compact steps,
    // without the persist/re-adopt this bridge never observes.
    let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
        .await
        .expect("scope counts before compaction");
    assert!(tail_before > 0, "sanity: a tail must exist above s1");
    let s2 = s1 + tail_before;
    raise_watermark(&rt, MODEL, s2)
        .await
        .expect("raise registry watermark past bridge watermark");
    compact_log(&rt, MODEL).await.expect("compact log");

    let generation_before = current_generation(&ann, &key).await;
    let query = fnv_to_vec("compaction guard seed note 0", DIMS);
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
    match outcome {
        FreshTailOutcome::Replace(candidates, reason) => {
            assert!(candidates.is_empty());
            assert_eq!(
                    reason,
                    Some("fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates"),
                );
        }
        FreshTailOutcome::Ops(ops) => {
            panic!("a tail above the registry minimum cannot repair stale candidates: {ops:?}")
        }
        FreshTailOutcome::Skipped(_) => {
            panic!("an unproved publication must drop stale candidates, not skip")
        }
    }
    assert!(
        current_generation(&ann, &key).await > generation_before,
        "the mismatch must force re-adoption (bump_generation) so a \
             future query gets a fresh bridge"
    );
}

/// A peer's newer persisted segment must be loaded, searched directly, and returned as `Replace`, never merged with stale candidates.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_reresolves_to_a_newer_persisted_segment_on_mismatch() {
    const MODEL: &str = "adr118-reresolve-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("reresolve seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    // A new note lands after the checkpoint.
    let fresh = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "reresolve distinctive fresh note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create fresh note");

    let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
        .await
        .expect("scope counts before peer checkpoint");
    assert!(tail_before > 0, "sanity: a tail must exist above s1");
    let s2 = s1 + tail_before;

    // Simulate a PEER PROCESS's real checkpoint: replay the tail into a
    // fresh load of the persisted segment, persist it back to disk at
    // s2, and raise+compact the registry — all WITHOUT touching this
    // process's in-memory `ann` map, which stays pinned at the stale s1
    // bridge (the mismatch this leg must detect and recover from).
    let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
    let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
    let (ops, new_s) = fetch_final_tail(&rt, MODEL, s1, None)
        .await
        .expect("fetch tail for peer replay");
    peer_bridge
        .apply_final_ops(ops, new_s)
        .expect("apply peer replay");
    peer_bridge
        .save_atomic(&dir)
        .expect("persist peer checkpoint");
    raise_watermark(&rt, MODEL, s2)
        .await
        .expect("raise registry watermark");
    compact_log(&rt, MODEL).await.expect("compact log");

    let generation_before = current_generation(&ann, &key).await;
    let query = fnv_to_vec("reresolve distinctive fresh note", DIMS);
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
    let candidates = match outcome {
        FreshTailOutcome::Replace(candidates, _) => candidates,
        FreshTailOutcome::Ops(_) => panic!(
            "expected re-resolution to replace candidates outright, not \
                 just return ops to merge into the stale bridge's candidates"
        ),
        FreshTailOutcome::Skipped(_) => {
            panic!("expected successful re-resolution, not a skip")
        }
    };
    assert!(
        candidates.iter().any(|(id, _)| *id == fresh.id),
        "the re-resolved segment's own search must surface the note \
             that only the peer's checkpoint (not this process's stale \
             bridge) reflects, got: {candidates:?}"
    );
    assert!(
        current_generation(&ann, &key).await > generation_before,
        "re-resolution must still force re-adoption so a future query \
             installs this segment as the served bridge"
    );
}

/// A delta HEAD can be valid while a chunk it names is gone. Its watermark
/// then promises a re-resolution the segment load cannot deliver, and a
/// write compacted into that chunk is in neither the stale candidates nor
/// the retained log. The leg must drop the stale candidates with a
/// disclosed reason, never skip and serve them, and never floor them.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_a_valid_delta_head_names_a_missing_chunk() {
    const MODEL: &str = "adr118-reresolve-broken-delta-chain-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("broken delta chain seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    let inside = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "broken delta chain note inside the published delta",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create delta note");

    // A peer publishes a delta checkpoint over the persisted base, then
    // raises the registry to the delta watermark and compacts through it,
    // leaving this process's in-memory bridge pinned at `s1`.
    let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
    let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
    let (ops, delta_s) = fetch_final_tail(&rt, MODEL, s1, None)
        .await
        .expect("fetch tail for peer replay");
    assert!(
        delta_s > s1,
        "sanity: the delta must cover a write above s1"
    );
    let raw_count = ops.len() as u64;
    peer_bridge
        .apply_final_ops(ops.clone(), delta_s)
        .expect("apply peer replay");
    peer_bridge.record_delta_batch(ops, delta_s, raw_count);
    assert!(
        !peer_bridge.needs_full_compaction(),
        "sanity: the peer checkpoint must publish a delta, not a full segment"
    );
    let publication = delta::write(&dir, &peer_bridge).expect("publish peer delta");
    raise_watermark(&rt, MODEL, delta_s)
        .await
        .expect("raise registry watermark to the delta watermark");
    compact_log(&rt, MODEL).await.expect("compact log");
    assert!(
        AnnBridge::load(&dir).is_ok(),
        "control: the intact chain must load before its chunk is removed"
    );

    std::fs::remove_file(dir.join(format!("memory_delta-{}.bin", publication.last_nonce)))
        .expect("remove the chunk the delta HEAD names");
    let base_seq = read_commit_info(&dir)
        .expect("read base commit")
        .and_then(|info| info.last_applied_seq)
        .expect("base watermark");
    assert_eq!(
        effective_persisted_state(&dir, base_seq)
            .expect("the delta HEAD alone is still valid")
            .0,
        delta_s,
        "precondition: the HEAD still promises the delta watermark, so the \
             mismatch preflight chooses re-resolution"
    );
    assert!(
        AnnBridge::load(&dir).is_err(),
        "precondition: the segment load must reject the broken chain"
    );
    let (retained, _) = fetch_final_tail(&rt, MODEL, s1, None)
        .await
        .expect("fetch the retained log above the bridge watermark");
    assert!(
        !retained.iter().any(|(id, _)| *id == inside.id),
        "precondition: the write inside the broken chain is gone from the \
             retained log, so no tail can restore it, got: {retained:?}"
    );

    let generation_before = current_generation(&ann, &key).await;
    let query = fnv_to_vec("broken delta chain note inside the published delta", DIMS);
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await;
    match outcome {
        FreshTailOutcome::Replace(candidates, reason) => {
            assert!(
                candidates.is_empty(),
                "the stale candidates must be dropped, got: {candidates:?}"
            );
            assert_eq!(
                reason,
                Some("fresh-tail: re-resolved segment load failed; dropped stale candidates"),
                "the drop must disclose its failure site"
            );
        }
        FreshTailOutcome::Ops(ops) => panic!(
            "a floored tail cannot restore a write compacted into the broken \
                 chain; merging it would serve the stale candidates: {ops:?}"
        ),
        FreshTailOutcome::Skipped(reason) => {
            panic!("a skip serves the stale candidates unmerged: {reason}")
        }
    }
    assert!(
        current_generation(&ann, &key).await > generation_before,
        "the drop must force re-adoption so a future query gets a fresh bridge"
    );
}

/// A persisted base at `s1` plus a peer's delta checkpoint over it, with
/// the registry raised to the delta watermark and the log compacted
/// through it: this process's bridge stays pinned at `s1`, and the write
/// inside the delta is gone from the retained log.
struct CompactedPeerDelta {
    rt: TestRuntime,
    ann: SharedAnn,
    key: AnnKey,
    s1: u64,
    delta_s: u64,
    inside: Uuid,
    dir: std::path::PathBuf,
}

async fn compacted_peer_delta(model: &str, dims: usize) -> CompactedPeerDelta {
    let rt = test_runtime_with_hash_embedder(model, dims);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("compacted peer delta seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }
    let ann = new_shared();
    let key = AnnKey::from_token(model);
    ensure_ann_for_model(&rt, &token, &ann, model)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");
    let inside = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "compacted peer delta note inside the published delta",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create delta note")
        .id;

    let dir = ann_segment_dir(&rt, model).expect("segment dir (file-backed test runtime)");
    let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
    let (ops, delta_s) = fetch_final_tail(&rt, model, s1, None)
        .await
        .expect("fetch tail for peer replay");
    assert!(
        delta_s > s1,
        "sanity: the delta must cover a write above s1"
    );
    let raw_count = ops.len() as u64;
    peer_bridge
        .apply_final_ops(ops.clone(), delta_s)
        .expect("apply peer replay");
    peer_bridge.record_delta_batch(ops, delta_s, raw_count);
    assert!(
        !peer_bridge.needs_full_compaction(),
        "sanity: the peer checkpoint must publish a delta, not a full segment"
    );
    delta::write(&dir, &peer_bridge).expect("publish peer delta");
    raise_watermark(&rt, model, delta_s)
        .await
        .expect("raise registry watermark to the delta watermark");
    compact_log(&rt, model).await.expect("compact log");
    let (retained, _) = fetch_final_tail(&rt, model, s1, None)
        .await
        .expect("fetch the retained log above the bridge watermark");
    assert!(
        !retained.iter().any(|(id, _)| *id == inside),
        "precondition: the write inside the delta is gone from the retained \
             log, so no tail can restore it, got: {retained:?}"
    );
    CompactedPeerDelta {
        rt,
        ann,
        key,
        s1,
        delta_s,
        inside,
        dir,
    }
}

/// Assert the leg dropped the stale candidates with `expected_reason` and
/// forced re-adoption.
async fn assert_dropped_stale_candidates(
    outcome: FreshTailOutcome,
    expected_reason: &'static str,
    fixture: &CompactedPeerDelta,
    generation_before: u64,
) {
    match outcome {
        FreshTailOutcome::Replace(candidates, reason) => {
            assert!(
                candidates.is_empty(),
                "the stale candidates must be dropped, got: {candidates:?}"
            );
            assert_eq!(
                reason,
                Some(expected_reason),
                "the drop must disclose its failure site"
            );
        }
        FreshTailOutcome::Ops(ops) => panic!(
            "a floored tail cannot restore write {} compacted into the delta; \
                 merging it would serve the stale candidates: {ops:?}",
            fixture.inside
        ),
        FreshTailOutcome::Skipped(reason) => {
            panic!("a skip serves the stale candidates unmerged: {reason}")
        }
    }
    assert!(
        current_generation(&fixture.ann, &fixture.key).await > generation_before,
        "the drop must force re-adoption so a future query gets a fresh bridge"
    );
}

/// The mismatch preflight reads the base commit record and the delta HEAD
/// to decide whether a newer segment exists. A HEAD it cannot read is not
/// evidence that none does: the leg must drop the stale candidates, never
/// floor them at the registry minimum and serve them as healthy.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_the_delta_head_cannot_be_read() {
    const MODEL: &str = "adr118-reresolve-unreadable-delta-head-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    let base_seq = read_commit_info(&fixture.dir)
        .expect("read base commit")
        .and_then(|info| info.last_applied_seq)
        .expect("base watermark");
    assert_eq!(
        effective_persisted_state(&fixture.dir, base_seq)
            .expect("control: the intact HEAD reads")
            .0,
        fixture.delta_s,
        "control: the intact HEAD promises the delta watermark"
    );

    std::fs::write(fixture.dir.join(delta::HEAD_FILE), b"not a delta head")
        .expect("overwrite the delta HEAD");
    assert!(
        effective_persisted_state(&fixture.dir, base_seq).is_err(),
        "precondition: the preflight's delta HEAD read must fail"
    );

    let generation_before = current_generation(&fixture.ann, &fixture.key).await;
    let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
    let outcome = fresh_tail_leg(
        &fixture.rt,
        &fixture.ann,
        &fixture.key,
        MODEL,
        &query,
        10,
        Some(fixture.s1),
    )
    .await;
    assert_dropped_stale_candidates(
        outcome,
        "fresh-tail: persisted segment state read failed; dropped stale candidates",
        &fixture,
        generation_before,
    )
    .await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_publication_metadata_is_missing() {
    const MODEL: &str = "ann-missing-publication-coverage-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    std::fs::remove_file(fixture.dir.join("metadata.bin")).expect("remove fixture metadata");
    assert!(read_commit_info(&fixture.dir).unwrap().is_none());

    let generation_before = current_generation(&fixture.ann, &fixture.key).await;
    let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
    let outcome = fresh_tail_leg(
        &fixture.rt,
        &fixture.ann,
        &fixture.key,
        MODEL,
        &query,
        10,
        Some(fixture.s1),
    )
    .await;
    assert_dropped_stale_candidates(
        outcome,
        "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
        &fixture,
        generation_before,
    )
    .await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_publication_metadata_is_malformed() {
    const MODEL: &str = "ann-malformed-publication-coverage-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    std::fs::write(
        fixture.dir.join("metadata.bin"),
        b"invalid fixture commit record",
    )
    .expect("replace fixture metadata");
    assert!(read_commit_info(&fixture.dir).unwrap().is_none());

    let generation_before = current_generation(&fixture.ann, &fixture.key).await;
    let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
    let outcome = fresh_tail_leg(
        &fixture.rt,
        &fixture.ann,
        &fixture.key,
        MODEL,
        &query,
        10,
        Some(fixture.s1),
    )
    .await;
    assert_dropped_stale_candidates(
        outcome,
        "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
        &fixture,
        generation_before,
    )
    .await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_delta_head_is_missing_below_minimum() {
    const MODEL: &str = "ann-missing-head-coverage-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    let base_seq = read_commit_info(&fixture.dir)
        .unwrap()
        .and_then(|info| info.last_applied_seq)
        .expect("fixture base watermark");
    assert_eq!(base_seq, fixture.s1);
    assert_eq!(
        effective_persisted_state(&fixture.dir, base_seq).unwrap().0,
        fixture.delta_s
    );
    std::fs::remove_file(fixture.dir.join(delta::HEAD_FILE)).expect("remove fixture HEAD");
    assert_eq!(
        effective_persisted_state(&fixture.dir, base_seq).unwrap().0,
        base_seq
    );
    assert!(
        base_seq < fixture.delta_s,
        "base cannot cover the compacted prefix"
    );

    let generation_before = current_generation(&fixture.ann, &fixture.key).await;
    let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
    let outcome = fresh_tail_leg(
        &fixture.rt,
        &fixture.ann,
        &fixture.key,
        MODEL,
        &query,
        10,
        Some(fixture.s1),
    )
    .await;
    assert_dropped_stale_candidates(
        outcome,
        "fresh-tail: persisted segment does not cover registry minimum; dropped stale candidates",
        &fixture,
        generation_before,
    )
    .await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn segment_classifier_rebuilds_when_delta_head_is_missing_below_active_watermark() {
    const MODEL: &str = "ann-missing-head-rebuild-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    std::fs::remove_file(fixture.dir.join(delta::HEAD_FILE)).expect("remove fixture HEAD");
    assert_eq!(
        read_own_watermark(&fixture.rt, MODEL).await.unwrap(),
        Some(fixture.delta_s as i64)
    );
    let restarted = new_shared();
    let mut details = AnnWarmDetails::default();
    let outcome = classify_and_adopt_segment(
        &fixture.rt,
        &restarted,
        &fixture.key,
        MODEL,
        &fixture.dir,
        0,
        0,
        &mut details,
    )
    .await;
    assert!(
        matches!(outcome, SegmentOutcome::Cold),
        "an empty compacted tail cannot make an old base Hot"
    );
    assert!(restarted.indexes.read().await.get(&fixture.key).is_none());

    let token = fixture.rt.authorize(Namespace::local()).unwrap();
    let status = ensure_ann_for_model(&fixture.rt, &token, &restarted, MODEL)
        .await
        .unwrap();
    assert!(matches!(status, AnnEnsureStatus::Built { .. }));
    assert!(bridge_applied_seq(&restarted, &fixture.key).await.unwrap() >= fixture.delta_s);
    let query = fnv_to_vec("compacted peer delta note inside the published delta", DIMS);
    let candidates = search_loaded(&restarted, &fixture.key, &query, 10)
        .await
        .unwrap()
        .unwrap();
    assert!(
        candidates.iter().any(|(id, _)| *id == fixture.inside),
        "full rebuild must recover the fixture write already compacted out of the log"
    );
}

/// Re-resolution loads an intact newer segment but its search fails. The
/// stale candidates still miss the write compacted into the delta, so the
/// leg must drop them rather than skip and serve them.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_stale_candidates_when_the_re_resolved_search_fails() {
    const MODEL: &str = "adr118-reresolve-search-failure-test-model";
    const DIMS: usize = 8;
    let fixture = compacted_peer_delta(MODEL, DIMS).await;
    let reloaded = AnnBridge::load(&fixture.dir).expect("control: the intact chain loads");
    assert_eq!(
        reloaded.index.last_applied_seq(),
        Some(fixture.delta_s),
        "control: re-resolution would load the delta watermark"
    );
    // A query whose width differs from the index makes the re-resolved
    // search fail after the load succeeds.
    let query = vec![0.5_f32; DIMS + 1];
    assert!(
        reloaded
            .search_with_route(&query, 10, AnnScoreRoute::Memory)
            .is_err(),
        "precondition: the re-resolved search must fail for this query"
    );

    let generation_before = current_generation(&fixture.ann, &fixture.key).await;
    let outcome = fresh_tail_leg(
        &fixture.rt,
        &fixture.ann,
        &fixture.key,
        MODEL,
        &query,
        10,
        Some(fixture.s1),
    )
    .await;
    assert_dropped_stale_candidates(
        outcome,
        "fresh-tail: re-resolved segment search failed; dropped stale candidates",
        &fixture,
        generation_before,
    )
    .await;
}

/// A re-resolution that loses its post-search SQL leg must return a reasoned `Replace`, not `None` or a stale-resurrecting `Skipped`.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_post_reresolution_sql_failure_is_a_reasoned_replace() {
    const MODEL: &str = "adr118-reresolve-reasoned-replace-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("reasoned replace seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    // A new note lands after the checkpoint; the peer checkpoint below
    // persists it, and its presence in the outcome's candidates is the
    // proof that the REASONED Replace still serves the re-resolved
    // segment rather than falling back to the stale one.
    let fresh = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "reasoned replace distinctive fresh note",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create fresh note");

    let (_live, tail_before) = scope_counts(&rt, MODEL, s1)
        .await
        .expect("scope counts before peer checkpoint");
    assert!(tail_before > 0, "sanity: a tail must exist above s1");
    let s2 = s1 + tail_before;

    // Peer checkpoint: persist a segment at s2, raise+compact through it
    // — the mismatch the leg re-resolves from, exactly as in
    // fresh_tail_leg_reresolves_to_a_newer_persisted_segment_on_mismatch.
    let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
    let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
    let (ops, new_s) = fetch_final_tail(&rt, MODEL, s1, None)
        .await
        .expect("fetch tail for peer replay");
    peer_bridge
        .apply_final_ops(ops, new_s)
        .expect("apply peer replay");
    peer_bridge
        .save_atomic(&dir)
        .expect("persist peer checkpoint");
    raise_watermark(&rt, MODEL, s2)
        .await
        .expect("raise registry watermark");
    compact_log(&rt, MODEL).await.expect("compact log");

    // Pause the leg after its segment load+search, before the SQL phase
    // whose failure this test injects.
    ann.reresolve_race_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let paused = ann.reresolve_race_notify.notified();

    let query = fnv_to_vec("reasoned replace distinctive fresh note", DIMS);
    let handle = tokio::spawn({
        let rt = rt.clone();
        let ann = ann.clone();
        let key = key.clone();
        async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
    });
    paused.await;

    // Fault injection: remove the registry table so the leg's re-read —
    // its first post-re-resolution SQL statement — fails. (Test-fixture
    // database, mirroring the DROP TABLE fault pattern in
    // handlers/recall.rs's event-store acquisition test.)
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("fault injection writer");
        w.execute(SqlStatement {
            sql: "DROP TABLE ann_consumer_watermark".into(),
            params: vec![],
            label: Some("test_drop_registry_for_reasoned_replace".into()),
        })
        .await
        .expect("drop registry table");
    }

    ann.reresolve_race_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.reresolve_race_release.notify_one();

    let outcome = handle.await.expect("fresh_tail_leg task");
    match outcome {
        FreshTailOutcome::Replace(candidates, reason) => {
            let reason = reason.expect(
                "a post-re-resolution SQL failure must be DISCLOSED: \
                     Replace(_, None) reports read-your-writes visibility \
                     the leg no longer proved",
            );
            assert!(
                reason.starts_with("fresh-tail:"),
                "the disclosure must name its failure site, got: {reason:?}"
            );
            assert!(
                candidates.iter().any(|(id, _)| *id == fresh.id),
                "a reasoned Replace must still serve the re-resolved \
                     segment's candidates (which alone reflect the peer's \
                     checkpoint), got: {candidates:?}"
            );
        }
        FreshTailOutcome::Ops(_) => panic!(
            "expected re-resolution to replace candidates outright, not \
                 return ops against the stale bridge"
        ),
        FreshTailOutcome::Skipped(_) => panic!(
            "a failure AFTER successful re-resolution must not discard \
                 the coherent re-resolved candidates by skipping"
        ),
    }
}

/// A peer checkpoint that advances the registry minimum past the just-loaded
/// segment must make `fresh_tail_reresolve` reload rather than floor the
/// interleaved window (see `docs/ann.md` for the convergence argument).
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_reresolve_revalidates_registry_minimum_against_interleaved_compaction() {
    const MODEL: &str = "adr118-reresolve-interleave-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("interleave seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    // A write lands after the checkpoint — this is what the first peer
    // checkpoint (below) will persist into its segment.
    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "interleave first-checkpoint write",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create first-checkpoint note");

    let (_live, tail1) = scope_counts(&rt, MODEL, s1)
        .await
        .expect("scope counts before first peer checkpoint");
    assert!(tail1 > 0, "sanity: a tail must exist above s1");
    let s2 = s1 + tail1;

    // Peer checkpoint #1: persist a segment at s2, raise+compact through
    // it. This is the mismatch `fresh_tail_leg` will detect against the
    // stale in-memory bridge still pinned at s1, and `new_s` it will
    // re-resolve to.
    let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
    let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
    let (ops1, applied_s2) = fetch_final_tail(&rt, MODEL, s1, None)
        .await
        .expect("fetch tail for first peer checkpoint");
    peer_bridge
        .apply_final_ops(ops1, applied_s2)
        .expect("apply first peer checkpoint");
    peer_bridge
        .save_atomic(&dir)
        .expect("persist first peer checkpoint");
    raise_watermark(&rt, MODEL, s2)
        .await
        .expect("raise registry watermark to s2");
    compact_log(&rt, MODEL)
        .await
        .expect("compact log through s2");

    // Arm the race: `fresh_tail_reresolve` will pause right after it
    // loads and searches the s2 segment, before it re-validates the
    // registry minimum.
    ann.reresolve_race_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let paused = ann.reresolve_race_notify.notified();

    let generation_before = current_generation(&ann, &key).await;
    let query = fnv_to_vec("interleave post-compaction write", DIMS);
    let handle = tokio::spawn({
        let rt = rt.clone();
        let ann = ann.clone();
        let key = key.clone();
        async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
    });

    // Wait for the leg to reach the armed pause (after its segment load,
    // before its registry re-check) before racing a second checkpoint in.
    paused.await;

    // A further write lands, THEN a peer's second checkpoint covers it:
    // persist a segment at s3, raise+compact through it. This physically
    // removes the (s2, s3] log window — including the write below — from
    // `ann_write_log`, and is exactly the interleaving the fix must
    // detect via its re-validated registry-minimum read.
    let second_checkpoint_write = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "interleave second-checkpoint write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create second-checkpoint note");
    let (_live, tail2) = scope_counts(&rt, MODEL, s2)
        .await
        .expect("scope counts before second peer checkpoint");
    assert!(tail2 > 0, "sanity: a tail must exist above s2");
    let s3 = s2 + tail2;
    let mut peer_bridge2 = AnnBridge::load(&dir).expect("load s2 segment for second checkpoint");
    let (ops2, applied_s3) = fetch_final_tail(&rt, MODEL, s2, None)
        .await
        .expect("fetch tail for second peer checkpoint");
    peer_bridge2
        .apply_final_ops(ops2, applied_s3)
        .expect("apply second peer checkpoint");
    peer_bridge2
        .save_atomic(&dir)
        .expect("persist second peer checkpoint");
    raise_watermark(&rt, MODEL, s3)
        .await
        .expect("raise registry watermark to s3");
    compact_log(&rt, MODEL)
        .await
        .expect("compact log through s3");

    // A write above s3 — still present in the log, not compacted away —
    // must survive the coherent tail scan once the loop converges on
    // the s3 segment.
    let post = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "interleave post-compaction write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create post-compaction note");

    // Release the paused leg into its (now re-validated) registry-minimum
    // check and tail scan.
    ann.reresolve_race_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.reresolve_race_release.notify_one();

    let outcome = handle.await.expect("fresh_tail_leg task");
    let candidates = match outcome {
        FreshTailOutcome::Replace(candidates, _) => candidates,
        FreshTailOutcome::Ops(_) => panic!(
            "expected re-resolution to replace candidates outright, not \
                 just return ops to merge into the stale bridge's candidates"
        ),
        FreshTailOutcome::Skipped(_) => {
            panic!("expected the interleaved-compaction re-resolution to converge, not a skip")
        }
    };
    assert!(
        candidates
            .iter()
            .any(|(id, _)| *id == second_checkpoint_write.id),
        "a write compacted into the interleaved (s2, s3] window must be \
             recovered by reloading the >= s3 segment on the second round, \
             not silently dropped by a scan floored at s3 with candidates \
             still pinned to the stale s2 segment: {candidates:?}"
    );
    assert!(
        candidates.iter().any(|(id, _)| *id == post.id),
        "a write committed above the converged s3 watermark must \
             survive the coherent tail scan: {candidates:?}"
    );
    assert!(
        current_generation(&ann, &key).await > generation_before,
        "re-resolution must still force re-adoption so a future query \
             gets a fresh bridge"
    );
}

/// Exhausting [`FRESH_TAIL_RERESOLVE_MAX_ROUNDS`] under back-to-back peer checkpoints must fall back to the floored scan.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_reresolve_falls_back_to_floor_after_max_rounds() {
    const MODEL: &str = "adr118-reresolve-exhaustion-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("exhaustion seed note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create seed note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s1 = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark after initial warm");

    // First checkpoint (pre-spawn, mirrors the two-round interleave
    // test): this is what `fresh_tail_serving` resolves `new_s` to
    // before ever calling `fresh_tail_reresolve`.
    let write_a = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "exhaustion round-1 write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create round-1 write");
    let dir = ann_segment_dir(&rt, MODEL).expect("segment dir (file-backed test runtime)");
    let (_live, tail1) = scope_counts(&rt, MODEL, s1)
        .await
        .expect("scope counts before first checkpoint");
    assert!(tail1 > 0, "sanity: a tail must exist above s1");
    let s2 = s1 + tail1;
    {
        let mut peer_bridge = AnnBridge::load(&dir).expect("load persisted segment");
        let (ops, applied) = fetch_final_tail(&rt, MODEL, s1, None)
            .await
            .expect("fetch tail for first checkpoint");
        peer_bridge
            .apply_final_ops(ops, applied)
            .expect("apply first checkpoint");
        peer_bridge
            .save_atomic(&dir)
            .expect("persist first checkpoint");
    }
    raise_watermark(&rt, MODEL, s2)
        .await
        .expect("raise registry watermark to s2");
    compact_log(&rt, MODEL)
        .await
        .expect("compact log through s2");

    ann.reresolve_race_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut paused = ann.reresolve_race_notify.notified();

    let generation_before = current_generation(&ann, &key).await;
    let query = fnv_to_vec("exhaustion beyond-floor write", DIMS);
    let handle = tokio::spawn({
        let rt = rt.clone();
        let ann = ann.clone();
        let key = key.clone();
        async move { fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s1)).await }
    });

    // Two more checkpoints, one per round's pause, each removing that
    // round's gap write from `ann_write_log` — but each gap write is
    // carried forward because the peer checkpoint that compacts it away
    // also folds it into the segment this loop reloads next round.
    let mut prev_s = s2;
    let mut gap_writes = Vec::new();
    for round_idx in 1..=2u32 {
        paused.await;
        let gap_write = rt
            .create_note_with_decay_for_embedding_model(
                &token,
                "memory",
                None,
                &format!("exhaustion round-{} gap write", round_idx + 1),
                Some(0.7),
                0.01,
                None,
                vec![],
                None,
            )
            .await
            .expect("create gap write");
        let (_live, tail) = scope_counts(&rt, MODEL, prev_s)
            .await
            .expect("scope counts before interleaved checkpoint");
        assert!(
            tail > 0,
            "sanity: a tail must exist above the prior watermark"
        );
        let next_s = prev_s + tail;
        {
            let mut peer_bridge = AnnBridge::load(&dir).expect("load segment for checkpoint");
            let (ops, applied) = fetch_final_tail(&rt, MODEL, prev_s, None)
                .await
                .expect("fetch tail for interleaved checkpoint");
            peer_bridge
                .apply_final_ops(ops, applied)
                .expect("apply interleaved checkpoint");
            peer_bridge
                .save_atomic(&dir)
                .expect("persist interleaved checkpoint");
        }
        raise_watermark(&rt, MODEL, next_s)
            .await
            .expect("raise registry watermark");
        compact_log(&rt, MODEL).await.expect("compact log");

        let next_paused = ann.reresolve_race_notify.notified();
        ann.reresolve_race_release.notify_one();
        paused = next_paused;
        gap_writes.push(gap_write);
        prev_s = next_s;
    }

    // Third (terminal-round) checkpoint: its gap write lands in the
    // window the floored fallback cannot see (round == MAX exhausts the
    // loop before it can reload past this checkpoint).
    paused.await;
    let lost_write = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "exhaustion round-4 lost write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create terminal-round gap write");
    let (_live, tail) = scope_counts(&rt, MODEL, prev_s)
        .await
        .expect("scope counts before terminal checkpoint");
    assert!(
        tail > 0,
        "sanity: a tail must exist above the prior watermark"
    );
    let s_final = prev_s + tail;
    {
        let mut peer_bridge = AnnBridge::load(&dir).expect("load segment for checkpoint");
        let (ops, applied) = fetch_final_tail(&rt, MODEL, prev_s, None)
            .await
            .expect("fetch tail for terminal checkpoint");
        peer_bridge
            .apply_final_ops(ops, applied)
            .expect("apply terminal checkpoint");
        peer_bridge
            .save_atomic(&dir)
            .expect("persist terminal checkpoint");
    }
    raise_watermark(&rt, MODEL, s_final)
        .await
        .expect("raise registry watermark to s_final");
    compact_log(&rt, MODEL)
        .await
        .expect("compact log through s_final");

    // A write above the terminal floor must still surface through the
    // floored fallback's own (still-real) tail scan.
    let beyond_floor = rt
        .create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            "exhaustion beyond-floor write",
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create beyond-floor write");

    ann.reresolve_race_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.reresolve_race_release.notify_one();

    let outcome = handle.await.expect("fresh_tail_leg task");
    let candidates = match outcome {
        FreshTailOutcome::Replace(candidates, _) => candidates,
        FreshTailOutcome::Ops(_) => panic!(
            "expected the terminal round to replace candidates outright, \
                 not just return ops to merge into the stale bridge's candidates"
        ),
        FreshTailOutcome::Skipped(_) => {
            panic!("expected the bound-exhaustion floored fallback, not a skip")
        }
    };
    assert!(
        candidates.iter().any(|(id, _)| *id == write_a.id),
        "the pre-spawn checkpoint's write must survive every reload: {candidates:?}"
    );
    for gap_write in &gap_writes {
        assert!(
            candidates.iter().any(|(id, _)| *id == gap_write.id),
            "a gap write compacted into a NON-terminal round's reloaded \
                 segment must be recovered, not dropped: {candidates:?}"
        );
    }
    assert!(
        !candidates.iter().any(|(id, _)| *id == lost_write.id),
        "the terminal round's own gap write is the documented \
             ADR-118 mismatch-window loss (its segment is never reloaded \
             once the bound is exhausted) — it must NOT silently reappear \
             here, or this assertion is guarding a fix that changed \
             behavior without updating this test: {candidates:?}"
    );
    assert!(
        candidates.iter().any(|(id, _)| *id == beyond_floor.id),
        "a write committed above the terminal floor must survive the \
             floored fallback's own tail scan: {candidates:?}"
    );
    assert!(
        current_generation(&ann, &key).await > generation_before,
        "the bound-exhaustion floored fallback must still force \
             re-adoption so a future query gets a fresh bridge"
    );
}

/// Absent a durable registry row, the leg must not trust `S = 0` as complete — it registers pending and drops stale candidates.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_leg_drops_candidates_and_reregisters_when_consumer_row_absent() {
    const MODEL: &str = "adr118-registration-precondition-test-model";
    const DIMS: usize = 8;
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    rt.create_note_with_decay_for_embedding_model(
        &token,
        "memory",
        None,
        "registration precondition seed note",
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("create seed note");

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm");
    let s = bridge_applied_seq(&ann, &key)
        .await
        .expect("bridge watermark");

    // Delete this consumer's durable registry row directly, simulating a
    // process that has never registered (or whose row was reset).
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3"
                .into(),
            params: vec![
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
                SqlValue::Text(MODEL.into()),
            ],
            label: Some("test_delete_consumer_watermark_row".into()),
        })
        .await
        .expect("delete registry row");
    }
    assert!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read watermark")
            .is_none(),
        "sanity: the registry row must be gone before the leg runs"
    );

    let query = fnv_to_vec("registration precondition seed note", DIMS);
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 10, Some(s)).await;
    match outcome {
        FreshTailOutcome::Replace(hits, reason) => {
            assert!(
                hits.is_empty(),
                "the exact leg must discard candidates captured before registry loss"
            );
            assert!(
                reason.is_some_and(|r| !r.is_empty()),
                "dropping candidates is a degraded serve and must carry a \
                     failure-site reason for disclosure"
            );
        }
        _ => panic!(
            "the exact leg must drop candidates when this consumer's durable \
                 registration row is absent"
        ),
    }
    assert!(
        search_loaded_with_seq(&ann, &key, &query, 10)
            .await
            .expect("search after registry loss")
            .is_none(),
        "registry loss must evict the unprotected in-process bridge"
    );
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read watermark"),
        Some(PENDING_WATERMARK),
        "the leg must re-register this consumer in the closed pending state"
    );
}
