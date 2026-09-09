//! Thin adapters: the runtime owns stream semantics on every transport.
use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde::Deserialize;
use serde_json::Value;

use super::common::{canonical_note_kind, deser};
use crate::KgPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendParams {
    stream: String,
    record: Value,
    expected_seq: Option<i64>,
    note_kind: Option<String>,
    tags: Option<Vec<String>>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    stream: String,
    after: Option<i64>,
    limit: Option<i64>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatParams {
    stream: String,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

impl KgPack {
    pub(crate) async fn handle_stream_append(&self, token: &NamespaceToken, params: Value, registry: &VerbRegistry) -> Result<Value, RuntimeError> {
        // Presence matters: explicit null is a supplied fence, and a JSON null
        // record is valid while a missing record is not.
        if params.get("fence").is_some() {
            return Err(RuntimeError::InvalidInput("stream.append fence is reserved for a later slice with versioned leases".into()));
        }
        if params.get("record").is_none() {
            return Err(RuntimeError::InvalidInput("stream.append requires record (any JSON value, including null)".into()));
        }
        let p: AppendParams = deser(params)?;
        let kind = canonical_note_kind(p.note_kind.as_deref().unwrap_or("observation"), registry)?;
        self.runtime.stream_append(token, &p.stream, &p.record, p.expected_seq, &kind, p.tags).await
    }

    pub(crate) async fn handle_stream_read(&self, token: &NamespaceToken, params: Value) -> Result<Value, RuntimeError> {
        let p: ReadParams = deser(params)?;
        self.runtime.stream_read(token, &p.stream, p.after.unwrap_or(0), p.limit.unwrap_or(1000)).await
    }

    pub(crate) async fn handle_stream_stat(&self, token: &NamespaceToken, params: Value) -> Result<Value, RuntimeError> {
        let p: StatParams = deser(params)?;
        self.runtime.stream_stat(token, &p.stream).await
    }
}
