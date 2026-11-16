//! Connected comma-MATCH syntax, direct AST admission, and compatibility contracts.

use khive_query::ast::{
    CompareOp, Condition, ConditionValue, MatchPattern, OrderDirection, OrderItem, PatternElement,
    PropertyRef,
};
use khive_query::{
    compile, compile_with_mailbox, parse, parse_auto, validate, CompileOptions, CompiledQuery,
    GqlQuery, QueryError, QueryLanguage, QueryMailboxScope, QueryValue, WhereExpr,
};

fn gql(text: &str) -> GqlQuery {
    parse(QueryLanguage::Gql, text).unwrap()
}

fn sql(text: &str) -> CompiledQuery {
    compile(&gql(text), &CompileOptions::default()).unwrap()
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
fn comma_patterns_parse_one_global_tail_and_ordered_iterators() {
    let source = "MATCH (a:concept)-[e:extends]->(b), (a)-[f:variant_of]->(c) WHERE e.metadata.order >= 0 AND f.metadata.enabled = true RETURN a.id, e.metadata, f.id ORDER BY e.metadata.order, f.metadata.order DESC SKIP 2 LIMIT 5";
    for mut query in [
        gql(source),
        parse_auto(source).unwrap(),
        khive_query::parsers::gql::parse(source).unwrap(),
    ] {
        assert_eq!(query.additional_patterns.len(), 1);
        assert_eq!(
            query
                .patterns()
                .map(|pattern| pattern.elements.len())
                .collect::<Vec<_>>(),
            [3, 3]
        );
        assert_eq!(
            query
                .where_clause
                .conditions()
                .map(|condition| condition.variable.as_str())
                .collect::<Vec<_>>(),
            ["e", "f"]
        );
        assert_eq!(query.return_items.len(), 3);
        assert_eq!(query.order_by[1].variable, "f");
        assert_eq!(query.order_by[1].direction, OrderDirection::Desc);
        assert_eq!((query.offset, query.limit), (2, Some(5)));
        for pattern in query.patterns_mut() {
            for element in &mut pattern.elements {
                if let PatternElement::Edge(edge) = element {
                    edge.relations = vec!["extends".into()];
                }
            }
        }
        assert!(query
            .patterns()
            .flat_map(|pattern| pattern.edges())
            .all(|edge| edge.relations == ["extends"]));
        validate(&mut query).unwrap();
        compile(&query, &CompileOptions::default()).unwrap();
    }
    for malformed in [
        "MATCH (a), RETURN a",
        "MATCH (a),, (a) RETURN a",
        "MATCH ,(a) RETURN a",
        "MATCH (a) MATCH (a) RETURN a",
        "MATCH (a), (a) RETURN a AS x",
        "MATCH (a), (a) RETURN DISTINCT a",
        "MATCH (a), (a) OPTIONAL MATCH (b) RETURN a",
        "MATCH (a), (a) RETURN count(a)",
    ] {
        assert!(parse(QueryLanguage::Gql, malformed).is_err(), "{malformed}");
    }
}

#[test]
fn comma_patterns_validate_complete_connectivity_and_binding_kinds() {
    for source in [
        "MATCH (a)-[e]->(b), (c)-[f]->(d), (b)-[g]->(c) RETURN a.id, d.id",
        "MATCH (b)-[g]->(c), (a)-[e]->(b), (c)-[f]->(d) RETURN a.id, d.id",
        "MATCH (a)-[e]->(b)-[g]->(c), (c)-[f]->(d) RETURN a.id, d.id",
        "MATCH (a)-[e]->(b), (b)-[f]->(a) RETURN a.id",
        "MATCH (a), (a:concept {name: 'one'}) RETURN a.id",
        "MATCH (a)-[e]->(b), (b)-[f]->(A) RETURN A.id",
        "MATCH (a)-[e]->(), (a)-[f]->() RETURN a.id",
    ] {
        compile(&gql(source), &CompileOptions::default())
            .unwrap_or_else(|error| panic!("{source}: {error}"));
    }
    for (source, message) in [
        (
            "MATCH (a), (b) RETURN a",
            "connected through shared named nodes",
        ),
        (
            "MATCH ()-[e]->(b), ()-[f]->(c) RETURN b",
            "connected through shared named nodes",
        ),
        (
            "MATCH (a)-[e:extends*1..2]->(b), (a)-[f]->(c) RETURN a",
            "canonical fixed one-hop edges",
        ),
        (
            "MATCH (a)-[e]->(b), (a)-[f:observed_as_target]->(c) RETURN a",
            "canonical fixed one-hop edges",
        ),
        (
            "MATCH (a)-[e]->(b), (a)-[e]->(c) RETURN a",
            "repeated edge variable 'e' not supported",
        ),
        (
            "MATCH (a)-[e]->(b), (a)-[f]->(a) RETURN a",
            "repeated node variable 'a'",
        ),
    ] {
        let error = compile(&gql(source), &CompileOptions::default()).unwrap_err();
        assert!(
            matches!(&error, QueryError::Unsupported(_)),
            "{source}: {error}"
        );
        assert!(error.to_string().contains(message), "{error}");
    }
    for source in [
        "MATCH (a)-[e]->(b), (e)-[f]->(c) RETURN a",
        "MATCH (a)-[e]->(b), (a)-[b]->(c) RETURN a",
    ] {
        let error = compile(&gql(source), &CompileOptions::default()).unwrap_err();
        assert!(
            matches!(&error, QueryError::Validation(message) if message.contains("cannot bind both a node and an edge"))
        );
    }
}

#[test]
fn comma_disconnected_direct_ast_is_refused_before_lowering() {
    // Construct the additional path independently so parser rejection cannot establish this guard.
    let mut query = gql("MATCH (a)-[e:extends]->(b) RETURN a.id");
    query
        .additional_patterns
        .push(gql("MATCH (c)-[f:extends]->(d) RETURN c.id").pattern);
    let error = compile(&query, &CompileOptions::default()).unwrap_err();
    assert!(
        matches!(&error, QueryError::Unsupported(message) if message.contains("connected through shared named nodes")),
        "{error}"
    );
    // A third component joins the first two even though their textual prefix is disconnected.
    query
        .additional_patterns
        .push(gql("MATCH (b)-[g:extends]->(c) RETURN b.id").pattern);
    compile(&query, &CompileOptions::default()).unwrap();
}

#[test]
fn comma_direct_ast_rejects_malformed_later_paths_and_injected_references() {
    let baseline =
        gql("MATCH (a)-[e:extends]->(b), (a)-[f:variant_of]->(c) RETURN e.id, f.metadata");
    let mut empty = baseline.clone();
    empty
        .additional_patterns
        .push(MatchPattern { elements: vec![] });
    assert!(
        matches!(compile(&empty, &CompileOptions::default()), Err(QueryError::Validation(message)) if message == "empty additional MATCH pattern")
    );
    let mut malformed = baseline.clone();
    malformed.additional_patterns[0].elements.remove(0);
    assert!(matches!(
        compile(&malformed, &CompileOptions::default()),
        Err(QueryError::Validation(_))
    ));
    for field in ["namespace", "id; DELETE FROM entities"] {
        let mut query = baseline.clone();
        query.order_by.push(OrderItem {
            variable: "c".into(),
            property: PropertyRef::Field(field.into()),
            direction: OrderDirection::Asc,
        });
        assert!(compile(&query, &CompileOptions::default()).is_err());
    }
    for property in [
        PropertyRef::MetadataPath(vec![]),
        PropertyRef::MetadataPath(vec!["x') OR 1=1 --".into()]),
        PropertyRef::MetadataPath(vec!["x\0".into()]),
    ] {
        let mut query = baseline.clone();
        query.where_clause = WhereExpr::Condition(Condition {
            variable: "f".into(),
            property: property.clone(),
            op: CompareOp::Eq,
            value: ConditionValue::Integer(1),
        });
        assert!(compile(&query, &CompileOptions::default()).is_err());
        query.where_clause = WhereExpr::True;
        query.order_by.push(OrderItem {
            variable: "f".into(),
            property,
            direction: OrderDirection::Asc,
        });
        assert!(compile(&query, &CompileOptions::default()).is_err());
    }
    for tail in [
        "WHERE c.namespace = 'other' RETURN a.id",
        "RETURN a.id ORDER BY c.namespace",
        "RETURN missing.id",
        "WHERE missing.name = 'x' RETURN a.id",
        "RETURN a.id ORDER BY missing.id",
    ] {
        let query = gql(&format!(
            "MATCH (a)-[e:extends]->(b), (a)-[f:variant_of]->(c) {tail}"
        ));
        assert!(
            compile(&query, &CompileOptions::default()).is_err(),
            "{tail}"
        );
    }
    let mut namespace = baseline.clone();
    if let PatternElement::Node(node) = &mut namespace.additional_patterns[0].elements[0] {
        node.properties
            .insert("namespace".into(), ConditionValue::String("other".into()));
    }
    assert!(matches!(
        compile(&namespace, &CompileOptions::default()),
        Err(QueryError::Validation(_))
    ));
    compile(&baseline, &CompileOptions::default()).unwrap();
}

#[test]
fn comma_compilation_keeps_first_alias_all_filters_and_bound_metadata() {
    let query = gql("MATCH (a:concept {name: 'first'})-[e:extends]->(b), (a:entity {name: 'second'})-[f:variant_of]->(c) WHERE e.metadata.order > 1 OR c.name = 'last' RETURN a.id, e.metadata, f.metadata ORDER BY f.metadata.order DESC SKIP 2 LIMIT 5");
    let compiled = compile(&query, &CompileOptions::default()).unwrap();
    assert!(compiled.sql.contains("n0.id AS \"a_id\""));
    assert!(compiled
        .sql
        .contains("n2.substrate_kind = n0.substrate_kind"));
    assert!(compiled.sql.contains("n2.id = n0.id"));
    assert!(compiled.sql.contains("n0.name = ?2 COLLATE NOCASE"));
    assert!(compiled.sql.contains("n2.name = ?5 COLLATE NOCASE"));
    assert!(compiled
        .sql
        .contains("(json_extract(e0.metadata, ?7) > ?8 OR n3.name = ?9 COLLATE NOCASE)"));
    assert!(compiled.sql.contains("ORDER BY json_extract(e1.metadata, ?10) COLLATE BINARY DESC, n0.substrate_kind, n0.id, e0.id, n1.substrate_kind, n1.id, n2.substrate_kind, n2.id, e1.id, n3.substrate_kind, n3.id LIMIT ?12 OFFSET ?11"));
    assert_eq!(
        params(&compiled),
        [
            "text:concept",
            "text:first",
            "text:extends",
            "text:entity",
            "text:second",
            "text:variant_of",
            "text:$.order",
            "integer:1",
            "text:last",
            "text:$.order",
            "integer:2",
            "integer:3"
        ]
    );
    assert!(!compiled.sql.contains("SELECT DISTINCT"));
    let mailbox = QueryMailboxScope {
        actor_id: "reader".into(),
        legacy_local: false,
    };
    let scoped = compile_with_mailbox(&query, &CompileOptions::default(), &mailbox).unwrap();
    // One caller parameter binding is reused by all filtered sources, not one actor per path.
    assert_eq!(
        params(&scoped)
            .iter()
            .filter(|value| value.as_str() == "text:reader")
            .count(),
        1
    );
    assert!(scoped.sql.matches("CROSS JOIN").count() == 1);
    assert!(scoped.sql.contains("e1.metadata AS \"f_metadata\""));
}

#[test]
fn comma_extension_preserves_single_path_literal_sql_and_parameters() {
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
    let metadata = sql("MATCH (a)-[e:extends]->(b) WHERE e.metadata.rank = 2 RETURN e.metadata ORDER BY e.metadata.rank DESC");
    assert_eq!(metadata.sql, format!("SELECT e0.metadata AS \"e_metadata\" FROM ({PRIMARY_NODES}) n0 JOIN graph_edges e0 ON e0.source_id = n0.id AND e0.deleted_at IS NULL JOIN ({PRIMARY_NODES}) n1 ON n1.id = e0.target_id WHERE n0.deleted_at IS NULL AND e0.relation = ?1 AND n1.deleted_at IS NULL AND json_extract(e0.metadata, ?2) = ?3 ORDER BY json_extract(e0.metadata, ?4) COLLATE BINARY DESC, n0.substrate_kind, n0.id, e0.id, n1.substrate_kind, n1.id LIMIT ?6 OFFSET ?5"));
    assert_eq!(
        params(&metadata),
        [
            "text:extends",
            "text:$.rank",
            "integer:2",
            "text:$.rank",
            "integer:0",
            "integer:501"
        ]
    );
    let synthetic = sql("MATCH (ev)-[e:observed_as_target]->(b) RETURN b.id");
    assert_eq!(synthetic.sql, format!("SELECT n1.id AS \"b_id\" FROM events n0 JOIN event_observations e0 ON e0.event_id = n0.id JOIN ({OBSERVATION_TARGETS}) n1 ON n1.id = e0.entity_id AND n1.referent_kind = e0.referent_kind WHERE e0.role = ?1 AND ((e0.role IN ('candidate', 'selected') AND e0.referent_kind IN ('entity', 'note')) OR (e0.role = 'target' AND e0.referent_kind IN ('entity', 'note')) OR (e0.role = 'signal' AND e0.referent_kind IN ('entity', 'note'))) AND n1.deleted_at IS NULL ORDER BY n0.id, e0.event_id, e0.role, e0.position, n1.referent_kind, n1.id LIMIT ?3 OFFSET ?2"));
    assert_eq!(
        params(&synthetic),
        ["text:target", "integer:0", "integer:501"]
    );
    let sparql = parse(
        QueryLanguage::Sparql,
        "SELECT ?b WHERE { ?a :extends ?b . }",
    )
    .unwrap();
    assert!(sparql.additional_patterns.is_empty());
    let compiled = compile(&sparql, &CompileOptions::default()).unwrap();
    assert_eq!(compiled.sql, format!("SELECT n1.id AS \"b_id\", n1.namespace AS \"b_namespace\", n1.kind AS \"b_kind\", n1.entity_type AS \"b_entity_type\", n1.name AS \"b_name\", n1.properties AS \"b_properties\", n1.created_at AS \"b_created_at\", n1.updated_at AS \"b_updated_at\" FROM ({PRIMARY_NODES}) n0 JOIN graph_edges e0 ON e0.source_id = n0.id AND e0.deleted_at IS NULL JOIN ({PRIMARY_NODES}) n1 ON n1.id = e0.target_id WHERE n0.deleted_at IS NULL AND e0.relation = ?1 AND n1.deleted_at IS NULL ORDER BY n0.substrate_kind, n0.id, e0.id, n1.substrate_kind, n1.id LIMIT ?3 OFFSET ?2"));
    assert_eq!(
        params(&compiled),
        ["text:extends", "integer:0", "integer:501"]
    );
    assert!(parse(
        QueryLanguage::Sparql,
        "SELECT ?a WHERE { ?a :extends ?b . ?a :variant_of ?c . }"
    )
    .is_err());
}

// Exact single-path snapshots retained from the pre-comma compiler.
const LEGACY_FIXED_SQL: &str = r#"SELECT n0.id AS "a_id" FROM (SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS substrate_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS substrate_kind FROM notes UNION ALL SELECT id, namespace, kind, NULL AS entity_type, verb AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, payload AS properties, created_at, created_at AS updated_at, NULL AS deleted_at, 'event' AS substrate_kind FROM events UNION ALL SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, metadata AS properties, created_at, updated_at, deleted_at, 'edge' AS substrate_kind FROM graph_edges) n0  WHERE n0.deleted_at IS NULL ORDER BY n0.substrate_kind, n0.id LIMIT ?2 OFFSET ?1"#;

const LEGACY_RECURSIVE_SQL: &str = r#"WITH RECURSIVE primary_nodes AS (SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS substrate_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS substrate_kind FROM notes UNION ALL SELECT id, namespace, kind, NULL AS entity_type, verb AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, payload AS properties, created_at, created_at AS updated_at, NULL AS deleted_at, 'event' AS substrate_kind FROM events UNION ALL SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, metadata AS properties, created_at, updated_at, deleted_at, 'edge' AS substrate_kind FROM graph_edges), traverse(start_id, current_id, depth, path, total_weight, via_edge, via_relation, via_weight) AS (SELECT s.id, e.target_id, 1, s.id || ',' || e.target_id, e.weight, e.id, e.relation, e.weight FROM primary_nodes s JOIN graph_edges e ON e.source_id = s.id AND e.deleted_at IS NULL AND e.relation = ?1 JOIN primary_nodes next_node ON next_node.id = (e.target_id) AND next_node.deleted_at IS NULL WHERE s.deleted_at IS NULL UNION ALL SELECT t.start_id, e.target_id, t.depth + 1, t.path || ',' || e.target_id, t.total_weight + e.weight, e.id, e.relation, e.weight FROM traverse t CROSS JOIN graph_edges e ON e.source_id = t.current_id AND e.deleted_at IS NULL AND e.relation = ?1 JOIN primary_nodes next_node ON next_node.id = (e.target_id) AND next_node.deleted_at IS NULL WHERE t.depth < ?2 AND (',' || t.path || ',') NOT LIKE '%,' || e.target_id || ',%' ) SELECT DISTINCT s.id AS "a_id", t.via_edge AS "e_id", r.id AS "b_id", t.depth AS "_depth", t.total_weight AS "_total_weight" FROM traverse t JOIN primary_nodes s ON s.id = t.start_id JOIN primary_nodes r ON r.id = t.current_id WHERE r.deleted_at IS NULL AND t.depth >= ?3 ORDER BY "_depth", "_total_weight" DESC, "a_id", "e_id", "b_id" LIMIT ?5 OFFSET ?4"#;

const PRIMARY_NODES: &str = r#"SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS substrate_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS substrate_kind FROM notes UNION ALL SELECT id, namespace, kind, NULL AS entity_type, verb AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, payload AS properties, created_at, created_at AS updated_at, NULL AS deleted_at, 'event' AS substrate_kind FROM events UNION ALL SELECT id, namespace, relation AS kind, NULL AS entity_type, NULL AS name, NULL AS description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, metadata AS properties, created_at, updated_at, deleted_at, 'edge' AS substrate_kind FROM graph_edges"#;

const OBSERVATION_TARGETS: &str = r#"SELECT id, namespace, kind, entity_type, name, description, NULL AS content, NULL AS status, NULL AS salience, NULL AS decay_factor, properties, created_at, updated_at, deleted_at, 'entity' AS referent_kind FROM entities UNION ALL SELECT id, namespace, kind, NULL AS entity_type, name, NULL AS description, content, status, salience, decay_factor, properties, created_at, updated_at, deleted_at, 'note' AS referent_kind FROM notes"#;
