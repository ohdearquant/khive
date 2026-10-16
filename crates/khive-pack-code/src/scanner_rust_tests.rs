use super::*;

#[test]
fn scans_function_struct_enum_type_trait() {
    let src = r#"
            /// Doc for f.
            pub fn f() {}
            pub struct S;
            pub enum E { A, B }
            pub type Alias = S;
            pub trait T {}
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<(&str, RustDeclKind)> = scan
        .declarations
        .iter()
        .map(|d| (d.name.as_str(), d.kind))
        .collect();
    assert!(names.contains(&("f", RustDeclKind::Function)));
    assert!(names.contains(&("S", RustDeclKind::Datatype)));
    assert!(names.contains(&("E", RustDeclKind::Datatype)));
    assert!(names.contains(&("Alias", RustDeclKind::Datatype)));
    assert!(names.contains(&("T", RustDeclKind::Interface)));
    let f = scan.declarations.iter().find(|d| d.name == "f").unwrap();
    // Verbatim (ADR-069 D5): the space rustc keeps between `///` and the
    // comment text is not trimmed away.
    assert_eq!(f.doc.as_deref(), Some(" Doc for f."));
}

#[test]
fn scans_impl_trait_for_type_relation() {
    let src = r#"
            pub struct S;
            pub trait T {}
            impl T for S {}
            impl S { pub fn inherent(&self) {} }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    assert_eq!(scan.impls.len(), 1, "inherent impl must not be captured");
    assert_eq!(scan.impls[0].type_path, vec!["S".to_string()]);
    assert_eq!(scan.impls[0].trait_path, vec!["T".to_string()]);
}

#[test]
fn collects_bare_call_targets_not_method_calls() {
    let src = r#"
            pub fn helper() {}
            pub fn caller() {
                helper();
                self.method_call();
                other::path::nested_call();
            }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let caller = scan
        .declarations
        .iter()
        .find(|d| d.name == "caller")
        .unwrap();
    assert!(caller
        .calls
        .iter()
        .any(|c| c == &vec!["helper".to_string()]));
    assert!(caller
        .calls
        .iter()
        .any(|c| c == &vec!["other", "path", "nested_call"]));
    assert!(!caller
        .calls
        .iter()
        .any(|c| c.last().map(String::as_str) == Some("method_call")));
}

#[test]
fn invalid_syntax_is_a_reported_error_not_a_silent_empty_scan() {
    let err = scan_rust_source("fn f( {{{ not valid rust").unwrap_err();
    assert!(!err.to_string().is_empty());
}

/// Inherent impl methods become `Type::method` declarations; trait-impl
/// methods become `<Type as Trait>::method` (Rust's own qualified form);
/// trait default bodies become `Trait::method`; a signature-only trait
/// method extracts too, just with no call list to scan.
#[test]
fn extracts_impl_and_trait_default_methods() {
    let src = r#"
            pub struct S;
            impl S { pub fn m() {} }
            pub trait T {
                fn required(&self);
                fn provided(&self) { helper(); }
            }
            impl T for S {
                fn required(&self) {}
            }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<&str> = scan.declarations.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"S::m"));
    assert!(names.contains(&"T::provided"));
    assert!(
        names.contains(&"T::required"),
        "signature-only trait method must extract as its own declaration"
    );
    assert!(
        names.contains(&"<S as T>::required"),
        "a trait-impl method must use the qualified <Type as Trait> form, not bare Type::method"
    );
    let provided = scan
        .declarations
        .iter()
        .find(|d| d.name == "T::provided")
        .unwrap();
    assert!(provided
        .calls
        .iter()
        .any(|c| c == &vec!["helper".to_string()]));
}

/// Two trait impls on one type with the same method name must produce
/// two distinct symbols (`<S as A>::f` and `<S as B>::f`), never both
/// collapsing onto bare `S::f`.
#[test]
fn two_trait_impls_with_same_method_name_produce_distinct_symbols() {
    let src = r#"
            pub struct S;
            pub trait A { fn f(&self) {} }
            pub trait B { fn f(&self) {} }
            impl A for S { fn f(&self) {} }
            impl B for S { fn f(&self) {} }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<&str> = scan.declarations.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"<S as A>::f"));
    assert!(names.contains(&"<S as B>::f"));
    assert!(
        !names.contains(&"S::f"),
        "trait-impl methods must never collapse onto the bare Type::method form"
    );
}

