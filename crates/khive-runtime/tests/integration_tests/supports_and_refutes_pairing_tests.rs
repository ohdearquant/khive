// =============================================================================
// Epistemic endpoint tests (ADR-055 Phase 2+3)
// =============================================================================

// --- Entity→Entity ACCEPT cases ---

/// Concept→Concept supports: base allowlist row.
#[tokio::test]
async fn link_concept_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Finding A", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim B", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, a.id);
    assert_eq!(edge.target_id, b.id);
}

/// Document→Concept supports: base allowlist row.
#[tokio::test]
async fn link_document_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let doc = rt
        .create_entity_with_embedding_report(&tok, "document", None, "Paper X", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Hypothesis Y",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, doc.id, claim.id, EdgeRelation::Supports, 0.9, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
}

/// Concept→Concept refutes: base allowlist row.
#[tokio::test]
async fn link_concept_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let a = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Counter-evidence",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim B", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Refutes, 0.7, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

/// Document→Concept refutes: base allowlist row.
#[tokio::test]
async fn link_document_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let doc = rt
        .create_entity_with_embedding_report(
            &tok,
            "document",
            None,
            "Negative study",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let claim = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "Claim C", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let edge = rt
        .link(&tok, doc.id, claim.id, EdgeRelation::Refutes, 0.85, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

// --- Note→Note ACCEPT cases ---

/// Note→Note supports: same substrate, any note kind allowed.
#[tokio::test]
async fn link_note_note_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let finding = rt
        .create_note(
            &tok,
            "observation",
            Some("Finding note"),
            "experiment shows positive result",
            Some(0.8),
            None,
            vec![],
        )
        .await
        .unwrap();
    let claim = rt
        .create_note(
            &tok,
            "question",
            Some("Claim note"),
            "does intervention work?",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            finding.id,
            claim.id,
            EdgeRelation::Supports,
            0.9,
            None,
        )
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, finding.id);
    assert_eq!(edge.target_id, claim.id);
}

/// Note→Note refutes: same substrate allowed.
#[tokio::test]
async fn link_note_note_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let counter = rt
        .create_note(
            &tok,
            "observation",
            Some("Counter finding"),
            "null result from replication",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hypothesis = rt
        .create_note(
            &tok,
            "insight",
            Some("Hypothesis"),
            "the intervention increases outcome",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            counter.id,
            hypothesis.id,
            EdgeRelation::Refutes,
            0.75,
            None,
        )
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

// --- Cross-substrate REJECT cases ---

/// Note→Entity supports: cross-substrate, must error.
#[tokio::test]
async fn link_note_entity_supports_rejected() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "finding note",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Some concept",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let result = rt
        .link(&tok, note.id, entity.id, EdgeRelation::Supports, 0.8, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "note→entity supports must be rejected (cross-substrate); got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

/// Entity→Note refutes: cross-substrate, must error.
#[tokio::test]
async fn link_entity_note_refutes_rejected() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "A concept", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "some note",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let result = rt
        .link(&tok, entity.id, note.id, EdgeRelation::Refutes, 0.5, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "entity→note refutes must be rejected (cross-substrate); got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("refutes"),
        "error message must name the relation 'refutes'; got: {msg}"
    );
}

// --- Disallowed entity pair REJECT case ---

/// Person→Concept supports: not in base allowlist, must error naming the relation.
#[tokio::test]
async fn link_person_concept_supports_rejected_with_relation_name() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let person = rt
        .create_entity_with_embedding_report(
            &tok,
            "person",
            None,
            "Researcher A",
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
            "Hypothesis Z",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let result = rt
        .link(&tok, person.id, claim.id, EdgeRelation::Supports, 0.5, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "person→concept supports is not in base allowlist; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}
