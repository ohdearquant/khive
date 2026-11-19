use super::*;

#[test]
fn rust_use_extracts_crate_path() {
    let content = "use serde::Serialize;\nuse crate::foo::Bar;\n";
    let raw = extract_raw_imports("rust", content);
    assert!(raw.contains(&"serde::Serialize".to_string()));
    assert!(raw.contains(&"crate::foo::Bar".to_string()));
}

#[test]
fn rust_classify_external_vs_intra() {
    assert_eq!(
        classify_import("rust", "serde::Serialize", "crate", "mycrate", false),
        Resolved::ExternalProject("serde".to_string())
    );
    assert_eq!(
        classify_import("rust", "mycrate::foo::Bar", "crate", "mycrate", false),
        Resolved::IntraModule("foo::Bar".to_string())
    );
    assert_eq!(
        classify_import(
            "rust",
            "std::collections::HashMap",
            "crate",
            "mycrate",
            false
        ),
        Resolved::Skip
    );
}

#[test]
fn python_classify_relative_import() {
    assert_eq!(
        classify_import("python", ".sibling", "pkg.mod", "pkg", false),
        Resolved::IntraModule("pkg.sibling".to_string())
    );
    assert_eq!(
        classify_import("python", "requests", "pkg.mod", "pkg", false),
        Resolved::ExternalProject("requests".to_string())
    );
}

#[test]
fn python_relative_import_from_package_init_stays_in_package() {
    // `from .x import A` in `pkg/x/__init__.py` (module path `pkg.x`):
    // the leading dot refers to `pkg.x` itself, so the target is the
    // same-name submodule `pkg.x.x`, never a self-loop on `pkg.x`.
    assert_eq!(
        classify_import("python", ".x", "pkg.x", "pkg", true),
        Resolved::IntraModule("pkg.x.x".to_string())
    );
    // `from .y import B` in `pkg/x/__init__.py` -> `pkg.x.y`.
    assert_eq!(
        classify_import("python", ".y", "pkg.x", "pkg", true),
        Resolved::IntraModule("pkg.x.y".to_string())
    );
    // Regular-module behavior unchanged: `from .y import C` in
    // `pkg/x/z.py` -> `pkg.x.y`.
    assert_eq!(
        classify_import("python", ".y", "pkg.x.z", "pkg", false),
        Resolved::IntraModule("pkg.x.y".to_string())
    );
    // Each additional dot climbs one package from the declarer's own
    // package: `from ..a import D` in `pkg/x/__init__.py` -> `pkg.a`.
    assert_eq!(
        classify_import("python", "..a", "pkg.x", "pkg", true),
        Resolved::IntraModule("pkg.a".to_string())
    );
    // A genuinely nameless relative import in `pkg/x/__init__.py` (a
    // star or parenthesized `from . import`) resolves to the package
    // itself — a self-reference, skipped rather than recorded as a
    // self-loop edge. Named submodule imports never reach this shape:
    // `extract_raw_imports` expands them per imported name (#1692).
    assert_eq!(
        classify_import("python", ".", "pkg.x", "pkg", true),
        Resolved::Skip
    );
}

#[test]
fn python_dotless_from_import_expands_imported_names() {
    // `from . import z` keeps the imported name — for a dotless relative
    // import the name *is* the target module (#1692).
    let raw = extract_raw_imports("python", "from . import z\n");
    assert_eq!(raw, vec![".z".to_string()]);
    // Comma-separated and aliased names expand one specifier per name.
    let raw = extract_raw_imports("python", "from .. import a, b as _b\n");
    assert_eq!(raw, vec!["..a".to_string(), "..b".to_string()]);
    // Star imports extract no plain name; the bare package specifier is
    // kept so the pre-expansion behavior (a package-level reference) is
    // preserved rather than dropped.
    let raw = extract_raw_imports("python", "from . import *\n");
    assert_eq!(raw, vec![".".to_string()]);
    // Parenthesized groups fall back to the bare package specifier
    // rather than fabricating a name (multi-line groups stay out of
    // scope for the line-based scanner).
    let raw = extract_raw_imports("python", "from . import (a, b)\n");
    assert_eq!(raw, vec![".".to_string()]);
    // A specifier with a real module part is unchanged — the imported
    // names stay out of it.
    let raw = extract_raw_imports("python", "from .x import A\n");
    assert_eq!(raw, vec![".x".to_string()]);
    let raw = extract_raw_imports("python", "from pkg.mod import thing\n");
    assert_eq!(raw, vec!["pkg.mod".to_string()]);
    // Trailing comments are not part of the name list.
    let raw = extract_raw_imports("python", "from . import z  # re-export\n");
    assert_eq!(raw, vec![".z".to_string()]);
}

#[test]
fn python_relative_import_above_project_root_is_skipped() {
    // Climbing to or above the scanner's project root is unattributable
    // (Python rejects relative imports beyond the top-level package) —
    // never a root-level intra-project target. Both declarer shapes.
    assert_eq!(
        classify_import("python", "..outside", "pkg", "pkg", true),
        Resolved::Skip
    );
    assert_eq!(
        classify_import("python", "..outside", "pkg.mod", "pkg", false),
        Resolved::Skip
    );
    assert_eq!(
        classify_import("python", "...outside", "pkg.x", "pkg", true),
        Resolved::Skip
    );
    // One level short of the root still resolves normally.
    assert_eq!(
        classify_import("python", "..a", "pkg.x.y", "pkg", false),
        Resolved::IntraModule("pkg.a".to_string())
    );
}

#[test]
fn typescript_classify_relative_vs_package() {
    assert_eq!(
        classify_import("typescript", "./util", "", "", false),
        Resolved::IntraModule("./util".to_string())
    );
    assert_eq!(
        classify_import("typescript", "left-pad", "", "", false),
        Resolved::ExternalProject("left-pad".to_string())
    );
}

#[test]
fn module_path_for_rust_lib_root_is_crate() {
    let root = Path::new("/proj");
    let file = Path::new("/proj/src/lib.rs");
    assert_eq!(
        module_path_for_file(file, root, "rust"),
        Some("crate".to_string())
    );
}

#[test]
fn module_path_for_rust_nested_module() {
    let root = Path::new("/proj");
    let file = Path::new("/proj/src/foo/bar.rs");
    assert_eq!(
        module_path_for_file(file, root, "rust"),
        Some("foo::bar".to_string())
    );
}

#[test]
fn module_path_for_rust_main_root_does_not_alias_lib_root() {
    let root = Path::new("/proj");
    let file = Path::new("/proj/src/main.rs");
    assert_eq!(
        module_path_for_file(file, root, "rust"),
        Some("crate::main".to_string())
    );
}

#[test]
fn module_path_for_python_init_uses_parent() {
    let root = Path::new("/proj");
    let file = Path::new("/proj/pkg/__init__.py");
    assert_eq!(
        module_path_for_file(file, root, "python"),
        Some("pkg".to_string())
    );
}
