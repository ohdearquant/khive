//! Verb handlers for the blob pack — thin wrappers over `BlobStore`.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

use khive_runtime::daemon::MAX_FRAME_BYTES;
use khive_runtime::{BlobHydrator, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::{BlobStore, UploadId};

use crate::uploads::UploadManager;

/// Ceiling on the size of any object this verb surface will hydrate into
/// memory, on either the write path (`blob.put`'s decoded size) or the read
/// path (`blob.get`'s fetch). Base64-encoded JSON runs roughly 33% larger
/// than the underlying bytes, so on the put side this bounds both request
/// body and in-memory blowup for one MCP call, not only the stored object.
/// Using one shared ceiling for both verbs guarantees that anything this
/// surface can store, it can also retrieve: `blob.get` checked against a
/// smaller limit than `blob.put` would strand an object callers put through
/// this very server.
///
/// Set to ADR-111's 64 MiB v1 object ceiling (`docs/adr/ADR-111-blob-store.md`),
/// matching `khive_db::stores::blob_s3::MAX_OBJECT_BYTES`
/// exactly. `FsBlobStore::put` enforces no ceiling of its own, so this verb-level
/// bound is what makes put/get behavior backend-independent: an object this
/// surface accepts against an `FsBlobStore` install must also fit through an
/// `S3BlobStore` install without a surprise rejection on `put`.
pub(crate) const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Reserve for the JSON envelope around `bytes` in a `blob.get` response
/// (`content_ref`, `size`, `range`, field names, and braces/quoting) —
/// comfortably larger than its actual size (well under 200 bytes) so the
/// frame-fit check below is conservative by construction.
const RESPONSE_ENVELOPE_RESERVE_BYTES: u64 = 4096;

/// The largest raw (pre-base64) byte count a `blob.get` response can return
/// without its serialized frame exceeding the daemon's `MAX_FRAME_BYTES` IPC
/// cap (`crates/khive-runtime/src/daemon.rs`). Base64 expands 3 raw bytes
/// into 4 encoded characters, so the frame budget is scaled by 3/4 after
/// reserving room for the rest of the response envelope.
fn max_returnable_raw_bytes() -> u64 {
    let frame_budget = (MAX_FRAME_BYTES as u64).saturating_sub(RESPONSE_ENVELOPE_RESERVE_BYTES);
    frame_budget * 3 / 4
}

/// Room for upload call fields inside the request parser's ops string.
pub const REQUEST_RESERVE: u64 = 8192;

/// Maximum decoded part that fits the live parser and daemon frame budgets.
pub fn max_request_part_raw_bytes() -> u64 {
    let budget = khive_request::MAX_OPS_INPUT_LEN.min(MAX_FRAME_BYTES) as u64;
    budget.saturating_sub(REQUEST_RESERVE) * 3 / 4
}

pub(crate) fn blob_store(runtime: &KhiveRuntime) -> Result<Arc<dyn BlobStore>, RuntimeError> {
    runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or \
             KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}

fn blob_hydrator(runtime: &KhiveRuntime) -> Result<Arc<BlobHydrator>, RuntimeError> {
    runtime.blob_hydrator().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or \
             KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutParams {
    bytes: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetParams {
    content_ref: String,
    #[serde(default, deserialize_with = "deserialize_range")]
    range: Option<RangeParams>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeParams {
    #[serde(default, deserialize_with = "deserialize_range_offset")]
    offset: u64,
    #[serde(default, deserialize_with = "deserialize_range_length")]
    length: Option<u64>,
}

fn deserialize_range_offset<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value.as_u64().ok_or_else(|| {
        serde::de::Error::custom(format!(
            "range.offset must be a non-negative integer, got {value}"
        ))
    })
}

fn deserialize_range_length<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "range.length must be a non-negative integer, got {value}"
                ))
            })
        })
        .transpose()
}

fn deserialize_range<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<RangeParams>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    value
        .map(|value| {
            if !value.is_object() {
                return Err(serde::de::Error::custom(
                    "range must be a JSON object with optional offset/length",
                ));
            }
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        })
        .transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatParams {
    content_ref: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginParams {
    size: u64,
    content_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutPartParams {
    upload_id: String,
    index: u64,
    bytes: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadParams {
    upload_id: String,
}

fn parse_params<T: DeserializeOwned>(params: Value, verb: &str) -> Result<T, RuntimeError> {
    if !params.is_object() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb} arguments must be a JSON object"
        )));
    }
    serde_json::from_value(params)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid {verb} arguments: {error}")))
}

fn parse_content_ref(raw: &str, verb: &str) -> Result<ContentRef, RuntimeError> {
    ContentRef::from_hex(raw)
        .map_err(|e| RuntimeError::InvalidInput(format!("{verb}: invalid content_ref: {e}")))
}

