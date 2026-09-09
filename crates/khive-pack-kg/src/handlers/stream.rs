//! Thin adapters: the runtime owns stream semantics on every transport.
use khive_runtime::{
    NamespaceToken, RuntimeError, StreamAppendSpec, StreamBatchMember, VerbRegistry,
};
use khive_types::{Details, KhiveError};
use serde::Deserialize;
use serde_json::{json, Value};

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchParams {
    ops: Vec<Value>,
    fence: Option<Value>,
    observed: Option<Value>,
    atomic: Option<bool>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendMember {
    #[serde(rename = "op")]
    _op: String,
    stream: String,
    record: Value,
    expected_seq: Option<i64>,
}

/// The keyed-note surface a fence, an observation set and the write member
/// need; named in every refusal until it lands on this server.
const MISSING_KEYED_SURFACE: &str =
    "requires versioned keyed notes (expected_version), which this server does not carry yet";

/// A member's own refusal: the ADR-172 error shape with its discriminator and,
/// in atomic mode, the list index of the member that earned it.
fn member_refusal(
    error: KhiveError,
    mut pairs: Vec<(&'static str, String)>,
    member: Option<usize>,
) -> StreamBatchMember {
    if let Some(member) = member {
        pairs.push(("member", member.to_string()));
    }
    StreamBatchMember::Refused(error.with_details(Details::new_owned(pairs)))
}

impl KgPack {
    pub(crate) async fn handle_stream_append(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Presence matters: explicit null is a supplied fence, and a JSON null
        // record is valid while a missing record is not.
        if params.get("fence").is_some() {
            return Err(RuntimeError::InvalidInput(
                "stream.append fence is reserved for a later slice with versioned leases".into(),
            ));
        }
        if params.get("record").is_none() {
            return Err(RuntimeError::InvalidInput(
                "stream.append requires record (any JSON value, including null)".into(),
            ));
        }
        let p: AppendParams = deser(params)?;
        let kind = canonical_note_kind(p.note_kind.as_deref().unwrap_or("observation"), registry)?;
        self.runtime
            .stream_append(token, &p.stream, &p.record, p.expected_seq, &kind, p.tags)
            .await
    }

    pub(crate) async fn handle_stream_read(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ReadParams = deser(params)?;
        self.runtime
            .stream_read(
                token,
                &p.stream,
                p.after.unwrap_or(0),
                p.limit.unwrap_or(1000),
            )
            .await
    }

    pub(crate) async fn handle_stream_stat(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: StatParams = deser(params)?;
        self.runtime.stream_stat(token, &p.stream).await
    }

    /// Both modes validate every member's shape before anything is written;
    /// the mode decides whether a member's own refusal stops the batch.
    pub(crate) async fn handle_stream_batch(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: BatchParams = deser(params)?;
        // A JSON null fence or observation set is the same as none: unlike
        // stream.append, the batch defaults its mode by presence.
        let fence = p.fence.filter(|value| !value.is_null());
        let observed = p.observed.filter(|value| !value.is_null());
        let atomic = p.atomic.unwrap_or(fence.is_some());
        if fence.is_some() && !atomic {
            return Err(RuntimeError::InvalidInput(
                "stream.batch atomic=false cannot carry a fence: a fence admits no partial commit"
                    .into(),
            ));
        }
        if observed.is_some() && !atomic {
            return Err(RuntimeError::InvalidInput(
                "stream.batch observed requires atomic mode: an observation set is a precondition for one transaction"
                    .into(),
            ));
        }
        if fence.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "stream.batch fence {MISSING_KEYED_SURFACE}"
            )));
        }
        if observed.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "stream.batch observed {MISSING_KEYED_SURFACE}"
            )));
        }
        if p.ops.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "stream.batch requires at least one member: an empty ops list takes the writer for a batch that writes nothing".into(),
            ));
        }
        let note_kind = canonical_note_kind("observation", registry)?;
        let mut members = Vec::with_capacity(p.ops.len());
        for (index, member) in p.ops.into_iter().enumerate() {
            let Some(op) = member.get("op").and_then(Value::as_str) else {
                return Err(RuntimeError::InvalidInput(format!(
                    "stream.batch member {index} must be an object with an op string"
                )));
            };
            let placed = atomic.then_some(index);
            members.push(match op {
                "append" => {
                    if member.get("record").is_none() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "stream.batch member {index} requires record (any JSON value, including null)"
                        )));
                    }
                    let m: AppendMember = deser(member).map_err(|e| {
                        RuntimeError::InvalidInput(format!("stream.batch member {index}: {e}"))
                    })?;
                    StreamBatchMember::Append(StreamAppendSpec {
                        stream: m.stream,
                        record: m.record,
                        expected_seq: m.expected_seq,
                        note_kind: note_kind.clone(),
                        tags: None,
                    })
                }
                "write" => member_refusal(
                    KhiveError::invalid_input(format!(
                        "stream.batch write member {MISSING_KEYED_SURFACE}"
                    )),
                    vec![
                        ("reason", "member_unavailable".into()),
                        ("op", "write".into()),
                    ],
                    placed,
                ),
                other => member_refusal(
                    KhiveError::conflict("stream.batch member names no member operation"),
                    vec![("reason", "unknown_op".into()), ("op", other.into())],
                    placed,
                ),
            });
        }
        let results = if atomic {
            match self.runtime.stream_batch_atomic(token, members).await? {
                Ok(results) => results,
                Err(refusal) => return Err(refusal.error.into()),
            }
        } else {
            self.runtime.stream_batch_per_member(token, members).await?
        };
        Ok(json!({"results": results, "committed": true}))
    }
}