#[test]
fn generic_trait_and_inherent_impls_have_distinct_method_names() {
    let src = r#"
            struct S;
            impl From<u8> for S { fn from(_: u8) -> Self { S } }
            impl From<u16> for S { fn from(_: u16) -> Self { S } }
            struct G<T>(T);
            impl G<u8> { fn f() {} }
            impl G<u16> { fn f() {} }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<&str> = scan.declarations.iter().map(|d| d.name.as_str()).collect();
    for name in [
        "<S as From<u8>>::from",
        "<S as From<u16>>::from",
        "G<u8>::f",
        "G<u16>::f",
    ] {
        assert!(names.contains(&name), "missing {name}: {names:?}");
    }
    assert_eq!(scan.impls.len(), 2);
    for imp in &scan.impls {
        assert_eq!(imp.type_path, vec!["S".to_string()]);
        assert_eq!(imp.trait_path, vec!["From".to_string()]);
    }
}

#[test]
fn nested_generic_path_arguments_are_rendered_in_method_identity() {
    let src = r#"
            impl outer::G<inner::T<u8>> { fn f() {} }
            impl outer::G<inner::T<u16>> { fn f() {} }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<&str> = scan.declarations.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"outer::G<inner::T<u8>>::f"));
    assert!(names.contains(&"outer::G<inner::T<u16>>::f"));
}

/// Inline `mod inner { .. }` recurses at a nested module path; a module
/// declaration itself is emitted too.
#[test]
fn recurses_into_inline_modules() {
    let src = r#"
            pub mod inner {
                pub fn f() {}
                pub struct S;
            }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let module_decl = scan
        .declarations
        .iter()
        .find(|d| d.name == "inner" && d.kind == RustDeclKind::Module)
        .expect("inline mod becomes a module declaration");
    assert!(module_decl.module_segments.is_empty());
    let f = scan
        .declarations
        .iter()
        .find(|d| d.name == "f")
        .expect("nested fn extracted");
    assert_eq!(f.module_segments, vec!["inner".to_string()]);
    let s = scan
        .declarations
        .iter()
        .find(|d| d.name == "S")
        .expect("nested struct extracted");
    assert_eq!(s.module_segments, vec!["inner".to_string()]);
}

/// Two types that share a final path segment name (`a::T`, `b::T`) but
/// differ in their full qualified path must not collapse impl method
/// declarations onto the same `T::method` name.
#[test]
fn qualified_impl_types_with_same_final_segment_do_not_collide() {
    let src = r#"
            impl a::T { pub fn m() {} }
            impl b::T { pub fn m() {} }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let names: Vec<&str> = scan.declarations.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"a::T::m"));
    assert!(names.contains(&"b::T::m"));
    assert!(
        !names.contains(&"T::m"),
        "qualified impl types must keep their full path in the method identity"
    );
}

/// `impl !Trait for T` (negative impl) asserts absence of the relation
/// and must never be recorded as a positive `implements` edge.
#[test]
fn negative_impl_is_not_recorded_as_a_positive_implements_relation() {
    let src = r#"
            pub struct S;
            pub trait T {}
            impl !T for S {}
        "#;
    let scan = scan_rust_source(src).expect("parses");
    assert!(
        scan.impls.is_empty(),
        "a negative impl must not produce an implements relation"
    );
}

/// `union` declarations extract as datatype entities alongside
/// struct/enum.
#[test]
fn extracts_union_as_datatype() {
    let src = r#"
            pub union U { a: i32, b: f32 }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    let decl = scan
        .declarations
        .iter()
        .find(|d| d.name == "U")
        .expect("union extracted");
    assert_eq!(decl.kind, RustDeclKind::Datatype);
}

/// Impls inside inline modules compose both rules.
#[test]
fn impl_inside_inline_module_gets_nested_module_segments() {
    let src = r#"
            pub mod inner {
                pub struct S;
                pub trait T {}
                impl T for S {}
                impl S { pub fn m() {} }
            }
        "#;
    let scan = scan_rust_source(src).expect("parses");
    assert_eq!(scan.impls.len(), 1);
    assert_eq!(scan.impls[0].module_segments, vec!["inner".to_string()]);
    let m = scan
        .declarations
        .iter()
        .find(|d| d.name == "S::m")
        .expect("method inside inline module extracted");
    assert_eq!(m.module_segments, vec!["inner".to_string()]);
}
