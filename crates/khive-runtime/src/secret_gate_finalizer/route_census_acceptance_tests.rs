use std::collections::BTreeSet;

use super::super::declaration::{Acceptance, RouteInventoryEntry, ROUTE_INVENTORY};
use super::acceptance::check_acceptance_paths;
use super::acceptance_sources::acceptance_roots;

fn row(path: &'static str) -> RouteInventoryEntry {
    RouteInventoryEntry {
        id: "fixture.acceptance",
        acceptance: Acceptance::Test { path },
        ..ROUTE_INVENTORY[0]
    }
}

fn sources(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(path, text)| ((*path).into(), (*text).into()))
        .collect()
}

fn unit_roots() -> BTreeSet<String> {
    BTreeSet::from(["sample/src/lib.rs".into()])
}

#[test]
fn acceptance_requires_an_exact_reachable_test_path() {
    let sources = sources(&[
        ("sample/src/lib.rs", "#[cfg(test)] mod tests;"),
        (
            "sample/src/tests.rs",
            r##"
            #[test] fn accepted() {}
            fn ordinary() {}
            mod nested { #[tokio::test] async fn accepted() {} }
            const DECOY: &str = "#[test] fn string_only() {}";
        "##,
        ),
        ("sample/src/orphan.rs", "#[test] fn orphan() {}"),
        ("sample/tests/support/orphan.rs", "#[test] fn orphan() {}"),
    ]);
    for path in [
        "sample/src/tests.rs::accepted",
        "sample/src/tests.rs::nested::accepted",
    ] {
        check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).unwrap();
    }
    for path in [
        "sample/src/missing.rs::accepted",
        "sample/src/tests.rs::renamed",
        "sample/src/tests.rs::ordinary",
        "sample/src/tests.rs::string_only",
        "sample/src/tests.rs::wrong::accepted",
        "sample/src/orphan.rs::orphan",
        "sample/tests/support/orphan.rs::orphan",
    ] {
        let error = check_acceptance_paths(&sources, &unit_roots(), &[row(path)])
            .expect_err("a same-named function alone does not establish acceptance");
        assert!(error.contains("fixture.acceptance"), "{error}");
        assert!(error.contains(path), "{error}");
    }
}

#[test]
fn acceptance_follows_test_modules_path_overrides_and_includes() {
    let sources = sources(&[
        (
            "sample/src/lib.rs",
            r#"
            #[cfg(test)] mod tests;
            #[cfg_attr(test, path = "actual.rs")] mod named;
            mod outer { #[path = "odd.rs"] mod fixture; }
            include!("included.rs");
            #[cfg(not(test))] mod unavailable;
        "#,
        ),
        ("sample/src/tests.rs", "mod child;"),
        (
            "sample/src/tests/child.rs",
            "#[tokio::test] async fn nested() {}",
        ),
        ("sample/src/actual.rs", "#[test] fn renamed() {}"),
        ("sample/src/named.rs", "#[test] fn orphan() {}"),
        ("sample/src/outer/odd.rs", "#[test] fn overridden() {}"),
        (
            "sample/src/included.rs",
            "mod nested { #[test] fn included() {} }",
        ),
        ("sample/src/unavailable.rs", "#[test] fn unavailable() {}"),
    ]);
    for path in [
        "sample/src/tests/child.rs::nested",
        "sample/src/actual.rs::renamed",
        "sample/src/outer/odd.rs::overridden",
        "sample/src/included.rs::nested::included",
    ] {
        check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).unwrap();
    }
    assert!(check_acceptance_paths(
        &sources,
        &unit_roots(),
        &[row("sample/src/unavailable.rs::unavailable")]
    )
    .is_err());
    assert!(check_acceptance_paths(
        &sources,
        &unit_roots(),
        &[row("sample/src/named.rs::orphan")]
    )
    .is_err());
}

