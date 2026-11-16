//! Public syntax/AST/compiler contracts for edge metadata and caller ordering.

use khive_query::ast::{Condition, ConditionValue, OrderDirection, OrderItem, PropertyRef};
use khive_query::{
    compile, parse, parse_auto, CompileOptions, CompiledQuery, GqlQuery, QueryError, QueryLanguage,
    QueryValue, ReturnItem, WhereExpr,
};

fn gql(input: &str) -> GqlQuery {
    parse(QueryLanguage::Gql, input).unwrap()
}

fn sql(input: &str) -> CompiledQuery {
    compile(&gql(input), &CompileOptions::default()).unwrap()
}

fn params(compiled: &CompiledQuery) -> Vec<String> {
    compiled
        .params
        .iter()
        .map(|value| match value {
            QueryValue::Null => "null".into(),
            QueryValue::Integer(value) => format!("integer:{value}"),
            QueryValue::Float(value) => format!("float:{value}"),
            QueryValue::Text(value) => format!("text:{value}"),
            QueryValue::Blob(value) => format!("blob:{value:?}"),
        })
        .collect()
}

#[test]
fn edge_metadata_paths_and_order_parse_through_all_gql_entry_points() {
    let input = "MATCH (a)-[e:contains]->(b) WHERE e.metadata.nested.order >= 2 \
                 RETURN b.name, e.metadata ORDER BY e.metadata.order DESC, b.name ASC SKIP 2 LIMIT 8";
    for query in [
        parse(QueryLanguage::Gql, input).unwrap(),
        parse_auto(input).unwrap(),
        khive_query::parsers::gql::parse(input).unwrap(),
    ] {
        assert_eq!(
            query.return_items,
            [
                ReturnItem::Property("b".into(), "name".into()),
                ReturnItem::Property("e".into(), "metadata".into()),
            ]
        );
        let condition = query.where_clause.conditions().next().unwrap();
        assert_eq!(
            condition.property,
            PropertyRef::MetadataPath(vec!["nested".into(), "order".into()])
        );
        assert_eq!(
            query.order_by,
            [
                OrderItem {
                    variable: "e".into(),
                    property: PropertyRef::MetadataPath(vec!["order".into()]),
                    direction: OrderDirection::Desc
                },
                OrderItem {
                    variable: "b".into(),
                    property: PropertyRef::Field("name".into()),
                    direction: OrderDirection::Asc
                },
            ]
        );
        assert_eq!(query.offset, 2);
        assert_eq!(query.limit, Some(8));
    }
    assert_eq!(
        gql("MATCH (a) RETURN a.id ORDER BY a.id").order_by[0].direction,
        OrderDirection::Asc
    );
}

#[test]
fn fixed_metadata_and_scalar_order_lower_before_stable_ties() {
    let compiled = sql("MATCH (a)-[e:contains]->(b) \
        WHERE e.metadata.order = 9007199254740993 AND b.properties.order = '9007199254740993' \
        RETURN e.id, e.metadata ORDER BY e.metadata.order DESC, b.name ASC");
    assert!(compiled.sql.contains("e0.metadata AS \"e_metadata\""));
    assert!(compiled.sql.contains("json_extract(e0.metadata, ?2) = ?3"));
    assert!(compiled
        .sql
        .contains("json_extract(n1.properties, '$.order') = ?4 COLLATE NOCASE"));
    assert!(compiled.sql.contains("ORDER BY json_extract(e0.metadata, ?5) COLLATE BINARY DESC, n1.name COLLATE BINARY ASC, n0.substrate_kind, n0.id, e0.id, n1.substrate_kind, n1.id LIMIT"));
    assert_eq!(
        params(&compiled),
        [
            "text:contains",
            "text:$.order",
            "integer:9007199254740993",
            "text:9007199254740993",
            "text:$.order",
            "integer:0",
            "integer:501"
        ]
    );
    let whole_edge = sql("MATCH (a)-[e:contains]->(b) RETURN e");
    assert!(
        !whole_edge.sql.contains("e0.metadata AS"),
        "whole-edge projection remains unchanged"
    );
    let data_namespace = sql("MATCH (a)-[e:contains]->(b) WHERE e.metadata.namespace = 'visible-data' RETURN e.id ORDER BY e.metadata.namespace");
    assert!(data_namespace.sql.contains("json_extract(e0.metadata, ?2)"));
    assert_eq!(
        params(&data_namespace)
            .iter()
            .filter(|v| *v == "text:$.namespace")
            .count(),
        2
    );
    let properties_order = sql("MATCH (a) RETURN a.id ORDER BY a.properties.nested.order");
    assert!(properties_order
        .sql
        .contains("ORDER BY json_extract(n0.properties, ?1) COLLATE BINARY ASC"));
    assert_eq!(
        params(&properties_order),
        ["text:$.nested.order", "integer:0", "integer:501"]
    );
    assert!(params(&data_namespace).contains(&"text:visible-data".to_owned()));
}

