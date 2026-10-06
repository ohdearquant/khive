use super::*;

#[test]
fn static_sql_keeps_named_route_guard_class_and_occurrences() {
    let route = *ROUTE_INVENTORY
        .iter()
        .find(|route| route.id == "gtd.transition.statement")
        .unwrap();
    let path = "khive-pack-gtd/src/handlers.rs";
    let sql = "UPDATE notes SET properties=?1 WHERE id=?2";
    let assets =
        StaticSqlSources::from([("khive-pack-gtd/sql/fixture.sql".into(), format!("{sql}\n"))]);
    let inline = format!("fn gtd_transition_statement() {{ reject_reserved_secret_gate_property(properties); call({sql:?}); }}");
    let original = scan_source_population(&[(path.into(), inline)]).unwrap();
    for loader in [
        "khive_runtime::sql!(\"fixture\")",
        "include_str!(\"../sql/fixture.sql\")",
    ] {
        let source = format!("fn gtd_transition_statement() {{ reject_reserved_secret_gate_property(properties); call({loader}); }}");
        let population =
            scan_source_population_with_sql(&[(path.into(), source.clone())], &assets).unwrap();
        let before = &original.properties[0];
        let after = &population.properties[0];
        assert_eq!(
            (
                &after.key,
                after.target,
                after.route_class,
                &after.class,
                after.write_count,
                &after.calls
            ),
            (
                &before.key,
                before.target,
                before.route_class,
                &before.class,
                before.write_count,
                &before.calls
            )
        );
        assert!(check_inventory(&population.properties, &[route], 0).is_ok());
        let unchecked = source.replace("reject_reserved_secret_gate_property(properties);", "");
        let population =
            scan_source_population_with_sql(&[(path.into(), unchecked)], &assets).unwrap();
        assert!(check_inventory(&population.properties, &[route], 0).is_err());
        let doubled = source.replace(
            &format!("call({loader});"),
            &format!("call({loader}); call({loader});"),
        );
        let population =
            scan_source_population_with_sql(&[(path.into(), doubled)], &assets).unwrap();
        assert_eq!(population.properties[0].write_count, 2);
        assert!(check_inventory(&population.properties, &[route], 0).is_err());
    }
}

#[test]
fn static_sql_refuses_shadowed_loaders_without_poisoning_sibling_scopes() {
    let assets = StaticSqlSources::from([(
        "sample/sql/query.sql".into(),
        "UPDATE notes SET properties='{}'".into(),
    )]);
    for source in [
        "mod khive_runtime {} fn writer() { khive_runtime::sql!(\"query\"); }",
        "fn writer() { use other as khive_runtime; khive_runtime::sql!(\"query\"); }",
        "fn writer() { use unknown::*; khive_runtime::sql!(\"query\"); }",
        "macro_rules! include_str { () => {}; } fn writer() { include_str!(\"../sql/query.sql\"); }",
    ] {
        assert!(scan_source_population_with_sql(&[("sample/src/lib.rs".into(), source.into())], &assets).is_err(), "{source}");
    }
    let sources = vec![
        (
            "sample/src/lib.rs".into(),
            "mod unrelated; mod writer;".into(),
        ),
        ("sample/src/unrelated.rs".into(), "use unknown::*;".into()),
        (
            "sample/src/writer.rs".into(),
            "fn writer() { khive_runtime::sql!(\"query\"); }".into(),
        ),
    ];
    assert_eq!(
        scan_source_population_with_sql(&sources, &assets)
            .unwrap()
            .properties
            .len(),
        1
    );
    let sources = vec![
        (
            "sample/src/lib.rs".into(),
            "macro_rules! include_str { () => {}; } mod writer;".into(),
        ),
        (
            "sample/src/writer.rs".into(),
            "fn writer() { include_str!(\"../sql/query.sql\"); }".into(),
        ),
    ];
    assert!(scan_source_population_with_sql(&sources, &assets).is_err());
}

