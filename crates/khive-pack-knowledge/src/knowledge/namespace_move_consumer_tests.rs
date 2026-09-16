//! The executable acceptance for ADR-189 Amendment 1: a moved vector leaves the
//! source namespace's index and arrives in the target's.
//!
//! ## Why the assertion is about the index and not about the log
//!
//! The amendment's correction is that `ann_write_log` takes two entries per moved
//! vector, a `delete` under the source as well as the `upsert` under the target.
//! Asserting that two rows were appended is a claim about what was written, and it
//! passes against a consumer that never reads them. What the correction exists to
//! prevent is a source index that keeps answering searches for a subject that has
//! left, so that is what this asserts.
//!
//! ## Why it drives the consumer rather than the search verb
//!
//! Two instruments were tried. The first asserted on `knowledge.search`'s results
//! for the source namespace and it does not work: after the move the atom row's
//! namespace is the target, so the store returns no candidate for it whatever the
//! write log says. That arm passed with the `delete` appended under the target
//! instead of the source, which is precisely the defect it was written to catch.
//! It is recorded here because the arm looked correct and the only thing that
//! separated it from a real one was running the falsifier.
//!
//! A rebuild has the same problem for the same reason: `knowledge.index` rebuilds
//! a namespace from `knowledge_atoms WHERE namespace = ?`, so a rebuilt source
//! index is empty after the move no matter what was appended.
//!
//! So the arm holds the source bridge at the watermark it had before the move and
//! drives `vamana::fresh_tail_leg` directly, which is the consumer step the
//! amendment names: read the write log past the loaded watermark, coalesce the ops
//! and merge them into the candidates the bridge served. The subject leaving the
//! merged list is then caused by the `delete` row and by nothing else.
//!
//! A route names a subject class rather than a subject, so both seeded atoms move
//! and the merged list is empty by design. "The subject is absent" is therefore
//! satisfiable by an empty list arriving for any reason, and the arm asserts the
//! cause separately: the tail carries one instruction per moved vector and one of
//! them is a delete naming this subject.
//!
//! ## Falsifier
//!
//! Append the source-side row under the target instead of the source in
//! `move_vectors`, which keeps `ann_log_appended` at two so only the merge can
//! redden, and this arm must FAIL with the moved subject still in the merged list.
//! Measured both ways: green unmutated, red mutated.
//!
//! ## Scope
//!
//! `move_namespace` has no verb in front of it yet, so this test is the caller and
//! opens its own connection to the same file-backed store between the two searches.
//! That is what a maintenance tool would do today, and it is the only way to reach
//! the primitive from above `khive-db`.

use super::ann_degrade_tests::{build_registry, file_rt_with_fake_embedder, DIM};
use crate::knowledge::vamana;
use khive_db::namespace_move::{move_namespace, MoveRequest, MoveRoute, SubjectClass};
use khive_runtime::Namespace;
use serde_json::json;
use uuid::Uuid;

const SOURCE: &str = "local";
const TARGET: &str = "moved-to";

/// The fake embedder returns the same unit vector for every text, so the index
/// cannot rank one atom above another and this query is only ever a way of asking
/// the bridge which subjects it is willing to return. That is exactly the question
/// the arm has: membership, not order.
fn membership_query() -> Vec<f32> {
    vec![1.0f32 / (DIM as f32).sqrt(); DIM]
}

async fn returned_ids(ann: &vamana::SharedAnn, key: &vamana::AnnKey) -> Option<Vec<Uuid>> {
    vamana::search_loaded(ann, key, &membership_query(), 16)
        .await
        .map(|hits| hits.into_iter().map(|(id, _)| id).collect())
}