#[test]
fn acceptance_rejects_items_and_files_disabled_in_test_configuration() {
    let sources = sources(&[
        (
            "sample/src/lib.rs",
            r#"
            mod disabled;
            #[cfg(any())] #[test] fn never() {}
            #[cfg(not(test))] #[test] fn production_only() {}
            #[cfg_attr(test, cfg(not(test)))] #[test] fn disabled_attribute() {}
            #[cfg_attr(test, test)] fn conditional_test() {}
            #[cfg(all(test, feature = "optional"))] #[test] fn optional() {}
            fn ordinary() { #[test] fn inner() {} }
        "#,
        ),
        (
            "sample/src/disabled.rs",
            "#![cfg(not(test))]\n#[test] fn disabled() {}",
        ),
    ]);
    check_acceptance_paths(
        &sources,
        &unit_roots(),
        &[row("sample/src/lib.rs::optional")],
    )
    .unwrap();
    check_acceptance_paths(
        &sources,
        &unit_roots(),
        &[row("sample/src/lib.rs::conditional_test")],
    )
    .unwrap();
    for path in [
        "sample/src/lib.rs::disabled_attribute",
        "sample/src/lib.rs::never",
        "sample/src/lib.rs::production_only",
        "sample/src/lib.rs::ordinary::inner",
        "sample/src/disabled.rs::disabled",
    ] {
        assert!(
            check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).is_err(),
            "{path}"
        );
    }
}

#[test]
fn acceptance_uses_manifest_targets_and_reachable_integration_helpers() {
    let sources = sources(&[
        ("sample/src/lib.rs", "#[test] fn unit() {}"),
        ("sample/src/main.rs", "#[test] fn binary() {}"),
        ("sample/src/bin/tool.rs", "#[test] fn binary() {}"),
        ("sample/src/bin/tool/helper.rs", "#[test] fn orphan() {}"),
        (
            "sample/tests/suite.rs",
            "mod support; #[test] fn integration() {}",
        ),
        ("sample/tests/support/mod.rs", "#[test] fn support() {}"),
        ("sample/tests/nested/main.rs", "#[test] fn nested() {}"),
        (
            "sample/tests/unreferenced/helper.rs",
            "#[test] fn orphan() {}",
        ),
    ]);
    let manifests = vec![("sample".into(), "[package]\nname = 'sample'".into())];
    let roots = acceptance_roots(&sources, &manifests).unwrap();
    assert_eq!(
        roots,
        BTreeSet::from([
            "sample/src/lib.rs".into(),
            "sample/src/main.rs".into(),
            "sample/src/bin/tool.rs".into(),
            "sample/tests/suite.rs".into(),
            "sample/tests/nested/main.rs".into(),
        ])
    );
    for path in [
        "sample/src/lib.rs::unit",
        "sample/src/main.rs::binary",
        "sample/src/bin/tool.rs::binary",
        "sample/tests/suite.rs::integration",
        "sample/tests/support/mod.rs::support",
        "sample/tests/nested/main.rs::nested",
    ] {
        check_acceptance_paths(&sources, &roots, &[row(path)]).unwrap();
    }
    for path in [
        "sample/src/bin/tool/helper.rs::orphan",
        "sample/tests/unreferenced/helper.rs::orphan",
    ] {
        assert!(
            check_acceptance_paths(&sources, &roots, &[row(path)]).is_err(),
            "{path}"
        );
    }
}

