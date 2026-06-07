//! Query language detection and dispatch.

use crate::ast::GqlQuery;
use crate::error::QueryError;
use crate::parsers;

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
pub fn parse_auto(input: &str) -> Result<GqlQuery, QueryError> {
    let trimmed = input.trim();
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