#[tokio::test]
async fn the_source_index_stops_answering_for_a_moved_subject() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db_path = dir.path().join("move-consumer.db");
    // ADR-118's fresh-tail serving is what reads the write log past the loaded
    // watermark. Set explicitly rather than inherited, so the arm does not go
    // quietly inert if the default flips.
    let rt = file_rt_with_fake_embedder(db_path.clone()).with_ann_fresh_tail_enabled(true);
    let registry = build_registry(&rt);

    // Two atoms, because a one-atom corpus cannot tell "the moved subject left"
    // from "the index is empty".
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "move-consumer-subject",
                        "name": "Move Consumer Subject",
                        "content": "dense retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique mover01"
                    },
                    {
                        "slug": "move-consumer-resident",
                        "name": "Move Consumer Resident",
                        "content": "ranking fusion pipeline embedding rerank cosine similarity unique mover02 transformer attention mechanism self-attention encoder decoder positional feed forward dense neural network gradient"
                    }
                ]
            }),
        )
        .await
        .expect("upsert atoms");

    let indexed = registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
        .await
        .expect("index with rebuild_ann=true");
    let embedded = indexed.get("indexed").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        embedded >= 2,
        "the corpus must actually have been embedded for any of this to mean anything; \
         got indexed={embedded} from {indexed}"
    );

    let model = rt.default_embedder_name().to_string();
    let source_key = vamana::AnnKey::new(SOURCE, &model);
    let target_key = vamana::AnnKey::new(TARGET, &model);

    let ann = vamana::new_shared();
    vamana::warm_known_snapshots(&rt, &ann).await;

    // Control 1, and it is the load-bearing one. Without it a miss after the move
    // is explained by a consumer that was never warm, which has nothing to do with
    // the move. The watermark is captured with the candidates under one guard, so
    // the tail read below starts exactly where these candidates end.
    let query = membership_query();
    let (before, watermark) = vamana::search_loaded_with_seq(&ann, &source_key, &query, 16)
        .await
        .expect("the source index must be loaded before the move");
    assert!(
        before.len() >= 2,
        "pre-state: the source index must hold both atoms, got {} candidates: {before:?}",
        before.len()
    );
    let subject = before.first().expect("at least one candidate").0;

    // The move. No verb reaches this yet, so the test is the caller.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("second connection to the store");
        let request = MoveRequest::new(
            SOURCE,
            vec![MoveRoute {
                class: SubjectClass::Atom,
                target: TARGET.to_string(),
            }],
        );
        let counts = move_namespace(&conn, &request).expect("move succeeds");
        assert!(
            counts.ann_log_appended >= 2,
            "a moved vector owes one delete under the source and one upsert under the \
             target; got ann_log_appended={}",
            counts.ann_log_appended
        );
    }

    // No rebuild here on purpose: the source bridge stays at the watermark it had,
    // and the search advances the consumer over the write log past that watermark.
    // The consumer step: read the write log past the watermark the bridge is at,
    // and merge. No rebuild, so the bridge still holds the subject and only a
    // `delete` row under the source can take it back out.
    let ops =
        match vamana::fresh_tail_leg(&rt, &ann, &source_key, &query, 16, Some(watermark)).await {
            vamana::FreshTailOutcome::Ops(ops) => ops,
            vamana::FreshTailOutcome::Replace { .. } => panic!(
                "the fresh-tail leg returned a replacement segment; this arm needs the \
                 coalesced-ops form, because a replacement is another bridge's answer"
            ),
            vamana::FreshTailOutcome::Skipped => panic!(
                "the fresh-tail leg was skipped, so nothing read the write log and this \
                 arm would be asserting an absence it never looked for"
            ),
        };
    // Control 2. A route names a subject CLASS, not a subject, so both seeded
    // atoms move and the merged list below is expected to be empty. That makes
    // "the subject is absent" satisfiable by an empty list arriving for any
    // reason, so the cause is asserted here: the tail was read, and it carries a
    // delete instruction naming this subject. `None` is the delete form.
    assert!(
        ops.iter().any(|(id, op)| *id == subject && op.is_none()),
        "the tail read past the watermark must carry a delete for the moved subject; \
         {subject} not among {ops:?}"
    );
    assert_eq!(
        ops.len(),
        before.len(),
        "one instruction per moved vector: the source served {} subjects and the tail \
         carries {} ops",
        before.len(),
        ops.len()
    );

    let merged = vamana::merge_fresh_tail(before.clone(), &query, ops);
    let moved_still_served = merged.iter().any(|(id, _)| *id == subject);
    assert!(
        !moved_still_served,
        "the source must stop answering for a subject that has left: {subject} was still \
         in the merged candidates after the move. before={before:?} merged={merged:?}"
    );

    // Control 2: the target has to be able to serve it, or the arm above is
    // satisfied by a move that lost the vector entirely.
    let target_token = rt
        .authorize(Namespace::parse(TARGET).expect("target namespace"))
        .expect("authorize target");
    let target_ann = vamana::new_shared();
    let _ = crate::knowledge::KnowledgeHandlers::index(
        &rt,
        &target_token,
        json!({ "rebuild_ann": true }),
        &target_ann,
        None,
    )
    .await
    .expect("build the target index");
    let target_ids = returned_ids(&target_ann, &target_key)
        .await
        .unwrap_or_default();
    assert!(
        target_ids.contains(&subject),
        "the target index must serve the moved subject; {subject} not in {target_ids:?}"
    );
}