fn parse_upload_id(raw: &str, verb: &str) -> Result<UploadId, RuntimeError> {
    UploadId::from_hex(raw).map_err(|error| RuntimeError::InvalidInput(format!("{verb}: {error}")))
}

pub(crate) async fn handle_begin(
    uploads: &UploadManager,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let BeginParams { size, content_ref } = parse_params(params, "blob.begin")?;
    let reference = content_ref
        .as_deref()
        .map(|raw| parse_content_ref(raw, "blob.begin"))
        .transpose()?;
    uploads
        .begin(
            size,
            reference,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .await
}

pub(crate) async fn handle_put_part(
    uploads: &UploadManager,
    params: Value,
) -> Result<Value, RuntimeError> {
    let PutPartParams {
        upload_id,
        index,
        bytes: b64,
    } = parse_params(params, "blob.put_part")?;
    let id = parse_upload_id(&upload_id, "blob.put_part")?;
    let limit = max_request_part_raw_bytes();
    // Permit one extra decoded byte so the boundary is decided on raw
    // length; bound larger inputs before allocating a decoded buffer.
    if b64.len() as u64 > limit.saturating_mul(4) / 3 + 4 {
        // Validate without allocating the oversized decoded object. Only
        // the final quartet may contain padding; decode it with the same
        // engine to check canonical padding bits and obtain the exact size.
        let encoded = b64.as_bytes();
        if encoded.len() % 4 != 0 {
            return Err(RuntimeError::InvalidInput(
                "blob.put_part: invalid base64 length".into(),
            ));
        }
        let (prefix, suffix) = encoded.split_at(encoded.len() - 4);
        if !prefix
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/'))
        {
            return Err(RuntimeError::InvalidInput(
                "blob.put_part: invalid base64 alphabet or padding".into(),
            ));
        }
        let mut tail = [0; 3];
        let tail_len = BASE64.decode_slice(suffix, &mut tail).map_err(|error| {
            RuntimeError::InvalidInput(format!("blob.put_part: invalid base64: {error}"))
        })?;
        let decoded_len = (prefix.len() / 4 * 3 + tail_len) as u64;
        return uploads.reject_oversized_part(&id, index, decoded_len).await;
    }
    let bytes = BASE64.decode(b64).map_err(|error| {
        RuntimeError::InvalidInput(format!("blob.put_part: invalid base64: {error}"))
    })?;
    uploads.put_part(&id, index, bytes).await
}

pub(crate) async fn handle_commit(
    uploads: &UploadManager,
    params: Value,
) -> Result<Value, RuntimeError> {
    let UploadParams { upload_id } = parse_params(params, "blob.commit")?;
    uploads
        .commit(&parse_upload_id(&upload_id, "blob.commit")?)
        .await
}

pub(crate) async fn handle_abort(
    uploads: &UploadManager,
    params: Value,
) -> Result<Value, RuntimeError> {
    let UploadParams { upload_id } = parse_params(params, "blob.abort")?;
    uploads
        .abort(&parse_upload_id(&upload_id, "blob.abort")?)
        .await
}

/// `blob.put` — store `bytes` (base64), returning the resulting `ContentRef`.
/// The `bytes` field is required; a missing or non-string value is `InvalidInput`.
///
/// This verb does not accept a server-local file path: reading an arbitrary
/// path on the server host would be an exfiltration surface for any caller
/// reaching the verb. Callers that want to store a file read it themselves and
/// pass the base64 bytes.
pub(crate) async fn handle_put(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let PutParams { bytes: b64 } = parse_params(params, "blob.put")?;
    if runtime.is_read_only() {
        return Err(RuntimeError::InvalidInput(
            "blob.put is unavailable because the blob pack runtime is read-only".to_string(),
        ));
    }
    let store = blob_store(runtime)?;

    // Bound the decode before allocating: 4 base64 chars encode 3 bytes, so an
    // input longer than MAX_OBJECT_BYTES * 4/3 cannot fit under the ceiling. Reject
    // an oversized put here rather than materializing it in memory first.
    let max_b64_len = MAX_OBJECT_BYTES.saturating_mul(4) / 3 + 4;
    if b64.len() as u64 > max_b64_len {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.put: base64 input is {} chars, exceeding the {MAX_OBJECT_BYTES}-byte ceiling",
            b64.len()
        )));
    }
    let bytes = BASE64.decode(b64).map_err(|e| {
        RuntimeError::InvalidInput(format!("blob.put: \"bytes\" is not valid base64: {e}"))
    })?;
    if bytes.len() as u64 > MAX_OBJECT_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.put: input is {} bytes, exceeding the {MAX_OBJECT_BYTES}-byte maximum",
            bytes.len()
        )));
    }

    let size = bytes.len();
    let content_ref = store.put(bytes).await?;
    Ok(json!({ "content_ref": content_ref.to_string(), "size": size }))
}

