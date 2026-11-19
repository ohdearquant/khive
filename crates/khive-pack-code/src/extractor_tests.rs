use super::*;
use crate::scanner_rust::scan_rust_source;

#[test]
fn adapts_rust_scan_into_language_agnostic_shape() {
    let scan = scan_rust_source(
        r#"
            pub struct S;
            pub trait T {}
            impl T for S {}
            pub fn f() { helper(); }
            "#,
    )
    .expect("parses");
    let extracted = from_rust_scan(scan);
    assert_eq!(extracted.declarations.len(), 3);
    assert_eq!(extracted.impls.len(), 1);
    assert_eq!(extracted.impls[0].type_path, vec!["S".to_string()]);
    assert_eq!(extracted.impls[0].trait_path, vec!["T".to_string()]);
    let f = extracted
        .declarations
        .iter()
        .find(|d| d.name == "f")
        .unwrap();
    assert_eq!(f.kind, DeclKind::Function);
    assert!(f.calls.iter().any(|c| c.segments == vec!["helper"]));
    assert!(!f.content_hash.is_empty());
}

#[test]
fn identical_declarations_hash_identically() {
    let a = from_rust_scan(scan_rust_source("pub fn f() {}").unwrap());
    let b = from_rust_scan(scan_rust_source("pub fn f() {}").unwrap());
    assert_eq!(
        a.declarations[0].content_hash,
        b.declarations[0].content_hash
    );
}

#[test]
fn changed_body_changes_hash() {
    let a = from_rust_scan(scan_rust_source("pub fn f() { 1; }").unwrap());
    let b = from_rust_scan(scan_rust_source("pub fn f() { 2; }").unwrap());
    assert_ne!(
        a.declarations[0].content_hash,
        b.declarations[0].content_hash
    );
}

#[test]
fn code_token_round_trips_through_from_code_token() {
    for kind in [
        DeclKind::Function,
        DeclKind::Datatype,
        DeclKind::Interface,
        DeclKind::Module,
    ] {
        assert_eq!(DeclKind::from_code_token(kind.code_token()), Some(kind));
    }
    assert_eq!(DeclKind::from_code_token("bogus"), None);
}