#[test]
fn recursive_metadata_uses_final_phase_and_projected_values() {
    let compiled = sql("MATCH (a)-[e:contains*1..3]->(b) \
        WHERE b.name = 'last' OR e.metadata.order = 2 \
        RETURN a.id, a.name, e.metadata ORDER BY a.name DESC, e.metadata.order");
    assert!(compiled.sql.contains("via_weight, via_metadata) AS"));
    assert_eq!(
        compiled
            .sql
            .matches("e.id, e.relation, e.weight, e.metadata")
            .count(),
        2
    );
    assert!(compiled.sql.contains("t.via_metadata AS \"e_metadata\""));
    assert!(compiled
        .sql
        .contains("JOIN primary_nodes s ON s.id = t.start_id"));
    assert!(compiled.sql.contains("WHERE r.deleted_at IS NULL AND (r.name = ?3 COLLATE NOCASE OR json_extract(t.via_metadata, ?4) = ?5)"));
    assert!(compiled.sql.contains("ORDER BY \"a_name\" COLLATE BINARY DESC, json_extract(\"e_metadata\", ?7) COLLATE BINARY ASC, \"_depth\", \"_total_weight\" DESC"));
    assert_eq!(
        params(&compiled),
        [
            "text:contains",
            "integer:3",
            "text:last",
            "text:$.order",
            "integer:2",
            "integer:1",
            "text:$.order",
            "integer:0",
            "integer:501"
        ]
    );
    let properties_order = sql(
        "MATCH (a)-[e:contains*1..2]->(b) RETURN a.properties ORDER BY a.properties.nested.order",
    );
    assert!(properties_order
        .sql
        .contains("ORDER BY json_extract(\"a_properties\", ?4) COLLATE BINARY ASC"));
    assert_eq!(
        params(&properties_order),
        [
            "text:contains",
            "integer:2",
            "integer:1",
            "text:$.nested.order",
            "integer:0",
            "integer:501"
        ]
    );
    let filter_only =
        sql("MATCH (a)-[e:contains*1..2]->(b) WHERE e.metadata.order = 2 RETURN b.id");
    assert!(filter_only.sql.contains("via_metadata) AS"));
    assert!(!filter_only.sql.contains("AS \"e_metadata\""));
    let node_order = sql("MATCH (a)-[e:contains*1..2]->(b) RETURN a ORDER BY a.name");
    assert!(!node_order.sql.contains("via_metadata"));
}

#[test]
fn recursive_order_rejects_hidden_values_and_start_final_or() {
    for input in [
        "MATCH (a)-[e:contains*1..2]->(b) RETURN e ORDER BY e.metadata.order",
        "MATCH (a)-[e:contains*1..2]->(b) RETURN a ORDER BY a.description",
        "MATCH (a)-[e:contains*1..2]->(b) RETURN b.id ORDER BY a.name",
    ] {
        let error = compile(&gql(input), &CompileOptions::default()).unwrap_err();
        assert!(
            matches!(error, QueryError::Unsupported(ref message) if message.contains("variable-length ORDER BY must be derived from RETURN projections")),
            "{error:?}"
        );
    }
    for input in [
        "MATCH (a)-[e:contains*1..2]->(b) WHERE a.name = 'first' OR e.metadata.order = 1 RETURN b.id",
        "MATCH (a)-[e:contains*1..2]->(b) WHERE a.name = 'first' OR b.name = 'last' RETURN b.id",
    ] {
        assert!(matches!(compile(&gql(input), &CompileOptions::default()), Err(QueryError::Unsupported(_))));
    }
    let positive = sql("MATCH (a)-[e:contains*1..2]->(b) WHERE a.name = 'first' AND e.metadata.order = 2 RETURN a.description, e.metadata ORDER BY a.description, e.metadata.order");
    assert!(positive.sql.contains("s.description AS \"a_description\""));
}

