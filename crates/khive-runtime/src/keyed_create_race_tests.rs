//! ADR-172 Amendment 6 acceptance items 3-5: a keyed singleton create's
//! preparation (annotation-target validation, embedding-model resolution)
//! runs outside the writer, and its replay/conflict decision is made fresh,
//! inside the writer transaction, at admission time. These tests reproduce
//! the two-phase race deterministically through `crate::note_write::race_seam`
//! (a test-only pause after the initial holder check and preparation, before
//! writer admission) instead of
//! relying on scheduler luck or sleeps, and confirm the existing fault
//! injection arms cannot be misread as a replay.

use super::*;

use crate::note_write::race_seam;
use tokio::sync::Barrier;

type CreateOutcome = RuntimeResult<(Note, crate::retrieval::EmbeddingTruncationReport)>;

/// The test's side of the pause point's two meetings. The first returns once
/// the parked create has finished its initial holder check and preparation,
/// so any write the test makes afterwards lands between that create's first
/// look and its final writer admission. The timeout turns a create that
/// resolved before reaching the pause into a test failure instead of a hang.
async fn meet(barrier: &Barrier) {
    tokio::time::timeout(std::time::Duration::from_secs(10), barrier.wait())
        .await
        .expect("the parked create must reach its pause point");
}

fn spawn_racer(
    runtime: Arc<KhiveRuntime>,
    token: NamespaceToken,
    barrier: Arc<Barrier>,
    content: &'static str,
    key: String,
    properties: Option<serde_json::Value>,
) -> tokio::task::JoinHandle<CreateOutcome> {
    tokio::spawn(race_seam::AFTER_PREPARE_BARRIER.scope(barrier, async move {
        runtime
            .create_note_with_options(
                &token,
                "head",
                None,
                content,
                None,
                None,
                None,
                properties,
                vec![],
                None,
                NoteWriteOptions {
                    key: Some(key),
                    embed: Some(false),
                    ..Default::default()
                },
            )
            .await
    }))
}

fn one_winner_one_conflict(a: CreateOutcome, b: CreateOutcome) -> (Note, RuntimeError) {
    match (a, b) {
        (Ok((note, _)), Err(error)) => (note, error),
        (Err(error), Ok((note, _))) => (note, error),
        other => panic!("expected exactly one winner and one key_conflict, got {other:?}"),
    }
}

/// Two callers race to create the same key with byte-identical content and
/// properties. Both observe an absent key during preparation; only one wins
/// writer admission, and the loser's key_conflict names the winner and
/// reports `equal:true`, because the comparison happens fresh, inside the
/// writer transaction, against the row the winner just committed, not
/// against whatever either caller observed before the race.
///
/// Control: `atomic_runner.rs`'s `note_guard.filter(|_| index == 0)` gate
/// (the guard that lets `classify_refusal` run inside the writer
/// transaction after the just-failed guarded insert) mutated to
/// `note_guard.filter(|_| false)` so the comparison never runs; this test
/// then gets a generic rolled-back error instead of a key_conflict with
/// equal:true, and fails.
#[tokio::test]
async fn item3_concurrent_equal_racers_settle_to_one_holder_and_a_true_replay() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let properties = Some(json!({"n": 1}));

    let a = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"equal\"}",
        "item3/equal".into(),
        properties.clone(),
    );
    let b = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"equal\"}",
        "item3/equal".into(),
        properties,
    );

    let (winner, loser) = one_winner_one_conflict(
        a.await.expect("racer a task"),
        b.await.expect("racer b task"),
    );
    assert_eq!(winner.key.as_deref(), Some("item3/equal"));

    let details = details(loser);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], winner.id.to_string());
    assert_eq!(
        details["equal"], "true",
        "an identical racer must be reported equal: {details}"
    );

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "item3/equal", Some("head"))
        .await
        .unwrap();
    assert_eq!(live.len(), 1, "exactly one live holder must remain");
    assert_eq!(live[0].id, winner.id);
}

