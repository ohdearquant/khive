use super::*;

#[test]
fn duplicate_paths_preserve_each_entry_and_last_module_binding() {
    let sources = vec![
        ("sample/src/lib.rs".into(), "mod alias; mod writer;".into()),
        (
            "sample/src/alias.rs".into(),
            r#"
                pub use khive_db::stores::note::NOTE_INSERT_IF_ABSENT_SQL as SQL;
                fn first(store: &dyn NoteStore, note: Note) {
                    store.upsert_note(note);
                }
                fn shared(note: Note) {
                    note_upsert_statement(&note);
                }
            "#
            .into(),
        ),
        (
            "sample/src/alias.rs".into(),
            r#"
                pub use khive_db::stores::note::NOTE_UPSERT_SQL as SQL;
                fn second(store: &dyn NoteStore, note: Note) {
                    store.upsert_note(note);
                }
                fn shared(store: &dyn NoteStore, a: Note, b: Note) {
                    store.upsert_note(a);
                    store.upsert_note(b);
                }
            "#
            .into(),
        ),
        (
            "sample/src/writer.rs".into(),
            r#"
                fn write(conn: &Connection) {
                    conn.prepare_cached(crate::alias::SQL);
                }
            "#
            .into(),
        ),
    ];
    let population = scan_source_population(&sources).expect("parse every original entry");
    assert_eq!(
        population
            .properties
            .iter()
            .map(|site| site.key.as_str())
            .collect::<Vec<_>>(),
        [
            "sample/src/alias.rs::first",
            "sample/src/alias.rs::second",
            "sample/src/alias.rs::shared",
            "sample/src/writer.rs::write",
        ],
        "scanning a last-wins path map would lose the first entry's function"
    );
    for name in ["first", "second"] {
        let site = population
            .properties
            .iter()
            .find(|site| site.key == format!("sample/src/alias.rs::{name}"))
            .expect("both original function bodies are scanned");
        assert_eq!(site.write_count, 1);
        assert_eq!(site.target, Substrate::Note);
    }

    let shared = &population.properties[2];
    assert_eq!(shared.write_count, 2, "the last scan replaces the same key");
    assert_eq!(shared.class, DetectedClass::WholeObject);
    assert_eq!(
        shared.evidence,
        BTreeSet::from(["store.upsert_note".into()]),
        "the first entry's builder evidence must not be unioned into the last scan"
    );

    let writer = &population.properties[3];
    assert_eq!(writer.write_count, 1);
    assert_eq!(
        writer.evidence,
        BTreeSet::from(["SQL constant NOTE_UPSERT_SQL".into()]),
        "module indexing still resolves the last entry's exported constant"
    );
    assert!(population.runtime_tables.is_empty());
}

#[test]
fn skipped_source_syntax_errors_preserve_discovery_order_and_diagnostics() {
    let malformed = ["fn broken(", "fn broken() { let value = ; }"];
    let diagnostics = malformed.map(|source| {
        syn::parse_file(source)
            .err()
            .expect("each malformed source is a real parse failure")
            .to_string()
    });
    assert_ne!(diagnostics[0], diagnostics[1], "order must be observable");

    for (first_path, second_path, root) in [
        (
            "sample/src/first.rs",
            "sample/src/second.rs",
            r#"
                #[cfg(test)] #[path = "first.rs"] mod first;
                #[cfg(test)] #[path = "second.rs"] mod second;
            "#,
        ),
        ("sample/tests/first.rs", "sample/tests/second.rs", ""),
        ("sample/benches/first.rs", "sample/benches/second.rs", ""),
    ] {
        for order in [[0, 1], [1, 0]] {
            let sources = vec![
                ("sample/src/lib.rs".into(), root.into()),
                (first_path.into(), malformed[order[0]].into()),
                (second_path.into(), malformed[order[1]].into()),
            ];
            let failure = scan_source_population(&sources)
                .expect_err("discovery must parse sources that production scanning skips");
            assert_eq!(
                failure, diagnostics[order[0]],
                "preserve the earliest, unprefixed discovery diagnostic for {first_path}"
            );
        }
    }
}
