//! `khive-query` — backend-agnostic GQL/SPARQL parsing and SQL compilation.
//!
//! Use `parse_auto` to detect syntax (SELECT → SPARQL, MATCH → GQL), or call
//! `parse(QueryLanguage::Gql/Sparql, …)` explicitly, then `compile(&ast, &opts)`.

pub mod ast;
pub mod compilers;
pub mod error;
pub mod parsers;
pub mod validate;

pub use ast::{GqlQuery, QueryValue, ReturnItem, WhereExpr};
pub use compilers::sql::{compile, CompileOptions, CompiledQuery};
pub use error::QueryError;
pub use validate::{validate, validate_pattern_shape, validate_with_warnings, MAX_DEPTH};

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

/// Auto-detect language and parse.
///
/// - Starts with `SELECT` → SPARQL
/// - Starts with `MATCH` → GQL
///
/// Uses byte-prefix checking to avoid panicking on non-ASCII input at byte
/// boundary 6 (fix for UTF-8 slice panic on non-ASCII first character).
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