#[test]
fn metadata_and_order_reject_invalid_parser_and_raw_ast_inputs() {
    for input in [
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY e.metadata.order SIDEWAYS",
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY e.metadata.order NULLS FIRST",
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY count(e.id)",
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY e.metadata.order,",
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY e.metadata.",
        "MATCH (a)-[e:contains]->(b) RETURN e.metadata ORDER BY e.other.path",
    ] {
        assert!(parse(QueryLanguage::Gql, input).is_err(), "{input}");
    }
    let input = "MATCH (a)-[e:contains]->(b), (a)-[x]->(c) RETURN e.metadata";
    let query = parse(QueryLanguage::Gql, input).expect("connected comma patterns parse");
    compile(&query, &CompileOptions::default()).expect("connected comma patterns compile");
    let input = "MATCH (a)-[e:contains]->(b), (c)-[x]->(d) RETURN e.metadata";
    let query = parse(QueryLanguage::Gql, input).expect("disconnected comma patterns parse");
    assert!(matches!(
        compile(&query, &CompileOptions::default()),
        Err(QueryError::Unsupported(_))
    ));
    for input in [
        "MATCH (a)-[e:contains]->(b) RETURN e.properties",
        "MATCH (a)-[e:contains]->(b) RETURN e.id ORDER BY e.metadata",
        "MATCH (a)-[e:contains]->(b) RETURN a.id ORDER BY a.properties",
        "MATCH (a)-[e:contains]->(b) RETURN e.id ORDER BY e.namespace",
        "MATCH (a)-[e:contains]->(b) RETURN e.id ORDER BY missing.id",
        "MATCH (a)-[e:contains]->(b) RETURN a.id ORDER BY a.metadata.order",
        "MATCH (a)-[e:contains]->(b) WHERE a.metadata.order = 1 RETURN a.id",
        "MATCH (a)-[e:contains]->(b) WHERE e.metadata = '{}' RETURN e.id",
    ] {
        assert!(
            compile(&gql(input), &CompileOptions::default()).is_err(),
            "{input}"
        );
    }
    for property in [
        PropertyRef::Field("namespace".into()),
        PropertyRef::Field("id; DELETE FROM entities".into()),
        PropertyRef::MetadataPath(Vec::new()),
        PropertyRef::MetadataPath(vec!["order') OR 1=1 --".into()]),
        PropertyRef::MetadataPath(vec!["order\u{0}".into()]),
    ] {
        let mut query = gql("MATCH (a)-[e:contains]->(b) RETURN e.metadata");
        query.order_by.push(OrderItem {
            variable: "e".into(),
            property: property.clone(),
            direction: OrderDirection::Asc,
        });
        assert!(
            compile(&query, &CompileOptions::default()).is_err(),
            "{property:?}"
        );
        query.order_by.clear();
        query.where_clause = WhereExpr::Condition(Condition {
            variable: "e".into(),
            property,
            op: khive_query::ast::CompareOp::Eq,
            value: ConditionValue::Integer(1),
        });
        assert!(compile(&query, &CompileOptions::default()).is_err());
    }
}

#[test]
fn synthetic_edges_refuse_metadata_without_changing_existing_queries() {
    for tail in [
        "RETURN e.metadata",
        "WHERE e.metadata.order = 1 RETURN a.id",
        "RETURN a.id ORDER BY e.metadata.order",
    ] {
        let query = gql(&format!(
            "MATCH (a:event)-[e:observed_as_target]->(b) {tail}"
        ));
        assert!(compile(&query, &CompileOptions::default()).is_err());
    }
    let positive = sql(
        "MATCH (a:event)-[:observed_as_target]->(b) RETURN a.id, b.id ORDER BY a.created_at DESC",
    );
    assert!(positive.sql.contains("JOIN event_observations e0"));
    assert!(positive.sql.contains("ORDER BY n0.created_at COLLATE BINARY DESC, n0.id, e0.event_id, e0.role, e0.position, n1.referent_kind, n1.id"));
}

