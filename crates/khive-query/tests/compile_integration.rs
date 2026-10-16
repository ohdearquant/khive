//! Integration tests for the SQL compiler through the public API.
//!
//! Tests cover fixed-length, variable-length, synthetic edge, and WHERE clause
//! compilation paths. Formerly inline in `compilers/sql.rs`; moved here per
//! QUERY-AUD-002.

use khive_query::ast::{QueryValue, ReturnItem};
use khive_query::{
    compile, parse, parse_auto, CompileOptions, CompiledQuery, QueryError, QueryLanguage,
};

fn opts() -> CompileOptions {
    CompileOptions::default()
}

fn scoped(namespace: &str) -> CompileOptions {
    CompileOptions {
        scopes: vec![namespace.to_string()],
        max_limit: 500,
    }
}

include!("compile_integration/fixed_properties_tests.rs");

include!("compile_integration/recursive_properties_tests.rs");

include!("compile_integration/recursive_and_sparql_tests.rs");

include!("compile_integration/synthetic_and_endpoint_tests.rs");

include!("compile_integration/inline_literals_tests.rs");

include!("compile_integration/integer_precision_tests.rs");

include!("compile_integration/numeric_grammar_and_operators_tests.rs");

include!("compile_integration/substrate_labels_tests.rs");

include!("compile_integration/sparql_integer_precision_tests.rs");
