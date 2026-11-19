#[test]
fn assert_select_only_accepts_select_and_with() {
    assert!(
        assert_select_only("SELECT a FROM entities WHERE 1=1").is_ok(),
        "SELECT must be accepted"
    );
    assert!(
        assert_select_only("WITH RECURSIVE traverse AS (...) SELECT ...").is_ok(),
        "WITH must be accepted (recursive CTE)"
    );
}

#[test]
fn assert_select_only_rejects_write_sql_with_readonly_message() {
    for stmt in &[
        "INSERT INTO entities VALUES (?)",
        "UPDATE entities SET name = ?",
        "DELETE FROM entities WHERE id = ?",
        "DROP TABLE entities",
    ] {
        let err = assert_select_only(stmt).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(_)),
            "write SQL must return Compile error for '{stmt}'; got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("read-only"),
            "error must mention 'read-only' for '{stmt}'; got: {msg}"
        );
        assert!(
            msg.contains("create") && msg.contains("delete"),
            "error must name the mutation verbs for '{stmt}'; got: {msg}"
        );
    }
}

#[test]
fn readonly_guard_does_not_break_valid_gql_compile() {
    let q = gql::parse("MATCH (a:concept)-[:extends]->(b) RETURN a LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.starts_with("SELECT"),
        "valid GQL must compile to SELECT; sql: {}",
        compiled.sql
    );
}

#[test]
fn readonly_guard_does_not_break_valid_cte_compile() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.starts_with("WITH RECURSIVE"),
        "variable-length GQL must compile to WITH RECURSIVE; sql: {}",
        compiled.sql
    );
}

#[test]
fn gql_write_form_rejected_before_compile() {
    use crate::parsers::gql;
    let err = gql::parse("CREATE (n:concept) RETURN n").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "GQL CREATE must be Unsupported; got {err:?}"
    );
    assert!(
        err.to_string().contains("read-only"),
        "error must mention read-only; got: {err}"
    );
}

#[test]
fn sparql_write_form_rejected_before_compile() {
    use crate::parsers::sparql;
    let err = sparql::parse("INSERT DATA { ?a :extends ?b }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "SPARQL INSERT must be Unsupported; got {err:?}"
    );
    assert!(
        err.to_string().contains("read-only"),
        "error must mention read-only; got: {err}"
    );
}

#[test]
fn duplicate_inline_property_rejected() {
    let result = gql::parse("MATCH (n {name: 'A', name: 'B'}) RETURN n");
    assert!(
        result.is_err(),
        "duplicate property 'name' in node props must be rejected"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("duplicate") || err.contains("name"),
        "error should mention duplicate or key name: {err}"
    );
}

#[test]
fn unknown_synthetic_relation_rejected_at_compile() {
    let q = gql::parse("MATCH (a)-[:observed_as_bogus]->(b) RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "unknown synthetic relation must return Validation error; got {err:?}"
    );
}

/// Canonical `annotates` can bind every legal target substrate (issue #467).
#[test]
fn canonical_edge_annotates_compiles_note_source_and_any_target_substrate() {
    let q = gql::parse("MATCH (n)-[:annotates]->(x) RETURN n.id, x.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled.sql.contains("FROM entities n0"),
        "endpoint must not be hard-bound to entities only; sql: {}",
        compiled.sql
    );
    for table in ["FROM notes", "FROM events", "FROM graph_edges"] {
        assert!(
            compiled.sql.contains(table),
            "endpoint source must admit {table} rows for annotates; sql: {}",
            compiled.sql
        );
    }
}

/// Canonical `supports` can bind note-to-note endpoints.
#[test]
fn canonical_edge_supports_compiles_note_note_endpoints() {
    let q = gql::parse("MATCH (a)-[:supports]->(b) RETURN a.id, b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.sql.matches("FROM notes").count(),
        2,
        "both supports endpoints must admit note rows; sql: {}",
        compiled.sql
    );
}

/// Pack-declared relations can bind task-note endpoints.
#[test]
fn canonical_edge_depends_on_compiles_pack_task_note_endpoints() {
    let q =
        gql::parse("MATCH (a:task)-[:depends_on]->(b:task) RETURN a.id, b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.sql.matches("FROM notes").count(),
        2,
        "task-kind depends_on endpoints must admit note rows; sql: {}",
        compiled.sql
    );
    let task_params = compiled
        .params
        .iter()
        .filter(|p| matches!(p, QueryValue::Text(s) if s == "task"))
        .count();
    assert_eq!(
        task_params, 2,
        "kind='task' must be bound for both endpoints"
    );
}

/// Recursive canonical traversal uses the primary-substrate union throughout.
#[test]
fn variable_length_canonical_compiles_primary_substrate_nodes() {
    let q = gql::parse("MATCH (a)-[:supports*1..2]->(b) RETURN b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled.sql.contains("FROM entities s"),
        "seed must not be hard-bound to entities only; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("JOIN primary_nodes next_node"),
        "intermediate must join primary_nodes; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("JOIN primary_nodes r"),
        "final endpoint must join primary_nodes; sql: {}",
        compiled.sql
    );
}

/// `observed_as_target` admits entity and note referents (issue #468).
#[test]
fn synthetic_edge_observed_as_target_compiles_entity_referents() {
    let q = gql::parse("MATCH (ev)-[:observed_as_target]->(t) RETURN t.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled
            .sql
            .contains("JOIN notes n1 ON n1.id = e0.entity_id AND e0.referent_kind = 'note'"),
        "target join must not be hard-bound to notes; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("FROM entities"),
        "observation target source must admit entity referents; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("n1.referent_kind = e0.referent_kind"),
        "target join must discriminate by referent_kind instead of hard-coding 'note'; sql: {}",
        compiled.sql
    );
}

/// `observed_as_signal` admits entity and note referents.
#[test]
fn synthetic_edge_observed_as_signal_compiles_entity_and_note_referents() {
    let q = gql::parse("MATCH (ev)-[:observed_as_signal]->(t) RETURN t.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("e0.role = 'signal' AND e0.referent_kind IN ('entity', 'note')"),
        "signal role must be admitted against entity or note referents; sql: {}",
        compiled.sql
    );
}

/// Search observations may select entities; recall and rerank still emit note referents.
#[test]
fn synthetic_edge_candidate_and_selected_admit_entity_and_note_referents() {
    let q =
        gql::parse("MATCH (ev)-[:observed_as_selected]->(m:entity) RETURN m.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(
            "e0.role IN ('candidate', 'selected') AND e0.referent_kind IN ('entity', 'note')"
        ),
        "selected/candidate roles must follow the stored entity/note referent kind; sql: {}",
        compiled.sql
    );
}

#[test]
fn where_is_null_compiles_to_is_null_sql() {
    let q = gql::parse("MATCH (n:entity) WHERE n.name IS NULL RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("IS NULL") && !compiled.sql.contains("IS NOT NULL"),
        "expected IS NULL (not IS NOT NULL) in sql: {}",
        compiled.sql
    );
}

#[test]
fn where_is_not_null_compiles_to_is_not_null_sql() {
    let q = gql::parse("MATCH (n:entity) WHERE n.name IS NOT NULL RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("IS NOT NULL"),
        "expected IS NOT NULL in sql: {}",
        compiled.sql
    );
}
