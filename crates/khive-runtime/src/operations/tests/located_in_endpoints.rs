use super::*;

// ── Location endpoint pair (ADR-196) ─────────────────────────────────────
// Base rows are concept->concept and org->concept (amended 2026-10-05); other
// base kinds are left to the first pack that emits them.

#[tokio::test]
async fn link_concept_located_in_concept_allowed_other_base_kinds_rejected() {
    let rt = rt();
    let tok = NamespaceToken::local();

    let pneumonia = rt
        .create_entity(&tok, "concept", None, "Pneumonia", None, None, vec![])
        .await
        .unwrap();
    let lung = rt
        .create_entity(&tok, "concept", None, "Lung", None, None, vec![])
        .await
        .unwrap();

    let result = rt
        .link(
            &tok,
            pneumonia.id,
            lung.id,
            EdgeRelation::LocatedIn,
            1.0,
            None,
        )
        .await;
    assert!(
        result.is_ok(),
        "concept->concept located_in must be allowed by the ADR-196 \
         endpoint amendment; got {result:?}"
    );
    let edge = result.unwrap();
    assert_eq!(edge.relation, EdgeRelation::LocatedIn);
    assert!(
        edge.metadata.is_none(),
        "located_in carries no governed metadata and infers none; got {:?}",
        edge.metadata
    );

    let page = rt
        .create_entity(&tok, "document", None, "Atlas page", None, None, vec![])
        .await
        .unwrap();
    let concept_to_doc = rt
        .link(
            &tok,
            pneumonia.id,
            page.id,
            EdgeRelation::LocatedIn,
            1.0,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        concept_to_doc
            .to_string()
            .contains("base endpoint allowlist"),
        "concept->document located_in must be refused with the \
         endpoint-contract error; got {concept_to_doc}"
    );
}
