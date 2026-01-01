//! SQL lives beside the code as `sql/<name>.sql`: one statement per file,
//! `?N` binds only, never string-built, loaded at compile time by `sql!`.

pub(crate) use khive_runtime::sql;
