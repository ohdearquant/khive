//! Rust Scanner (ADR-085 Amendment 2 B2): syntax-only declaration extraction
//! via `syn`. No type-checking, no compilation — a declaration's doc comment
//! is transcribed verbatim (ADR-069 D5, "transcribe, do not invent"); nothing
//! is synthesized.
//!
//! Scope of this slice: top-level items only (`fn`, `struct`, `enum`, `type`,
//! `trait`, `impl Trait for Type`). Items nested inside another item (a `mod
//! { .. }` block, or a method inside an `impl`/`trait` body) are not scanned
//! — the D2 `module` unit this pack ingests today is one Rust file (L1.5's
//! `module_path_for_file`), not a nested `mod` block, and impl/trait methods
//! need a stable qualified-name scheme this slice does not define. Both are
//! documented follow-up slices, not silent gaps.

use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprCall, Item, ItemFn, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RustDeclKind {
    Function,
    Datatype,
    Interface,
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
    /// Call-target paths found in a function body (coverage-floor call
    /// extraction — see `collect_calls`). Empty for non-function kinds.
    pub calls: Vec<CallRef>,
}

/// A call-target path as written at the call site, e.g. `["helper"]` for a
/// bare `helper()` or `["crate", "foo", "bar"]` for `crate::foo::bar()`.
/// Kept as raw segments rather than pre-resolved: resolving `crate`/`self`/
/// `super` qualifiers requires the calling declaration's own module path,
/// which this scanner does not have (that context lives in
/// `source_ingest::resolve_call_target`).
#[derive(Debug, Clone)]
pub(crate) struct CallRef {
    pub segments: Vec<String>,
}

/// A syntactically resolvable `impl Trait for Type` relationship (D3 rule 13:
/// `datatype implements interface`). Both names are unqualified idents; the
/// caller resolves them against the project's own declaration set.
#[derive(Debug, Clone)]
pub(crate) struct RustImplRelation {
    pub type_name: String,
    pub trait_name: String,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RustFileScan {
    pub declarations: Vec<RustDeclaration>,
    pub impls: Vec<RustImplRelation>,
}

/// Parse `content` as a Rust source file and extract top-level declarations.
/// Syntax errors are returned to the caller rather than silently skipping
/// the file — a scanner that swallows parse failures would produce
/// incomplete graphs indistinguishable from a fully-scanned empty file.
pub(crate) fn scan_rust_source(content: &str) -> Result<RustFileScan, syn::Error> {
    let file = syn::parse_file(content)?;
    let mut out = RustFileScan::default();
    for item in &file.items {
        scan_item(item, &mut out);
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

fn type_name_of(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn scan_item(item: &Item, out: &mut RustFileScan) {
    match item {
        Item::Fn(f) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Function,
            name: f.sig.ident.to_string(),
            doc: doc_from_attrs(&f.attrs),
            span_text: span_text(f),
            calls: collect_calls(f),
        }),
        Item::Struct(s) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: s.ident.to_string(),
            doc: doc_from_attrs(&s.attrs),
            span_text: span_text(s),
            calls: Vec::new(),
        }),
        Item::Enum(e) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: e.ident.to_string(),
            doc: doc_from_attrs(&e.attrs),
            span_text: span_text(e),
            calls: Vec::new(),
        }),
        Item::Type(t) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: t.ident.to_string(),
            doc: doc_from_attrs(&t.attrs),
            span_text: span_text(t),
            calls: Vec::new(),
        }),
        Item::Trait(tr) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Interface,
            name: tr.ident.to_string(),
            doc: doc_from_attrs(&tr.attrs),
            span_text: span_text(tr),
            calls: Vec::new(),
        }),
        Item::Impl(imp) => {
            // `impl Trait for Type` only — an inherent `impl Type { .. }`
            // (imp.trait_ is None) has no D3 rule 13 target.
            if let Some((_, path, _)) = &imp.trait_ {
                if let (Some(trait_name), Some(type_name)) = (
                    path.segments.last().map(|s| s.ident.to_string()),
                    type_name_of(&imp.self_ty),
                ) {
                    out.impls.push(RustImplRelation {
                        type_name,
                        trait_name,
                    });
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
    calls: Vec<CallRef>,
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
                self.calls.push(CallRef { segments });
            }
        }
        visit::visit_expr_call(self, node);
    }
}

fn collect_calls(f: &ItemFn) -> Vec<CallRef> {
    let mut collector = CallCollector { calls: Vec::new() };
    collector.visit_block(&f.block);
    collector.calls
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
        assert_eq!(scan.impls[0].type_name, "S");
        assert_eq!(scan.impls[0].trait_name, "T");
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
            .any(|c| c.segments == vec!["helper".to_string()]));
        assert!(caller
            .calls
            .iter()
            .any(|c| c.segments == vec!["other", "path", "nested_call"]));
        assert!(!caller
            .calls
            .iter()
            .any(|c| c.segments.last().map(String::as_str) == Some("method_call")));
    }

    #[test]
    fn invalid_syntax_is_a_reported_error_not_a_silent_empty_scan() {
        let err = scan_rust_source("fn f( {{{ not valid rust").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
