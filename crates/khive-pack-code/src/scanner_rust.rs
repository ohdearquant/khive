//! Rust Scanner (ADR-085 Amendment 2 B2): syntax-only declaration extraction
//! via `syn`. No type-checking, no compilation — a declaration's doc comment
//! is transcribed verbatim (ADR-069 D5, "transcribe, do not invent"); nothing
//! is synthesized.
//!
//! Scope of this slice: top-level items, inline `mod { .. }` blocks (recursed
//! into, at nested module paths), `impl` methods (inherent methods named
//! `Type::method`, trait-impl methods named `<Type as Trait>::method`), and
//! every trait method — default-bodied or signature-only — named
//! `Trait::method`. A `mod foo;` file-backed module declaration has no inline
//! content to recurse into — that file is discovered and scanned
//! independently by the ingest pipeline's own file walk.

use syn::visit::{self, Visit};
use syn::{Attribute, Block, Expr, ExprCall, Item, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RustDeclKind {
    Function,
    Datatype,
    Interface,
    Module,
}

#[derive(Debug, Clone)]
pub(crate) struct RustDeclaration {
    pub kind: RustDeclKind,
    pub name: String,
    /// Doc comment lines (`///` / `#[doc = "..."]`), transcribed verbatim
    /// and joined with `\n`. `None` when the declaration carries none.
    pub doc: Option<String>,
    /// Token-stream rendering of the item, used only as `content_hash`
    /// input (change detection, not identity — B4).
    pub span_text: String,
    /// Call-target paths found in a function/method body (coverage-floor
    /// call extraction — see `collect_calls`), each a raw path segment list.
    /// Empty for non-callable kinds.
    pub calls: Vec<Vec<String>>,
    /// Module path segments this declaration lives under, relative to the
    /// file's own module root: empty at top level, `["inner"]`
    /// inside `mod inner { .. }`.
    pub module_segments: Vec<String>,
    /// Named-type paths this declaration references (D3 rules 2-7: function
    /// signature parameter/return types and bounds, struct/enum field
    /// types, a type alias's target, trait supertraits) -- resolved against
    /// the project's own declaration set by the ingest pipeline, same as
    /// `calls`. Each entry is the full path as written (e.g. `["a", "T"]`
    /// for `a::T`).
    pub type_refs: Vec<Vec<String>>,
}

/// A syntactically resolvable `impl Trait for Type` relationship (D3 rule 13:
/// `datatype implements interface`). Both sides are the FULL path as written
/// at the impl site (e.g. `["traits", "T"]` for `impl traits::T for ..`),
/// not just the last segment — a qualified path can name a type or trait
/// declared in a different module than the impl block itself, and only the
/// caller (which has the project-wide symbol index and the same
/// `crate`/`self`/`super`-aware resolver used for call targets) can resolve
/// that.
#[derive(Debug, Clone)]
pub(crate) struct RustImplRelation {
    pub type_path: Vec<String>,
    pub trait_path: Vec<String>,
    pub module_segments: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RustFileScan {
    pub declarations: Vec<RustDeclaration>,
    pub impls: Vec<RustImplRelation>,
}

/// Parse `content` as a Rust source file and extract declarations, recursing
/// into inline modules.
///
/// Syntax errors are returned to the caller rather than silently skipping
/// the file — a scanner that swallows parse failures would produce
/// incomplete graphs indistinguishable from a fully-scanned empty file.
pub(crate) fn scan_rust_source(content: &str) -> Result<RustFileScan, syn::Error> {
    let file = syn::parse_file(content)?;
    let mut out = RustFileScan::default();
    for item in &file.items {
        scan_item(item, &[], &mut out);
    }
    Ok(out)
}

fn doc_from_attrs(attrs: &[Attribute]) -> Option<String> {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(expr_lit) = &nv.value {
                if let syn::Lit::Str(s) = &expr_lit.lit {
                    // Transcribe verbatim (ADR-069 D5) — do not trim, that would
                    // silently alter the doc comment's recorded text.
                    lines.push(s.value());
                }
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn span_text<T: quote::ToTokens>(node: &T) -> String {
    quote::ToTokens::to_token_stream(node).to_string()
}

/// The qualified `::`-joined path of a type reference as written at the impl
/// site (e.g. `"a::T"` for `impl a::T { .. }`), not just its last segment --
/// two distinct types that happen to share a final segment name (`a::T` and
/// `b::T`) must not collapse onto the same `T::method` declaration name.
fn type_name_of(ty: &Type) -> Option<String> {
    type_path_segments(ty).map(|segs| segs.join("::"))
}

/// The full path segments of a type reference, e.g. `["types", "S"]` for
/// `types::S` — `None` for a non-path type (`&T`, tuples, etc.), which has
/// no D3 rule 13 target regardless.
fn type_path_segments(ty: &Type) -> Option<Vec<String>> {
    match ty {
        Type::Path(p) => Some(
            p.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect(),
        ),
        _ => None,
    }
}

/// The full path segments of a trait reference, e.g. `["traits", "T"]` for
/// `impl traits::T for ..`.
fn trait_path_segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

fn scan_item(item: &Item, module_segments: &[String], out: &mut RustFileScan) {
    match item {
        Item::Fn(f) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Function,
            name: f.sig.ident.to_string(),
            doc: doc_from_attrs(&f.attrs),
            span_text: span_text(f),
            calls: collect_calls(&f.block),
            module_segments: module_segments.to_vec(),
            type_refs: collect_type_refs_from_signature(&f.sig),
        }),
        Item::Struct(s) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: s.ident.to_string(),
            doc: doc_from_attrs(&s.attrs),
            span_text: span_text(s),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
            type_refs: collect_type_refs_from_fields(&s.fields),
        }),
        Item::Enum(e) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: e.ident.to_string(),
            doc: doc_from_attrs(&e.attrs),
            span_text: span_text(e),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
            type_refs: e
                .variants
                .iter()
                .flat_map(|v| collect_type_refs_from_fields(&v.fields))
                .collect(),
        }),
        Item::Union(u) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: u.ident.to_string(),
            doc: doc_from_attrs(&u.attrs),
            span_text: span_text(u),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
            type_refs: Vec::new(),
        }),
        Item::Type(t) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: t.ident.to_string(),
            doc: doc_from_attrs(&t.attrs),
            span_text: span_text(t),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
            type_refs: collect_type_refs_from_type(&t.ty),
        }),
        Item::Trait(tr) => {
            out.declarations.push(RustDeclaration {
                kind: RustDeclKind::Interface,
                name: tr.ident.to_string(),
                doc: doc_from_attrs(&tr.attrs),
                span_text: span_text(tr),
                calls: Vec::new(),
                module_segments: module_segments.to_vec(),
                type_refs: collect_type_refs_from_bounds(&tr.supertraits),
            });
            // Trait methods: a default-body method hashes and scans calls
            // from its body; a signature-only method (`fn sig(&self);`, no
            // default) still becomes its own `Trait::method` declaration —
            // same as a default-bodied one, just with an empty call list
            // since there is no body to scan.
            let trait_name = tr.ident.to_string();
            for trait_item in &tr.items {
                if let syn::TraitItem::Fn(m) = trait_item {
                    let calls = m.default.as_ref().map(collect_calls).unwrap_or_default();
                    out.declarations.push(RustDeclaration {
                        kind: RustDeclKind::Function,
                        name: format!("{trait_name}::{}", m.sig.ident),
                        doc: doc_from_attrs(&m.attrs),
                        span_text: span_text(m),
                        calls,
                        module_segments: module_segments.to_vec(),
                        type_refs: collect_type_refs_from_signature(&m.sig),
                    });
                }
            }
        }
        Item::Impl(imp) => {
            // `impl Trait for Type` only — an inherent `impl Type { .. }`
            // (imp.trait_ is None) has no D3 rule 13 target. The qualified
            // (`::`-joined) trait path is used, not just its last segment,
            // for the same collision reason as `type_name_of`.
            let trait_name = imp
                .trait_
                .as_ref()
                .map(|(_, path, _)| trait_path_segments(path).join("::"));
            // `impl !Trait for Type` (negative impl, first tuple element
            // `Some(bang)`) asserts the ABSENCE of the relation, not its
            // presence — recording it as a positive `implements` edge would
            // be factually backwards. The ADR-085 D3 contract has no
            // negative-impl relation, so it is skipped rather than modeled.
            if let Some((bang, path, _)) = &imp.trait_ {
                if bang.is_none() {
                    if let Some(type_path) = type_path_segments(&imp.self_ty) {
                        out.impls.push(RustImplRelation {
                            type_path,
                            trait_path: trait_path_segments(path),
                            module_segments: module_segments.to_vec(),
                        });
                    }
                }
            }
            // Methods: both inherent and trait impls, named `Type::method`.
            // Call edges from a method use the enclosing module (not the
            // type) as caller module path — achieved simply by not nesting
            // `module_segments` under the type name.
            //
            // Trait-impl methods are named `<Type as Trait>::method`, Rust's
            // own canonical qualified form — two trait impls on the same
            // type with the same method name (`impl A for S { fn f() }` and
            // `impl B for S { fn f() }`) would otherwise both reduce to
            // `S::f` and silently overwrite one another. Inherent methods
            // keep the plain `Type::method` form.
            if let Some(type_name) = type_name_of(&imp.self_ty) {
                for impl_item in &imp.items {
                    if let syn::ImplItem::Fn(m) = impl_item {
                        let name = match &trait_name {
                            Some(t) => format!("<{type_name} as {t}>::{}", m.sig.ident),
                            None => format!("{type_name}::{}", m.sig.ident),
                        };
                        out.declarations.push(RustDeclaration {
                            kind: RustDeclKind::Function,
                            name,
                            doc: doc_from_attrs(&m.attrs),
                            span_text: span_text(m),
                            calls: collect_calls(&m.block),
                            module_segments: module_segments.to_vec(),
                            type_refs: collect_type_refs_from_signature(&m.sig),
                        });
                    }
                }
            }
        }
        Item::Mod(m) => {
            // `mod foo { .. }` with inline content only; `mod foo;`
            // (file-backed) has no `content` and is discovered by the
            // ingest pipeline's own per-file walk instead.
            if let Some((_, items)) = &m.content {
                let name = m.ident.to_string();
                out.declarations.push(RustDeclaration {
                    kind: RustDeclKind::Module,
                    name: name.clone(),
                    doc: doc_from_attrs(&m.attrs),
                    span_text: span_text(m),
                    calls: Vec::new(),
                    module_segments: module_segments.to_vec(),
                    type_refs: Vec::new(),
                });
                let mut nested = module_segments.to_vec();
                nested.push(name);
                for nested_item in items {
                    scan_item(nested_item, &nested, out);
                }
            }
        }
        _ => {}
    }
}

