use super::*;

fn one_write_route() -> RouteInventoryEntry {
    RouteInventoryEntry {
        id: "fixture.checked-write",
        site: "sample/src/lib.rs::write",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        reservation: Reservation::NamedCheck {
            function: "reject_reserved_secret_gate_property",
            file: "khive-runtime/src/secret_gate.rs",
        },
        ..ROUTE_INVENTORY[0]
    }
}

#[test]
fn second_properties_write_requires_its_own_declared_count() {
    let checked_a = r#"
        fn write(store: &dyn NoteStore, a: Note) {
            reject_reserved_secret_gate_property(&a.properties);
            store.upsert_note(a);
        }
    "#;
    let first = scan_sources(&[("sample/src/lib.rs".into(), checked_a.into())])
        .expect("parse the checked first write");
    assert!(
        check_inventory(&first, &[one_write_route()], 1).is_ok(),
        "the declared checked first write must pass"
    );

    for expression in [
        r#"format!("UPDATE notes SET properties = ?1 WHERE id = ?2")"#,
        r#"concat!("UPDATE notes SET properties = ?1 WHERE id = ?2")"#,
        r#"vec!["UPDATE notes SET properties = ?1 WHERE id = ?2"]"#,
    ] {
        let source = format!(
            "fn write() {{ reject_reserved_secret_gate_property(properties); let statement = {expression}; }}"
        );
        let sites = scan_sources(&[("sample/src/lib.rs".into(), source)])
            .expect("parse the single checked SQL macro write");
        let result = check_inventory(&sites, &[one_write_route()], 1);
        assert!(
            result.is_ok(),
            "one macro write must be counted once: {result:?}"
        );
    }

    let unchecked_b = r#"
        fn write(store: &dyn NoteStore, a: Note, b: Note) {
            reject_reserved_secret_gate_property(&a.properties);
            store.upsert_note(a);
            store.upsert_note(b);
        }
    "#;
    let both = scan_sources(&[("sample/src/lib.rs".into(), unchecked_b.into())])
        .expect("parse both writes");
    let result = check_inventory(&both, &[one_write_route()], 1);
    let failure = result.expect_err("one declared write must not cover checked A and unchecked B");
    assert!(failure.contains("write count"), "{failure}");
}

fn placeholder_sources() -> Vec<(&'static str, String)> {
    let mut sources = [
        ("insert", "INSERT INTO {table} (value) VALUES (?1)"),
        (
            "insert-or-ignore",
            "INSERT OR IGNORE INTO {table} (value) VALUES (?1)",
        ),
        (
            "insert-or-replace",
            "INSERT OR REPLACE INTO {table} (value) VALUES (?1)",
        ),
        (
            "insert-or-abort",
            "INSERT OR ABORT INTO {table} (value) VALUES (?1)",
        ),
        (
            "insert-or-fail",
            "INSERT OR FAIL INTO {table} (value) VALUES (?1)",
        ),
        (
            "insert-or-rollback",
            "INSERT OR ROLLBACK INTO {table} (value) VALUES (?1)",
        ),
        ("replace", "REPLACE INTO {table} (value) VALUES (?1)"),
        ("update", "UPDATE {table} SET value = ?1 WHERE id = ?2"),
    ]
    .into_iter()
    .map(|(case, sql)| {
        (
            case,
            format!("fn write() {{ let statement = format!({sql:?}); }}"),
        )
    })
    .collect::<Vec<_>>();
    sources.extend([
        (
            "continued-insert",
            r#"fn write() {
            let statement = format!("INSERT \
                INTO {table} (value) VALUES (?1)");
        }"#
            .into(),
        ),
        (
            "continued-insert-or-ignore",
            r#"fn write() {
            let statement = format!("INSERT OR \
                IGNORE INTO {table} (value) VALUES (?1)");
        }"#
            .into(),
        ),
        (
            "continued-replace",
            r#"fn write() {
            let statement = format!("REPLACE \
                INTO {table} (value) VALUES (?1)");
        }"#
            .into(),
        ),
        (
            "continued-update",
            r#"fn write() {
            let statement = format!("UPDATE \
                {table} SET value = ?1 WHERE id = ?2");
        }"#
            .into(),
        ),
    ]);
    sources
}

#[test]
fn undeclared_placeholder_table_writes_are_refused() {
    let mut missed = Vec::new();
    for (case, source) in placeholder_sources() {
        match scan_sources(&[("sample/src/lib.rs".into(), source)]) {
            Err(failure) if failure.contains("unmapped runtime-table write") => {}
            result => missed.push(format!("{case}: {result:?}")),
        }
    }
    assert!(
        missed.is_empty(),
        "every placeholder-table write needs a declaration: {missed:?}"
    );
}