/// Same race, different content. The loser's key_conflict must report
/// `equal:false`: the two candidates disagree, so a create cannot silently
/// resolve to a replay of the other caller's payload.
///
/// Control (shared with item 4/5's differing-holder tests):
/// `note_write.rs`'s `existing_content == claim.content && existing_properties
/// == claim.properties` forced to always evaluate `true`; this test then
/// reads `equal:true` for genuinely different content, and fails.
#[tokio::test]
async fn item3_concurrent_different_content_racers_conflict_without_replay() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));

    let a = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"a\"}",
        "item3/diff-content".into(),
        None,
    );
    let b = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"b\"}",
        "item3/diff-content".into(),
        None,
    );

    let (winner, loser) = one_winner_one_conflict(
        a.await.expect("racer a task"),
        b.await.expect("racer b task"),
    );
    let details = details(loser);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], winner.id.to_string());
    assert_eq!(
        details["equal"], "false",
        "differing content must not report equal: {details}"
    );

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(
            token.namespace().as_str(),
            "item3/diff-content",
            Some("head"),
        )
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, winner.id);
}

/// Same race, identical content but different properties. The equality
/// comparison is content AND properties, so a properties-only difference
/// must conflict rather than replay.
#[tokio::test]
async fn item3_concurrent_different_properties_racers_conflict_without_replay() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));

    let a = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"props\"}",
        "item3/diff-props".into(),
        Some(json!({"n": 1})),
    );
    let b = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"race\":\"props\"}",
        "item3/diff-props".into(),
        Some(json!({"n": 2})),
    );

    let (_winner, loser) = one_winner_one_conflict(
        a.await.expect("racer a task"),
        b.await.expect("racer b task"),
    );
    let details = details(loser);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(
        details["equal"], "false",
        "differing properties must not report equal: {details}"
    );
}

/// An equal holder appears after the racer's initial holder check and is
/// changed before the racer's writer admission. The racer's candidate was
/// equal to the holder as first written, but the final comparison reads the
/// CURRENT row, so it must conflict against the now-current content, not
/// replay against a state that no longer exists.
#[tokio::test]
async fn item4_holder_changed_mid_flight_conflict_reflects_current_state() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));

    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{}",
        "item4/change".into(),
        None,
    );
    meet(&barrier).await;
    let seed = create(&runtime, &token, "item4/change", Some(false)).await;
    assert_eq!(seed.version, 1);
    runtime
        .update_note(&token, seed.id, patch("{\"changed\":true}", 1, Some(false)))
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "the racer's stale-equal candidate must conflict against the now-current holder",
    );
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], seed.id.to_string());
    assert_eq!(
        details["equal"], "false",
        "must compare against CURRENT content, not content observed before the race: {details}"
    );
}

/// A holder appears after the racer's initial holder check and is
/// soft-deleted before the racer's writer admission. The racer must see the
/// key as free at admission time and insert fresh, minting a new id rather
/// than resurrecting the deleted row.
#[tokio::test]
async fn item4_holder_deleted_mid_flight_lets_the_racer_insert_fresh() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));

    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{}",
        "item4/delete".into(),
        None,
    );
    meet(&barrier).await;
    let seed = create(&runtime, &token, "item4/delete", Some(false)).await;
    assert!(
        runtime.delete_note(&token, seed.id, false).await.unwrap(),
        "soft delete must report a live row removed"
    );
    meet(&barrier).await;

    let (note, _) = racer.await.expect("racer task").expect(
        "the key is free once its only holder is soft-deleted, so the racer must insert fresh",
    );
    assert_ne!(
        note.id, seed.id,
        "a fresh insert must mint a new id, not resurrect the deleted holder"
    );
    assert_eq!(note.key.as_deref(), Some("item4/delete"));

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "item4/delete", Some("head"))
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, note.id);
}

/// A holder appears after the racer's initial holder check, then is deleted
/// and replaced by a different note under the same key before the racer's
/// writer admission. The racer's conflict must name the REPLACEMENT holder,
/// never the deleted original.
#[tokio::test]
async fn item4_holder_replaced_mid_flight_conflict_names_the_new_holder() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));

    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{}",
        "item4/replace".into(),
        None,
    );
    meet(&barrier).await;
    let original = create(&runtime, &token, "item4/replace", Some(false)).await;
    assert!(runtime
        .delete_note(&token, original.id, false)
        .await
        .unwrap());
    let (replacement, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{\"replaced\":true}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("item4/replace".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(replacement.id, original.id);
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "the racer's candidate must conflict against the replacement holder, \
         not silently replay against the deleted original",
    );
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(
        details["existing_id"],
        replacement.id.to_string(),
        "must name the CURRENT (replacement) holder: {details}"
    );
    assert_eq!(details["equal"], "false");
}

