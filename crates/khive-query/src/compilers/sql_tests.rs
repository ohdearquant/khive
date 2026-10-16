use super::*;
use crate::parsers::gql;

fn opts() -> CompileOptions {
    CompileOptions::default()
}

fn scoped(namespace: &str) -> CompileOptions {
    CompileOptions {
        scopes: vec![namespace.to_string()],
        max_limit: 500,
    }
}

include!("sql_tests/patterns_and_limits_tests.rs");

include!("sql_tests/properties_and_patterns_tests.rs");

include!("sql_tests/projection_and_synthetic_tests.rs");

include!("sql_tests/traversal_and_guardrails_tests.rs");

include!("sql_tests/readonly_and_substrates_tests.rs");
