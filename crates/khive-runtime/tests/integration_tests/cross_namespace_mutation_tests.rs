// =============================================================================
// PR-A1: cross-namespace note by-ID operations (update_note / delete_note)
// =============================================================================

/// update_note via a foreign-namespace token must succeed (PR-A1).
/// Non-vacuity: this test FAILS if the old visible-set guard is restored.
#[tokio::test]
async fn update_note_cross_namespace_succeeds() {
    use khive_runtime::NotePatch;

    let rt = rt();
    let tok_a = rt
        .authorize(Namespace::parse("note-ns-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("note-ns-b").unwrap())
        .unwrap();

    let note = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "original content",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(note.namespace.as_str(), "note-ns-a");

    // Update from a different token — must succeed.
    let patch = NotePatch::new(None, Some("updated content".to_string()), None, None, None);
    let updated = rt
        .update_note_with_embedding_report(&tok_b, note.id, patch)
        .await
        .map(|(record, _report)| record);
    assert!(
        updated.is_ok(),
        "update_note from foreign token must succeed; got {:?}",
        updated
    );
    let updated = updated.unwrap();
    assert_eq!(updated.content, "updated content");
    // Namespace on the record must NOT change to tok_b's namespace.
    assert_eq!(
        updated.namespace.as_str(),
        "note-ns-a",
        "namespace must remain the record's stored namespace after cross-ns update"
    );
}

/// delete_note (soft and hard) via a foreign-namespace token must succeed (PR-A1).
/// Non-vacuity: this test FAILS if the old ensure_namespace guard is restored.
#[tokio::test]
async fn delete_note_cross_namespace_succeeds() {
    let rt = rt();
    let tok_a = rt.authorize(Namespace::parse("del-ns-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("del-ns-b").unwrap()).unwrap();

    // --- soft delete from foreign token ---
    let note_soft = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "soft target",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let soft_result = rt.delete_note(&tok_b, note_soft.id, false).await;
    assert!(
        soft_result.unwrap(),
        "cross-namespace soft delete_note must return true"
    );
    // Confirm gone via live query.
    let after_soft = rt
        .get_note_including_deleted(&tok_a, note_soft.id)
        .await
        .unwrap();
    assert!(
        after_soft.is_some(),
        "soft-deleted note must still appear in including_deleted"
    );

    // --- hard delete from foreign token ---
    let note_hard = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "hard target",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hard_result = rt.delete_note(&tok_b, note_hard.id, true).await;
    assert!(
        hard_result.unwrap(),
        "cross-namespace hard delete_note must return true"
    );
    let after_hard = rt
        .get_note_including_deleted(&tok_a, note_hard.id)
        .await
        .unwrap();
    assert!(
        after_hard.is_none(),
        "hard-deleted note must not appear even via including_deleted"
    );
}

// =============================================================================
// PR-A1: delete_edge cross-namespace audit-namespace tests
// =============================================================================

/// Soft-delete an edge via a foreign-namespace token.
///
/// Asserts: (1) the row is soft-deleted, (2) the EdgeDeleted audit event's
/// namespace == the record's own namespace (not the caller's).
///
/// Non-vacuity: this test FAILS (RC 101) if `delete_edge` uses the caller token
/// for event attribution instead of the record token derived from edge.namespace.
#[tokio::test]
async fn delete_edge_cross_namespace_audit_uses_record_namespace_soft() {
    let rt = rt();
    let tok_owner = rt.authorize(Namespace::parse("ns-owner").unwrap()).unwrap();
    let tok_caller = rt
        .authorize(Namespace::parse("ns-caller").unwrap())
        .unwrap();

    // Create two entities in ns-owner and link them.
    let src = rt
        .create_entity_with_embedding_report(
            &tok_owner,
            "concept",
            None,
            "AuditSrcSoft",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let tgt = rt
        .create_entity_with_embedding_report(
            &tok_owner,
            "concept",
            None,
            "AuditTgtSoft",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok_owner, src.id, tgt.id, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();
    let edge_id: Uuid = edge.id.into();

    // Soft-delete via a foreign-namespace caller token — must succeed.
    let deleted = rt.delete_edge(&tok_caller, edge_id, false).await.unwrap();
    assert!(deleted, "cross-namespace soft delete_edge must return true");

    // Row must be soft-deleted (gone from live query, present via including_deleted).
    let live = rt.get_edge(&tok_owner, edge_id).await.unwrap();
    assert!(
        live.is_none(),
        "soft-deleted edge must not appear in live get_edge"
    );
    let incl = rt
        .get_edge_including_deleted(&tok_owner, edge_id)
        .await
        .unwrap();
    assert!(
        incl.is_some(),
        "soft-deleted edge must appear via get_edge_including_deleted"
    );

    // Audit event namespace must be the record's namespace (ns-owner), not the caller's (ns-caller).
    let events = rt
        .list_events(
            &tok_owner,
            EventFilter {
                kinds: vec![EventKind::EdgeDeleted],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    let delete_event = events
        .items
        .iter()
        .find(|e| e.target_id == Some(edge_id))
        .expect("EdgeDeleted event must exist for the deleted edge");
    assert_eq!(
        delete_event.namespace, "ns-owner",
        "EdgeDeleted event namespace must be the record's namespace (ns-owner), not the caller's"
    );
    assert_eq!(
        delete_event
            .payload
            .get("namespace")
            .and_then(|v| v.as_str()),
        Some("ns-owner"),
        "EdgeDeleted payload.namespace must be the record's namespace (ns-owner)"
    );
}

/// Hard-delete an edge via a foreign-namespace token.
///
/// Asserts: (1) the row is hard-removed, (2) the EdgeDeleted audit event's
/// namespace == the record's own namespace (not the caller's).
///
/// Non-vacuity: this test FAILS (RC 101) if `delete_edge` uses the caller token
/// for event attribution instead of the record token derived from edge.namespace.
#[tokio::test]
async fn delete_edge_cross_namespace_audit_uses_record_namespace_hard() {
    let rt = rt();
    let tok_owner = rt
        .authorize(Namespace::parse("ns-owner-hard").unwrap())
        .unwrap();
    let tok_caller = rt
        .authorize(Namespace::parse("ns-caller-hard").unwrap())
        .unwrap();

    // Create two entities in ns-owner-hard and link them.
    let src = rt
        .create_entity_with_embedding_report(
            &tok_owner,
            "concept",
            None,
            "AuditSrcHard",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let tgt = rt
        .create_entity_with_embedding_report(
            &tok_owner,
            "concept",
            None,
            "AuditTgtHard",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok_owner, src.id, tgt.id, EdgeRelation::Extends, 0.6, None)
        .await
        .unwrap();
    let edge_id: Uuid = edge.id.into();

    // Hard-delete via a foreign-namespace caller token — must succeed.
    let deleted = rt.delete_edge(&tok_caller, edge_id, true).await.unwrap();
    assert!(deleted, "cross-namespace hard delete_edge must return true");

    // Row must be hard-removed (not present even via including_deleted).
    let incl = rt
        .get_edge_including_deleted(&tok_owner, edge_id)
        .await
        .unwrap();
    assert!(
        incl.is_none(),
        "hard-deleted edge must not appear via get_edge_including_deleted"
    );

    // Audit event namespace must be the record's namespace (ns-owner-hard), not the caller's.
    let events = rt
        .list_events(
            &tok_owner,
            EventFilter {
                kinds: vec![EventKind::EdgeDeleted],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    let delete_event = events
        .items
        .iter()
        .find(|e| e.target_id == Some(edge_id))
        .expect("EdgeDeleted event must exist for the hard-deleted edge");
    assert_eq!(
        delete_event.namespace, "ns-owner-hard",
        "EdgeDeleted event namespace must be record's namespace (ns-owner-hard), not caller's"
    );
    assert_eq!(
        delete_event
            .payload
            .get("namespace")
            .and_then(|v| v.as_str()),
        Some("ns-owner-hard"),
        "EdgeDeleted payload.namespace must be the record's namespace (ns-owner-hard)"
    );
}
