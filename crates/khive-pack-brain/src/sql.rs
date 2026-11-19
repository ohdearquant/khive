//! SQL lives beside the code as `sql/<name>.sql`: one statement per file,
//! `?N` binds only, never string-built, loaded at compile time by `sql!`.

pub(crate) use khive_runtime::sql;

pub(crate) fn sql_err(context: &str, error: impl std::fmt::Display) -> khive_runtime::RuntimeError {
    khive_runtime::RuntimeError::internal_with_context(context, error)
}
