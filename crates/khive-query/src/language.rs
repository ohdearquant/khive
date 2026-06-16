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

/// Parse a query string in the given language into a [`GqlQuery`] AST.
pub fn parse(language: QueryLanguage, input: &str) -> Result<GqlQuery, QueryError> {
    match language {
        QueryLanguage::Gql => parsers::gql::parse(input),
        QueryLanguage::Sparql => parsers::sparql::parse(input),
    }
}

/// Auto-detect language and parse (`SELECT` → SPARQL, `MATCH` → GQL, fallback → GQL).
///
/// Write-shaped input is rejected before dialect dispatch with a clear,
/// actionable error naming the mutation verbs to use instead.  This guard
/// runs first so that forms such as `WITH <g> DELETE …` and
/// `PREFIX ex: <…> INSERT DATA { … }` — which don't start with `SELECT` and
/// therefore fall through to the GQL path — are caught on the public entry
/// point rather than producing a generic parse error.
///
/// The per-parser guards in `parsers::gql` and `parsers::sparql` remain in
/// place for defense-in-depth for direct callers of those functions.
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
        // Fall back to GQL to preserve existing behavior for unknown prefixes.
        parsers::gql::parse(trimmed)
    }
}

/// Unified write-shape guard: rejects GQL/Cypher mutations and SPARQL Update
/// ops before dialect dispatch.  Uses `leading_keyword` so that SPARQL
/// prologues (PREFIX / BASE) and line comments are skipped before the keyword
/// is inspected.
fn reject_write(input: &str) -> Result<(), QueryError> {
    match leading_keyword(input).as_str() {
        // GQL / Cypher write forms
        "CREATE" | "DELETE" | "DETACH" | "SET" | "REMOVE" | "MERGE" | "INSERT" | "UPDATE"
        // SPARQL Update forms
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

    // --- Read-only public-path regression tests (#16) ---

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
        // Proves both prologue-skip (PREFIX) AND WITH keyword on the public path.
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
        // LOAD is in reject_write's union set but NOT in reject_gql_write, so
        // if reject_write is reverted this routes to GQL and returns a Parse
        // error, not Unsupported. This test is therefore sensitive to the
        // parse_auto guard specifically.
        let err = parse_auto("LOAD <http://e/data>").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "LOAD must return Unsupported on the public path; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }
}
