//! ADR-008 canonical relation regression matrix.
//!
//! Table-driven tests covering all 15 ADR-002 edge relations through the parser
//! and validator paths. Ensures relation parsing, alias normalization, and
//! EdgeRelation delegation work for every canonical relation.

use khive_query::{compile, parse, validate, CompileOptions, QueryLanguage};

fn opts() -> CompileOptions {
    CompileOptions::default()
}

/// All 15 canonical edge relations from ADR-002.
const ALL_RELATIONS: &[&str] = &[
    "contains",
    "part_of",
    "instance_of",
    "extends",
    "variant_of",
    "introduced_by",
    "supersedes",
    "derived_from",
    "precedes",
    "depends_on",
    "enables",
    "implements",
    "competes_with",
    "composed_with",
    "annotates",
];

#[test]
fn all_canonical_relations_parse_and_validate() {
    for relation in ALL_RELATIONS {
        let gql = format!("MATCH (a)-[:{relation}]->(b) RETURN a");
        let mut q = parse(QueryLanguage::Gql, &gql).unwrap_or_else(|e| {
            panic!("parse failed for relation '{relation}': {e}");
        });
        validate(&mut q).unwrap_or_else(|e| {
            panic!("validate failed for relation '{relation}': {e}");
        });
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!(
            edge.relations,
            vec![relation.to_string()],
            "relation '{relation}' must normalize to itself"
        );
    }
}

#[test]
fn all_canonical_relations_compile_to_sql() {
    for relation in ALL_RELATIONS {
        let gql = format!("MATCH (a)-[:{relation}]->(b) RETURN a LIMIT 10");
        let q = parse(QueryLanguage::Gql, &gql).unwrap();
        let compiled = compile(&q, &opts()).unwrap_or_else(|e| {
            panic!("compile failed for relation '{relation}': {e}");
        });
        assert!(
            compiled.sql.contains("JOIN graph_edges"),
            "relation '{relation}' must produce a graph_edges join"
        );
    }
}

#[test]
fn canonical_relations_case_insensitive() {
    let case_variants = &[
        ("EXTENDS", "extends"),
        ("Contains", "contains"),
        ("PART_OF", "part_of"),
        ("Instance_Of", "instance_of"),
        ("VARIANT_OF", "variant_of"),
        ("Introduced_By", "introduced_by"),
        ("SUPERSEDES", "supersedes"),
        ("Derived_From", "derived_from"),
        ("PRECEDES", "precedes"),
        ("Depends_On", "depends_on"),
        ("ENABLES", "enables"),
        ("Implements", "implements"),
        ("COMPETES_WITH", "competes_with"),
        ("Composed_With", "composed_with"),
        ("ANNOTATES", "annotates"),
    ];

    for (input, expected) in case_variants {
        let gql = format!("MATCH (a)-[:{input}]->(b) RETURN a");
        let mut q = parse(QueryLanguage::Gql, &gql).unwrap();
        validate(&mut q).unwrap_or_else(|e| {
            panic!("validate failed for '{input}': {e}");
        });
        let edge = q.pattern.edges().next().unwrap();
        assert_eq!(
            edge.relations[0], *expected,
            "'{input}' must normalize to '{expected}'"
        );
    }
}

#[test]
fn invalid_relation_rejected() {
    let invalid_relations = &[
        "not_a_relation",
        "related_to",
        "has",
        "is",
        "belongs_to",
        "uses",
        "",
    ];

    for relation in invalid_relations {
        if relation.is_empty() {
            continue;
        }
        let gql = format!("MATCH (a)-[:{relation}]->(b) RETURN a");
        let mut q = parse(QueryLanguage::Gql, &gql).unwrap();
        let result = validate(&mut q);
        assert!(
            result.is_err(),
            "invalid relation '{relation}' must be rejected"
        );
    }
}

#[test]
fn all_canonical_relations_in_where_clause() {
    for relation in ALL_RELATIONS {
        let gql = format!("MATCH (a)-[e]->(b) WHERE e.relation = '{relation}' RETURN a");
        let q = parse(QueryLanguage::Gql, &gql).unwrap();
        let result = compile(&q, &opts());
        assert!(
            result.is_ok(),
            "WHERE e.relation = '{relation}' must compile; got {:?}",
            result.err()
        );
    }
}

#[test]
fn invalid_relation_in_where_rejected() {
    let gql = "MATCH (a)-[e]->(b) WHERE e.relation = 'not_a_relation' RETURN a";
    let q = parse(QueryLanguage::Gql, gql).unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_err(),
        "invalid relation in WHERE must be rejected"
    );
}

#[test]
fn all_canonical_relations_via_sparql() {
    for relation in ALL_RELATIONS {
        let sparql = format!("SELECT ?a ?b WHERE {{ ?a :{relation} ?b . }}");
        let q = parse(QueryLanguage::Sparql, &sparql).unwrap_or_else(|e| {
            panic!("SPARQL parse failed for relation '{relation}': {e}");
        });
        let compiled = compile(&q, &opts()).unwrap_or_else(|e| {
            panic!("SPARQL compile failed for relation '{relation}': {e}");
        });
        assert!(
            compiled.sql.contains("JOIN graph_edges"),
            "SPARQL relation '{relation}' must produce a graph_edges join"
        );
    }
}

#[test]
fn multi_relation_pipe_syntax() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends|variant_of|introduced_by]->(b) RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("IN"),
        "multi-relation must use IN clause; sql: {}",
        compiled.sql
    );
}
