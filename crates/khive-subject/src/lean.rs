use std::io::BufRead;

use serde::Deserialize;

use crate::{RawDecl, RawDecls, ScanError, Scanner};

/// Reads declaration-extract JSONL without retaining a corpus between calls.
#[derive(Clone, Copy, Debug, Default)]
pub struct LeanJsonlScanner;

#[derive(Deserialize)]
struct LeanDecl {
    name: String,
    kind: String,
    #[serde(rename = "type")]
    type_expr: String,
    #[serde(alias = "refs_in_corpus")]
    refs: Vec<String>,
    doc: Option<String>,
    module: String,
}

impl Scanner for LeanJsonlScanner {
    type Error = ScanError;

    fn scan<R: BufRead>(&self, mut source: R) -> Result<RawDecls, Self::Error> {
        let mut declarations = Vec::new();
        let mut text = String::new();
        let mut line = 0;
        loop {
            text.clear();
            let next_line = line + 1;
            let bytes = source
                .read_line(&mut text)
                .map_err(|source| ScanError::Read {
                    line: next_line,
                    source,
                })?;
            if bytes == 0 {
                return Ok(declarations);
            }
            line = next_line;
            let record: LeanDecl =
                serde_json::from_str(&text).map_err(|source| ScanError::Json { line, source })?;
            declarations.push(RawDecl {
                name: record.name,
                kind: record.kind,
                type_expr: record.type_expr,
                refs: record.refs,
                doc: record.doc,
                module: record.module,
            });
        }
    }
}
