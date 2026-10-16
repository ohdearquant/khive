//! Syntax and lexical observations of source-file inputs, without opening files.
//!
//! These observations are not a complete read graph, target/cfg classification,
//! or permission to omit work. Opaque macros, dynamic reads and unsupported
//! syntax remain explicit residuals for the caller to preserve.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Lit, Meta, Token};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SourceRead {
    pub kind: String,
    pub path: String,
    pub base: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SourceReadScan {
    pub reads: Vec<SourceRead>,
    pub unresolved: Vec<String>,
}

impl SourceReadScan {
    fn unresolved(&mut self, reason: &str) {
        if !self.unresolved.iter().any(|existing| existing == reason) {
            self.unresolved.push(reason.to_string());
        }
    }

    fn literal(&mut self, kind: &str, path: String, base: &str) {
        if !safe_literal_path(&path) {
            self.unresolved("unsafe, glob, or noncanonical source-read path literal");
            return;
        }
        if !self
            .reads
            .iter()
            .any(|read| read.kind == kind && read.path == path && read.base == base)
        {
            self.reads.push(SourceRead {
                kind: kind.to_string(),
                path,
                base: base.to_string(),
            });
        }
    }

    fn repository_literal(&mut self, value: &str) {
        if value.starts_with("crates/") || value.starts_with("scripts/") {
            self.literal("repo_literal", value.to_string(), "repository");
        } else if value.starts_with("../crates/")
            || value.starts_with("./crates/")
            || value.starts_with("../scripts/")
            || value.starts_with("./scripts/")
            || value.starts_with("crates\\")
            || value.starts_with("scripts\\")
        {
            self.unresolved("repository path literal requires unsupported normalization");
        }
    }
}

fn safe_literal_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('~')
        && !path.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\\' | ':' | '*' | '?' | '[' | ']' | '{' | '}')
        })
        && path.split('/').all(|component| !component.is_empty())
}

/// Extract literal observations only; no filesystem or environment access.
pub(crate) fn extract_source_reads(language: &str, content: &str) -> SourceReadScan {
    match language {
        "rust" => match syn::parse_file(content) {
            Ok(file) => {
                let mut scanner = RustReads {
                    scan: SourceReadScan::default(),
                    inline_module_depth: 0,
                };
                scanner.visit_file(&file);
                scanner.scan
            }
            Err(_) => SourceReadScan {
                reads: Vec::new(),
                unresolved: vec!["Rust source could not be parsed".to_string()],
            },
        },
        "python" => python_reads(content),
        _ => SourceReadScan {
            reads: Vec::new(),
            unresolved: vec!["source-reader language is unsupported".to_string()],
        },
    }
}

struct RustReads {
    scan: SourceReadScan,
    inline_module_depth: usize,
}

impl RustReads {
    fn module_attribute(&mut self, attr: &Attribute, external: bool) {
        if !attr.path().is_ident("path") {
            self.visit_attribute(attr);
            return;
        }
        if self.inline_module_depth != 0 || !external {
            self.scan
                .unresolved("Rust path attribute in an inline module context is unsupported");
            return;
        }
        if let Meta::NameValue(meta) = &attr.meta {
            if let Expr::Lit(value) = &meta.value {
                if let Lit::Str(path) = &value.lit {
                    self.scan.literal("path_attribute", path.value(), "file");
                    return;
                }
            }
        }
        self.scan.unresolved("Rust path attribute is not a literal");
    }
}

