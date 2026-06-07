//! Backend-agnostic GQL/SPARQL parsing and SQL compilation.

pub mod ast;
pub mod compilers;
pub mod error;
pub mod language;
pub mod parsers;
pub mod validate;

pub use ast::{GqlQuery, QueryValue, ReturnItem, WhereExpr};
pub use compilers::sql::{compile, CompileOptions, CompiledQuery};
pub use error::QueryError;
pub use language::{parse, parse_auto, QueryLanguage};
pub use validate::{validate, validate_pattern_shape, validate_with_warnings, MAX_DEPTH};
