#[test]
fn variable_length_where_valid_node_property_compiles() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.name LIKE "%SpecInfer%" RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("s.name LIKE ?"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_where_unknown_depth_field_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.name LIKE "%SpecInfer%" AND b._depth > 1 RETURN b.name"#,
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg)
            if msg.contains("unknown node field '_depth'")
                && msg.contains("properties.<path>")),
        "got {err:?}"
    );
}

#[test]
fn variable_length_where_explicit_property_named_depth_uses_json_properties() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.name LIKE "%SpecInfer%" AND b.properties._depth > 1 RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("json_extract(r.properties, '$._depth') > ?"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_where_id_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.id = "concept-1" RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("s.id = ?"),
        "start-node id must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.id'"),
        "id must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_where_description_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE b.description = "abstract" RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("r.description = ?"),
        "end-node description must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.description'"),
        "description must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_where_created_at_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.created_at > "2026-01-01" RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("s.created_at > ?"),
        "start-node created_at must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.created_at'"),
        "created_at must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_where_dedicated_field_and_arbitrary_property_together() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (a)-[:precedes*1..3]->(b) WHERE a.id = "concept-1" AND b.properties.custom_tag = "urgent" RETURN b.name"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("s.id = ?"),
        "dedicated field id must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.id'"),
        "id must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
    assert!(
        compiled
            .sql
            .contains("json_extract(r.properties, '$.custom_tag')"),
        "explicit property custom_tag must compile through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn entity_type_compiles_as_direct_column_not_json_extract() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {entity_type: 'paper'})-[:extends]->(m) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".entity_type = ?"),
        "entity_type must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("json_extract"),
        "entity_type must NOT use json_extract; sql: {}",
        compiled.sql
    );
    let has_paper_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
    assert!(
        has_paper_param,
        "entity_type value 'paper' must appear as a bound parameter"
    );
}

#[test]
fn fixed_length_where_id_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) WHERE a.id = 'concept-1' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".id = ?"),
        "id must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.id'"),
        "id must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn fixed_length_where_description_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) WHERE a.description = 'abstract' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".description = ?"),
        "description must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.description'"),
        "description must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn fixed_length_where_created_at_resolves_to_direct_column() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) WHERE a.created_at > '2026-01-01' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".created_at > ?"),
        "created_at must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.created_at'"),
        "created_at must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
}

#[test]
fn fixed_length_where_dedicated_field_and_arbitrary_property_together() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) WHERE a.id = 'concept-1' AND a.properties.custom_tag = 'urgent' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".id = ?"),
        "dedicated field id must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("'$.id'"),
        "id must NOT be resolved through json_extract; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("json_extract(") && compiled.sql.contains("'$.custom_tag'"),
        "explicit property custom_tag must compile through json_extract; sql: {}",
        compiled.sql
    );
}