impl<'ast> Visit<'ast> for RustReads {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        for attr in &module.attrs {
            self.module_attribute(attr, module.content.is_none());
        }
        if let Some((_, items)) = &module.content {
            self.inline_module_depth += 1;
            for item in items {
                self.visit_item(item);
            }
            self.inline_module_depth -= 1;
        }
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if attr.path().is_ident("doc") {
            // Desugared doc comments are literal strings, not source paths.
            // Explicit #[doc = include_str!(...)] still reads its input.
            if let Meta::NameValue(meta) = &attr.meta {
                if !matches!(&meta.value, Expr::Lit(_)) {
                    self.visit_expr(&meta.value);
                }
            }
            return;
        }
        if matches!(&attr.meta, Meta::List(_)) && !attr.path().is_ident("cfg") {
            self.scan
                .unresolved("Rust attribute expansion is not source-read classified");
        }
        if attr.path().is_ident("path") || attr.path().is_ident("cfg_attr") {
            self.scan
                .unresolved("Rust path/conditional attribute context is unsupported");
            return;
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        self.scan.repository_literal(&literal.value());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let Some(name) = mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        else {
            self.scan.unresolved("Rust macro identity is unavailable");
            return;
        };
        let args = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone());
        if matches!(name.as_str(), "include" | "include_str" | "include_bytes") {
            if let Ok(args) = &args {
                if args.len() == 1 {
                    if let Some(Expr::Lit(value)) = args.first() {
                        if let Lit::Str(path) = &value.lit {
                            self.scan.literal(&name, path.value(), "file");
                            return;
                        }
                    }
                }
            }
            self.scan
                .unresolved("Rust include macro has a nonliteral or unsupported argument");
            return;
        }
        // Even expression-shaped arguments cannot prove an arbitrary macro's
        // expansion or implicit filesystem behavior. Do not claim completeness.
        self.scan
            .unresolved("Rust macro expansion is not source-read classified");
        if let Ok(args) = &args {
            for arg in args {
                visit::visit_expr(self, arg);
            }
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref() {
            if path
                .path
                .segments
                .last()
                .is_some_and(|segment| known_read_name(&segment.ident.to_string()))
            {
                self.scan
                    .unresolved("Rust filesystem/read call requires semantic resolution");
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if known_read_name(&call.method.to_string()) {
            self.scan
                .unresolved("Rust filesystem/read method requires semantic resolution");
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn known_read_name(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "read_text"
            | "read_bytes"
            | "read_to_string"
            | "read_to_end"
            | "read_exact"
            | "read_line"
            | "read_until"
            | "read_dir"
            | "read_link"
            | "open"
            | "glob"
            | "rglob"
            | "walk"
            | "walk_dir"
            | "listdir"
            | "scandir"
            | "metadata"
            | "symlink_metadata"
            | "stat"
            | "lstat"
            | "canonicalize"
            | "resolve"
            | "exists"
            | "try_exists"
            | "is_file"
            | "is_dir"
    )
}

fn python_read_calls() -> &'static Regex {
    static CALLS: OnceLock<Regex> = OnceLock::new();
    CALLS.get_or_init(|| {
        Regex::new(concat!(
            r"\b(?:read|read_text|read_bytes|read_to_string|read_to_end|read_dir|read_link|",
            r"open|glob|rglob|walk|walk_dir|listdir|scandir|stat|lstat|resolve|exists|is_file|is_dir)\s*\("
        ))
        .expect("fixed source-reader expression")
    })
}

fn python_reads(content: &str) -> SourceReadScan {
    let mut scan = SourceReadScan::default();
    scan.unresolved("Python reader scan is lexical; syntax and target roles are unverified");
    let bytes = content.as_bytes();
    let mut masked = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' {
            let end = content[index..]
                .find('\n')
                .map_or(bytes.len(), |n| index + n);
            mask_python_span(&mut masked[index..end]);
            index = end;
            continue;
        }
        let Some((quote_start, prefix)) = python_string_start(content, index) else {
            index += content[index..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let quote = bytes[quote_start];
        let width = if bytes.get(quote_start..quote_start + 3) == Some(&[quote; 3]) {
            3
        } else {
            1
        };
        let start = quote_start + width;
        let mut end = start;
        let mut closed = false;
        while end < bytes.len() {
            if bytes
                .get(end..end + width)
                .is_some_and(|part| part.iter().all(|b| *b == quote))
            {
                closed = true;
                break;
            }
            if width == 1 && matches!(bytes[end], b'\n' | b'\r') {
                break;
            }
            if bytes[end] == b'\\' {
                end += 1;
                if end == bytes.len() {
                    break;
                }
            }
            end += content[end..].chars().next().map_or(1, char::len_utf8);
        }
        let token_end = if closed { end + width } else { end };
        mask_python_span(&mut masked[index..token_end]);
        if !closed {
            scan.unresolved("Python string literal is unterminated or unsupported");
        } else if prefix.contains('f') {
            scan.unresolved("Python formatted string requires semantic resolution");
        } else if prefix.contains('b') {
            scan.unresolved("Python byte string is not a verified Unicode path");
        } else if !prefix.contains('r') && content[start..end].contains('\\') {
            scan.unresolved("Python escaped string requires literal decoding");
        } else {
            scan.repository_literal(&content[start..end]);
        }
        index = token_end;
    }
    match std::str::from_utf8(&masked) {
        Ok(masked) if python_read_calls().is_match(masked) => {
            scan.unresolved("Python filesystem/read call requires semantic resolution");
        }
        Ok(_) => {}
        Err(_) => scan.unresolved("Python lexical masking could not preserve Unicode"),
    }
    scan
}

fn mask_python_span(bytes: &mut [u8]) {
    for byte in bytes {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

fn python_string_start(content: &str, index: usize) -> Option<(usize, String)> {
    let bytes = content.as_bytes();
    if matches!(bytes[index], b'\'' | b'"') {
        return Some((index, String::new()));
    }
    if index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_') {
        return None;
    }
    let mut end = index;
    while end < bytes.len()
        && end - index < 2
        && matches!(
            bytes[end],
            b'r' | b'R' | b'u' | b'U' | b'b' | b'B' | b'f' | b'F'
        )
    {
        end += 1;
    }
    if end == index
        || !bytes
            .get(end)
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
    {
        return None;
    }
    Some((end, content[index..end].to_ascii_lowercase()))
}

#[cfg(test)]
#[path = "source_reads_tests.rs"]
mod tests;
