use super::*;

#[test]
fn synthetic_census_controls() {
    let vector_sites = scan_sources(&[(
        "sample/src/lib.rs".into(),
        "fn entity() { let _ = vec![PlanStatement { statement: entity_upsert_statement(&value) }]; } fn note() { let _ = vec![note_upsert_statement(&value); 2]; }".into(),
    )])
    .unwrap();
    assert_eq!(vector_sites.len(), 2);
    assert!(vector_sites.iter().any(|site| {
        site.key == "sample/src/lib.rs::entity"
            && site.evidence.contains("builder.entity_upsert_statement")
    }));
    assert!(vector_sites.iter().any(|site| {
        site.key == "sample/src/lib.rs::note"
            && site.evidence.contains("builder.note_upsert_statement")
    }));
    assert!(check_inventory(&vector_sites, &[], 0)
        .unwrap_err()
        .contains("unmapped"));

    let whole = scan_sources(&[(
        "sample/src/lib.rs".into(),
        "fn write(store: &dyn NoteStore, note: Note) { store.upsert_note(note); }".into(),
    )])
    .unwrap();
    assert!(check_inventory(&whole, &[], 0)
        .unwrap_err()
        .contains("unmapped"));
    assert!(check_inventory(&[], &ROUTE_INVENTORY[..1], 1)
        .unwrap_err()
        .contains("orphan"));

    for path in [
        "$.\"khive:secret_gate\"",
        "$[\"khive:secret_gate\"]",
        "$.khive:secret_gate",
        "$",
    ] {
        let source = format!("fn write(store: &dyn NoteStore) {{ store.try_patch_note_property(id, ns, filter, {path:?}, value, now); }}");
        let sites = scan_sources(&[("sample/src/lib.rs".into(), source)]).unwrap();
        assert_eq!(sites[0].class, DetectedClass::WholeObject, "{path}");
        let row = RouteInventoryEntry {
            id: "synthetic.single-key",
            site: "sample/src/lib.rs::write",
            target: Substrate::Note,
            write_class: WriteClass::SingleKey { key_path: "$.safe" },
            reservation: Reservation::ByConstruction,
            ..ROUTE_INVENTORY[0]
        };
        assert!(
            check_inventory(&sites, &[row], 1)
                .unwrap_err()
                .contains("write class"),
            "{path}"
        );
    }

    let single_key_sql = [
        "UPDATE",
        "notes",
        "SET",
        "properties",
        "=",
        "json_set(properties,",
        "'$.read',",
        "1),",
        "updated_at",
        "=",
        "2",
        "WHERE",
        "json_extract(properties,",
        "'$.status')",
        "=",
        "'pending'",
    ]
    .join(" ");
    assert_eq!(sql_target(&single_key_sql), Some(Substrate::Note));
    assert_eq!(sql_single_key_path(&single_key_sql), Some("$.read".into()));
    for prefix in [
        vec!["INSERT", "INTO", "entities"],
        vec!["INSERT", "OR", "IGNORE", "INTO", "entities"],
        vec!["INSERT", "OR", "REPLACE", "INTO", "entities"],
        vec!["REPLACE", "INTO", "entities"],
    ] {
        assert_eq!(sql_target(&prefix.join(" ")), Some(Substrate::Entity));
    }
    for (replace, target) in [
        (
            "REPLACE INTO notes (id, properties) VALUES (?1, ?2)",
            Substrate::Note,
        ),
        (
            "REPLACE INTO entities (id, properties) VALUES (?1, ?2)",
            Substrate::Entity,
        ),
    ] {
        assert_eq!(sql_target(replace), Some(target));
        let replace_sites = scan_sources(&[(
            "sample/src/lib.rs".into(),
            format!("fn replace() {{ let _ = {replace:?}; }}"),
        )])
        .unwrap();
        assert_eq!(replace_sites[0].target, target);
        assert!(check_inventory(&replace_sites, &[], 0)
            .unwrap_err()
            .contains("unmapped sample/src/lib.rs::replace"));
    }
    assert_eq!(
        sql_target(&["UPDATE", "entities", "SET", "properties", "=", "?1"].join(" ")),
        Some(Substrate::Entity)
    );
    assert_eq!(
        sql_target(&["UPDATE", "notes", "SET", "updated_at", "=", "?1"].join(" ")),
        None
    );
    for sql in [
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_set(properties,",
            "'$.status',",
            "1,",
            "'$.at',",
            "2)",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_set(properties,",
            "'$.nested.key',",
            "1)",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
        [
            "UPDATE",
            "notes",
            "SET",
            "properties",
            "=",
            "json_remove(json_set(properties,",
            "'$.safe',",
            "1),",
            "'$.other')",
            "WHERE",
            "id",
            "=",
            "1",
        ]
        .join(" "),
    ] {
        assert_eq!(sql_single_key_path(&sql), None, "{sql}");
    }

    let excluded = scan_sources(&[("sample/src/lib.rs".into(),
        "#[cfg(test)] mod tests { fn hidden(s: &dyn NoteStore, n: Note) { s.upsert_note(n); } } #[test] fn other(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into())]).unwrap();
    assert!(excluded.is_empty());
    assert!(check_inventory(&excluded, &[], 0).is_ok());

    let probe = (
        "sample/src/probe.rs".into(),
        "fn write(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into(),
    );
    for declaration in [
        "#[cfg(test)] #[path = \"probe.rs\"] mod tests;",
        "#[cfg(all(test, feature = \"extra\"))] #[path = \"probe.rs\"] mod tests;",
        "#[cfg(test)] mod probe;",
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), declaration.into()),
            probe.clone(),
        ];
        let sites = scan_sources(&sources).unwrap();
        assert!(sites.is_empty(), "{declaration}");
        assert!(check_inventory(&sites, &[], 0).is_ok(), "{declaration}");
    }
    for declaration in [
        "#[path = \"probe.rs\"] mod production;",
        "#[cfg(any(test, feature = \"extra\"))] #[path = \"probe.rs\"] mod production;",
        "#[cfg(test)] #[path = \"probe.rs\"] mod tests; #[path = \"probe.rs\"] mod production;",
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), declaration.into()),
            probe.clone(),
        ];
        let sites = scan_sources(&sources).unwrap();
        assert_eq!(sites.len(), 1, "{declaration}");
        assert!(
            check_inventory(&sites, &[], 0)
                .unwrap_err()
                .contains("unmapped"),
            "{declaration}"
        );
    }

    let transitive = scan_sources(&[
        (
            "sample/src/lib.rs".into(),
            "#[cfg(test)] #[path = \"probe.rs\"] mod tests;".into(),
        ),
        ("sample/src/probe.rs".into(), "mod nested;".into()),
        (
            "sample/src/probe/nested.rs".into(),
            "fn write(s: &dyn NoteStore, n: Note) { s.upsert_note(n); }".into(),
        ),
    ])
    .unwrap();
    assert!(transitive.is_empty());

    let included_test_module = scan_sources(&[
        (
            "sample/src/lib.rs".into(),
            "include!(\"included.rs\");".into(),
        ),
        (
            "sample/src/included.rs".into(),
            "#[cfg(all(test, feature = \"extra\"))] mod tests { fn hidden(s: &dyn NoteStore, n: Note) { s.upsert_note(n); } }".into(),
        ),
    ])
    .unwrap();
    assert!(included_test_module.is_empty());

    let mut trait_sources = live_workspace_sources()
        .into_iter()
        .filter(|(path, _)| {
            path == "khive-storage/src/entity.rs" || path == "khive-storage/src/note.rs"
        })
        .collect::<Vec<_>>();
    assert!(check_store_trait_methods(&trait_sources).is_ok());
    let note_trait = trait_sources
        .iter_mut()
        .find(|(path, _)| path == "khive-storage/src/note.rs")
        .expect("note trait source");
    let declaration = "pub trait NoteStore: Send + Sync + 'static {";
    assert_eq!(note_trait.1.matches(declaration).count(), 1);
    note_trait.1 = note_trait.1.replacen(
        declaration,
        "pub trait NoteStore: Send + Sync + 'static { fn write_note_properties(&self) {}",
        1,
    );
    assert!(check_store_trait_methods(&trait_sources)
        .unwrap_err()
        .contains("unclassified store method write_note_properties"));
}
