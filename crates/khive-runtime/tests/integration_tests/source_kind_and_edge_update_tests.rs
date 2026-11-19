// --- Remaining allowlist source kinds ---

/// Dataset→Concept supports: base allowlist row (previously untested source kind).
#[tokio::test]
async fn link_dataset_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ds = rt
        .create_entity_with_embedding_report(&tok, "dataset", None, "Bench-X", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Hypothesis Q",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, ds.id, claim.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
}

/// Artifact→Concept refutes: base allowlist row (previously untested source kind).
#[tokio::test]
async fn link_artifact_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let art = rt
        .create_entity_with_embedding_report(
            &tok,
            "artifact",
            None,
            "Checkpoint-v1",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim R", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, art.id, claim.id, EdgeRelation::Refutes, 0.7, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

/// Artifact→Concept supports: base allowlist row (previously untested combination).
#[tokio::test]
async fn link_artifact_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let art = rt
        .create_entity_with_embedding_report(
            &tok,
            "artifact",
            None,
            "Checkpoint-v2",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim T", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, art.id, claim.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, art.id);
    assert_eq!(edge.target_id, claim.id);
}

/// Dataset→Concept refutes: base allowlist row (previously untested combination).
#[tokio::test]
async fn link_dataset_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ds = rt
        .create_entity_with_embedding_report(&tok, "dataset", None, "Bench-Y", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Hypothesis W",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, ds.id, claim.id, EdgeRelation::Refutes, 0.75, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
    assert_eq!(edge.source_id, ds.id);
    assert_eq!(edge.target_id, claim.id);
}

// --- update_edge parity tests ---

/// (a) update_edge legal entity edge → Supports on allowlist pair: accepted.
/// Uses concept→concept: start with Extends, update to Supports.
#[tokio::test]
async fn update_edge_to_supports_on_legal_entity_pair_accepted() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let evidence = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Evidence concept",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Hypothesis H",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    // Start with Extends (legal for concept→concept).
    let edge = rt
        .link(
            &tok,
            evidence.id,
            claim.id,
            EdgeRelation::Extends,
            0.9,
            None,
        )
        .await
        .unwrap();
    // Update the relation to Supports — concept→concept is in the Supports allowlist.
    let updated = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge to supports on concept→concept must be accepted");
    assert_eq!(updated.relation, EdgeRelation::Supports);
}

/// (b) update_edge entity edge → Supports on off-allowlist pair: rejected.
#[tokio::test]
async fn update_edge_to_supports_on_disallowed_entity_pair_rejected() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let person = rt
        .create_entity_with_embedding_report(
            &tok,
            "person",
            None,
            "Researcher B",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let concept = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim S", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    // Person→Concept with a relation that IS legal to start (introduced_by is
    // illegal for person→concept too — use enables which IS legal for person
    // is also illegal, use instance_of which allows *→concept).
    // Simplest: use instance_of (valid for *→concept) to create the edge first.
    let edge = rt
        .link(
            &tok,
            person.id,
            concept.id,
            EdgeRelation::InstanceOf,
            1.0,
            None,
        )
        .await
        .unwrap();
    // Now update to Supports — person is not in the allowlist for supports.
    let result = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "update_edge to supports on person→concept must be rejected; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
    assert!(
        msg.contains(
            "currently legal relations for person -> concept under the loaded endpoint rules: instance_of"
        ),
        "error message must expose the derived legal-relations set from the shared validator \
         update_edge reaches at operations.rs; got: {msg}"
    );
}

/// (c) update_edge note→entity annotates edge → Supports: rejected (cross-substrate).
#[tokio::test]
async fn update_edge_annotates_to_supports_rejected_cross_substrate() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Target concept",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "some observation",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    // Create note→entity annotates edge (the only legal cross-substrate relation).
    let edge = rt
        .link(&tok, note.id, entity.id, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();
    // Update to Supports → must fail (note→entity is cross-substrate for supports).
    let result = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "update_edge note→entity annotates → supports must be rejected; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

/// (d) update_edge note→note edge → Refutes: accepted (same substrate).
#[tokio::test]
async fn update_edge_note_note_to_refutes_accepted() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note_a = rt
        .create_note(
            &tok,
            "observation",
            None,
            "prior finding",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .unwrap();
    let note_b = rt
        .create_note(
            &tok,
            "insight",
            None,
            "derived claim",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    // Create a note→note edge with Supports first.
    let edge = rt
        .link(
            &tok,
            note_a.id,
            note_b.id,
            EdgeRelation::Supports,
            0.8,
            None,
        )
        .await
        .unwrap();
    // Update to Refutes — note→note same-substrate, must be accepted.
    let updated = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Refutes),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge note→note supports → refutes must be accepted");
    assert_eq!(updated.relation, EdgeRelation::Refutes);
}
