//! The `sql!` macro is defined once in khive-runtime and invoked from this crate. Every
//! invoking crate relies on two properties, and this test pins both from the one crate
//! that is not khive-runtime: the statement comes from the invoking crate's own `sql/`
//! directory, and the trailing newline the file carries on disk is trimmed.

#[test]
fn shared_sql_macro_reads_own_statement_file_and_trims_its_newline() {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("sql")
        .join("projects_by_slug_select.sql");
    let on_disk = std::fs::read_to_string(&file).expect("read this crate's statement file");
    assert!(
        on_disk.ends_with('\n'),
        "{file:?} must end with a newline, or the trim checks below prove nothing"
    );

    let from_macro: &'static str = khive_runtime::sql!("projects_by_slug_select");

    assert!(!from_macro.is_empty(), "statement is empty");
    assert!(
        !from_macro.ends_with(|c: char| c.is_ascii_whitespace()),
        "the macro must drop the trailing whitespace of the file: {from_macro:?}"
    );
    assert_eq!(
        from_macro,
        on_disk.trim_ascii_end(),
        "the macro must return this crate's file text, trailing whitespace removed"
    );
}
