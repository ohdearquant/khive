//! SQL lives beside the code as `sql/<name>.sql`: one statement per file,
//! `?N` binds only, never string-built, loaded at compile time by `sql!`.

/// The statement text of `sql/<name>.sql`, checked in at compile time.
macro_rules! sql {
    ($name:literal) => {
        const {
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql"))
                .trim_ascii_end()
        }
    };
}
pub(crate) use sql;
