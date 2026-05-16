//! `khive-query` — backend-agnostic GQL/SPARQL parsing and SQL compilation.
//!
//! # Two entry points
//!
//! ## Explicit language
//! ```ignore
//! use khive_query::{QueryLanguage, parse, compile, CompileOptions};
//!
//! let ast = parse(QueryLanguage::Gql, "MATCH (a:concept)-[:extends]->(b) RETURN b LIMIT 10")?;
//! let compiled = compile(&ast, &CompileOptions::default())?;
//! ```
//!
//! ## Auto-detect (SELECT → SPARQL, MATCH → GQL)
//! ```ignore
//! use khive_query::parse_auto;
//!
//! let ast = parse_auto("SELECT ?a ?b WHERE { ?a :extends ?b . }")?;
//! ```

pub mod ast;
pub mod compilers;
pub mod error;
pub mod parsers;
pub mod validate;

pub use ast::{GqlQuery, ReturnItem};
pub use compilers::sql::{compile, CompileOptions, CompiledQuery};
pub use error::QueryError;
pub use validate::{validate, MAX_DEPTH};

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
pub fn parse_auto(input: &str) -> Result<GqlQuery, QueryError> {
    let trimmed = input.trim();
    if trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("SELECT") {
        parsers::sparql::parse(trimmed)
    } else {
        parsers::gql::parse(trimmed)
    }
}
