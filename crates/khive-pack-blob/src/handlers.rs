//! Verb handlers for the blob pack — thin wrappers over `BlobStore`.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::BlobStore;

/// Ceiling on the size of any object this verb surface will hydrate into
/// memory, on either the write path (`blob.put`'s decoded size) or the read
/// path (`blob.get`'s fetch). Base64-encoded JSON runs roughly 33% larger
/// than the underlying bytes, so on the put side this bounds both request
/// body and in-memory blowup for one MCP call, not only the stored object.
/// Using one shared ceiling for both verbs guarantees that anything this
/// surface can store, it can also retrieve: `blob.get` checked against a
/// smaller limit than `blob.put` would strand an object callers put through
/// this very server.
const MAX_OBJECT_BYTES: u64 = 128 * 1024 * 1024;

fn blob_store(runtime: &KhiveRuntime) -> Result<Arc<dyn BlobStore>, RuntimeError> {
    runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or \
             KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}

fn required_str<'a>(params: &'a Value, field: &str, verb: &str) -> Result<&'a str, RuntimeError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{verb} requires a non-empty string field {field:?}"
            ))
        })
}

fn parse_content_ref(params: &Value, verb: &str) -> Result<ContentRef, RuntimeError> {
    let raw = required_str(params, "content_ref", verb)?;
    ContentRef::from_hex(raw)
        .map_err(|e| RuntimeError::InvalidInput(format!("{verb}: invalid content_ref: {e}")))
}

/// Strictly parse an optional `range` field into `(offset, length)`.
///
/// `range`, when present and non-null, must be a JSON object. `offset` and
/// `length`, when present, must each be a JSON unsigned integer — a string,
/// a negative number, or a float is rejected by name rather than silently
/// coerced or defaulted. Absent `range`/`offset`/`length` are the only
/// permitted omissions (offset defaults to 0, length to "through the end").
fn parse_range(params: &Value, verb: &str) -> Result<Option<(u64, Option<u64>)>, RuntimeError> {
    let range = match params.get("range") {
        None | Some(Value::Null) => return Ok(None),
        Some(range) => range,
    };
    let Value::Object(range) = range else {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: range must be a JSON object with optional offset/length, got {range}"
        )));
    };
    let offset = match range.get("offset") {
        None => 0,
        Some(v) => v.as_u64().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{verb}: range.offset must be a non-negative integer, got {v}"
            ))
        })?,
    };
    let length = match range.get("length") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{verb}: range.length must be a non-negative integer, got {v}"
            ))
        })?),
    };
    Ok(Some((offset, length)))
}

/// Digest-verify `bytes` against the ref they were stored/retrieved under.
///
/// The CAS backend never re-validates on read (`khive-db/src/stores/blob.rs`
/// trusts the filesystem), so this is the one place a bit-rotted or
/// hand-tampered object gets caught instead of silently round-tripping under
/// a mismatched digest.
fn verify_digest(bytes: &[u8], expected: &ContentRef) -> bool {
    let actual = ContentRef::from_digest_bytes(blake3::hash(bytes).as_bytes());
    &actual == expected
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
    let store = blob_store(runtime)?;

    let b64 = params.get("bytes").and_then(Value::as_str).ok_or_else(|| {
        RuntimeError::InvalidInput("blob.put requires \"bytes\" (base64)".to_string())
    })?;
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
pub(crate) async fn handle_get(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let store = blob_store(runtime)?;
    let content_ref = parse_content_ref(&params, "blob.get")?;
    let range = parse_range(&params, "blob.get")?;

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

    let bytes = store.get(&content_ref).await?;
    if !verify_digest(&bytes, &content_ref) {
        return Err(RuntimeError::Internal(format!(
            "blob.get: object stored under {content_ref} is corrupt (digest mismatch on read)"
        )));
    }

    let total_len = bytes.len();
    let (slice, range_out) = match range {
        None => (bytes.as_slice(), None),
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
    let store = blob_store(runtime)?;
    let content_ref = parse_content_ref(&params, "blob.stat")?;

    match store.size(&content_ref).await? {
        None => Ok(json!({ "content_ref": content_ref.to_string(), "exists": false })),
        Some(size) => Ok(json!({
            "content_ref": content_ref.to_string(),
            "exists": true,
            "size": size,
        })),
    }
}