/// A dependent create carries a fence on a different note's version. That
/// fenced note's version is bumped while the dependent create's own
/// preparation sits parked, so the fence that was valid at prep time is
/// stale by admission time. `check_fence` runs unconditionally inside the
/// writer transaction (`atomic_runner.rs` calls it before the plan's own
/// statement loop), so the stale fence must refuse the create and leave no
/// row behind.
///
/// Control: `atomic_runner.rs`'s `if let Some(guard) = note_guard {` (the
/// unconditional gate wrapping `check_fence`) mutated to
/// `note_guard.filter(|_| false)` so the fence is never checked; this test
/// then commits the dependent create despite the stale fence, and fails.
#[tokio::test]
async fn item4_fence_invalidated_mid_flight_refuses_the_dependent_create() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let fenced = create(&runtime, &token, "item4/fence-target", Some(false)).await;
    assert_eq!(fenced.version, 1);
    let barrier = Arc::new(Barrier::new(2));

    let racer = {
        let runtime = Arc::clone(&runtime);
        let token = token.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(race_seam::AFTER_PREPARE_BARRIER.scope(barrier, async move {
            runtime
                .create_note_with_options(
                    &token,
                    "head",
                    None,
                    "{}",
                    None,
                    None,
                    None,
                    None,
                    vec![],
                    None,
                    NoteWriteOptions {
                        key: Some("item4/fence-dependent".into()),
                        embed: Some(false),
                        fence: Some(
                            NoteFence {
                                key: "item4/fence-target".into(),
                                kind: "head".into(),
                                expected_version: Some(1),
                                live_until: None,
                                id: None,
                            }
                            .into(),
                        ),
                        ..Default::default()
                    },
                )
                .await
        }))
    };

    meet(&barrier).await;
    runtime
        .update_note(
            &token,
            fenced.id,
            patch("{\"bumped\":true}", 1, Some(false)),
        )
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "a fence valid at prep time but stale at admission time must refuse the dependent create",
    );
    let details = details(error);
    assert_eq!(details["reason"], "fence_conflict");
    assert_eq!(details["key"], "item4/fence-target");

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(
            token.namespace().as_str(),
            "item4/fence-dependent",
            Some("head"),
        )
        .await
        .unwrap();
    assert!(
        live.is_empty(),
        "a fence-refused create must not leave a row behind"
    );
}

/// A racer's preparation is parked; an independent writer completes a full,
/// separate create under the SAME key while the racer sits parked, proving
/// the pause holds no writer-wide lock. Equal content: the racer's own
/// admission-time comparison reads the just-committed independent row and
/// replays against it.
#[tokio::test]
async fn item5_independent_writer_completes_with_equal_content_then_racer_replays() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"v\":1}",
        "item5/equal".into(),
        None,
    );
    meet(&barrier).await;

    let (independent, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{\"v\":1}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("item5/equal".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "the racer's equal payload must replay against the just-committed independent holder",
    );
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], independent.id.to_string());
    assert_eq!(details["equal"], "true");
}

/// Same shape, different content: the independent writer's holder and the
/// racer's own candidate disagree, so the racer must conflict, not replay.
#[tokio::test]
async fn item5_independent_writer_completes_with_different_content_then_racer_conflicts() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"v\":1}",
        "item5/diff".into(),
        None,
    );
    meet(&barrier).await;

    let (independent, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{\"v\":2}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("item5/diff".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer
        .await
        .expect("racer task")
        .expect_err("a different independent holder must conflict, not replay");
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], independent.id.to_string());
    assert_eq!(details["equal"], "false");
}

/// An independent writer proceeds on an UNRELATED key while the racer sits
/// parked, proving the pause holds no writer-wide lock and that the racer's
/// own key stays genuinely free throughout. When released, the racer must
/// insert fresh.
#[tokio::test]
async fn item5_independent_writer_on_another_key_proceeds_while_racer_is_parked() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "{\"v\":1}",
        "item5/absent".into(),
        None,
    );
    meet(&barrier).await;

    let unrelated = create(&runtime, &token, "item5/absent-control", Some(false)).await;
    meet(&barrier).await;

    let (note, _) = racer
        .await
        .expect("racer task")
        .expect("a still-absent key must insert fresh");
    assert_eq!(note.key.as_deref(), Some("item5/absent"));
    assert_ne!(note.id, unrelated.id);

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(token.namespace().as_str(), "item5/absent", Some("head"))
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, note.id);
}

