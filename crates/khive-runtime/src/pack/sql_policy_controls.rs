// Keep fixture SQL excluded by the unchanged inline-SQL test-module predicate.
#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn still_inline_control_requires_production_sql() {
        let root = PathBuf::from("/crates/db");
        let mut sources = BTreeMap::new();
        for path in [
            "src/tests.rs",
            "src/foo_tests.rs",
            "src/tests/fixture.rs",
            "src/benches/fixture.rs",
            "tests/api.rs",
        ] {
            sources.insert(
                root.join(path),
                r#"const Q: &str = "SELECT id FROM notes";"#.into(),
            );
        }
        sources.insert(
            root.join("src/lib.rs"),
            r#"#[cfg(test)] mod tests { const Q: &str = "SELECT id FROM notes"; }"#.into(),
        );
        assert!(!still_inline_seen(&sources, &root));
        sources.insert(
            root.join("src/query.rs"),
            r#"const Q: &str = "SELECT id FROM notes";"#.into(),
        );
        assert!(still_inline_seen(&sources, &root));
        sources.remove(&root.join("src/query.rs"));
        assert!(!still_inline_seen(&sources, &root));
    }

    #[test]
    fn out_of_line_test_module_declaration_keeps_the_code_after_it() {
        let source = r#"#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

impl Store {
    fn q() -> &'static str { "SELECT id FROM notes" }
}

#[cfg(test)]
mod inline {
    const Q: &str = "SELECT secret FROM notes";
}
"#;
        assert_eq!(
            sql_literals(&strip_test_modules(source)),
            vec!["SELECT id FROM notes".to_string()]
        );
    }

    #[test]
    fn test_fixture_braces_do_not_hide_production_sql() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/lib.rs");
        for fixture in [
            r#"const BRACE: &str = "{";"#,
            r##"const BRACE: &str = r#"{"#;"##,
            r#"const BRACE: &[u8] = b"{";"#,
            "const BRACE: char = '{';",
            "// {",
            "/* { /* nested } */ { */",
            r#"const BRACE: &str = "}";"#,
        ] {
            let source = format!(
                "#[cfg(test)]\nmod tests {{\n{fixture}\n\
                 const Q: &str = \"SELECT fixture FROM notes\";\n}}\n\
                 const Q: &str = \"SELECT production FROM notes\";\n"
            );
            let sources = BTreeMap::from([(path.clone(), source.clone())]);
            assert_eq!(
                inline_errors(&sources, &root, &["demo"], &[]),
                vec!["inline SQL: demo/src/lib.rs: SELECT production FROM notes"],
                "{source}"
            );
            assert!(still_inline_seen(&sources, &root.join("demo")));
        }
    }

    #[test]
    fn test_module_mask_preserves_prefix_suffix_and_line_boundaries() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/lib.rs");
        for prefix in [
            "",
            "\u{feff}",
            "#!/usr/bin/env rust-script\n",
            "\u{feff}#!/usr/bin/env rust-script\r\n",
            "#![allow(dead_code)]\n",
            "\u{feff}#! /* prelude */ [allow(dead_code)]\r\n",
        ] {
            let before = format!("{prefix}const LABEL: &str = \"雪\"; ");
            let after = " const Q: &str = \"SELECT production FROM notes\";\r\n";
            for attributes in [
                "#[cfg(test)]\r\n#[doc = \"SELECT fixture_doc FROM notes\"]",
                "#[doc = \"SELECT fixture_doc FROM notes\"]\r\n#[cfg(test)]",
            ] {
                let source = format!(
                    "{before}{attributes}\r\nmod tests {{\r\n\
                     const Q: &str = \"SELECT fixture FROM notes\";\r\n}}{after}"
                );
                let masked = strip_test_modules(&source);
                assert_eq!(masked.len(), source.len());
                assert!(masked.starts_with(&before));
                assert!(masked.ends_with(after));
                for (index, byte) in source.bytes().enumerate() {
                    if matches!(byte, b'\r' | b'\n') {
                        assert_eq!(masked.as_bytes()[index], byte);
                    }
                }
                let sources = BTreeMap::from([(path.clone(), source)]);
                assert_eq!(
                    inline_errors(&sources, &root, &["demo"], &[]),
                    vec!["inline SQL: demo/src/lib.rs: SELECT production FROM notes"]
                );
            }
        }
    }

    #[test]
    fn only_actual_direct_cfg_test_inline_modules_are_excluded() {
        let source = r##"
const MARKER: &str = r#"#[cfg(test)] mod decoy {"#;
// #[cfg(test)] mod comment_decoy {
mod outer {
    #[cfg(test)]
    mod fixture {
        const Q: &str = "SELECT fixture FROM notes";
        #[cfg(test)]
        mod nested { const Q: &str = "SELECT nested_fixture FROM notes"; }
    }
    const Q: &str = "SELECT nested_production FROM notes";
}
#[cfg(feature = "demo")]
mod conditional { const Q: &str = "SELECT conditional FROM notes"; }
#[cfg(all(test, feature = "demo"))]
mod compound { const Q: &str = "SELECT compound FROM notes"; }
const Q: &str = "SELECT production FROM notes";
"##;
        let root = PathBuf::from("/crates");
        let sources = BTreeMap::from([(root.join("demo/src/lib.rs"), source.into())]);
        assert_eq!(
            inline_errors(&sources, &root, &["demo"], &[]),
            vec![
                "inline SQL: demo/src/lib.rs: SELECT nested_production FROM notes",
                "inline SQL: demo/src/lib.rs: SELECT conditional FROM notes",
                "inline SQL: demo/src/lib.rs: SELECT compound FROM notes",
                "inline SQL: demo/src/lib.rs: SELECT production FROM notes",
            ]
        );
        let masked = strip_test_modules(source);
        assert!(masked.contains(r##"const MARKER: &str = r#"#[cfg(test)] mod decoy {"#;"##));
        assert!(masked.contains("// #[cfg(test)] mod comment_decoy {"));
    }

    #[test]
    #[should_panic(expected = "parse Rust source for inline SQL policy")]
    fn malformed_inline_source_fails_closed() {
        let root = PathBuf::from("/crates");
        let sources = BTreeMap::from([(
            root.join("demo/src/lib.rs"),
            "#[cfg(test)] mod broken {".into(),
        )]);
        inline_errors(&sources, &root, &["demo"], &[]);
    }

    fn fixture(source: &str) -> (BTreeMap<PathBuf, String>, BTreeSet<PathBuf>, PathBuf) {
        let root = PathBuf::from("/crates");
        let sources = BTreeMap::from([
            (root.join("khive-runtime/src/sql_include.rs"), r#"#[macro_export]
                macro_rules! sql { ($name:literal) => { const {
                    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sql/", $name, ".sql")).trim_ascii_end()
                } }; }"#.into()),
            (root.join("khive-runtime/src/lib.rs"), "mod sql_include;".into()),
            (root.join("demo/src/lib.rs"), source.into()),
            (root.join("demo/src/sql.rs"), "pub(crate) use khive_runtime::sql;".into()),
        ]);
        (
            sources,
            BTreeSet::from([root.join("demo/sql/query.sql")]),
            root,
        )
    }

    #[test]
    fn loaders_use_exact_owning_assets_and_detect_removal() {
        for source in [
            r#"const Q: &str = khive_runtime::sql!("query");"#,
            r#"mod sql; const Q: &str = crate::sql::sql!("query");"#,
            r#"mod sql; use crate::sql::sql; const Q: &str = sql!(r"query");"#,
            r#"use khive_runtime::sql as statement; const Q: &str = statement!("qu\u{65}ry");"#,
            r#"const MIGRATION: &str = include_str!("../sql/query.sql");"#,
            r#"const Q: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), concat!("/sql/", "query.sql")));"#,
            r#"const DATA: &str = include_str!("../data.json"); const Q: &str = include_str!("../sql/query.sql");"#,
        ] {
            let (mut sources, mut assets, root) = fixture(source);
            assert!(
                reference_errors(&sources, &assets, &root).is_empty(),
                "{source}"
            );
            assets.insert(root.join("demo/sql/unused.sql"));
            assert!(reference_errors(&sources, &assets, &root)
                .iter()
                .any(|e| e.ends_with("unused.sql")));
            assets.remove(&root.join("demo/sql/unused.sql"));
            sources.insert(root.join("demo/src/lib.rs"), String::new());
            assert!(reference_errors(&sources, &assets, &root)
                .iter()
                .any(|e| e.ends_with("query.sql")));
            sources.insert(root.join("demo/src/lib.rs"), source.into());
            assert!(reference_errors(&sources, &assets, &root).is_empty());
        }
    }

    #[test]
    fn runtime_root_macro_is_canonical_only_in_runtime() {
        let (mut sources, _, root) = fixture("");
        let asset = root.join("khive-runtime/sql/query.sql");
        let assets = BTreeSet::from([asset]);
        sources.insert(
            root.join("khive-runtime/src/lib.rs"),
            "mod sql_include; const Q: &str = crate::sql!(\"query\");".into(),
        );
        assert!(reference_errors(&sources, &assets, &root).is_empty());
        let (foreign, foreign_assets, _) = fixture("const Q: &str = crate::sql!(\"query\");");
        assert!(reference_errors(&foreign, &foreign_assets, &root)
            .iter()
            .any(|e| e.starts_with("unused SQL:")));
        sources.insert(
            root.join("khive-runtime/src/sql_include.rs"),
            "#[macro_export] macro_rules! sql { ($name:literal) => { $name }; }".into(),
        );
        assert!(reference_errors(&sources, &assets, &root)
            .iter()
            .any(|e| e.starts_with("unused SQL:")));
    }

    #[test]
    fn strings_identifiers_shadowing_and_foreign_paths_do_not_count() {
        for source in [
            r#"// khive_runtime::sql!("query");
                const Q: &str = "khive_runtime::sql!(\"query\")";"#,
            r#"const Q: &str = "SELECT query FROM query";"#,
            r#"const Q: &str = include_str!("../../other/sql/query.sql");"#,
            r#"macro_rules! sql { ($x:literal) => { $x }; } const Q: &str = sql!("query");"#,
            r#"mod khive_runtime { macro_rules! sql { ($x:literal) => { $x }; } } const Q: &str = khive_runtime::sql!("query");"#,
            r#"mod sql; use crate::sql::sql; fn f() { macro_rules! sql { ($x:literal) => { $x }; } let _ = sql!("query"); }"#,
            r#"use khive_runtime::sql as statement; fn f() { use foreign::statement; let _ = statement!("query"); }"#,
            r#"#[macro_use] mod foreign { macro_rules! include_str { ($x:literal) => { $x }; } } const Q: &str = include_str!("../sql/query.sql");"#,
            r#"macro_rules! include_str { ($x:literal) => { $x }; } const Q: &str = include_str!("../sql/query.sql");"#,
            r#"fn f() { let generated = quote!(khive_runtime::sql!("query")); }"#,
            r#"const Q: &str = concat!("SELECT ", "query FROM table");"#,
        ] {
            let (sources, assets, root) = fixture(source);
            assert!(
                reference_errors(&sources, &assets, &root)
                    .iter()
                    .any(|e| e.starts_with("unused SQL:")),
                "{source}"
            );
        }
    }

    #[test]
    fn malformed_and_missing_loaders_fail_closed() {
        for source in [
            r#"const Q: &str = khive_runtime::sql!("typo");"#,
            r#"const Q: &str = khive_runtime::sql!(dynamic_name);"#,
            r#"const Q: &str = khive_runtime::sql!("query", "extra");"#,
            r#"const Q: &str = include_str!(computed_sql_path());"#,
            r#"const Q: &str = include_str!(concat!(env!("OTHER"), "/query.sql"));"#,
        ] {
            let (sources, assets, root) = fixture(source);
            let errors = reference_errors(&sources, &assets, &root);
            assert!(errors.len() >= 2, "{source}: {errors:?}");
        }
    }

    #[test]
    fn declared_child_globs_import_only_that_modules_names() {
        let (mut sources, assets, root) =
            fixture(r#"mod inner; use inner::*; const Q: &str = khive_runtime::sql!("query");"#);
        let inner = root.join("demo/src/inner.rs");
        sources.insert(
            inner.clone(),
            "use super::*; pub(super) fn helper() {}".into(),
        );
        assert!(reference_errors(&sources, &assets, &root).is_empty());
        for (child, why) in [
            (
                "pub(super) mod khive_runtime {}",
                "the glob brings in a shadowing module",
            ),
            (
                "pub(super) use dependency::*;",
                "a glob re-export imports unknown names",
            ),
            (
                "thread_local! { static X: u8 = 0; }",
                "an item macro may define any name",
            ),
        ] {
            sources.insert(inner.clone(), child.into());
            assert!(
                reference_errors(&sources, &assets, &root)
                    .iter()
                    .any(|e| e.starts_with("unused SQL:")),
                "{why}"
            );
        }
        let (undeclared, assets, root) =
            fixture(r#"use inner::*; const Q: &str = khive_runtime::sql!("query");"#);
        assert!(reference_errors(&undeclared, &assets, &root)
            .iter()
            .any(|e| e.starts_with("unused SQL:")));
    }

    #[test]
    fn sql_included_outside_the_inventory_must_exist() {
        let (sources, _, fixture_root) = fixture(
            r#"const Q: &str = khive_runtime::sql!("query");
                const D: &str = include_str!("../docs/audit.sql");"#,
        );
        let root = std::env::temp_dir().join(format!("khive-sql-policy-{}", std::process::id()));
        let sources = sources
            .into_iter()
            .map(|(path, text)| (root.join(path.strip_prefix(&fixture_root).unwrap()), text))
            .collect::<BTreeMap<_, _>>();
        let assets = BTreeSet::from([root.join("demo/sql/query.sql")]);
        let doc = root.join("demo/docs/audit.sql");
        assert!(reference_errors(&sources, &assets, &root)
            .iter()
            .any(|e| e.starts_with("missing SQL:") && e.ends_with("audit.sql")));
        std::fs::create_dir_all(doc.parent().unwrap()).unwrap();
        std::fs::write(&doc, "SELECT 1;\n").unwrap();
        let errors = reference_errors(&sources, &assets, &root);
        // A file under sql/ that the inventory does not list is refused even though it exists.
        let nested = root.join("demo/sql/nested/query.sql");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "SELECT 1;\n").unwrap();
        let mut nested_sources = sources.clone();
        nested_sources.insert(
            root.join("demo/src/lib.rs"),
            r#"const Q: &str = khive_runtime::sql!("query");
                const N: &str = include_str!("../sql/nested/query.sql");"#
                .into(),
        );
        let nested_errors = reference_errors(&nested_sources, &assets, &root);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            nested_errors.iter().any(|e| e.starts_with("missing SQL:")
                && e.ends_with("query.sql")
                && e.contains("nested")),
            "{nested_errors:?}"
        );
    }

    #[test]
    fn module_ancestry_preserves_imports_and_shadowing() {
        let (mut sources, assets, root) = fixture("mod sql; use crate::sql::sql; mod child;");
        sources.insert(
            root.join("demo/src/child.rs"),
            "use super::*; const Q: &str = sql!(\"query\");".into(),
        );
        assert!(reference_errors(&sources, &assets, &root).is_empty());
        sources.insert(
            root.join("demo/src/lib.rs"),
            "use dependency::*; mod child;".into(),
        );
        sources.insert(
            root.join("demo/src/child.rs"),
            r#"const Q: &str = khive_runtime::sql!("query");"#.into(),
        );
        assert!(reference_errors(&sources, &assets, &root).is_empty());
        for (parent, child) in [
            (
                r#"#[macro_use] mod foreign { macro_rules! format { ($($t:tt)*) => { 0 }; } } mod child;"#,
                r#"const Q: usize = format!(khive_runtime::sql!("query"));"#,
            ),
            (
                r#"mod sql; macro_rules! sql { ($x:literal) => { $x }; } mod child;"#,
                r#"use super::*; const Q: &str = sql!("query");"#,
            ),
            (
                r#"macro_rules! format { ($($t:tt)*) => { 0 }; } mod child;"#,
                r#"const Q: usize = format!(khive_runtime::sql!("query"));"#,
            ),
            (
                r#"macro_rules! statement { ($($t:tt)*) => { 0 }; } mod child;"#,
                r#"use khive_runtime::sql as statement; const Q: usize = statement!("query");"#,
            ),
        ] {
            sources.insert(root.join("demo/src/lib.rs"), parent.into());
            sources.insert(root.join("demo/src/child.rs"), child.into());
            assert!(reference_errors(&sources, &assets, &root)
                .iter()
                .any(|e| e.starts_with("unused SQL:")));
        }
    }

    #[test]
    fn named_keep_opts_in_only_its_unconverted_source_and_remains_exact() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/query.rs");
        let original = r#"const Q: &str = "SELECT id FROM notes";"#;
        let mut sources = BTreeMap::from([
            (path.clone(), original.into()),
            (
                root.join("demo/src/unconverted.rs"),
                r#"const Q: &str = "SELECT unrelated FROM notes";"#.into(),
            ),
        ]);
        let keep = || Keep {
            name: "one_dynamic_query",
            path: "demo/src/query.rs",
            sql: "SELECT id FROM notes",
            reason: "Opt one source into the policy before converting its crate.",
        };
        assert!(inline_errors(&sources, &root, &[], &[keep()]).is_empty());
        sources.insert(
            path.clone(),
            format!("{original}\nconst EXTRA: &str = \"SELECT extra FROM notes\";"),
        );
        assert_eq!(
            inline_errors(&sources, &root, &[], &[keep()]),
            vec!["inline SQL: demo/src/query.rs: SELECT extra FROM notes"]
        );
        sources.insert(path.clone(), format!("{original}\n{original}"));
        assert_eq!(
            inline_errors(&sources, &root, &[], &[keep()]),
            vec!["inline keep one_dynamic_query expected once, found 2"]
        );
        sources.remove(&path);
        assert_eq!(
            inline_errors(&sources, &root, &[], &[keep()]),
            vec!["inline keep one_dynamic_query expected once, found 0"]
        );
    }

    #[test]
    fn named_keeps_are_exact_live_single_statement_exceptions() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/lib.rs");
        let sql = "SELECT id FROM {table}";
        let source = r#"fn query() { let _ = format!("SELECT id FROM {table}"); }"#;
        let mut sources = BTreeMap::from([(path.clone(), source.into())]);
        let keep = || Keep {
            name: "dynamic_table",
            path: "demo/src/lib.rs",
            sql,
            reason: "Runtime table identifier.",
        };
        assert!(inline_errors(&sources, &root, &["demo"], &[keep()]).is_empty());
        assert!(!inline_errors(&sources, &root, &["demo"], &[]).is_empty());
        assert!(!inline_errors(
            &sources,
            &root,
            &["demo"],
            &[Keep {
                reason: "",
                ..keep()
            }]
        )
        .is_empty());
        assert!(!inline_errors(&sources, &root, &["demo"], &[keep(), keep()]).is_empty());
        for changed in [
            "",
            r#"fn f() { let _ = "SELECT other FROM {table}"; }"#,
            r#"fn f() { let _ = "SELECT id FROM {table}"; let _ = "SELECT id FROM {table}"; }"#,
            r#"fn f() { let _ = "SELECT id FROM {table}"; let _ = "SELECT secret FROM notes"; }"#,
        ] {
            sources.insert(path.clone(), changed.into());
            assert!(
                !inline_errors(&sources, &root, &["demo"], &[keep()]).is_empty(),
                "{changed}"
            );
        }
    }

    #[test]
    fn with_materialization_renderings_trip_the_inline_sql_control() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/lib.rs");
        for (sql, expected) in [
            (
                "WITH t AS (SELECT 1)\nSELECT * FROM t",
                "WITH t AS (SELECT 1) SELECT * FROM t",
            ),
            (
                "WITH t AS\nMATERIALIZED (SELECT 1)\nSELECT * FROM t",
                "WITH t AS MATERIALIZED (SELECT 1) SELECT * FROM t",
            ),
            (
                "WITH t AS\nNOT MATERIALIZED (SELECT 1)\nSELECT * FROM t",
                "WITH t AS NOT MATERIALIZED (SELECT 1) SELECT * FROM t",
            ),
            (
                "WITH t AS (SELECT 1),\nu AS (SELECT * FROM t)\nSELECT * FROM u",
                "WITH t AS (SELECT 1), u AS (SELECT * FROM t) SELECT * FROM u",
            ),
            (
                "WITH t AS\nMATERIALIZED (SELECT 1),\nu AS\nMATERIALIZED (SELECT * FROM t)\nSELECT * FROM u",
                "WITH t AS MATERIALIZED (SELECT 1), u AS MATERIALIZED (SELECT * FROM t) SELECT * FROM u",
            ),
            (
                "WITH t AS\nNOT MATERIALIZED (SELECT 1),\nu AS\nNOT MATERIALIZED (SELECT * FROM t)\nSELECT * FROM u",
                "WITH t AS NOT MATERIALIZED (SELECT 1), u AS NOT MATERIALIZED (SELECT * FROM t) SELECT * FROM u",
            ),
        ] {
            for rendered in [
                format!("\"{expected}\""),
                format!("\"{}\"", sql.replace('\n', "\\n")),
                format!("\"{}\"", sql.replace('\n', " \\\n            ")),
                format!("r#\"{sql}\"#"),
            ] {
                let source = format!("const Q: &str = {rendered};");
                assert_eq!(sql_literals(&source), vec![expected.to_owned()], "{source}");
                let sources = BTreeMap::from([(path.clone(), source)]);
                assert_eq!(
                    inline_errors(&sources, &root, &["demo"], &[]),
                    vec![format!("inline SQL: demo/src/lib.rs: {expected}")],
                );
                let keep = Keep {
                    name: "rendering_control",
                    path: "demo/src/lib.rs",
                    sql: expected,
                    reason: "Synthetic statement for the must-fail rendering control.",
                };
                assert!(inline_errors(&sources, &root, &["demo"], &[keep]).is_empty());
            }
        }
    }

    #[test]
    fn materialized_keeps_still_require_one_exact_path_and_statement() {
        let root = PathBuf::from("/crates");
        let path = root.join("demo/src/lib.rs");
        for sql in [
            "WITH t AS MATERIALIZED (SELECT 1) SELECT * FROM t",
            "WITH t AS NOT MATERIALIZED (SELECT 1) SELECT * FROM t",
        ] {
            let source = format!("const Q: &str = \"{sql}\";");
            let keep = || Keep {
                name: "hinted_statement",
                path: "demo/src/lib.rs",
                sql,
                reason: "Synthetic exact-keep control.",
            };
            let duplicate = BTreeMap::from([(path.clone(), format!("{source}\n{source}"))]);
            assert_eq!(
                inline_errors(&duplicate, &root, &["demo"], &[keep()]),
                vec!["inline keep hinted_statement expected once, found 2"],
            );
            let missing = BTreeMap::from([(path.clone(), String::new())]);
            assert_eq!(
                inline_errors(&missing, &root, &["demo"], &[keep()]),
                vec!["inline keep hinted_statement expected once, found 0"],
            );
            let moved = BTreeMap::from([(root.join("demo/src/other.rs"), source)]);
            assert_eq!(
                inline_errors(&moved, &root, &["demo"], &[keep()]),
                vec![
                    format!("inline SQL: demo/src/other.rs: {sql}"),
                    "inline keep hinted_statement expected once, found 0".to_owned(),
                ],
            );
            let changed = sql.replace("SELECT 1", "SELECT 2");
            let changed_sources =
                BTreeMap::from([(path.clone(), format!("const Q: &str = \"{changed}\";"))]);
            assert_eq!(
                inline_errors(&changed_sources, &root, &["demo"], &[keep()]),
                vec![
                    format!("inline SQL: demo/src/lib.rs: {changed}"),
                    "inline keep hinted_statement expected once, found 0".to_owned(),
                ],
            );
        }
    }

    #[test]
    fn inline_module_paths_preserve_textual_macro_scope() {
        for (source, child) in [
            (
                r#"mod outer { macro_rules! format { ($($t:tt)*) => {0}; } #[path="child.rs"] mod child; }"#,
                "src/outer/child.rs",
            ),
            (
                r#"#[path="special"] mod outer { macro_rules! format { ($($t:tt)*) => {0}; } #[path="child.rs"] mod child; }"#,
                "src/special/child.rs",
            ),
        ] {
            let (mut sources, assets, root) = fixture(source);
            sources.insert(
                root.join("demo").join(child),
                r#"const Q: usize = format!(khive_runtime::sql!("query"));"#.into(),
            );
            assert!(reference_errors(&sources, &assets, &root)
                .iter()
                .any(|e| e.starts_with("unused SQL:")));
            // The same path resolution must also credit an actual unshadowed load.
            sources.insert(
                root.join("demo").join(child),
                r#"const Q: &str = khive_runtime::sql!("query");"#.into(),
            );
            assert!(reference_errors(&sources, &assets, &root).is_empty());
        }
        let (mut sources, assets, root) = fixture("mod holder;");
        sources.insert(root.join("demo/src/holder.rs"),
            r#"mod outer { macro_rules! format { ($($t:tt)*) => {0}; } #[path="child.rs"] mod child; }"#.into());
        sources.insert(
            root.join("demo/src/holder/outer/child.rs"),
            r#"const Q: usize = format!(khive_runtime::sql!("query"));"#.into(),
        );
        assert!(reference_errors(&sources, &assets, &root)
            .iter()
            .any(|e| e.starts_with("unused SQL:")));
    }
}