#[test]
fn static_sql_keeps_backend_and_test_exclusions_and_fixed_key_classes() {
    let assets = StaticSqlSources::from([
        ("sample/sql/query.sql".into(), "UPDATE notes SET properties=json_set(properties,'$.channel_slug',?1,'$.quarantine_content_ref',?2)".into()),
        ("khive-db/sql/query.sql".into(), "INSERT INTO notes (properties) VALUES (?1)".into()),
    ]);
    let sources = vec![
        ("sample/src/lib.rs".into(), "#[cfg(test)] mod hidden { fn writer() { khive_runtime::sql!(\"missing\"); } } fn writer() { let _ = vec![khive_runtime::sql!(\"query\")]; }".into()),
        ("khive-db/src/lib.rs".into(), "const QUERY: &str = include_str!(\"../sql/query.sql\");".into()),
    ];
    let population = scan_source_population_with_sql(&sources, &assets).unwrap();
    assert_eq!(population.properties.len(), 1);
    assert_eq!(population.properties[0].write_count, 1);
    assert_eq!(
        population.properties[0].route_class,
        RouteClass::Application
    );
    assert_eq!(
        population.properties[0].class,
        DetectedClass::FixedKeySet(BTreeSet::from([
            "$.channel_slug".into(),
            "$.quarantine_content_ref".into()
        ]))
    );
}

#[test]
fn static_sql_distinguishes_module_item_scope_from_textual_macro_scope() {
    let assets = StaticSqlSources::from([(
        "sample/sql/query.sql".into(),
        "UPDATE notes SET properties='{}'".into(),
    )]);
    let writer = "fn writer() { khive_runtime::sql!(\"query\"); }";
    for parent in [
        "use other as khive_runtime; mod child;",
        "mod unrelated { use unknown::*; } mod child;",
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), parent.into()),
            ("sample/src/child.rs".into(), writer.into()),
        ];
        assert_eq!(
            scan_source_population_with_sql(&sources, &assets)
                .unwrap()
                .properties
                .len(),
            1
        );
    }
    let included = "fn writer() { include_str!(\"../sql/query.sql\"); }";
    for (parent, accepted) in [
        ("mod child; macro_rules! include_str { () => {}; }", true),
        ("macro_rules! include_str { () => {}; } mod child;", false),
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), parent.into()),
            ("sample/src/child.rs".into(), included.into()),
        ];
        assert_eq!(
            scan_source_population_with_sql(&sources, &assets).is_ok(),
            accepted,
            "{parent}"
        );
    }
    for (source, accepted) in [
        ("fn writer() { include_str!(\"../sql/query.sql\"); } macro_rules! include_str { () => {}; }", true),
        ("macro_rules! include_str { () => {}; } fn writer() { include_str!(\"../sql/query.sql\"); }", false),
        ("use other as khive_runtime; mod child { fn writer() { khive_runtime::sql!(\"query\"); } }", true),
        ("#![no_implicit_prelude] fn writer() { khive_runtime::sql!(\"query\"); }", false),
        ("extern crate other as khive_runtime; mod child { fn writer() { khive_runtime::sql!(\"query\"); } }", false),
        ("extern crate self as khive_runtime; mod child { fn writer() { khive_runtime::sql!(\"query\"); } }", false),
        ("use other as khive_runtime; fn writer() { ::khive_runtime::sql!(\"query\"); }", true),
    ] {
        assert_eq!(scan_source_population_with_sql(&[("sample/src/lib.rs".into(), source.into())], &assets).is_ok(), accepted, "{source}");
    }
    let sources = vec![
        (
            "sample/src/lib.rs".into(),
            "use other as khive_runtime; include!(\"included.rs\");".into(),
        ),
        ("sample/src/included.rs".into(), writer.into()),
    ];
    assert!(scan_source_population_with_sql(&sources, &assets).is_err());
}

