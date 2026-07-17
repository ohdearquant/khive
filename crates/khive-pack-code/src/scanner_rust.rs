//! Rust Scanner (ADR-085 Amendment 2 B2): syntax-only declaration extraction
//! via `syn`. No type-checking, no compilation — a declaration's doc comment
//! is transcribed verbatim (ADR-069 D5, "transcribe, do not invent"); nothing
//! is synthesized.
//!
//! Scope of this slice: top-level items, inline `mod { .. }` blocks (recursed
//! into, at nested module paths — finding-3a), `impl` methods (named
//! `Type::method` — finding-3b), and trait default-body methods (named
//! `Trait::method` — finding-3c). A `mod foo;` file-backed module declaration
//! has no inline content to recurse into — that file is discovered and
//! scanned independently by the ingest pipeline's own file walk.

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
    /// file's own module root (finding-3a): empty at top level, `["inner"]`
    /// inside `mod inner { .. }`.
    pub module_segments: Vec<String>,
}

/// A syntactically resolvable `impl Trait for Type` relationship (D3 rule 13:
/// `datatype implements interface`). Both names are unqualified idents; the
/// caller resolves them against the project's own declaration set.
#[derive(Debug, Clone)]
pub(crate) struct RustImplRelation {
    pub type_name: String,
    pub trait_name: String,
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

fn type_name_of(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
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
        }),
        Item::Struct(s) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: s.ident.to_string(),
            doc: doc_from_attrs(&s.attrs),
            span_text: span_text(s),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
        }),
        Item::Enum(e) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: e.ident.to_string(),
            doc: doc_from_attrs(&e.attrs),
            span_text: span_text(e),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
        }),
        Item::Type(t) => out.declarations.push(RustDeclaration {
            kind: RustDeclKind::Datatype,
            name: t.ident.to_string(),
            doc: doc_from_attrs(&t.attrs),
            span_text: span_text(t),
            calls: Vec::new(),
            module_segments: module_segments.to_vec(),
        }),
        Item::Trait(tr) => {
            out.declarations.push(RustDeclaration {
                kind: RustDeclKind::Interface,
                name: tr.ident.to_string(),
                doc: doc_from_attrs(&tr.attrs),
                span_text: span_text(tr),
                calls: Vec::new(),
                module_segments: module_segments.to_vec(),
            });
            // Trait default-body methods (finding-3c): a signature-only
            // trait method has no body to hash or scan calls from, so only
            // methods carrying a default implementation become declarations.
            let trait_name = tr.ident.to_string();
            for trait_item in &tr.items {
                if let syn::TraitItem::Fn(m) = trait_item {
                    if let Some(block) = &m.default {
                        out.declarations.push(RustDeclaration {
                            kind: RustDeclKind::Function,
                            name: format!("{trait_name}::{}", m.sig.ident),
                            doc: doc_from_attrs(&m.attrs),
                            span_text: span_text(m),
                            calls: collect_calls(block),
                            module_segments: module_segments.to_vec(),
                        });
                    }
                }
            }
        }
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
                        module_segments: module_segments.to_vec(),
                    });
                }
            }
            // Methods (finding-3b): both inherent and trait impls, named
            // `Type::method`. Call edges from a method use the enclosing
            // module (not the type) as caller module path — achieved simply
            // by not nesting `module_segments` under the type name.
            if let Some(type_name) = type_name_of(&imp.self_ty) {
                for impl_item in &imp.items {
                    if let syn::ImplItem::Fn(m) = impl_item {
                        out.declarations.push(RustDeclaration {
                            kind: RustDeclKind::Function,
                            name: format!("{type_name}::{}", m.sig.ident),
                            doc: doc_from_attrs(&m.attrs),
                            span_text: span_text(m),
                            calls: collect_calls(&m.block),
                            module_segments: module_segments.to_vec(),
                        });
                    }
                }
            }
        }
        Item::Mod(m) => {
            // `mod foo { .. }` with inline content only (finding-3a);
            // `mod foo;` (file-backed) has no `content` and is discovered by
            // the ingest pipeline's own per-file walk instead.
            if let Some((_, items)) = &m.content {
                let name = m.ident.to_string();
                out.declarations.push(RustDeclaration {
                    kind: RustDeclKind::Module,
                    name: name.clone(),
                    doc: doc_from_attrs(&m.attrs),
                    span_text: span_text(m),
                    calls: Vec::new(),
                    module_segments: module_segments.to_vec(),
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

    /// finding-3: inherent + trait impl methods become `Type::method`
    /// declarations; trait default bodies become `Trait::method`.
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
        assert!(names.contains(&"S::required"));
        assert!(
            !names.contains(&"T::required"),
            "signature-only trait method must not become a declaration"
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

    /// finding-3a: inline `mod inner { .. }` recurses at a nested module
    /// path; a module declaration itself is emitted too.
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

    /// finding-3d: impls inside inline modules compose both rules.
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