/// `blob.get` — fetch an object by `content_ref`, base64-encoded in the
/// response, with an optional `{offset, length}` `range`. The range is
/// applied to the fully fetched object: `BlobStore` has no partial-read
/// capability today, so this is a slice, not a streamed range read — bounded
/// by `MAX_OBJECT_BYTES`, so any object this verb surface can store, it can
/// also retrieve.
///
/// Two bounds apply before any bytes are hydrated: the requested slice
/// (computed from `size()` when no range is given, or the range length
/// otherwise) must fit under the daemon's `MAX_FRAME_BYTES` IPC cap once
/// base64-encoded (`max_returnable_raw_bytes`), and complete-object hydration
/// enters the runtime's shared weighted raw-byte admission.
pub(crate) async fn handle_get(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let GetParams { content_ref, range } = parse_params(params, "blob.get")?;
    let content_ref = parse_content_ref(&content_ref, "blob.get")?;
    let range = range.map(|range| (range.offset, range.length));
    let store = blob_store(runtime)?;
    let hydrator = blob_hydrator(runtime)?;

    let size = store.size(&content_ref).await?.ok_or_else(|| {
        RuntimeError::NotFound(format!(
            "blob.get: no object stored under content_ref {content_ref}"
        ))
    })?;
    if size > MAX_OBJECT_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.get: object stored under {content_ref} is {size} bytes, exceeding the \
             {MAX_OBJECT_BYTES}-byte maximum this verb will hydrate"
        )));
    }
    if let Some((offset, _)) = range {
        if offset > size {
            return Err(RuntimeError::InvalidInput(format!(
                "blob.get: range offset {offset} exceeds object size {size}"
            )));
        }
    }
    let requested_len = match range {
        None => size,
        Some((offset, length)) => match length {
            Some(len) => len.min(size - offset),
            None => size - offset,
        },
    };
    let max_returnable = max_returnable_raw_bytes();
    if requested_len > max_returnable {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.get: requested slice of {requested_len} bytes would base64-encode to a \
             response exceeding the {MAX_FRAME_BYTES}-byte daemon frame cap ({max_returnable} \
             raw bytes max); pass a smaller range"
        )));
    }

    // `size` was checked against the verb ceiling above. Reserve the bytes
    // this immutable content-addressed object actually needs, not 64 MiB for
    // every tiny read; the verified hydrator still rejects a larger payload.
    let verified = hydrator.hydrate_verified(&content_ref, size).await?;
    let bytes = verified.bytes();
    let total_len = bytes.len();
    let (slice, range_out) = match range {
        None => (bytes, None),
        Some((offset, length)) => {
            let offset = offset as usize;
            if offset > total_len {
                return Err(RuntimeError::InvalidInput(format!(
                    "blob.get: range offset {offset} exceeds object size {total_len}"
                )));
            }
            let end = match length {
                Some(len) => offset.saturating_add(len as usize).min(total_len),
                None => total_len,
            };
            (
                &bytes[offset..end],
                Some(json!({ "offset": offset, "length": end - offset })),
            )
        }
    };

    if slice.len() as u64 > max_returnable {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.get: requested slice of {} bytes would base64-encode to a response exceeding \
             the {MAX_FRAME_BYTES}-byte daemon frame cap ({max_returnable} raw bytes max); pass \
             a smaller range",
            slice.len()
        )));
    }

    let mut out = json!({
        "content_ref": content_ref.to_string(),
        "bytes": BASE64.encode(slice),
        "size": total_len,
    });
    if let Some(range_out) = range_out {
        out["range"] = range_out;
    }
    Ok(out)
}

/// `blob.stat` — existence and size only, answered by `BlobStore::size` with
/// no object bytes ever hydrated. Digest verification is deliberately left to
/// `blob.get`'s read path, where the bytes are already in memory to serve —
/// `stat` never reads the object, so it has nothing to verify a digest
/// against.
pub(crate) async fn handle_stat(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let StatParams { content_ref } = parse_params(params, "blob.stat")?;
    let content_ref = parse_content_ref(&content_ref, "blob.stat")?;
    let store = blob_store(runtime)?;

    match store.size(&content_ref).await? {
        None => Ok(json!({ "content_ref": content_ref.to_string(), "exists": false })),
        Some(size) => Ok(json!({
            "content_ref": content_ref.to_string(),
            "exists": true,
            "size": size,
        })),
    }
}