#[test]
fn static_sql_loader_guard_ignores_quoted_spelling_but_refuses_opaque_calls() {
    let assets = StaticSqlSources::from([(
        "sample/sql/query.sql".into(),
        "UPDATE notes SET properties='{}'".into(),
    )]);
    let harmless = "fn writer() { format!(\"khive_runtime :: sql !\"); format!(\"include_str ! (query.sql)\"); }";
    assert!(scan_source_population_with_sql(
        &[("sample/src/lib.rs".into(), harmless.into())],
        &assets
    )
    .unwrap()
    .properties
    .is_empty());
    for source in [
        "fn writer() { format!(\"{}\", khive_runtime::sql!(\"query\")); }",
        "fn writer() { opaque! { label => include_str!(\"../sql/query.sql\") } }",
    ] {
        assert!(scan_source_population_with_sql(
            &[("sample/src/lib.rs".into(), source.into())],
            &assets
        )
        .is_err());
    }
}

#[test]
fn static_sql_macro_scope_follows_inline_path_attributes() {
    for (outer, child_path) in [
        ("macro_rules! include_str { () => {}; } mod nested { #[path=\"leaf.rs\"] mod leaf; }", "sample/src/outer/nested/leaf.rs"),
        ("macro_rules! include_str { () => {}; } #[path=\"custom_dir\"] mod nested { #[path=\"leaf.rs\"] mod leaf; }", "sample/src/custom_dir/leaf.rs"),
    ] {
        let sources = vec![
            ("sample/src/lib.rs".into(), "mod outer;".into()),
            ("sample/src/outer.rs".into(), outer.into()),
            (child_path.into(), "const SQL: &str = include_str!(\"query.sql\");".into()),
        ];
        let asset = format!("{}/query.sql", Path::new(child_path).parent().unwrap().display());
        let assets = StaticSqlSources::from([(asset, "UPDATE notes SET properties='{}'".into())]);
        assert!(scan_source_population_with_sql(&sources, &assets).is_err(), "{child_path}");
        let mut unshadowed = sources.clone();
        unshadowed[1].1 = outer.replace("macro_rules! include_str { () => {}; }", "");
        assert_eq!(scan_source_population_with_sql(&unshadowed, &assets).unwrap().properties.len(), 1);
    }
}

#[test]
fn static_sql_refuses_guarded_assets_that_no_followed_loader_reaches() {
    let path = "sample/sql/query.sql";
    let assets = StaticSqlSources::from([(
        path.into(),
        "UPDATE notes SET properties=?1 WHERE id=?2\n".into(),
    )]);
    let followed = "fn writer() { reject_reserved_secret_gate_property(properties); \
                    call(khive_runtime::sql!(\"query\")); }";
    let population =
        scan_source_population_with_sql(&[("sample/src/lib.rs".into(), followed.into())], &assets)
            .unwrap();
    assert!(population.resolved_sql.contains(path));
    assert!(check_sql_asset_reach(&assets, &population.resolved_sql).is_ok());
    for source in [
        "use crate::sql::sql; fn writer() { call(sql!(\"query\")); }",
        "fn writer() { call(include_bytes!(\"../sql/query.sql\")); }",
        "fn writer() { call(include_str!(concat!(\"../sql/query.\", \"sql\"))); }",
        "fn writer() {}",
    ] {
        let population = scan_source_population_with_sql(
            &[("sample/src/lib.rs".into(), source.into())],
            &assets,
        )
        .unwrap();
        assert!(population.properties.is_empty(), "{source}");
        let failure = check_sql_asset_reach(&assets, &population.resolved_sql).unwrap_err();
        assert!(failure.contains(path), "{source}: {failure}");
    }
    let read_only = StaticSqlSources::from([(
        path.into(),
        "SELECT properties FROM notes WHERE id=?1".into(),
    )]);
    assert!(check_sql_asset_reach(&read_only, &BTreeSet::new()).is_ok());
}
