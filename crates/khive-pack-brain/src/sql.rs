//! SQL lives beside the code as `sql/<name>.sql`: one statement per file,
//! `?N` binds only, never string-built, loaded at compile time by `sql!`.
//!
//! `include_str!` resolves at compile time, so a renamed or missing file is a
//! build error rather than a runtime one, and `scripts/lint-sql.sh` prepares
//! every one of those files against the real schema, so a table or column that
//! does not exist fails before a test ever runs.

/// The statement text of `sql/<name>.sql`, checked in at compile time.
macro_rules! sql {
    ($name:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql"))
    };
}
pub(crate) use sql;