/// A fresh-insert fault (the existing vector fault injection arm, already
/// reachable from this create path through `atomic_message.rs`'s
/// `maybe_inject_vector_failure`) must surface as a genuine error and roll
/// the whole unit back, leaving the key free for a later, unfaulted create.
/// It must never be misread as a key_conflict/replay: the guarded no-op
/// statement the injection splices in fails at a plan-statement index other
/// than 0, so `classify_refusal` never fires for it, and this test asserts
/// that structural property rather than merely a non-nil error.
///
/// Control: `note_write.rs`'s catch-all `Ok(AtomicRunOutcome::RolledBack {
/// failure, .. }) => Err(RuntimeError::Internal(...))` arm replaced with a
/// fabricated `Ok((note, prepared.embedding_truncation))`; this test then
/// reads a fabricated success (and a live row) for an injected fault, and
/// fails.
#[tokio::test]
async fn item5_fresh_insert_fault_rolls_back_and_leaves_the_key_free() {
    let (runtime, token, _service) = fixture();
    let ns = token.namespace().as_str().to_string();
    let _arm = crate::operations::arm_vector_fail_scoped(ns.as_str());

    let error = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("item5/fault".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect_err("an injected insertion fault must surface as a genuine error, never a replay");
    assert!(
        !matches!(error, RuntimeError::Khive(_)),
        "a fault-injected failure must not take the key_conflict/replay shape at all: {error:?}"
    );

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(ns.as_str(), "item5/fault", Some("head"))
        .await
        .unwrap();
    assert!(
        live.is_empty(),
        "a rolled-back insert must leave the key free: {live:?}"
    );
    drop(_arm);

    let (fresh, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("item5/fault".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("the key must still be free for a genuine, unfaulted create");
    assert_eq!(fresh.key.as_deref(), Some("item5/fault"));
}

// ADR-172 Amendment 6, "Transaction and preparation boundary": creation-only
// annotation resolution and embedding work run outside the writer and after
// an initial authoritative holder check, so an equal replay never depends on
// that creation-only work still being possible. The tests below exercise
// that boundary directly; items 3-5's tests above already cover the
// concurrent-racer shape this boundary is built on.

/// An equal replay must not require its annotation target to still exist:
/// the target is deleted after the original create, and a byte-identical
/// second request naming the same (now gone) target still replays, leaving
/// the holder row and its one `annotates` edge untouched. Two steps protect
/// this case: the initial holder check replays before preparation runs, and
/// a preparation failure is revalidated against the holder before it is
/// returned. With both removed, the annotates-existence loop in
/// `prepare_note_create` returns `NotFound` instead.
#[tokio::test]
async fn item4_equal_replay_survives_a_vanished_annotates_target() {
    let (runtime, token, _service) = fixture();
    let target = runtime
        .create_entity(
            &token,
            "concept",
            None,
            "boundary-target",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let (holder, _) = runtime
        .create_note_with_options(
            &token,
            "observation",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![target.id],
            None,
            NoteWriteOptions {
                key: Some("boundary/annotates-gone".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(runtime
        .delete_entity(&token, target.id, false)
        .await
        .unwrap());

    let edges_before = runtime
        .neighbors(
            &token,
            holder.id,
            khive_storage::types::Direction::Out,
            None,
            Some(vec![khive_storage::EdgeRelation::Annotates]),
        )
        .await
        .unwrap()
        .len();

    let error = runtime
        .create_note_with_options(
            &token,
            "observation",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![target.id],
            None,
            NoteWriteOptions {
                key: Some("boundary/annotates-gone".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect_err("an equal replay must not require its annotates target to still exist");
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["equal"], "true");
    assert_eq!(details["existing_id"], holder.id.to_string());

    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(holder.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current, holder,
        "a replay must leave the holder row untouched"
    );

    let edges_after = runtime
        .neighbors(
            &token,
            holder.id,
            khive_storage::types::Direction::Out,
            None,
            Some(vec![khive_storage::EdgeRelation::Annotates]),
        )
        .await
        .unwrap()
        .len();
    assert_eq!(
        edges_before, edges_after,
        "a replay must not add a new annotates edge"
    );
}

/// An equal replay must not require its embedding model to remain
/// resolvable. Resolving the embedding model is the first thing
/// `prepare_atomic_notes` does for a request that is not `embed:false`, so
/// preparation fails for this request whenever it runs; a
/// key_conflict/equal:true here (rather than an unknown-model error) shows
/// the replay does not depend on it. Whether preparation ran at all is
/// observed by `item4_initial_equal_replay_never_attempts_embedding`.
#[tokio::test]
async fn item4_equal_replay_survives_an_unregistered_embedding_model() {
    let (runtime, token, _service) = fixture();
    let holder = create(&runtime, &token, "boundary/model-gone", Some(false)).await;

    let error = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            Some("boundary-nonexistent-model"),
            NoteWriteOptions {
                key: Some("boundary/model-gone".into()),
                embed: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect_err("an equal replay must not require its embedding model to be resolvable");
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["equal"], "true");
    assert_eq!(details["existing_id"], holder.id.to_string());
}

/// Name and salience are creation-only options, not part of the equality
/// comparison (content and derived properties only), and a replay must not
/// adopt them onto the holder.
#[tokio::test]
async fn item4_equal_replay_with_different_name_and_salience_leaves_the_holder_unchanged() {
    let (runtime, token, _service) = fixture();
    let (holder, _) = runtime
        .create_note_with_options(
            &token,
            "observation",
            Some("the original name"),
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/name-salience".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(holder.name.as_deref(), Some("the original name"));

    let error = runtime
        .create_note_with_options(
            &token,
            "observation",
            Some("a different name"),
            "{}",
            None,
            Some(0.9),
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/name-salience".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect_err("a name/salience-only difference must still replay");
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["equal"], "true");

    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(holder.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current, holder,
        "a replay must leave the holder's name and salience untouched"
    );
}

/// Creation-only work is not run on an initial-equal replay at all. The
/// embedding service's own call counter is the witness: a key_conflict
/// alone does not prove this, because a failed preparation attempt is
/// captured and then discarded once the final writer transaction confirms
/// the same holder still matches, so the returned error looks the same
/// whether or not embedding ran. The replay asks for an embedding under
/// every registered model, which is this test's counting service. The last
/// create makes the same request under a fresh key and must move the
/// counter, which shows the first assertion could have failed.
#[tokio::test]
async fn item4_initial_equal_replay_never_attempts_embedding() {
    const CONTENT: &str = "{\"text\":\"a replay never embeds\"}";
    let (runtime, token, service) = fixture();
    let (holder, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            CONTENT,
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/no-embed-on-replay".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let calls_before = service.calls.load(std::sync::atomic::Ordering::SeqCst);

    let error = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            CONTENT,
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/no-embed-on-replay".into()),
                embed: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect_err("an equal replay must still report key_conflict");
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["equal"], "true");
    assert_eq!(details["existing_id"], holder.id.to_string());
    assert_eq!(
        service.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before,
        "an initial-equal replay must never call the embedding service"
    );

    runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            CONTENT,
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/no-embed-on-replay-control".into()),
                embed: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("a fresh key must create and embed");
    assert_eq!(
        service.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before + 1,
        "embedding this prefix must reach the service, or the replay assertion proves nothing"
    );
}

/// A fence stale at the initial check must refuse before preparation ever
/// runs. The create asks for an embedding, so preparation would call the
/// embedding service; the writer transaction that consumes a prepared plan
/// also checks fences, so the refusal alone cannot show where the fence
/// was checked, and the service's call counter is the witness.
#[tokio::test]
async fn item4_fence_stale_at_the_initial_check_refuses_before_preparation_runs() {
    let (runtime, token, service) = fixture();
    let fenced = create(&runtime, &token, "boundary/fence-target", Some(false)).await;
    assert_eq!(fenced.version, 1);
    runtime
        .update_note(
            &token,
            fenced.id,
            patch("{\"bumped\":true}", 1, Some(false)),
        )
        .await
        .unwrap();
    let calls_before = service.calls.load(std::sync::atomic::Ordering::SeqCst);

    let error = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/fence-dependent".into()),
                embed: Some(true),
                fence: Some(
                    NoteFence {
                        key: "boundary/fence-target".into(),
                        kind: "head".into(),
                        expected_version: Some(1),
                        live_until: None,
                        id: None,
                    }
                    .into(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect_err("a fence stale at the initial check must refuse before preparation runs");
    let details = details(error);
    assert_eq!(details["reason"], "fence_conflict");
    assert_eq!(details["key"], "boundary/fence-target");
    assert_eq!(
        service.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before,
        "a fence refused at the initial check must never reach preparation"
    );

    let live = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(
            token.namespace().as_str(),
            "boundary/fence-dependent",
            Some("head"),
        )
        .await
        .unwrap();
    assert!(
        live.is_empty(),
        "a fence-refused create must not leave a row behind"
    );

    runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/fence-dependent".into()),
                embed: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("the same create without the stale fence must succeed");
    assert_eq!(
        service.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before + 1,
        "this create's preparation must reach the service, or the refusal assertion proves nothing"
    );
}

/// Spawn a keyed create whose own preparation will fail (it names an
/// annotates target that was never created), pausing at the same
/// `race_seam` barrier preparation success uses, so a test can let an
/// independent writer act on the same key while the failure sits captured
/// and unconsumed.
fn spawn_prep_failing_racer(
    runtime: Arc<KhiveRuntime>,
    token: NamespaceToken,
    barrier: Arc<Barrier>,
    key: String,
    content: &'static str,
) -> tokio::task::JoinHandle<CreateOutcome> {
    let missing_target = uuid::Uuid::new_v4();
    tokio::spawn(race_seam::AFTER_PREPARE_BARRIER.scope(barrier, async move {
        runtime
            .create_note_with_options(
                &token,
                "head",
                None,
                content,
                None,
                None,
                None,
                None,
                vec![missing_target],
                None,
                NoteWriteOptions {
                    key: Some(key),
                    embed: Some(false),
                    ..Default::default()
                },
            )
            .await
    }))
}

/// Key initially absent; the racer's preparation fails (missing annotates
/// target) and is captured, not returned. While it sits parked at the pause,
/// an independent writer inserts an equal holder under the same key. The
/// final writer transaction must revalidate the holder before consuming the
/// captured failure, so the racer replays instead of surfacing NotFound.
#[tokio::test]
async fn item5_prepared_failure_then_an_equal_independent_holder_replays() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_prep_failing_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "boundary/prep-fails-equal".into(),
        "{}",
    );
    meet(&barrier).await;

    let (independent, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/prep-fails-equal".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "a parked preparation failure must not surface once an equal holder now exists",
    );
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], independent.id.to_string());
    assert_eq!(details["equal"], "true");
}

/// Same shape, a different independent holder: the racer must conflict
/// against it, discarding the captured preparation failure entirely.
#[tokio::test]
async fn item5_prepared_failure_then_a_different_independent_holder_conflicts() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_prep_failing_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "boundary/prep-fails-diff".into(),
        "{\"v\":1}",
    );
    meet(&barrier).await;

    let (independent, _) = runtime
        .create_note_with_options(
            &token,
            "head",
            None,
            "{\"v\":2}",
            None,
            None,
            None,
            None,
            vec![],
            None,
            NoteWriteOptions {
                key: Some("boundary/prep-fails-diff".into()),
                embed: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    meet(&barrier).await;

    let error = racer.await.expect("racer task").expect_err(
        "a different independent holder must conflict, discarding the preparation failure",
    );
    let details = details(error);
    assert_eq!(details["reason"], "key_conflict");
    assert_eq!(details["existing_id"], independent.id.to_string());
    assert_eq!(details["equal"], "false");
}

/// Same shape, the key still absent when the pause ends: the final writer
/// transaction finds nothing to revalidate against, so the captured
/// preparation failure (the original NotFound) is what the caller gets.
#[tokio::test]
async fn item5_prepared_failure_with_the_key_still_absent_returns_the_original_failure() {
    let (runtime, token, _service) = fixture();
    let runtime = Arc::new(runtime);
    let barrier = Arc::new(Barrier::new(2));
    let racer = spawn_prep_failing_racer(
        Arc::clone(&runtime),
        token.clone(),
        Arc::clone(&barrier),
        "boundary/prep-fails-absent".into(),
        "{}",
    );
    meet(&barrier).await;
    meet(&barrier).await;

    let error = racer
        .await
        .expect("racer task")
        .expect_err("a still-absent key must surface the original preparation failure");
    assert!(
        matches!(error, RuntimeError::NotFound(_)),
        "must be the original NotFound, not a key_conflict: {error:?}"
    );
}