#[test]
fn order_keywords_do_not_change_sparql_identifiers() {
    for word in ["order", "by", "asc", "desc", "metadata"] {
        let input = format!(
            "SELECT ?{word} WHERE {{ ?{word} :extends ?target . ?{word} :{word} 7 . }} LIMIT 2"
        );
        let query = parse(QueryLanguage::Sparql, &input).unwrap();
        assert!(query.order_by.is_empty());
        let compiled = compile(&query, &CompileOptions::default()).unwrap();
        assert!(params(&compiled).contains(&"integer:7".to_owned()));
        let query = gql(&format!(
            "MATCH (a)-[e:contains]->(b) RETURN e.id ORDER BY e.metadata.{word}"
        ));
        compile(&query, &CompileOptions::default()).unwrap();
    }
}

#[test]
fn legacy_fixed_and_recursive_sql_and_parameters_remain_exact() {
    let fixed = sql("MATCH (a) RETURN a.id");
    assert_eq!(fixed.sql, LEGACY_FIXED_SQL);
    assert_eq!(params(&fixed), ["integer:0", "integer:501"]);
    let recursive = sql("MATCH (a)-[e:extends*1..2]->(b) RETURN a.id, e.id, b.id");
    assert_eq!(recursive.sql, LEGACY_RECURSIVE_SQL);
    assert_eq!(
        params(&recursive),
        [
            "text:extends",
            "integer:2",
            "integer:1",
            "integer:0",
            "integer:501"
        ]
    );
}

// Literal snapshots derived from the pre-feature SQL lowering at 8d5267cc.
// They deliberately do not call the current renderer to construct expected SQL.
const LEGACY_FIXED_SQL: &str = r#"SELECT n0.id AS "a_id" FROM (SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS substrate_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS substrate_kind FROM notes UNION ALL SELECT id, namespace, kind, NULL AS entity_type, verb AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, payload AS properties, created_at, created_at AS updated_at, NULL AS deleted_at, 'event' AS substrate_kind FROM events UNION ALL SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, metadata AS properties, created_at, updated_at, deleted_at, 'edge' AS substrate_kind FROM graph_edges) n0  WHERE n0.deleted_at IS NULL ORDER BY n0.substrate_kind, n0.id LIMIT ?2 OFFSET ?1"#;

const LEGACY_RECURSIVE_SQL: &str = r#"WITH RECURSIVE primary_nodes AS (SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS substrate_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS substrate_kind FROM notes UNION ALL SELECT id, namespace, kind, NULL AS entity_type, verb AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, payload AS properties, created_at, created_at AS updated_at, NULL AS deleted_at, 'event' AS substrate_kind FROM events UNION ALL SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, metadata AS properties, created_at, updated_at, deleted_at, 'edge' AS substrate_kind FROM graph_edges), traverse(start_id, current_id, depth, path, total_weight, via_edge, via_relation, via_weight) AS (SELECT s.id, e.target_id, 1, s.id || ',' || e.target_id, e.weight, e.id, e.relation, e.weight FROM primary_nodes s JOIN graph_edges e ON e.source_id = s.id AND e.deleted_at IS NULL AND e.relation = ?1 JOIN primary_nodes next_node ON next_node.id = (e.target_id) AND next_node.deleted_at IS NULL WHERE s.deleted_at IS NULL UNION ALL SELECT t.start_id, e.target_id, t.depth + 1, t.path || ',' || e.target_id, t.total_weight + e.weight, e.id, e.relation, e.weight FROM traverse t CROSS JOIN graph_edges e ON e.source_id = t.current_id AND e.deleted_at IS NULL AND e.relation = ?1 JOIN primary_nodes next_node ON next_node.id = (e.target_id) AND next_node.deleted_at IS NULL WHERE t.depth < ?2 AND (',' || t.path || ',') NOT LIKE '%,' || e.target_id || ',%' ) SELECT DISTINCT s.id AS "a_id", t.via_edge AS "e_id", r.id AS "b_id", t.depth AS "_depth", t.total_weight AS "_total_weight" FROM traverse t JOIN primary_nodes s ON s.id = t.start_id JOIN primary_nodes r ON r.id = t.current_id WHERE r.deleted_at IS NULL AND t.depth >= ?3 ORDER BY "_depth", "_total_weight" DESC, "a_id", "e_id", "b_id" LIMIT ?5 OFFSET ?4"#;
