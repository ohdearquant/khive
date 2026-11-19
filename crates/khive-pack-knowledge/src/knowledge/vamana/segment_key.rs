use khive_runtime::KhiveRuntime;

/// Namespace key used in `retrieval_snapshots` for a given ns+model pair.
pub(crate) fn snapshot_key(namespace: &str, model: &str) -> String {
    format!("{namespace}::vamana::{model}")
}

/// Filesystem directory for v2 Vamana segment files for a given `(ns, model)` pair.
///
/// Returns `Some(<db-file>.ann/<hex>)` where `<hex>` is the lowercase hex encoding of
/// the bytes of `snapshot_key(ns, model)`, rooted beside the backing database file
/// (`backend_ann_root`) so co-located databases can never adopt each other's segments.
/// Hex encoding is injective, filesystem-safe, and reversible via
/// `decode_ann_dir_name`. Returns `None` for in-memory backends.
pub(super) fn ann_segment_dir(
    rt: &KhiveRuntime,
    ns: &str,
    model: &str,
) -> Option<std::path::PathBuf> {
    let ann_root = rt.backend_ann_root()?;
    Some(ann_segment_dir_from_root(&ann_root, ns, model))
}

pub(super) fn ann_segment_dir_from_root(
    ann_root: &std::path::Path,
    ns: &str,
    model: &str,
) -> std::path::PathBuf {
    let key = snapshot_key(ns, model);
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    ann_root.join(hex)
}

/// Decode a hex-encoded ann directory name back to `(namespace, model)`.
///
/// Reverses the encoding done by `ann_segment_dir`: hex-decodes `name` to bytes,
/// interprets them as UTF-8, then splits on `"::vamana::"`. Returns `None` on bad
/// hex, non-UTF-8 bytes, a missing separator, or empty namespace/model parts.
pub(super) fn decode_ann_dir_name(name: &str) -> Option<(String, String)> {
    let raw = name.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    // `as_chunks` is unstable on stable; keep `chunks_exact` until it lands.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    for pair in raw.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        bytes.push((hi * 16 + lo) as u8);
    }
    let key = String::from_utf8(bytes).ok()?;
    let (ns, model) = key.split_once("::vamana::")?;
    if ns.is_empty() || model.is_empty() {
        return None;
    }
    Some((ns.to_string(), model.to_string()))
}
