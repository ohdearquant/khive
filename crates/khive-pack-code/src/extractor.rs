//! Shared, ontology-driven Extractor (ADR-085 Amendment 2 B2): maps
//! per-Scanner output into this ADR's D2 subtypes. One shape
//! (`ExtractedFile`) crosses every language; nothing downstream of a
//! Scanner adapter branches on source language. `from_rust_scan` is the
//! only adapter this slice ships — Python/TypeScript/Lean adapters (B2's
//! delivery order) feed the exact same `ExtractedFile` shape when they land.

use crate::scanner_rust::{RustDeclKind, RustFileScan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DeclKind {
    Function,
    Datatype,
    Interface,
    Module,
}

impl DeclKind {
    /// The D2 canonical subtype token (never an alias — ADR-085 D2/B4).
    pub(crate) fn code_token(self) -> &'static str {
        match self {
            DeclKind::Function => "function",
            DeclKind::Datatype => "datatype",
            DeclKind::Interface => "interface",
            DeclKind::Module => "module",
        }
    }

    /// Inverse of `code_token`: recovers the `DeclKind` a
    /// previously-stored entity's `entity_type` column encodes, so a
    /// declaration loaded back from the database (rather than freshly
    /// extracted) can still key into `symbol_index` correctly.
    pub(crate) fn from_code_token(token: &str) -> Option<Self> {
        match token {
            "function" => Some(DeclKind::Function),
            "datatype" => Some(DeclKind::Datatype),
            "interface" => Some(DeclKind::Interface),
            "module" => Some(DeclKind::Module),
            _ => None,
        }
    }
}

impl From<RustDeclKind> for DeclKind {
    fn from(k: RustDeclKind) -> Self {
        match k {
            RustDeclKind::Function => DeclKind::Function,
            RustDeclKind::Datatype => DeclKind::Datatype,
            RustDeclKind::Interface => DeclKind::Interface,
            RustDeclKind::Module => DeclKind::Module,
        }
    }
}

/// A call-target path as written at the call site, e.g. `["helper"]` for a
/// bare `helper()` or `["crate", "foo", "bar"]` for `crate::foo::bar()`.
/// Language-neutral: kept as raw segments rather than
/// pre-resolved, since resolving `crate`/`self`/`super` qualifiers requires
/// the calling declaration's own module path, which lives in
/// `source_ingest::resolve_call_target`, not any per-language Scanner.
/// Each Scanner adapter (e.g. `from_rust_scan`) converts its own raw
/// call-site representation into this shared shape.
#[derive(Debug, Clone)]
pub(crate) struct CallRef {
    pub segments: Vec<String>,
}

/// A language-agnostic declaration ready for D2 mapping — the Extractor's
/// only input shape.
#[derive(Debug, Clone)]
pub(crate) struct ExtractedDeclaration {
    pub kind: DeclKind,
    pub name: String,
    pub description: Option<String>,
    pub content_hash: String,
    /// Call-target paths this declaration references (D3 rule 1: `function
    /// depends_on function`). Resolution against the project's own
    /// declaration set happens in the ingest pipeline, which has the
    /// project-wide view a single file's Extractor pass does not.
    pub calls: Vec<CallRef>,
    /// Module path segments this declaration lives under, relative to the
    /// declaring file's own module root: empty for a top-level
    /// file item, `["inner"]` for an item inside `mod inner { .. }`. The
    /// ingest pipeline appends these onto the file's module path to get the
    /// declaration's full module path.
    pub module_segments: Vec<String>,
}

/// A syntactically resolvable `datatype implements interface` relationship
/// (D3 rule 13), language-agnostic once past the adapter. Both `type_path`
/// and `trait_path` are the full path as written at the impl site —
/// resolution against the project's own declaration set (which may live in
/// a different module than the impl block) happens in the ingest pipeline,
/// the same `crate`/`self`/`super`-aware resolver call resolution uses.
#[derive(Debug, Clone)]
pub(crate) struct ExtractedImpl {
    pub type_path: Vec<String>,
    pub trait_path: Vec<String>,
    /// Same convention as `ExtractedDeclaration::module_segments` — the
    /// `impl` block's own module, relative to the file's module root.
    pub module_segments: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ExtractedFile {
    pub declarations: Vec<ExtractedDeclaration>,
    pub impls: Vec<ExtractedImpl>,
}

/// FNV-1a change-detection hash — the same algorithm `source_ingest`'s L1.5
/// module `content_hash` already uses, applied here to one declaration's
/// token-stream rendering rather than a whole file. Not a security
/// boundary, purely a changed-vs-unchanged signal (B4).
pub(crate) fn fnv1a(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in content.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Rust Scanner output adapter (B2's "per-Scanner-output-shape adapter").
pub(crate) fn from_rust_scan(scan: RustFileScan) -> ExtractedFile {
    let declarations = scan
        .declarations
        .into_iter()
        .map(|d| ExtractedDeclaration {
            kind: d.kind.into(),
            name: d.name,
            description: d.doc,
            content_hash: fnv1a(&d.span_text),
            calls: d
                .calls
                .into_iter()
                .map(|segments| CallRef { segments })
                .collect(),
            module_segments: d.module_segments,
        })
        .collect();
    let impls = scan
        .impls
        .into_iter()
        .map(|i| ExtractedImpl {
            type_path: i.type_path,
            trait_path: i.trait_path,
            module_segments: i.module_segments,
        })
        .collect();
    ExtractedFile {
        declarations,
        impls,
    }
}

#[cfg(test)]
mod tests {
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
}
