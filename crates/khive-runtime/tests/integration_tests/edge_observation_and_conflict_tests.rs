// =============================================================================
// F218 integration: synthetic observed_as_* edge end-to-end (CRIT-1 regression)
// =============================================================================

/// This test is the ONLY test that would have caught CRIT-1 (wrong JOIN target).
///
/// It seeds a real event + event_observations row and executes the canonical
/// ADR-041 §11 synthetic-edge GQL query end-to-end against an in-memory SQLite
/// database.  The old code joined `event_observations.event_id = entities.id`,
/// which can never match because the two ID spaces are disjoint.
#[tokio::test]
async fn synthetic_edge_observed_as_selected_returns_memory_note() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = "local";

    // Step 1: create a memory note (the observed entity).
    let memory_note = rt
        .create_note(
            &tok,
            "memory",
            None,
            "recalled memory content",
            Some(0.9),
            None,
            vec![],
        )
        .await
        .unwrap();
    let memory_id = memory_note.id;

    // Step 2: create an event of kind SearchExecuted with a payload that
    // includes `selected: [memory_id]`.  The storage layer's `append_event`
    // routes `SearchExecuted` to `decode_search_observations`, which requires
    // the `result_kind` discriminator (`entity` | `note`) to identify which
    // substrate owns every UUID in `candidates` and `selected`, then inserts a
    // row into `event_observations` with role="selected" and
    // entity_id=memory_id. (`selected` is part of `SearchExecuted`/
    // `RecallExecuted`'s ADR-041 projection contract; `RecallExecuted` still
    // decodes via `decode_recall_observations` from flat note UUID lists,
    // unlike `RerankExecuted`, which projects `selected` rows from
    // `final_scores`/`reranked` instead, since its typed payload has no
    // `selected` field.)
    let event_store = rt.events(&tok).unwrap();
    let mut event = Event::new(
        ns,
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    );
    event.payload = serde_json::json!({
        "result_kind": "note",
        "candidates": [],
        "selected": [memory_id.to_string()]
    });
    event_store.append_event(event).await.unwrap();

    // Step 3: execute the canonical ADR-041 §11 GQL query.
    // Before CRIT-1 fix: `FROM entities n0 JOIN event_observations e0 ON e0.event_id = n0.id`
    //   — IDs are disjoint, so zero rows returned.
    // After fix: `FROM events n0 JOIN event_observations e0 ON e0.event_id = n0.id`
    //   — correct join; the memory note is returned.
    let rows = rt
        .query(
            &tok,
            "MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN m",
        )
        .await
        .unwrap();

    assert!(
        !rows.is_empty(),
        "CRIT-1: synthetic edge query must return at least one row (memory note was seeded); \
         got 0 rows — event_observations join is broken"
    );

    // Verify the returned row contains our memory note's UUID.
    let memory_id_str = memory_id.to_string();
    let found = rows.iter().any(|row| {
        row.columns.iter().any(|col| {
            if let khive_storage::types::SqlValue::Text(s) = &col.value {
                s.contains(&memory_id_str)
            } else {
                false
            }
        })
    });
    assert!(
        found,
        "CRIT-1: returned rows must include the seeded memory note id {}; columns: {:?}",
        memory_id,
        rows.iter()
            .map(|r| r
                .columns
                .iter()
                .map(|c| (&c.name, &c.value))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
}

// =============================================================================
// update_edge conflict handling regression tests
// =============================================================================

/// Regression for Bug 1: when update_edge absorbs a conflict (the requested edge
/// is deleted and the existing canonical row is preserved unchanged, ADR-039 DO
/// NOTHING), the returned edge must carry the SURVIVING canonical row's id — not
/// the id of the deleted edge.
///
/// Setup: pre-create canonical A→B competes_with (E1), create A→B extends (E2).
/// Update E2's relation to competes_with. The returned id must be E1, not E2.
/// A subsequent get(returned_id) must succeed.
#[tokio::test]
async fn update_edge_returns_surviving_canonical_id_on_conflict() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "SurvA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "SurvB", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // E1: canonical competes_with between A and B (runtime canonicalises order).
    let e1 = rt
        .link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
        .await
        .unwrap();

    // E2: non-symmetric extends edge, using the higher-uuid as source so that
    // updating to competes_with will trigger a flip (endpoints_flipped=true path).
    let (src, tgt) = if a.id > b.id {
        (a.id, b.id)
    } else {
        (b.id, a.id)
    };
    let e2 = rt
        .link(&tok, src, tgt, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();

    // E1 and E2 must be different edges.
    assert_ne!(
        e1.id, e2.id,
        "pre-condition: E1 and E2 must be distinct edges"
    );

    // Update E2 to competes_with → conflict with E1 must be absorbed.
    let returned = rt
        .update_edge(
            &tok,
            e2.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::CompetesWith),
                weight: Some(0.9),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge must succeed even when conflict is absorbed");

    // Bug 1 assertion: returned id must be E1 (surviving canonical row), not E2 (deleted).
    assert_eq!(
        returned.id, e1.id,
        "Bug 1: update_edge must return the SURVIVING canonical row id (E1={:?}), \
         got E2={:?}",
        e1.id, returned.id
    );

    // get(returned.id) must succeed — it must not 404.
    let fetched = rt
        .get_edge(&tok, returned.id.into())
        .await
        .expect("get_edge on returned id must not error")
        .expect("get_edge on returned id must find a row (not 404)");
    assert_eq!(
        fetched.id, e1.id,
        "fetched row id must match E1 (surviving canonical)"
    );

    // E2 must no longer exist.
    let e2_lookup = rt
        .get_edge(&tok, e2.id.into())
        .await
        .expect("get_edge on deleted id must not error");
    assert!(
        e2_lookup.is_none(),
        "Bug 1: deleted edge E2 must not be findable after conflict absorption"
    );
}

/// Regression for Bug 2: when an edge's relation is updated to a symmetric relation
/// and the endpoints are ALREADY in canonical order (endpoints_flipped=false),
/// a pre-existing canonical row with the same natural key must still be detected and
/// absorbed — no UNIQUE-constraint error, no duplicate row.
///
/// Setup: ensure A < B (canonical order). Pre-create canonical A→B competes_with (E1).
/// Create A→B extends (E2, already canonical since A < B and extends is non-symmetric).
/// Update E2's relation to competes_with (endpoints_flipped=false because A < B).
/// Assert: exactly one live competes_with edge remains between A and B.
#[tokio::test]
async fn update_edge_canonical_orientation_conflict() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "CanOrA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "CanOrB", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // Determine canonical order: canon_lo < canon_hi.
    let (canon_lo, canon_hi) = if a.id < b.id {
        (a.id, b.id)
    } else {
        (b.id, a.id)
    };

    // E1: canonical competes_with (lower → higher, which is canonical).
    let e1 = rt
        .link(
            &tok,
            canon_lo,
            canon_hi,
            EdgeRelation::CompetesWith,
            1.0,
            None,
        )
        .await
        .unwrap();

    // E2: extends in the same canonical direction (lower → higher).
    // endpoints_flipped will be false when we update to competes_with.
    let e2 = rt
        .link(&tok, canon_lo, canon_hi, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();

    assert_ne!(
        e1.id, e2.id,
        "pre-condition: E1 and E2 must be distinct edges"
    );

    // Update E2's relation to competes_with — must not produce UNIQUE-constraint error.
    // Bug 2: the non-flipped path used to call upsert_edge which only checked ON CONFLICT(id),
    // missing the natural-key duplicate with a different id.
    rt.update_edge(
        &tok,
        e2.id.into(),
        EdgePatch {
            relation: Some(EdgeRelation::CompetesWith),
            ..Default::default()
        },
    )
    .await
    .expect("Bug 2: update_edge on canonical-orientation conflict must not error");

    // Verify exactly one live competes_with edge exists between canon_lo and canon_hi.
    let edges = rt
        .list_edges(
            &tok,
            khive_runtime::EdgeListFilter {
                source_id: Some(canon_lo),
                target_id: Some(canon_hi),
                relations: vec![EdgeRelation::CompetesWith],
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .expect("list_edges must succeed");

    assert_eq!(
        edges.len(),
        1,
        "Bug 2: exactly one competes_with edge must exist after non-flipped conflict absorption; \
         found {} edges: {edges:?}",
        edges.len()
    );
}
