//! Query language detection and dispatch.

use crate::ast::GqlQuery;
use crate::error::QueryError;
use crate::parsers;
use crate::parsers::sparql::leading_keyword;

/// Which query language the input is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryLanguage {
    Gql,
    Sparql,
}

/// Parses `input` as the selected language into a [`GqlQuery`].
///
/// # Errors
///
/// Returns [`QueryError`] when syntax is invalid, write-shaped, or unsupported.
/// See `crates/khive-query/docs/api/parsing.md` for accepted dialect subsets.
pub fn parse(language: QueryLanguage, input: &str) -> Result<GqlQuery, QueryError> {
    match language {
        QueryLanguage::Gql => parsers::gql::parse(input),
        QueryLanguage::Sparql => parsers::sparql::parse(input),
    }
}

/// Auto-detects SPARQL for `SELECT`, GQL for `MATCH`, and otherwise falls back to GQL.
///
/// Write-shaped input is rejected before dispatch, including SPARQL prologues.
///
/// # Errors
///
/// Returns [`QueryError`] when syntax is invalid, write-shaped, or unsupported.
/// See `crates/khive-query/docs/api/parsing.md` for detection and guard behavior.
pub fn parse_auto(input: &str) -> Result<GqlQuery, QueryError> {
    let trimmed = input.trim();
    reject_write(trimmed)?;
    if trimmed
        .as_bytes()
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"SELECT"))
    {
        parsers::sparql::parse(trimmed)
    } else if trimmed
        .as_bytes()
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"MATCH"))
    {
        parsers::gql::parse(trimmed)
    } else {
        // Preserve compatibility for inputs without a recognized leading keyword.
        parsers::gql::parse(trimmed)
    }
}

/// Rejects GQL/Cypher mutations and SPARQL Update before dialect dispatch.
fn reject_write(input: &str) -> Result<(), QueryError> {
    match leading_keyword(input).as_str() {
        "CREATE" | "DELETE" | "DETACH" | "SET" | "REMOVE" | "MERGE" | "INSERT" | "UPDATE"
        | "WITH" | "LOAD" | "CLEAR" | "DROP" | "COPY" | "MOVE" | "ADD" => {
            Err(QueryError::Unsupported(
                "the query verb is read-only; \
                 to mutate the graph use: create, update, link, merge, delete"
                    .into(),
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
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
        let err = parse_auto(
            "PREFIX ex: <http://e/> WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
        )
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
    fn parse_auto_load_rejected() {
        let err = parse_auto("LOAD <http://e/data>").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "LOAD must return Unsupported on the public path; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }
}
