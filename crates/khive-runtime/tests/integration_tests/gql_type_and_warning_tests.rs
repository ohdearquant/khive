#[tokio::test]
async fn query_static_possible_pattern_entity_of_type_pack_rule_no_false_warning() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // theorem depends_on definition has no base-contract row (concept->concept
    // depends_on is not in BASE_ENTITY_ENDPOINT_RULES) — only the installed
    // EntityOfType pack rule makes it possible.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let thm = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            Some("theorem"),
            "T1",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let def = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            Some("definition"),
            "D1",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, thm.id, def.id, EdgeRelation::DependsOn, 1.0, None)
        .await
        .unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept {entity_type: 'theorem'})-[:depends_on]->\
             (b:concept {entity_type: 'definition'}) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert!(
        result.warnings.is_empty(),
        "the hint must consult the pack's EntityOfType rule (matched against \
         each node's entity_type, not just its base kind) so this must not be \
         falsely flagged: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_possible_pattern_untyped_endpoints_with_entity_of_type_rule_no_false_warning()
{
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Same EntityOfType rule as the typed-pattern test above, but the pattern
    // here names no `entity_type` on either endpoint — just the base kind.
    // An untyped pattern endpoint has not ruled out any subtype, so the
    // installed theorem->definition rule still makes concept->concept
    // depends_on possible and this must not warn.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let thm = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            Some("theorem"),
            "T1",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let def = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            Some("definition"),
            "D1",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, thm.id, def.id, EdgeRelation::DependsOn, 1.0, None)
        .await
        .unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept)-[:depends_on]->(b:concept) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.rows.len(),
        1,
        "the stored theorem->definition edge must be returned: {:?}",
        result.rows
    );
    assert!(
        result.warnings.is_empty(),
        "an untyped pattern endpoint has not ruled out any entity_type, so it must not \
         be falsely flagged as impossible just because it names no entity_type filter: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_pattern_mismatching_entity_type_still_warns() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Same EntityOfType rule, but the pattern names an entity_type that does
    // NOT match the rule's source subtype ("lemma" vs the rule's "theorem")
    // — exact-match semantics must still apply and this must warn.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept {entity_type: 'lemma'})-[:depends_on]->\
             (b:concept {entity_type: 'definition'}) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.warnings.len(),
        1,
        "a pattern entity_type that mismatches the installed rule's subtype must still \
         be flagged as impossible: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_warning_accepted_pairs_include_entity_of_type_only_relation() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // depends_on has no base-contract person->person row (the base allowlist
    // only grants project/service/artifact/document pairs); the only way
    // concept->concept becomes accepted is the installed EntityOfType rule.
    // person->person stays impossible and must still warn, and the
    // accepted-pairs list in the warning text must surface the
    // EntityOfType-derived concept->concept pair rather than omitting it.
    rt.install_edge_rules(vec![khive_types::EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "theorem",
        },
        target: khive_types::EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: "definition",
        },
    }]);

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:person)-[:depends_on]->(b:person) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.warnings.len(), 1, "warnings: {:?}", result.warnings);
    assert!(
        result.warnings[0].contains("concept->concept"),
        "the accepted-pairs list must include the EntityOfType-derived concept->concept \
         pair for depends_on, not just base-allowlist pairs: {}",
        result.warnings[0]
    );
}

#[tokio::test]
async fn query_static_impossible_chained_pattern_warns_once_per_edge() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Two chained edges sharing node `b`, both statically impossible
    // (concept->concept precedes is never in the composed contract). Each
    // edge is scanned independently, so both must warn.
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept)-[:precedes]->(b:concept)-[:precedes]->(c:concept) RETURN a, b, c LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(
        result.warnings.len(),
        2,
        "a chained pattern with two statically-impossible edges sharing a node \
         must warn once per edge: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_check_skips_multi_relation_edge() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Two relations named on one edge — no single static triple to test, so
    // this must never warn even though neither relation accepts concept->concept.
    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept)-[:precedes|contains]->(b:concept) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.warnings.is_empty(),
        "a multi-relation edge names no single static triple to test: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_check_skips_variable_length_edge() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept)-[:precedes*1..2]->(b:concept) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.warnings.is_empty(),
        "a variable-length hop is not a single mandatory hop and must not warn: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_check_skips_undirected_edge() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (a:concept)-[:precedes]-(b:concept) RETURN a, b LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.warnings.is_empty(),
        "an undirected edge names no fixed (source, target) triple and must not warn: {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn query_static_impossible_pattern_warning_exact_message() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let result = rt
        .query_with_metadata(
            &tok,
            "MATCH (n:concept)-[:precedes]->(m:concept) RETURN n, m LIMIT 10",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    // Exact-message pin (issue #593's public contract): this is a load-bearing
    // string once callers script against it, so pin the full text, not just a
    // substring, catching any accidental wording/ordering drift.
    assert_eq!(
        result.warnings,
        vec![
            "pattern (concept)-[:precedes]->(concept) can never match: 'precedes' does not \
             accept concept->concept endpoints; accepted source->target kinds for 'precedes': \
             document->document, dataset->dataset, project->project, artifact->artifact, \
             service->service"
                .to_string()
        ]
    );
}

#[tokio::test]
async fn query_static_impossible_check_skips_sparql() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

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

    // Same statically-impossible shape as the GQL case above (precedes
    // concept->concept), expressed in SPARQL — must never receive the hint.
    let result = rt
        .query_with_metadata(
            &tok,
            "SELECT ?a ?b WHERE { ?a :precedes ?b . ?a a :concept . ?b a :concept . }",
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();

    assert!(
        result.warnings.is_empty(),
        "SPARQL must never receive the static GQL-pattern hint: {:?}",
        result.warnings
    );
}