/// Bare and last-segment call-target extraction (D3 rule 1: `function
/// depends_on function`), a coverage floor deliberately analogous to
/// L1.5's regex import scan: no type inference, so method calls
/// (`self.foo()`, `x.foo()`) are not resolvable from syntax alone and are
/// skipped rather than guessed at.
struct CallCollector {
    calls: Vec<Vec<String>>,
}

impl<'ast> Visit<'ast> for CallCollector {
    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let Expr::Path(p) = node.func.as_ref() {
            let segments: Vec<String> = p
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if !segments.is_empty() {
                self.calls.push(segments);
            }
        }
        visit::visit_expr_call(self, node);
    }
}

fn collect_calls(block: &Block) -> Vec<Vec<String>> {
    let mut collector = CallCollector { calls: Vec::new() };
    collector.visit_block(block);
    collector.calls
}

/// Named-type-path extraction (D3 rules 2-7): every `Type::Path` node
/// reachable from the visited syntax node, including inside generic
/// arguments (`Vec<T>` yields both `Vec` and `T`) -- resolving which of
/// those paths are real project declarations (versus a built-in like `Vec`
/// or `Option`) happens in the ingest pipeline's project-wide symbol index,
/// the same division of labor `CallCollector` uses for call targets.
struct TypeRefCollector {
    type_refs: Vec<Vec<String>>,
}

