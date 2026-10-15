use super::*;
use crate::error::QueryError;

#[test]
fn parse_auto_with_delete_rejected() {
    let err = parse_auto("WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "WITH … DELETE must return Unsupported on the public path; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
    assert!(
        msg.contains("create") && msg.contains("update") && msg.contains("delete"),
        "error must name the mutation verbs; got: {msg}"
    );
}

#[test]
fn parse_auto_prefixed_insert_data_rejected() {
    let err = parse_auto("PREFIX ex: <http://e/> INSERT DATA { ex:a ex:b ex:c }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "prefixed INSERT DATA must return Unsupported on the public path; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn parse_auto_prefixed_with_delete_rejected() {
    let err =
        parse_auto("PREFIX ex: <http://e/> WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }")
            .unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "PREFIX + WITH … DELETE must return Unsupported on the public path; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn parse_auto_detach_delete_rejected() {
    let err = parse_auto("DETACH DELETE (n)").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "DETACH DELETE must return Unsupported on the public path; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}

#[test]
fn parse_auto_gql_match_not_rejected() {
    let q = parse_auto("MATCH (a:concept) RETURN a").unwrap();
    assert!(!q.pattern.elements.is_empty(), "valid GQL MATCH must parse");
}

#[test]
fn parse_auto_sparql_select_not_rejected() {
    let q = parse_auto("SELECT ?a WHERE { ?a :extends ?b . }").unwrap();
    assert!(
        !q.pattern.elements.is_empty(),
        "valid SPARQL SELECT must parse"
    );
}

#[test]
fn parse_auto_with_language_reports_selected_parser() {
    let (gql_language, _) = parse_auto_with_language("MATCH (a:concept) RETURN a").unwrap();
    assert_eq!(gql_language, QueryLanguage::Gql);

    let (sparql_language, _) =
        parse_auto_with_language("SELECT ?a WHERE { ?a :extends ?b . }").unwrap();
    assert_eq!(sparql_language, QueryLanguage::Sparql);
}

#[test]
fn parse_auto_load_rejected() {
    let err = parse_auto("LOAD <http://e/data>").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "LOAD must return Unsupported on the public path; got {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("read-only"), "got: {msg}");
}
