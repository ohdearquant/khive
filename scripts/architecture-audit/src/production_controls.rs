use super::*;
fn put(snapshot: &mut Snapshot, path: &str, text: &str) {
    snapshot.blobs.insert(
        path.into(),
        Blob {
            mode: "100644".into(),
            oid: "1".repeat(40),
            text: text.into(),
        },
    );
}
fn fixture(source: &str) -> Snapshot {
    let mut snapshot = Snapshot {
        commit: "a".repeat(40),
        tree: "b".repeat(40),
        ..Snapshot::default()
    };
    put(
        &mut snapshot,
        "crates/demo/Cargo.toml",
        "[package]\nname='demo'\nversion='0.1.0'\n",
    );
    put(&mut snapshot, "crates/demo/src/lib.rs", source);
    put(
        &mut snapshot,
        "crates/khive-runtime/src/lib.rs",
        "mod sql_include;",
    );
    put(
        &mut snapshot,
        "crates/khive-runtime/src/sql_include.rs",
        r#"#[macro_export]
        macro_rules! sql { ($name:literal) => { const {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql")).trim_ascii_end()
        } }; }"#,
    );
    snapshot
}
fn report(snapshot: &Snapshot) -> Value {
    analyze(snapshot, &["demo".into()])
}
fn row(report: &Value) -> &Value {
    &report["crates"][0]
}
#[test]
fn production_items_are_masked_before_collection_and_children_never_reroot() {
    let mut snapshot = fixture(
        r#"
        const POSITIVE: &str = "SELECT id FROM live";
        #[cfg(test)] fn hidden() { let _ = "SELECT id FROM test_fn"; }
        #[cfg(test)] const HIDDEN: &str = "SELECT id FROM test_const";
        #[cfg(test)] static STATIC: &str = "SELECT id FROM test_static";
        #[cfg(test)] impl Demo { fn hidden() { let _ = "SELECT id FROM test_impl"; } }
        #[cfg(test)] trait Hidden { fn hidden() { let _ = "SELECT id FROM test_trait"; } }
        #[cfg(test)] extern "C" { fn hidden(); }
        #[cfg(test)] mod tests;
        fn nested() { #[cfg(test)] fn hidden() { let _ = "SELECT id FROM nested_test"; } }
    "#,
    );
    put(
        &mut snapshot,
        "crates/demo/src/tests.rs",
        "const Q: &str = \"SELECT id FROM child_test\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/orphan.rs",
        "const Q: &str = \"SELECT id FROM orphan\";",
    );
    let output = report(&snapshot);
    let r = row(&output);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(r["inline_sql_occurrences"], 1);
    assert_eq!(r["production_sql"], 1);
    assert!(r["unmounted_sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "crates/demo/src/tests.rs"));
    assert!(r["unmounted_sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "crates/demo/src/orphan.rs"));
    assert!(!r["test_only_source_ranges"].as_array().unwrap().is_empty());
}
#[test]
fn cfg_union_and_effective_paths_retain_provenance_and_refuse_unknown_path_choices() {
    let mut snapshot = fixture(
        r#"
        #[cfg(all(test, feature="x"))] const HIDDEN: &str = "SELECT id FROM hidden";
        #[cfg(any(test, feature="x"))] const CONDITIONAL: &str = "SELECT id FROM conditional";
        #[cfg(not(test))] const DEFINITE: &str = "SELECT id FROM definite";
        #[cfg_attr(not(test), path="selected.rs")] mod chosen;
        #[cfg_attr(test, path="missing.rs")] mod normal;
    "#,
    );
    put(
        &mut snapshot,
        "crates/demo/src/chosen.rs",
        "const TEST: &str = \"SELECT id FROM test_choice\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/selected.rs",
        "const Q: &str = \"SELECT id FROM selected\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/normal.rs",
        "const Q: &str = \"SELECT id FROM normal\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/missing.rs",
        "const TEST: &str = \"SELECT id FROM test_normal\";",
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(row(&output)["inline_sql_occurrences"], 4);
    assert_eq!(row(&output)["inline_conditional"], 1);
    put(&mut snapshot, "crates/demo/src/lib.rs", "const Q: &str = \"SELECT id FROM live\"; #[cfg_attr(feature=\"x\",path=\"selected.rs\")] mod normal;");
    let refused = report(&snapshot);
    assert_eq!(refused["complete"], false);
    assert!(!row(&refused)["diagnostics"].as_array().unwrap().is_empty());
}
#[test]
fn readers_are_sites_assets_are_distinct_and_test_only_unread_are_separate() {
    let mut snapshot = fixture(
        r#"
        use khive_runtime::sql as query;
        const ONE: &str = query!("one");
        const AGAIN: &str = query!("one");
        #[cfg(test)] const TEST: &str = query!("test");
        const INLINE: &str = "SELECT id FROM inline_table";
    "#,
    );
    for name in ["one", "test", "unused"] {
        put(
            &mut snapshot,
            &format!("crates/demo/sql/{name}.sql"),
            "SELECT id FROM stored;",
        );
    }
    let output = report(&snapshot);
    let r = row(&output);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(r["asset_total"], 3);
    assert_eq!(r["asset_production"], 1);
    assert_eq!(r["reader_site_count"], 2);
    assert_eq!(r["inline_sql_occurrences"], 1);
    assert_eq!(r["production_sql"], 2);
    assert_eq!(r["test_only_asset_count"], 1);
    assert_eq!(r["unread_asset_count"], 1);
    let readers = r["reader_sites"].as_array().unwrap();
    assert_ne!(readers[0]["start"], readers[1]["start"]);
    assert!(readers
        .iter()
        .all(|p| p["line"].as_u64().unwrap() > 1 && p["blob"].as_str().unwrap().len() == 40));
    snapshot.blobs.remove("crates/demo/sql/one.sql");
    let refused = report(&snapshot);
    assert_eq!(refused["complete"], false);
    assert_eq!(row(&refused)["asset_production"], 0);
    assert!(!row(&refused)["diagnostics"].as_array().unwrap().is_empty());
}
#[test]
fn immutable_static_includes_cross_crate_assets_and_ancestry_work_without_host_fallback() {
    let mut snapshot = fixture(
        r#"mod nested; const Q: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/own.sql"));"#,
    );
    put(
        &mut snapshot,
        "crates/demo/src/nested.rs",
        "use khive_runtime::sql; mod child;",
    );
    put(&mut snapshot, "crates/demo/src/nested/child.rs", "use super::*; const Q: &str = sql!(\"own\"); const CROSS: &str = include_str!(\"../../../other/sql/foreign.sql\");");
    put(
        &mut snapshot,
        "crates/demo/sql/own.sql",
        "SELECT id FROM own;",
    );
    put(
        &mut snapshot,
        "crates/other/Cargo.toml",
        "[package]\nname='other'\nversion='0.1.0'",
    );
    put(
        &mut snapshot,
        "crates/other/src/lib.rs",
        "const Q: &str = \"SELECT id FROM positive\";",
    );
    put(
        &mut snapshot,
        "crates/other/sql/foreign.sql",
        "SELECT id FROM foreign_table;",
    );
    let output = analyze(&snapshot, &["demo".into(), "other".into()]);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(output["crates"][0]["reader_site_count"], 3);
    assert_eq!(output["crates"][1]["asset_production"], 1);
    snapshot.blobs.remove("crates/other/sql/foreign.sql");
    let refused = report(&snapshot);
    assert_eq!(refused["complete"], false);
    assert!(!row(&refused)["diagnostics"].as_array().unwrap().is_empty());
}
#[test]
fn literals_are_ast_expressions_concat_is_one_format_arguments_are_independent() {
    let snapshot = fixture(
        r##"
        // "SELECT id FROM comment"
        #[doc = "SELECT id FROM docs"] fn f() {}
        macro_rules! symbolic { () => { "SELECT id FROM symbolic" } }
        const Q: &str = concat!("SELECT id ", "FROM joined");
        fn body() { let _ = format!("SELECT id FROM {table}", table="items");
            let _ = format!("{}", "SELECT id FROM nested");
            let _ = r#"SELECT id FROM raw_table"#; let _ = "SELECT id\nFROM escaped";
            let _ = "SELECT 1"; let _ = "update with a reason";
        }
    "##,
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(row(&output)["inline_sql_occurrences"], 5);
    assert_eq!(row(&output)["tableless_occurrences"], 1);
    assert!(row(&output)["inline_sites"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["value"] == "SELECT id FROM joined"));
}
#[test]
fn shadowed_loaders_opaque_sql_and_ambiguous_reachable_sources_refuse_with_positive() {
    for source in [
        "const Q: &str = \"SELECT id FROM live\"; macro_rules! sql { ($n:literal) => {0} } const BAD: i32 = sql!(\"query\");",
        "const Q: &str = \"SELECT id FROM live\"; let_sql!(\"SELECT id FROM opaque\");",
        "const Q: &str = \"SELECT id FROM live\"; mod ambiguous;",
        "const Q: &str = \"SELECT id FROM live\"; mod broken;",
    ] {
        let mut snapshot = fixture(source);
        put(&mut snapshot, "crates/demo/src/ambiguous.rs", "const Q: &str = \"SELECT id FROM one\";");
        put(&mut snapshot, "crates/demo/src/ambiguous/mod.rs", "const Q: &str = \"SELECT id FROM two\";");
        put(&mut snapshot, "crates/demo/src/broken.rs", "fn broken(");
        let output = report(&snapshot);
        // A foreign sql macro is not an asset reader; the other cases are incomplete.
        if source.contains("macro_rules!") { assert_eq!(row(&output)["reader_site_count"], 0); }
        else { assert_eq!(output["complete"], false); assert!(!row(&output)["diagnostics"].as_array().unwrap().is_empty()); }
        assert!(row(&output)["inline_sql_occurrences"].as_u64().unwrap() >= 1);
    }
}
#[test]
fn cargo_targets_are_explicit_and_inline_only_crates_are_not_lost() {
    let mut snapshot = fixture("const Q: &str = \"SELECT id FROM ignored_lib\";");
    put(&mut snapshot, "crates/demo/Cargo.toml", "[package]\nname='demo'\nversion='0.1.0'\nautobins=false\nautolib=false\n[lib]\npath='custom.rs'\n[[bin]]\nname='worker'\npath='worker.rs'\nrequired-features=['worker']");
    put(
        &mut snapshot,
        "crates/demo/custom.rs",
        "const Q: &str = \"SELECT id FROM lib\";",
    );
    put(
        &mut snapshot,
        "crates/demo/worker.rs",
        "const Q: &str = \"SELECT id FROM worker\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/bin/ignored.rs",
        "const Q: &str = \"SELECT id FROM ignored_bin\";",
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(row(&output)["inline_sql_occurrences"], 2);
    assert_eq!(row(&output)["inline_conditional"], 1);
    assert_eq!(row(&output)["asset_total"], 0);
    snapshot.blobs.remove("crates/demo/worker.rs");
    let refused = report(&snapshot);
    assert_eq!(refused["complete"], false);
    assert!(!refused["diagnostics"].as_array().unwrap().is_empty());
    assert_eq!(analyze(&snapshot, &["absent".into()])["complete"], false);
}
#[test]
fn conventional_auto_bins_and_conditional_duplicate_mounts_deduplicate_sites() {
    let mut snapshot = fixture("#[cfg(unix)] #[path=\"shared.rs\"] mod one; #[cfg(windows)] #[path=\"shared.rs\"] mod two;");
    put(
        &mut snapshot,
        "crates/demo/src/shared.rs",
        "const Q: &str = \"SELECT id FROM shared\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/bin/tool.rs",
        "const Q: &str = \"SELECT id FROM bin\";",
    );
    put(
        &mut snapshot,
        "crates/demo/src/bin/worker/main.rs",
        "const Q: &str = \"SELECT id FROM worker\";",
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(row(&output)["roots"].as_array().unwrap().len(), 3);
    assert_eq!(row(&output)["inline_sql_occurrences"], 3);
    assert_eq!(row(&output)["inline_conditional"], 1);
    let shared = row(&output)["inline_sites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "crates/demo/src/shared.rs")
        .unwrap();
    assert_eq!(shared["cfg"].as_array().unwrap().len(), 2);
}
#[test]
fn bom_shebang_crlf_offsets_and_false_file_cfg_are_original_bytes() {
    let source = "\u{feff}#!/bin/example\r\n#[cfg(test)] fn hidden() { let _ = \"SELECT id FROM hidden\"; }\r\nconst Q: &str = \"SELECT id FROM live\";\r\n";
    let snapshot = fixture(source);
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    let site = &row(&output)["inline_sites"][0];
    assert_eq!(site["line"], 3);
    let start = site["start"].as_u64().unwrap() as usize;
    let end = site["end"].as_u64().unwrap() as usize;
    assert_eq!(&source[start..end], "\"SELECT id FROM live\"");
    let mut snapshot = fixture("const Q: &str = \"SELECT id FROM live\"; mod test_file;");
    put(
        &mut snapshot,
        "crates/demo/src/test_file.rs",
        "#![cfg(test)] const Q: &str = \"SELECT id FROM hidden\";",
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(row(&output)["inline_sql_occurrences"], 1);
}

#[test]
fn quoted_format_targets_are_dynamic_only_at_actual_template_sites() {
    let snapshot = fixture(
        r#"const STATIC: &str = "SELECT id FROM \"{table}\"";
        fn f() { let _ = format!("SELECT id FROM \"{table}\"", table="notes");
            let _ = format!("SELECT id FROM \"{{table}}\""); }"#,
    );
    let output = report(&snapshot);
    assert_eq!(output["complete"], true, "{output}");
    let sites = row(&output)["inline_sites"].as_array().unwrap();
    assert_eq!(sites.len(), 3);
    assert_eq!(
        sites
            .iter()
            .filter(|s| s["targets"][0]["dynamic"] == true)
            .count(),
        1
    );
    assert_eq!(
        sites
            .iter()
            .filter(|s| s["targets"][0]["dynamic"] == false)
            .count(),
        2
    );
}

#[test]
fn missing_test_child_never_certifies_unread_assets_from_absence() {
    let mut snapshot = fixture(
        r#"use khive_runtime::sql;
        const POSITIVE: &str = sql!("live"); #[cfg(test)] mod missing_tests;"#,
    );
    put(
        &mut snapshot,
        "crates/demo/sql/live.sql",
        "SELECT id FROM live;",
    );
    put(
        &mut snapshot,
        "crates/demo/sql/unobserved.sql",
        "SELECT id FROM maybe_tests;",
    );
    let output = report(&snapshot);
    let r = row(&output);
    assert_eq!(output["complete"], false);
    assert_eq!(r["production_complete"], true);
    assert_eq!(r["test_source_complete"], false);
    assert_eq!(r["asset_classification_complete"], false);
    assert_eq!(r["production_sql"], 1);
    assert_eq!(r["unread_asset_count"], 0);
    assert_eq!(r["test_read_unknown_asset_count"], 1);
    assert_eq!(
        r["test_read_unknown_assets"][0],
        "crates/demo/sql/unobserved.sql"
    );
    assert!(!r["test_source_diagnostics"].as_array().unwrap().is_empty());
}

#[test]
fn gated_variant_and_field_const_blocks_do_not_contribute_production_sql() {
    let snapshot = fixture(
        r#"const P: &str = "SELECT id FROM live";
        enum E { #[cfg(test)] Hidden = { let _ = "SELECT id FROM hidden_variant"; 1 }, Visible = 2 }
        struct S { #[cfg(test)] hidden: [u8; { let _ = "SELECT id FROM hidden_field"; 1 }], visible: u8 }
    "#,
    );
    let output = report(&snapshot);
    let r = row(&output);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(r["inline_sql_occurrences"], 1);
    assert_eq!(r["inline_sites"][0]["targets"][0]["relation"], "live");
    assert!(
        r["test_only_source_ranges"][0]["excluded_ranges"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
}

#[test]
fn attributed_argument_and_default_descendants_are_masked_before_sql_collection() {
    let snapshot = fixture(
        r#"const POSITIVE: &str = "SELECT id FROM live";
        fn f(#[cfg(test)] arg: [u8; { let _ = "SELECT id FROM hidden_argument"; 1 }]) {}
        struct G<#[cfg(test)] const N: usize = { let _ = "SELECT id FROM hidden_const_default"; 1 },
            #[cfg(test)] T = [u8; { let _ = "SELECT id FROM hidden_type_default"; 1 }]> { value: u8 }
        type F = fn(#[cfg(test)] arg: [u8; { let _ = "SELECT id FROM hidden_bare_argument"; 1 }]);
        impl S { fn receiver(#[cfg(test)] self: [u8; { let _ = "SELECT id FROM hidden_receiver"; 1 }]) {} }
        fn closure() { let _ = |#[cfg(test)] arg: [u8; { let _ = "SELECT id FROM hidden_closure_argument"; 1 }]| {}; }
    "#,
    );
    let output = report(&snapshot);
    let r = row(&output);
    assert_eq!(output["complete"], true, "{output}");
    assert_eq!(r["inline_sql_occurrences"], 1);
    assert_eq!(r["inline_sites"][0]["targets"][0]["relation"], "live");
    assert!(
        r["test_only_source_ranges"][0]["excluded_ranges"]
            .as_array()
            .unwrap()
            .len()
            >= 6
    );
}

#[test]
fn canonical_macro_identity_respects_production_definition_and_mount_cfg() {
    let mut baseline = fixture("use khive_runtime::sql; const Q: &str = sql!(\"live\");");
    put(
        &mut baseline,
        "crates/demo/sql/live.sql",
        "SELECT id FROM live;",
    );
    let positive = report(&baseline);
    assert_eq!(positive["complete"], true, "{positive}");
    assert_eq!(row(&positive)["asset_production"], 1);
    for (path, prefix) in [
        ("crates/khive-runtime/src/sql_include.rs", "#[cfg(test)]\n"),
        ("crates/khive-runtime/src/lib.rs", "#[cfg(test)]\n"),
        ("crates/khive-runtime/src/sql_include.rs", "#![cfg(test)]\n"),
        ("crates/khive-runtime/src/lib.rs", "#![cfg(test)]\n"),
    ] {
        let mut snapshot = Snapshot {
            commit: baseline.commit.clone(),
            tree: baseline.tree.clone(),
            blobs: baseline.blobs.clone(),
        };
        let original = snapshot.blobs[path].text.clone();
        put(&mut snapshot, path, &format!("{prefix}{original}"));
        let output = report(&snapshot);
        assert_eq!(output["complete"], false, "{path}: {output}");
        assert_eq!(row(&output)["asset_production"], 0, "{path}: {output}");
        assert!(!row(&output)["diagnostics"].as_array().unwrap().is_empty());
    }
}

#[test]
fn read_asset_uncertainty_prevents_completion_without_erasing_known_credit() {
    let mut snapshot = fixture("const GOOD: &str = include_str!(\"../../owner/sql/good.sql\");");
    put(
        &mut snapshot,
        "crates/owner/Cargo.toml",
        "[package]\nname='owner'\nversion='0.1.0'\n",
    );
    put(
        &mut snapshot,
        "crates/owner/src/lib.rs",
        "pub fn no_sql() {}",
    );
    put(
        &mut snapshot,
        "crates/owner/sql/good.sql",
        "SELECT id FROM live;",
    );
    let selected = ["demo".into(), "owner".into()];
    let positive = analyze(&snapshot, &selected);
    assert_eq!(positive["complete"], true, "{positive}");
    let owner = positive["crates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["crate"] == "owner")
        .unwrap();
    assert_eq!(owner["production_sql"], 1);
    put(&mut snapshot, "crates/demo/src/lib.rs",
        "const GOOD: &str = include_str!(\"../../owner/sql/good.sql\"); const BAD: &str = include_str!(\"../../owner/sql/bad.sql\");");
    put(
        &mut snapshot,
        "crates/owner/sql/bad.sql",
        "SELECT id FROM notes; SELECT id FROM {opaque};",
    );
    let output = analyze(&snapshot, &selected);
    let r = output["crates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["crate"] == "owner")
        .unwrap();
    assert_eq!(output["complete"], false, "{output}");
    assert!(r["diagnostics"].as_array().unwrap().is_empty());
    assert_eq!(r["production_complete"], false);
    assert_eq!(r["asset_production"], 2);
    assert_eq!(r["production_sql"], 2);
    assert!(r["asset_sql_diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d.as_str().unwrap().starts_with("crates/owner/sql/bad.sql:")));
    let bad = r["assets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["path"] == "crates/owner/sql/bad.sql")
        .unwrap();
    assert_eq!(bad["production_read"], true);
    assert!(!bad["diagnostics"].as_array().unwrap().is_empty());
    assert!(bad["targets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["relation"] == "notes"));
}