impl<'ast> Visit<'ast> for TypeRefCollector {
    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        let segments: Vec<String> = node
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        if !segments.is_empty() {
            self.type_refs.push(segments);
        }
        visit::visit_type_path(self, node);
    }
}

/// Type references in a function/method signature: parameter types, the
/// return type, and generic bounds (`Signature::generics` carries both the
/// `<T: Bound>` params and the trailing `where` clause) -- D3 rules 2, 3,
/// 5, 6, 7 depending on what the source/target declarations turn out to be,
/// which only the ingest pipeline's project-wide symbol index can resolve.
fn collect_type_refs_from_signature(sig: &syn::Signature) -> Vec<Vec<String>> {
    let mut collector = TypeRefCollector {
        type_refs: Vec::new(),
    };
    collector.visit_signature(sig);
    collector.type_refs
}

/// Type references in a struct/enum-variant's fields (D3 rule 4/5:
/// `datatype depends_on datatype`/`interface`, "field / composition").
fn collect_type_refs_from_fields(fields: &syn::Fields) -> Vec<Vec<String>> {
    let mut collector = TypeRefCollector {
        type_refs: Vec::new(),
    };
    collector.visit_fields(fields);
    collector.type_refs
}

/// Type references in a single `Type` node (a type alias's own target).
fn collect_type_refs_from_type(ty: &Type) -> Vec<Vec<String>> {
    let mut collector = TypeRefCollector {
        type_refs: Vec::new(),
    };
    collector.visit_type(ty);
    collector.type_refs
}

/// Type references in a trait's supertrait bound list (D3 rule 6:
/// `interface depends_on interface`, "supertrait / bound") -- `trait T:
/// Super1 + Super2 { .. }`.
fn collect_type_refs_from_bounds(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>,
) -> Vec<Vec<String>> {
    let mut collector = TypeRefCollector {
        type_refs: Vec::new(),
    };
    for bound in bounds {
        collector.visit_type_param_bound(bound);
    }
    collector.type_refs
}

#[cfg(test)]
mod tests {
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
}
