// =============================================================================
// Multi-namespace read visibility (visible-set tokens)
// =============================================================================

/// ADR-007 PR-A1: visible-set enforcement on by-ID ops is removed.
/// list_entities / list_notes still filter by visible_namespaces (PR-B collapses that).
/// get_entity and get_note_including_deleted now return any record by UUID regardless
/// of the token's visible set.  Writes still land in the primary namespace only.
#[tokio::test]
async fn visible_set_reads_primary_and_extra_not_third() {
    let rt = rt();

    // Mint single-namespace write tokens for three isolated namespaces.
    let tok_a = rt.authorize(Namespace::parse("vis-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("vis-b").unwrap()).unwrap();
    let tok_c = rt.authorize(Namespace::parse("vis-c").unwrap()).unwrap();

    // Write one entity and one note in each namespace.
    let entity_a = rt
        .create_entity_with_embedding_report(&tok_a, "concept", None, "EntityA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let entity_b = rt
        .create_entity_with_embedding_report(&tok_b, "concept", None, "EntityB", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let entity_c = rt
        .create_entity_with_embedding_report(&tok_c, "concept", None, "EntityC", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let note_a = rt
        .create_note(&tok_a, "observation", None, "NoteA", None, None, vec![])
        .await
        .unwrap();
    let note_b = rt
        .create_note(&tok_b, "observation", None, "NoteB", None, None, vec![])
        .await
        .unwrap();
    let note_c = rt
        .create_note(&tok_c, "observation", None, "NoteC", None, None, vec![])
        .await
        .unwrap();

    // Mint a visible-set token: primary=vis-a, visible=[vis-a, vis-b].
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("vis-a").unwrap(),
            vec![Namespace::parse("vis-b").unwrap()],
        )
        .unwrap();

    // --- list_entities sees a+b, not c ---
    let visible_entities = rt.list_entities(&vis_tok, None, None, 50, 0).await.unwrap();
    let entity_names: Vec<&str> = visible_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(entity_names.contains(&"EntityA"), "EntityA must be visible");
    assert!(entity_names.contains(&"EntityB"), "EntityB must be visible");
    assert!(
        !entity_names.contains(&"EntityC"),
        "EntityC must NOT be visible"
    );

    // --- list_notes sees a+b, not c ---
    let visible_notes = rt.list_notes(&vis_tok, None, 50, 0).await.unwrap();
    let note_contents: Vec<&str> = visible_notes.iter().map(|n| n.content.as_str()).collect();
    assert!(note_contents.contains(&"NoteA"), "NoteA must be visible");
    assert!(note_contents.contains(&"NoteB"), "NoteB must be visible");
    assert!(
        !note_contents.contains(&"NoteC"),
        "NoteC must NOT be visible"
    );

    // --- get_entity: all three succeed by UUID (PR-A1: visible-set gate removed) ---
    rt.get_entity(&vis_tok, entity_a.id)
        .await
        .expect("get entity_a must succeed");
    rt.get_entity(&vis_tok, entity_b.id)
        .await
        .expect("get entity_b (visible non-primary) must succeed");
    rt.get_entity(&vis_tok, entity_c.id)
        .await
        .expect("get entity_c by UUID succeeds — visible-set gate removed in PR-A1");

    // --- get_note: all three returned by UUID (PR-A1: visible-set gate removed) ---
    let fetched_note_a = rt
        .get_note_including_deleted(&vis_tok, note_a.id)
        .await
        .expect("call must not error");
    assert!(
        fetched_note_a.is_some(),
        "note_a (primary namespace) must be returned"
    );

    let fetched_note_b = rt
        .get_note_including_deleted(&vis_tok, note_b.id)
        .await
        .expect("call must not error");
    assert!(
        fetched_note_b.is_some(),
        "note_b (visible non-primary) must be returned"
    );

    let fetched_note_c = rt
        .get_note_including_deleted(&vis_tok, note_c.id)
        .await
        .expect("call must not error");
    // PR-A1: by-ID get returns the note regardless of visible set (list_notes still filters — PR-B).
    assert!(
        fetched_note_c.is_some(),
        "note_c (outside visible set) must be returned by UUID via PR-A1 by-ID contract"
    );
    assert_eq!(
        fetched_note_c.as_ref().unwrap().namespace.as_str(),
        "vis-c",
        "fetched note_c must preserve its stored namespace"
    );

    // --- WRITE via vis_tok lands in primary (vis-a) only, not in vis-b ---
    let written = rt
        .create_entity_with_embedding_report(
            &vis_tok,
            "concept",
            None,
            "WrittenViaVisToken",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    assert_eq!(
        written.namespace.as_str(),
        "vis-a",
        "write must stamp primary namespace, not any extra-visible one"
    );

    // Verify vis-b does not contain the newly written entity.
    let b_entities = rt.list_entities(&tok_b, None, None, 50, 0).await.unwrap();
    let b_names: Vec<&str> = b_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !b_names.contains(&"WrittenViaVisToken"),
        "write must NOT appear in vis-b"
    );

    // Suppress unused-variable warnings for IDs we intentionally only inserted.
    let _ = note_a;
    let _ = note_c;
    let _ = entity_c;
}

/// Backward compatibility: a token minted via `authorize()` (no visibility)
/// behaves exactly as before — single namespace, strict equality on reads/writes.
/// This is the original namespace_isolation test reproduced verbatim to confirm
/// nothing regressed.
#[tokio::test]
async fn namespace_isolation_backward_compat() {
    let rt = rt();
    let ns_a_tok = rt.authorize(Namespace::parse("bc-a").unwrap()).unwrap();
    let ns_b_tok = rt.authorize(Namespace::parse("bc-b").unwrap()).unwrap();

    rt.create_entity_with_embedding_report(
        &ns_a_tok,
        "concept",
        None,
        "EntityA",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();
    rt.create_entity_with_embedding_report(
        &ns_b_tok,
        "concept",
        None,
        "EntityB",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let a_entities = rt
        .list_entities(&ns_a_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(a_entities.len(), 1);
    assert_eq!(a_entities[0].name, "EntityA");

    let b_entities = rt
        .list_entities(&ns_b_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(b_entities.len(), 1);
    assert_eq!(b_entities[0].name, "EntityB");
}
