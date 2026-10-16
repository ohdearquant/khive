//! Query language detection and dispatch.

use crate::ast::GqlQuery;
use crate::error::QueryError;
use crate::parsers;
use crate::parsers::sparql::leading_keyword;

pub(crate) const GQL_WRITE_KEYWORDS: &[&str] = &[
    "CREATE", "DELETE", "DETACH", "SET", "REMOVE", "MERGE", "INSERT", "UPDATE",
];
pub(crate) const SPARQL_WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "DELETE", "WITH", "LOAD", "CLEAR", "CREATE", "DROP", "COPY", "MOVE", "ADD",
];
pub(crate) const DEFAULT_HOP_CAP: usize = 5;

pub(crate) fn read_only_error(constructor: fn(String) -> QueryError) -> QueryError {
    constructor(
        "the query verb is read-only; \
         to mutate the graph use: create, update, link, merge, delete"
            .into(),
    )
}

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
    parse_auto_with_language(input).map(|(_, query)| query)
}

/// Auto-detects and parses `input`, retaining the selected query language.
///
/// # Errors
///
/// Returns [`QueryError`] when syntax is invalid, write-shaped, or unsupported.
pub fn parse_auto_with_language(input: &str) -> Result<(QueryLanguage, GqlQuery), QueryError> {
    let trimmed = input.trim();
    reject_write(trimmed)?;
    let language = if trimmed
        .as_bytes()
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"SELECT"))
    {
        QueryLanguage::Sparql
    } else {
        QueryLanguage::Gql
    };
    parse(language, trimmed).map(|query| (language, query))
}

/// Rejects GQL/Cypher mutations and SPARQL Update before dialect dispatch.
fn reject_write(input: &str) -> Result<(), QueryError> {
    let keyword = leading_keyword(input);
    if GQL_WRITE_KEYWORDS.contains(&keyword.as_str())
        || SPARQL_WRITE_KEYWORDS.contains(&keyword.as_str())
    {
        Err(read_only_error(QueryError::Unsupported))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "language_tests.rs"]
mod tests;
