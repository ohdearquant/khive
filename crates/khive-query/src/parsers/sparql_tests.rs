use super::*;

include!("sparql_tests/patterns_tests.rs");

include!("sparql_tests/repeated_constraints_tests.rs");

#[test]
fn disconnected_triples_rejected() {
    let err = parse("SELECT ?a ?d WHERE { ?a :extends ?b . ?c :implements ?d . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn branched_triples_rejected() {
    let err = parse("SELECT ?a ?b ?c WHERE { ?a :extends ?b . ?a :implements ?c . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn disconnected_kind_constraint_rejected() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c a :concept . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported for disconnected kind constraint, got {err:?}"
    );
}

#[test]
fn disconnected_property_constraint_rejected() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c :name 'LoRA' . }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported for disconnected property constraint, got {err:?}"
    );
}

#[test]
fn rejects_trailing_input_after_limit() {
    let err =
        parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10 GARBAGE").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "expected trailing-input parse error, got {err}"
    );
}

#[test]
fn sparql_offset_remains_outside_the_dialect() {
    let err = parse("SELECT ?a WHERE { ?a :extends ?b . } LIMIT 10 OFFSET 10").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "SPARQL OFFSET must not leak in through the shared AST, got {err}"
    );
}

#[test]
fn rejects_trailing_input_without_limit() {
    let err =
        parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } and then some").unwrap_err();
    assert!(
        err.to_string().contains("unexpected trailing input"),
        "expected trailing-input parse error, got {err}"
    );
}

#[test]
fn sparql_insert_data_rejected_with_readonly_message() {
    let err = parse("INSERT DATA { <a> :extends <b> }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "INSERT DATA must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("read-only"),
        "error must mention 'read-only'; got: {msg}"
    );
    assert!(
        msg.contains("create") && msg.contains("update") && msg.contains("delete"),
        "error must name the mutation verbs; got: {msg}"
    );
}

#[test]
fn sparql_delete_data_rejected_with_readonly_message() {
    let err = parse("DELETE DATA { <a> :extends <b> }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "DELETE DATA must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_delete_where_rejected_with_readonly_message() {
    let err = parse("DELETE WHERE { ?s :extends ?o }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "DELETE WHERE must return Unsupported; got {err:?}"
    );
}

#[test]
fn sparql_load_rejected_with_readonly_message() {
    let err = parse("LOAD <http://example.org/graph>").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "LOAD must return Unsupported; got {err:?}"
    );
}

#[test]
fn sparql_select_still_compiles_after_write_guard() {
    let q = parse("SELECT ?a WHERE { ?a :extends ?b . }").unwrap();
    assert!(!q.pattern.elements.is_empty(), "valid SELECT must parse");
}

#[test]
fn sparql_with_delete_where_rejected() {
    let err = parse("WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "WITH … DELETE … must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("read-only"),
        "error must mention 'read-only'; got: {msg}"
    );
    assert!(
        msg.contains("create") && msg.contains("update") && msg.contains("delete"),
        "error must name the mutation verbs; got: {msg}"
    );
}

#[test]
fn sparql_prefixed_insert_data_rejected() {
    let err = parse("PREFIX ex: <http://example.org/> INSERT DATA { ex:a ex:b ex:c }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "prefixed INSERT DATA must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_clear_graph_rejected() {
    let err = parse("CLEAR GRAPH <http://example.org/graph>").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "CLEAR GRAPH must return Unsupported; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn sparql_prefixed_select_write_guard_passes_through() {
    let err = parse("PREFIX ex: <http://example.org/> SELECT ?s WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        !matches!(err, QueryError::Unsupported(_)),
        "prefixed SELECT must not be rejected by the write guard; got {err:?}"
    );
}
