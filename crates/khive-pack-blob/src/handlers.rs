//! Verb handlers for the blob pack — thin wrappers over `BlobStore`.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::BlobStore;

/// Ceiling on a single `blob.put`'s decoded size. Base64-encoded JSON runs
/// roughly 33% larger than the underlying bytes, so this bounds both request
/// body and in-memory blowup for one MCP call, not only the stored object.
const MAX_PUT_BYTES: u64 = 128 * 1024 * 1024;

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
    let bytes = BASE64.decode(b64).map_err(|e| {
        RuntimeError::InvalidInput(format!("blob.put: \"bytes\" is not valid base64: {e}"))
    })?;
    if bytes.len() as u64 > MAX_PUT_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "blob.put: input is {} bytes, exceeding the {MAX_PUT_BYTES}-byte maximum",
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
/// capability today, so this is a slice, not a streamed range read.
pub(crate) async fn handle_get(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let store = blob_store(runtime)?;
    let content_ref = parse_content_ref(&params, "blob.get")?;

    let bytes = store.get(&content_ref).await?;
    if !verify_digest(&bytes, &content_ref) {
        return Err(RuntimeError::Internal(format!(
            "blob.get: object stored under {content_ref} is corrupt (digest mismatch on read)"
        )));
    }

    let total_len = bytes.len();
    let (slice, range_out) = match params.get("range").filter(|v| !v.is_null()) {
        None => (bytes.as_slice(), None),
        Some(range) => {
            let offset = range.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            if offset > total_len {
                return Err(RuntimeError::InvalidInput(format!(
                    "blob.get: range offset {offset} exceeds object size {total_len}"
                )));
            }
            let length = range
                .get("length")
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            let end = match length {
                Some(len) => offset.saturating_add(len).min(total_len),
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

/// `blob.stat` — existence + size, corruption-aware. `BlobStore` exposes no
/// size-only accessor (`khive-storage/src/blob.rs`), so an existing object's
/// size can only be answered by reading it in full through the existing
/// trait — this phase deliberately does not extend that trait (out of
/// scope for the phase-1 verb surface).
pub(crate) async fn handle_stat(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let store = blob_store(runtime)?;
    let content_ref = parse_content_ref(&params, "blob.stat")?;

    if !store.exists(&content_ref).await? {
        return Ok(json!({ "content_ref": content_ref.to_string(), "exists": false }));
    }

    let bytes = store.get(&content_ref).await?;
    let corrupt = !verify_digest(&bytes, &content_ref);

    Ok(json!({
        "content_ref": content_ref.to_string(),
        "exists": true,
        "size": bytes.len(),
        "corrupt": corrupt,
    }))
}
