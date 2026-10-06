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