#[test]
fn acceptance_respects_disabled_automatic_targets_and_explicit_paths() {
    let sources = sources(&[
        ("sample/src/lib.rs", "#[test] fn disabled() {}"),
        ("sample/src/main.rs", "#[test] fn disabled() {}"),
        ("sample/tests/auto.rs", "#[test] fn disabled() {}"),
        (
            "sample/tests/support/explicit.rs",
            "#[test] fn explicit() {}",
        ),
        ("sample/tests/manual.rs", "#[test] fn disabled() {}"),
    ]);
    let manifests = vec![(
        "sample".into(),
        r#"
        [package]
        name = "sample"
        autotests = false
        autobins = false
        [lib]
        test = false
        [[test]]
        name = "explicit"
        path = "tests/support/explicit.rs"
        [[test]]
        name = "manual"
        harness = false
    "#
        .into(),
    )];
    let roots = acceptance_roots(&sources, &manifests).unwrap();
    assert_eq!(
        roots,
        BTreeSet::from(["sample/tests/support/explicit.rs".into()])
    );
    check_acceptance_paths(
        &sources,
        &roots,
        &[row("sample/tests/support/explicit.rs::explicit")],
    )
    .unwrap();
    for path in [
        "sample/src/lib.rs::disabled",
        "sample/src/main.rs::disabled",
        "sample/tests/auto.rs::disabled",
        "sample/tests/manual.rs::disabled",
    ] {
        assert!(
            check_acceptance_paths(&sources, &roots, &[row(path)]).is_err(),
            "{path}"
        );
    }
}

#[test]
fn acceptance_explicit_target_alias_suppresses_automatic_discovery() {
    let sources = sources(&[
        ("sample/src/lib.rs", "#[test] fn unit() {}"),
        ("sample/tests/automatic.rs", "#[test] fn disabled() {}"),
        ("sample/tests/shared.rs", "#[test] fn shared() {}"),
    ]);
    let manifests = vec![(
        "sample".into(),
        r#"
        [package]
        name = "sample"
        [[test]]
        name = "disabled_alias"
        path = "tests/automatic.rs"
        harness = false
        [[test]]
        name = "library_alias"
        path = "src/lib.rs"
        test = false
        [[test]]
        name = "enabled_shared"
        path = "tests/shared.rs"
        [[test]]
        name = "disabled_shared"
        path = "tests/shared.rs"
        harness = false
    "#
        .into(),
    )];
    let roots = acceptance_roots(&sources, &manifests).unwrap();
    assert_eq!(
        roots,
        BTreeSet::from(["sample/src/lib.rs".into(), "sample/tests/shared.rs".into()])
    );
    for path in ["sample/src/lib.rs::unit", "sample/tests/shared.rs::shared"] {
        check_acceptance_paths(&sources, &roots, &[row(path)]).unwrap();
    }
    assert!(check_acceptance_paths(
        &sources,
        &roots,
        &[row("sample/tests/automatic.rs::disabled")]
    )
    .is_err());
}

#[test]
fn acceptance_preserves_each_include_origin_and_local_module_path() {
    let sources = sources(&[
        (
            "sample/src/lib.rs",
            r#"
            #[cfg(test)] mod first { include!("shared.rs"); }
            #[cfg(test)] mod second { include!("shared.rs"); }
        "#,
        ),
        (
            "sample/src/shared.rs",
            "mod nested { #[test] fn included() {} }",
        ),
    ]);
    for path in [
        "sample/src/lib.rs::first::nested::included",
        "sample/src/lib.rs::second::nested::included",
        "sample/src/shared.rs::nested::included",
    ] {
        check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).unwrap();
    }
    for path in [
        "sample/src/lib.rs::nested::included",
        "sample/src/lib.rs::wrong::nested::included",
    ] {
        let error = check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).unwrap_err();
        assert!(
            error.contains("fixture.acceptance") && error.contains(path),
            "{error}"
        );
    }
}

#[test]
fn acceptance_source_cycles_fail_with_row_and_path_without_recursing_forever() {
    let sources = sources(&[
        ("sample/src/lib.rs", "include!(\"cycle.rs\");"),
        (
            "sample/src/cycle.rs",
            "include!(\"lib.rs\"); #[test] fn unreachable() {}",
        ),
    ]);
    let path = "sample/src/cycle.rs::unreachable";
    let error = check_acceptance_paths(&sources, &unit_roots(), &[row(path)]).unwrap_err();
    assert!(error.contains("cyclic test source inclusion"), "{error}");
    assert!(
        error.contains("fixture.acceptance") && error.contains(path),
        "{error}"
    );
}
