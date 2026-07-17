//! Verb handlers for the blob pack — thin wrappers over `BlobStore`.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use khive_runtime::daemon::MAX_FRAME_BYTES;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::blob::ContentRef;
use khive_storage::BlobStore;
use tokio::sync::Semaphore;

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
/// Set to ADR-111's 64 MiB v1 object ceiling (`docs/adr/ADR-111-blob-store.md`
/// Amendment 2, "the existing whole-buffer trait is accepted for S3 v1 up to
/// 64 MiB per object"), matching `khive_db::stores::blob_s3::MAX_OBJECT_BYTES`
/// exactly. `FsBlobStore` enforces no ceiling of its own, so this verb-level
/// bound is what makes put/get behavior backend-independent: an object this
/// surface accepts against an `FsBlobStore` install must also fit through an
/// `S3BlobStore` install without a surprise rejection on `put`.
const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Bounds concurrent `blob.get` hydration. A single parallel batch can issue
/// up to 100 ops (ADR-016's batch cap); without a bound, each concurrent
/// `blob.get` could hydrate a full `MAX_OBJECT_BYTES` object and hold its
/// base64 encoding in memory at once. Four permits caps worst-case transient
/// memory for concurrent get hydration at roughly
/// `4 * MAX_OBJECT_BYTES` raw plus base64 blowup, independent of how many
/// `blob.get` ops land in one batch.
static GET_HYDRATION_PERMITS: Semaphore = Semaphore::const_new(4);

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
///
/// Two bounds apply before any bytes are hydrated: the requested slice
/// (computed from `size()` when no range is given, or the range length
/// otherwise) must fit under the daemon's `MAX_FRAME_BYTES` IPC cap once
/// base64-encoded (`max_returnable_raw_bytes`), and hydration itself runs
/// under `GET_HYDRATION_PERMITS` to bound worst-case concurrent memory
/// across a batch of `blob.get` ops.
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

    let _permit = GET_HYDRATION_PERMITS
        .acquire()
        .await
        .expect("GET_HYDRATION_PERMITS is never closed");
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
