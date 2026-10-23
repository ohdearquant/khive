//! The charter language (ADR-208): sources compile against a phrase registry into canonical,
//! digest-pinned definitions. The language names phrases; it never expresses predicates.

pub mod ast;
mod check;
pub mod definition;
mod lex;
mod parse;
pub mod registry;

use serde::Serialize;
use serde_json::Value;

pub use check::CompiledCharter;
pub use registry::Registry;

/// One refusal or parse error, located by 1-based source line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub line: usize,
    pub code: &'static str,
    pub message: String,
}

impl Diagnostic {
    pub fn new(line: usize, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            line,
            code,
            message: message.into(),
        }
    }
}

pub fn parse_charter_source(source: &str) -> Result<ast::CharterSource, Vec<Diagnostic>> {
    let nodes = lex::tree(lex::lex(source)?)?;
    parse::parse_charter(&nodes)
}

pub fn parse_vocabulary_source(source: &str) -> Result<ast::VocabularySource, Vec<Diagnostic>> {
    let nodes = lex::tree(lex::lex(source)?)?;
    parse::parse_vocabulary(&nodes)
}

/// Parse and validate a charter source against `registry`. Writes nothing.
pub fn compile(source: &str, registry: &Registry) -> Result<CompiledCharter, Vec<Diagnostic>> {
    let charter = parse_charter_source(source)?;
    check::check(&charter, registry)
}

/// JSON with object keys sorted at every level, independent of serde_json's map ordering.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String((*key).clone()).to_string());
                    out.push(':');
                    write(&map[*key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            scalar => out.push_str(&scalar.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out.into_bytes()
}

pub fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
