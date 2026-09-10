//! Thin adapters: the runtime owns stream semantics on every transport.
use khive_runtime::{
    note_write::{NoteFence, NoteWriteOptions},
    NamespaceToken, RuntimeError, StreamAppendSpec, StreamBatchMember, StreamObservation,
    StreamWriteSpec, VerbRegistry,
};
use khive_types::{Details, KhiveError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

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
    #[serde(
        default,
        deserialize_with = "khive_runtime::note_write::deserialize_optional_fences"
    )]
    fence: Option<khive_runtime::note_write::NoteFences>,
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
    #[serde(
        default,
        deserialize_with = "khive_runtime::note_write::deserialize_optional_fences"
    )]
    fence: Option<khive_runtime::note_write::NoteFences>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteMember {
    #[serde(rename = "op")]
    _op: String,
    key: String,
    kind: String,
    doc: Value,
    tags: Option<Vec<String>>,
    embed: Option<bool>,
    expected_version: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservedMember {
    key: String,
    kind: String,
    version: Option<i64>,
    live_until: Option<String>,
}

fn batch_fence(value: Value, registry: &VerbRegistry) -> Result<NoteFence, RuntimeError> {
    if value.is_array() {
        return Err(RuntimeError::InvalidInput(
            "stream.batch fence requires one object; list-valued fences are a later surface".into(),
        ));
    }
    let mut fence: NoteFence = deser(value)?;
    fence.kind = canonical_note_kind(&fence.kind, registry)?;
    fence.validate()?;
    Ok(fence)
}

fn batch_observed(
    value: Value,
    registry: &VerbRegistry,
) -> Result<Vec<StreamObservation>, RuntimeError> {
    let entries: Vec<Value> = deser(value)?;
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            if entry.get("version").is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "stream.batch observed entry {index} requires version (positive integer or null)"
                )));
            }
            let entry: ObservedMember = deser(entry)?;
            if entry.live_until.is_some() && entry.version.is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "stream.batch observed entry {index}: live_until requires a positive version"
                )));
            }
            NoteWriteOptions {
                key: Some(entry.key.clone()),
                expected_version: entry.version,
                ..Default::default()
            }
            .validate()?;
            Ok(StreamObservation {
                key: entry.key,
                kind: canonical_note_kind(&entry.kind, registry)?,
                version: entry.version,
                live_until: entry.live_until,
            })
        })
        .collect()
}

fn batch_key_error(
    error: RuntimeError,
    writes: &[Option<(String, String)>],
    token: &NamespaceToken,
    registry: &VerbRegistry,
) -> RuntimeError {
    let RuntimeError::Khive(error) = error else {
        return error;
    };
    let Some(details) = error.details() else {
        return error.into();
    };
    if details.get("reason") != Some("key_conflict") {
        return error.into();
    }
    let member = details.get("member");
    let write = member
        .and_then(|index| index.parse::<usize>().ok())
        .and_then(|index| writes.get(index))
        .and_then(Option::as_ref);
    if write.is_some_and(|(kind, key)| registry.allows_note_key_disclosure(token, kind, key)) {
        return error.into();
    }
    let mut pairs = vec![("reason", "key_conflict".into())];
    if let Some(key) = details.get("key") {
        pairs.push(("key", key.to_string()));
    }
    if let Some(member) = member {
        pairs.push(("member", member.to_string()));
    }
    error.with_details(Details::new_owned(pairs)).into()
}

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
        // A null record is valid JSON, but an absent record is malformed input.
        if params.get("record").is_none() {
            return Err(RuntimeError::InvalidInput(
                "stream.append requires record (any JSON value, including null)".into(),
            ));
        }
        let p: AppendParams = deser(params)?;
        let kind = canonical_note_kind(p.note_kind.as_deref().unwrap_or("observation"), registry)?;
        self.runtime
            .stream_append(
                token,
                &p.stream,
                &p.record,
                p.expected_seq,
                &kind,
                p.tags,
                p.fence,
            )
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
        let fence = fence
            .map(|value| batch_fence(value, registry))
            .transpose()?;
        let observed = observed
            .map(|value| batch_observed(value, registry))
            .transpose()?
            .unwrap_or_default();
        if p.ops.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "stream.batch requires at least one member: an empty ops list takes the writer for a batch that writes nothing".into(),
            ));
        }
        let note_kind = canonical_note_kind("observation", registry)?;
        let mut members = Vec::with_capacity(p.ops.len());
        let mut writes = Vec::with_capacity(p.ops.len());
        let mut write_keys = HashSet::new();
        for (index, member) in p.ops.into_iter().enumerate() {
            let Some(op) = member.get("op").and_then(Value::as_str) else {
                return Err(RuntimeError::InvalidInput(format!(
                    "stream.batch member {index} must be an object with an op string"
                )));
            };
            let placed = atomic.then_some(index);
            writes.push(None);
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
                        fence: m.fence,
                    })
                }
                "write" => {
                    if member.get("doc").is_none() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "stream.batch member {index} requires doc (any JSON value, including null)"
                        )));
                    }
                    let m: WriteMember = deser(member).map_err(|error| {
                        RuntimeError::InvalidInput(format!("stream.batch member {index}: {error}"))
                    })?;
                    let kind = canonical_note_kind(&m.kind, registry)?;
                    if kind == "scheduled_event" {
                        return Err(RuntimeError::InvalidInput(
                            "scheduled_event writes require schedule verbs".into(),
                        ));
                    }
                    NoteWriteOptions {
                        key: Some(m.key.clone()),
                        expected_version: m.expected_version,
                        embed: m.embed,
                        ..Default::default()
                    }
                    .validate()?;
                    let identity = (kind.clone(), m.key.clone());
                    if !write_keys.insert(identity.clone()) {
                        return Err(RuntimeError::InvalidInput(format!(
                            "stream.batch repeats write target ({kind}, {}) at member {index}",
                            m.key
                        )));
                    }
                    writes[index] = Some(identity);
                    StreamBatchMember::Write(StreamWriteSpec {
                        key: m.key,
                        kind,
                        doc: m.doc,
                        tags: m.tags,
                        embed: m.embed,
                        expected_version: m.expected_version,
                    })
                }
                other => member_refusal(
                    KhiveError::conflict("stream.batch member names no member operation"),
                    vec![("reason", "unknown_op".into()), ("op", other.into())],
                    placed,
                ),
            });
        }
        let results = if atomic {
            match self
                .runtime
                .stream_batch_atomic(token, members, fence, observed, registry)
                .await
                .map_err(|error| batch_key_error(error, &writes, token, registry))?
            {
                Ok(results) => results,
                Err(refusal) => {
                    return Err(batch_key_error(
                        refusal.error.into(),
                        &writes,
                        token,
                        registry,
                    ));
                }
            }
        } else {
            let mut results = self
                .runtime
                .stream_batch_per_member(token, members, registry)
                .await?;
            for (result, write) in results.iter_mut().zip(&writes) {
                if result["details"]["reason"] == "key_conflict"
                    && !write.as_ref().is_some_and(|(kind, key)| {
                        registry.allows_note_key_disclosure(token, kind, key)
                    })
                {
                    if let Some(details) = result["details"].as_object_mut() {
                        details.remove("existing_id");
                    }
                }
            }
            results
        };
        Ok(json!({"results": results, "committed": true}))
    }
}
