//! SQL lives beside the code as `sql/<name>.sql`: one statement per file,
//! `?N` binds only, never string-built, loaded at compile time by `sql!`.
//!
//! `include_str!` resolves at compile time, so a renamed or missing file is a
//! build error rather than a runtime one, and `scripts/lint-sql.sh` prepares
//! every one of those files against the real schema, so a table or column that
//! does not exist fails before a test ever runs.

/// The statement text of `sql/<name>.sql`, checked in at compile time.
///
/// The trailing newline every text file carries is trimmed, in a `const` block so
/// it costs nothing at run time and the result stays a `&'static str`. A statement
/// that used to be a Rust literal ended at its last word, and callers and tests
/// anchored on that: leaving the newline on would change the string a reader of
/// this file has no reason to think changed.
macro_rules! sql {
    ($name:literal) => {
        const {
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql"))
                .trim_ascii_end()
        }
    };
}
pub(crate) use sql;
