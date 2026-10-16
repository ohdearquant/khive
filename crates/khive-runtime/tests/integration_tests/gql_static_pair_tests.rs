// =============================================================================
// GQL static edge-pattern schema-mismatch hint (issue #593)
//
// A MATCH pattern that names an explicit relation and explicit entity kinds
// on both endpoints can be *statically* impossible under the composed edge
// endpoint contract (base allowlist + pack EDGE_RULES) — e.g. `precedes`
// never accepts concept->concept endpoints. Executed unchanged, this always
// returns zero rows, indistinguishable from "no data yet". These tests cover
// the warning that distinguishes the two cases, scoped to the exact cases
// where the emptiness is statically knowable.
// =============================================================================

#[tokio::test]
async fn query_static_impossible_pattern_precedes_concept_concept_warns_and_returns_empty() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Real `precedes` edges exist — just never between two concepts (issue
    // #593's repro: 3445 `precedes` edges in the wild, all document/project/etc).
    let d1 = rt
        .create_entity_with_embedding_report(&tok, "document", None, "Doc1", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let d2 = rt
        .create_entity_with_embedding_report(&tok, "document", None, "Doc2", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, d1.id, d2.id, EdgeRelation::Precedes, 1.0, None)
        .await
        .unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (n:concept)-[:precedes]->(m:concept) RETURN n.name, m.name LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.rows.is_empty(),
        "no concept->concept precedes edges exist: {:?}",
        result.rows
    );
    assert_eq!(result.warnings.len(), 1, "warnings: {:?}", result.warnings);
    let w = &result.warnings[0];
    assert!(w.contains("precedes"), "{w}");
    assert!(w.contains("concept"), "{w}");
    assert!(
        w.contains("document->document"),
        "must name an accepted pair for the relation: {w}"
    );
}

#[tokio::test]
async fn query_static_possible_pattern_extends_concept_concept_no_warning() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "LoRA2", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "QLoRA2", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, b.id, a.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (n:concept)-[:extends]->(m:concept) RETURN n, m LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.rows.len(),
        1,
        "concept->concept is a valid base-contract pair for extends"
    );
    assert!(
        result.warnings.is_empty(),
        "a statically-valid pattern must not warn: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_check_skips_unlabeled_source_node() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // The source node is unlabeled — no static (kind, relation, kind) triple
    // is named, so this must never warn regardless of what `precedes` accepts.
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (n)-[:precedes]->(m:document) RETURN n, m LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.warnings.is_empty(),
        "an unlabeled endpoint names no static triple to test: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_check_honors_pack_extended_endpoint_rules() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Mirrors khive-pack-kg's KG_EDGE_RULES: `person part_of org` is valid
    // only via a pack-declared rule, never the base allowlist alone. The hint
    // must consult the exact same composed rule set the link validator does.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::PartOf,
        source: khive_types::EndpointKind::EntityOfKind("person"),
        target: khive_types::EndpointKind::EntityOfKind("org"),
    }]);

    let p = rt
        .create_entity_with_embedding_report(&tok, "person", None, "Ada", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let o = rt
        .create_entity_with_embedding_report(&tok, "org", None, "Acme", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, p.id, o.id, EdgeRelation::PartOf, 1.0, None)
        .await
        .unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (n:person)-[:part_of]->(m:org) RETURN n, m LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert!(
        result.warnings.is_empty(),
        "a pack-declared endpoint rule must not be falsely flagged as impossible: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn link_and_hint_agree_special_relation_pack_rules_never_enforced() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // supersedes is a SPECIAL_RELATIONS entry (pack.rs): the live validator
    // resolves it in a dedicated branch (operations.rs) that only consults
    // base_entity_rule_allows, never pack_rule_allows. A pack rule for it must
    // therefore be inert on both paths: `link` must still reject the pair, and
    // the GQL static hint must still warn, even with the rule installed.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::Supersedes,
        source: khive_types::EndpointKind::EntityOfKind("person"),
        target: khive_types::EndpointKind::EntityOfKind("person"),
    }]);

    let p1 = rt
        .create_entity_with_embedding_report(&tok, "person", None, "Old Ada", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let p2 = rt
        .create_entity_with_embedding_report(&tok, "person", None, "New Ada", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let link_result = rt
        .link(&tok, p1.id, p2.id, EdgeRelation::Supersedes, 1.0, None)
        .await;
    assert!(
        link_result.is_err(),
        "person->person supersedes must be rejected: the special-relation branch \
         never consults the installed pack rule; got {link_result:?}"
    );

    let query_result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:person)-[:supersedes]->(b:person) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        query_result.warnings.len(),
        1,
        "the hint must agree with the validator and warn even though a pack rule \
         is installed for this special relation: {:?}",
        query_result.warnings
    );
}

#[tokio::test]
async fn query_static_possible_pattern_inbound_introduced_by_no_warning() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let doc = rt
        .create_entity_with_embedding_report(&tok, "document", None, "Paper", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let author = rt
        .create_entity_with_embedding_report(&tok, "person", None, "Author", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(
        &tok,
        doc.id,
        author.id,
        EdgeRelation::IntroducedBy,
        1.0,
        None,
    )
    .await
    .unwrap();

    // Same (document, introduced_by, person) triple as the base allowlist, but
    // expressed with an inbound (`<-`) arrow instead of the outbound (`->`)
    // form used elsewhere: source is the right-hand node (document), target is
    // the left-hand node (person).
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (p:person)<-[:introduced_by]-(d:document) RETURN d, p LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert!(
        result.warnings.is_empty(),
        "an inbound-arrow pattern naming a valid base-contract triple must not warn: {:?}",
        result.warnings
    );
}
