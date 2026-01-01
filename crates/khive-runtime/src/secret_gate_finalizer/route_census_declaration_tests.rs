use super::*;

#[test]
fn both_properties_writes_pass_when_both_are_declared() {
    let source = r#"
        fn write(store: &dyn NoteStore, a: Note, b: Note) {
            reject_reserved_secret_gate_property(&a.properties);
            store.upsert_note(a);
            store.upsert_note(b);
        }
    "#;
    let population = scan_source_population(&[("sample/src/lib.rs".into(), source.into())])
        .expect("parse both properties writes");
    let row = RouteInventoryEntry {
        id: "fixture.checked-write",
        site: "sample/src/lib.rs::write",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        reservation: Reservation::NamedCheck {
            function: "reject_reserved_secret_gate_property",
            file: "khive-runtime/src/secret_gate.rs",
        },
        expected_writes: 2,
        ..ROUTE_INVENTORY[0]
    };
    let result = check_population(&population, &[row], &[], 1);
    assert!(
        result.is_ok(),
        "both declared writes must pass the existing function-level check rule: {result:?}"
    );
}

#[test]
fn declared_non_properties_placeholder_table_writes_pass() {
    for sql in [
        "INSERT INTO {table} (value) VALUES (?1)",
        "INSERT OR IGNORE INTO {table} (value) VALUES (?1)",
        "REPLACE INTO {table} (value) VALUES (?1)",
        "UPDATE {table} SET value = ?1 WHERE id = ?2",
    ] {
        let source = format!("fn write() {{ let statement = format!({sql:?}); }}");
        let population = scan_source_population(&[("sample/src/lib.rs".into(), source)])
            .expect("parse the runtime-table write");
        let row = RuntimeTableWriteInventoryEntry {
            site: "sample/src/lib.rs::write",
            expected_writes: 1,
            properties_route: None,
        };
        let result = check_population(&population, &[], &[row], 0);
        assert!(
            result.is_ok(),
            "the explicit non-properties declaration must cover {sql}: {result:?}"
        );
    }
}

fn mapped_placeholder_rows() -> (RouteInventoryEntry, RuntimeTableWriteInventoryEntry) {
    (
        RouteInventoryEntry {
            id: "fixture.checked-write",
            site: "sample/src/lib.rs::write",
            target: Substrate::Note,
            write_class: WriteClass::WholeObject,
            reservation: Reservation::NamedCheck {
                function: "reject_reserved_secret_gate_property",
                file: "khive-runtime/src/secret_gate.rs",
            },
            expected_writes: 1,
            ..ROUTE_INVENTORY[0]
        },
        RuntimeTableWriteInventoryEntry {
            site: "sample/src/lib.rs::write",
            expected_writes: 1,
            properties_route: Some("fixture.checked-write"),
        },
    )
}

#[test]
fn declared_placeholder_properties_route_passes_with_its_named_check() {
    let source = r#"
        fn write() {
            reject_reserved_secret_gate_property(properties);
            let statement = format!("UPDATE {table} SET properties = ?1 WHERE id = ?2");
        }
    "#;
    let population = scan_source_population(&[("sample/src/lib.rs".into(), source.into())])
        .expect("parse the checked mapped runtime-table write");
    let (properties_row, runtime_row) = mapped_placeholder_rows();
    let result = check_population(&population, &[properties_row], &[runtime_row], 1);
    assert!(
        result.is_ok(),
        "the mapped properties route must retain its count and named check: {result:?}"
    );
}

#[test]
fn declared_placeholder_properties_route_requires_its_named_check() {
    let source = r#"
        fn write() {
            let statement = format!("UPDATE {table} SET properties = ?1 WHERE id = ?2");
        }
    "#;
    let population = scan_source_population(&[("sample/src/lib.rs".into(), source.into())])
        .expect("parse the unchecked mapped runtime-table write");
    let (properties_row, runtime_row) = mapped_placeholder_rows();
    let failure = check_population(&population, &[properties_row], &[runtime_row], 1)
        .expect_err("a runtime-table declaration must not replace the named reservation check");
    assert!(failure.contains("named check"), "{failure}");
}
