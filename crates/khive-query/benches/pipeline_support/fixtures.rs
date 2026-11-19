//! Exact legacy GQL inputs plus generated large/complex inputs.

use khive_query::ast::ConditionValue;
use khive_query::{CompiledQuery, GqlQuery, QueryValue};

pub const SCOPE: &str = "benchmark-query-private";
pub const GQL: &[(&str, &str)] = &[
    ("simple/simple_node", "MATCH (n:concept) RETURN n"),
    ("simple/two_node_edge", "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a, b"),
    ("simple/node_with_limit", "MATCH (n:document) RETURN n LIMIT 20"),
    ("medium/where_eq_string", "MATCH (a:concept)-[e:extends]->(b:project) WHERE b.name = 'lattice-inference' RETURN a LIMIT 10"),
    ("medium/where_and", "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' AND b.kind = 'concept' RETURN a, b"),
    ("medium/where_or", "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a"),
    ("medium/where_and_or", "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a"),
    ("medium/where_with_edge_var", "MATCH (a)-[e:implements]->(b:project) WHERE b.name = 'khive' RETURN a, e, b LIMIT 50"),
    ("medium/node_with_properties", "MATCH (n:document {entity_type: 'paper'}) RETURN n LIMIT 5"),
    ("complex/three_node_chain", "MATCH (a:concept)-[:introduced_by]->(p:paper)-[:introduced_by]->(c:concept) RETURN a, c"),
    ("complex/variable_length_multi_rel", "MATCH (a {name: 'LoRA'})-[:extends|variant_of*1..3]->(b) RETURN b LIMIT 20"),
    ("complex/variable_length_with_where", "MATCH (a:concept)-[:extends*1..5]->(b:concept) WHERE a.name = 'FlashAttention' RETURN b LIMIT 100"),
    ("complex/three_node_mixed_direction", "MATCH (a:person)<-[e:introduced_by]-(c:concept)-[:extends]->(b:concept) RETURN a, c, b LIMIT 10"),
    ("complex/node_multi_property_map", "MATCH (n:concept {name: 'LoRA', entity_type: 'algorithm'})-[:extends]->(b) RETURN b"),
    ("complex/undirected_edge", "MATCH (a:concept)-[e:competes_with]-(b:concept) RETURN a, b"),
 ];

pub fn large_gql() -> String {
    let properties = (0..20)
        .map(|i| format!("p{i:02}: 'generated-value-{i:02}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut query = format!("MATCH (n0:concept {{{properties}}})");
    for i in 1..10 {
        query.push_str(&format!("-[:depends_on]->(n{i}:concept)"));
    }
    query.push_str(" RETURN n0, n9 LIMIT 20");
    query
}

// Six flat triples, including four connected relation patterns. This is not
// nested SPARQL syntax: the public parser does not support nested paths/groups.
pub const COMPLEX_SPARQL: &str = "SELECT ?a ?e WHERE {
    ?a a :concept .
    ?a :depends_on{1,3} ?b .
    ?b :contains ?c .
    ?c :extends+ ?d .
    ?d :implements ?e .
    ?e :name 'generated-target' .
} LIMIT 20";

pub fn assert_shape(actual: &GqlQuery, expected: &GqlQuery) {
    assert_eq!(actual.return_items, expected.return_items);
    assert_eq!(
        actual.pattern.elements.len(),
        expected.pattern.elements.len()
    );
    assert_eq!(
        actual.pattern.nodes().count(),
        expected.pattern.nodes().count()
    );
    assert_eq!(
        actual.pattern.edges().count(),
        expected.pattern.edges().count()
    );
    assert_eq!(actual.limit, expected.limit);
    assert_eq!(actual.offset, expected.offset);
}

pub fn assert_compiled(compiled: &CompiledQuery, query: &GqlQuery) {
    assert!(compiled.sql.starts_with("SELECT") || compiled.sql.starts_with("WITH"));
    assert_eq!(compiled.return_vars, query.return_items);
    assert!(compiled.warnings.is_empty());
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(v) if v == SCOPE)));
    assert!(!compiled.sql.contains(SCOPE));
}

pub fn assert_large(query: &GqlQuery) {
    assert_eq!(query.pattern.nodes().count(), 10);
    assert_eq!(query.pattern.edges().count(), 9);
    assert_eq!(query.return_items.len(), 2);
    assert_eq!(query.return_items[0].variable(), "n0");
    assert_eq!(query.return_items[1].variable(), "n9");
    let first = query.pattern.nodes().next().expect("first node");
    assert_eq!(first.properties.len(), 20);
    for i in 0..20 {
        assert_eq!(
            first.properties.get(&format!("p{i:02}")),
            Some(&ConditionValue::String(format!("generated-value-{i:02}")))
        );
    }
}

pub fn assert_large_compiled(compiled: &CompiledQuery, query: &GqlQuery) {
    assert_compiled(compiled, query);
    for i in 0..20 {
        let expected = format!("generated-value-{i:02}");
        assert!(compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(v) if v == &expected)));
        assert!(!compiled.sql.contains(&expected));
    }
}

pub fn assert_sparql(query: &GqlQuery) {
    assert_eq!(query.pattern.nodes().count(), 5);
    assert_eq!(query.pattern.edges().count(), 4);
    assert_eq!(query.return_items.len(), 2);
    assert_eq!(query.return_items[0].variable(), "a");
    assert_eq!(query.return_items[1].variable(), "e");
    let expected = [
        ("depends_on", 1, 3),
        ("contains", 1, 1),
        ("extends", 1, 5),
        ("implements", 1, 1),
    ];
    for (edge, (relation, min, max)) in query.pattern.edges().zip(expected) {
        assert_eq!(edge.relations.len(), 1);
        assert_eq!(edge.relations[0], relation);
        assert_eq!((edge.min_hops, edge.max_hops), (min, max));
    }
    let first = query.pattern.nodes().next().expect("first node");
    assert_eq!(first.variable.as_deref(), Some("a"));
    assert_eq!(first.kind.as_deref(), Some("concept"));
    let last = query.pattern.nodes().last().expect("last node");
    assert_eq!(last.variable.as_deref(), Some("e"));
    assert_eq!(
        last.properties.get("name"),
        Some(&ConditionValue::String("generated-target".into()))
    );
    assert_eq!(query.limit, Some(20));
}
