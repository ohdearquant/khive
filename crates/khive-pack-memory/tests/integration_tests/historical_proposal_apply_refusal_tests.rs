/// A proposal approved before this hook existed fails visibly at apply. The pre-upgrade state
/// is seeded through the same runtime code `propose` and `review(decision="approve")` call
/// (`ProposalsProjectionWorker::on_proposal_created` / `reviewed_and_emit`, read at source in
/// `crates/khive-pack-kg/src/projection_worker/mod.rs`), bypassing only the verb-level checks
/// in `handlers/proposal.rs` (including this amendment's own new admission check, which did not
/// exist yet). `propose` itself can no longer produce this state going forward; this is exactly
/// the pre-upgrade scenario ADR-021's amendment names.
///
/// The AddNote apply path (`prepare_add_note`, read at source in
/// `crates/khive-runtime/src/atomic_prepare.rs`) writes exactly two things for a successful
/// note: a `notes` row and an `fts_notes` row. It creates no `graph_edges` row, and there is no
/// separate provenance table for a note: a provenance/annotation edge is written only when a
/// draft carries a `source_id`, a field `NoteDraft` (and therefore `AddNote`) does not have. All
/// four are asserted unchanged below; `graph_edges` is the direct confirmation of that source
/// read, not a claim this seam narrows it.
#[tokio::test]
async fn apply_of_pre_upgrade_approved_add_note_memory_changeset_fails_visibly_and_writes_nothing()
{
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = tok.namespace().as_str().to_owned();

    let proposal_id = Uuid::new_v4();
    let seeded_content = "a pre-upgrade approved memory draft that must never reach storage";
    let changeset = khive_types::ProposalChangeset::AddNote {
        note: khive_types::NoteDraft {
            kind: "memory".to_string(),
            content: seeded_content.to_string(),
            name: None,
            properties: None,
        },
    };

    // Seed the ProposalCreated event and the proposals_open insert exactly as `handle_propose`
    // does: append the event first, then call the same `on_proposal_created` it calls.
    let created_payload = khive_types::ProposalCreatedPayload {
        proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
        proposer: "alice".to_string(),
        title: "pre-upgrade approved memory add_note".to_string(),
        description: "seeded directly to simulate approval before this hook existed".to_string(),
        changeset,
        reviewers: vec![],
        expiry: None,
        parent_id: None,
    };
    let created_payload_json =
        serde_json::to_value(&created_payload).expect("serialize seeded created payload");
    let mut created_event = khive_storage::event::Event::new(
        ns.as_str(),
        "propose",
        khive_types::EventKind::ProposalCreated,
        khive_storage::SubstrateKind::Entity,
        "alice",
    );
    created_event.payload = created_payload_json.clone();
    created_event.aggregate_kind = Some("proposal".to_string());
    created_event.aggregate_id = Some(proposal_id);
    rt.events(&tok)
        .expect("event store")
        .append_event(created_event)
        .await
        .expect("append seeded ProposalCreated event");
    khive_pack_kg::projection_worker::ProposalsProjectionWorker::new(rt.clone())
        .on_proposal_created(
            &tok,
            proposal_id,
            "alice",
            "pre-upgrade approved memory add_note",
            None,
        )
        .await
        .expect("seed the real proposals_open insert (status open)");

    // Seed the ProposalReviewed event and the approving CAS exactly as `handle_review` does for
    // `decision="approve"`: build the same payload shape and call the same `reviewed_and_emit`
    // it calls. Reviewer "bob" is distinct from proposer "alice", matching the ordinary shape a
    // real approval takes; the self-approval guard lives in `handle_review`, not in
    // `reviewed_and_emit`, so it is not in play here either way.
    let reviewed_payload = khive_types::ProposalReviewedPayload {
        proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
        reviewer: "bob".to_string(),
        decision: khive_types::ProposalDecision::Approve,
        comment: None,
    };
    let review_payload_json =
        serde_json::to_value(&reviewed_payload).expect("serialize seeded review payload");
    let mut review_event = khive_storage::event::Event::new(
        ns.as_str(),
        "review",
        khive_types::EventKind::ProposalReviewed,
        khive_storage::SubstrateKind::Entity,
        "bob",
    );
    review_event.payload = review_payload_json;
    review_event.aggregate_kind = Some("proposal".to_string());
    review_event.aggregate_id = Some(proposal_id);
    let (cas_hit, _review_event_id) =
        khive_pack_kg::projection_worker::ProposalsProjectionWorker::new(rt.clone())
            .reviewed_and_emit(&tok, &reviewed_payload, review_event, true)
            .await
            .expect("seed the real review CAS update plus ProposalReviewed event");
    assert!(cas_hit, "seeded review CAS must hit (open -> approved)");

    // Snapshot every table the AddNote apply path can write to, plus the full ordered event
    // history and the proposals_open row, before the apply attempt.
    let before_notes = notes_kind_count_including_deleted(&rt, &ns, "memory").await;
    let before_fts_notes = scalar_count(
        &rt,
        "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1 AND kind = ?2",
        vec![
            SqlValue::Text(ns.clone()),
            SqlValue::Text("memory".to_string()),
        ],
    )
    .await;
    let before_edges = scalar_count(
        &rt,
        "SELECT COUNT(*) FROM graph_edges WHERE namespace = ?1",
        vec![SqlValue::Text(ns.clone())],
    )
    .await;
    let before_events = proposal_events_newest_first(&rt, &tok, proposal_id).await;
    assert_eq!(
        before_events.len(),
        2,
        "the seed must leave exactly the ProposalCreated and ProposalReviewed rows: {before_events:?}"
    );
    let before_row = proposals_open_row(&rt, &ns, proposal_id).await;
    assert_eq!(
        row_text(&before_row, "status").as_deref(),
        Some("approved"),
        "the seeded row must read back approved before the apply attempt"
    );

    let apply_result = khive_pack_kg::apply_worker::ProposalApplyWorker::new(rt.clone())
        .maybe_apply_with_report(&tok, proposal_id, &registry, None)
        .await;
    assert!(
        apply_result.is_ok(),
        "apply-time refusal surfaces through the existing failed-apply lifecycle, not as an \
         Err from maybe_apply_with_report: {apply_result:?}"
    );

    // Storage-level counts, not a retrieval result: `memory.recall` is a ranked hybrid search,
    // and an empty result does not by itself prove nothing was written. These read every table
    // the apply path can write to, directly.
    assert_eq!(
        before_notes,
        notes_kind_count_including_deleted(&rt, &ns, "memory").await,
        "a refused apply must write no memory note (notes, including deleted)"
    );
    assert_eq!(
        before_fts_notes,
        scalar_count(
            &rt,
            "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1 AND kind = ?2",
            vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text("memory".to_string()),
            ],
        )
        .await,
        "a refused apply must write no fts_notes row"
    );
    assert_eq!(
        before_edges,
        scalar_count(
            &rt,
            "SELECT COUNT(*) FROM graph_edges WHERE namespace = ?1",
            vec![SqlValue::Text(ns.clone())],
        )
        .await,
        "a refused apply must write no graph_edges row (AddNote's apply path never writes one)"
    );

    let after_events = proposal_events_newest_first(&rt, &tok, proposal_id).await;
    assert_eq!(
        after_events.len(),
        3,
        "a failed apply must append exactly one event to the two seeded rows: {after_events:?}"
    );
    assert_eq!(
        after_events[0].kind,
        khive_types::EventKind::ProposalApplied,
        "the newest row (created_at DESC) must be the appended ProposalApplied event: {after_events:?}"
    );
    assert!(
        after_events[0].payload["result"]["failed"]["error"].is_string(),
        "the appended ProposalApplied event must carry the Failed variant (externally tagged \
         as \"failed\"): {:?}",
        after_events[0].payload
    );
    assert_eq!(
        &after_events[1..],
        before_events.as_slice(),
        "the two seeded rows must read back byte-identical, in the same order, behind the \
         appended row: before={before_events:?} after={after_events:?}"
    );

    let after_row = proposals_open_row(&rt, &ns, proposal_id).await;
    for column in PROPOSALS_OPEN_COLUMNS_UNTOUCHED_BY_THE_FAILURE_LIFECYCLE
        .iter()
        .copied()
    {
        assert_eq!(
            format!("{:?}", before_row.get(column)),
            format!("{:?}", after_row.get(column)),
            "column {column:?} must be unchanged by the failure lifecycle"
        );
    }
    assert_eq!(
        row_text(&after_row, "status").as_deref(),
        Some("approved"),
        "status must read back approved: approved -> applying -> approved nets to the value it \
         started at"
    );
    assert!(
        row_i64(&after_row, "updated_at") > row_i64(&before_row, "updated_at"),
        "updated_at is the one column the approved -> applying -> approved lifecycle is \
         documented to move (proposals_mark_applying.sql, proposals_revert_to_approved.sql): \
         before={before_row:?} after={after_row:?}"
    );
}
