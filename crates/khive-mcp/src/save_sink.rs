//! File sink for `request` results — writes JSONL and returns a self-describing manifest.
//!
//! Why the manifest matters: a sink that self-reports null counts catches bulk export
//! corruption (e.g. `content=null` across 10 000 rows) in one second rather than after
//! a downstream agent fleet has graded blind.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Write `results_envelope` as JSONL to `path` and return the self-describing manifest.
///
/// Layout of `results_envelope`:
/// ```json
/// { "results": [ {"ok": bool, "tool": str, "result": ...}, ... ], "summary": {...} }
/// ```
///
/// Each entry in `results` becomes one line of JSONL. The manifest returned is:
/// ```json
/// {
///   "path": "<abs path>",
///   "rows": <N>,
///   "per_column_null_counts": { "<field>": <null_count>, ... },
///   "schema_fingerprint": "<sha256 of sorted field names>",
///   "checksum": "<sha256 of file bytes>"
/// }
/// ```
///
/// Errors are propagated as `anyhow::Error` so callers can convert to their preferred
/// error type (`McpError::internal_error` on the MCP path; `anyhow::bail!` on the CLI path).
pub fn write_and_manifest(results_envelope: &Value, path: &Path) -> anyhow::Result<Value> {
    // Collect per-op rows from the results array.
    let results_arr = results_envelope
        .get("results")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    // Serialize to JSONL: one line per row entry.
    let mut jsonl_bytes: Vec<u8> = Vec::new();
    for row in results_arr {
        let line =
            serde_json::to_vec(row).map_err(|e| anyhow::anyhow!("serialize result row: {e}"))?;
        jsonl_bytes.extend_from_slice(&line);
        jsonl_bytes.push(b'\n');
    }

    // Write atomically via tmp+rename when the target is on the same filesystem.
    // Falls back to direct write when the rename crosses filesystems.
    write_atomic(path, &jsonl_bytes)?;

    // Compute manifest fields.
    let rows = results_arr.len();

    // Per-column null counts and schema: inspect each `.result` value.
    // Only object-shaped results contribute to the column schema; scalar /
    // array results contribute nothing (they have no named fields).
    let mut null_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut seen_fields: BTreeSet<String> = BTreeSet::new();

    for row in results_arr {
        if let Some(Value::Object(obj)) = row.get("result") {
            for (key, val) in obj {
                seen_fields.insert(key.clone());
                if val.is_null() {
                    *null_counts.entry(key.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    // Schema fingerprint: sha256 of sorted field names joined by "|".
    let schema_input = seen_fields.iter().cloned().collect::<Vec<_>>().join("|");
    let schema_fingerprint = hex_sha256(schema_input.as_bytes());

    // File checksum: sha256 of JSONL bytes.
    let checksum = hex_sha256(&jsonl_bytes);

    // Canonical absolute path for the manifest.
    let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Convert per_column_null_counts to a JSON object.
    let null_counts_val: Value = serde_json::to_value(&null_counts).unwrap_or_else(|_| json!({}));

    Ok(json!({
        "path": abs_path.to_string_lossy(),
        "rows": rows,
        "per_column_null_counts": null_counts_val,
        "schema_fingerprint": schema_fingerprint,
        "checksum": checksum,
    }))
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Write `data` to `path` using a tmp+rename strategy when possible.
fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    // If the parent doesn't exist, create it.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("create parent dir {}: {e}", parent.display()))?;
        }
    }

    // Try tmp+rename for atomicity.
    // Build the tmp extension without a leading dot so extensionless paths
    // (e.g. `/tmp/outfile`) don't produce a double-dot (`outfile..tmp`).
    let tmp_ext = match path.extension() {
        Some(e) => format!("{}.tmp", e.to_string_lossy()),
        None => "tmp".to_string(),
    };
    let tmp_path = path.with_extension(tmp_ext);
    if std::fs::write(&tmp_path, data).is_ok() {
        if std::fs::rename(&tmp_path, path).is_ok() {
            return Ok(());
        }
        // Rename failed (cross-filesystem); clean up tmp and fall through.
        let _ = std::fs::remove_file(&tmp_path);
    }

    // Direct write fallback.
    std::fs::write(path, data).map_err(|e| anyhow::anyhow!("write file {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn make_envelope(results: Vec<Value>) -> Value {
        let total = results.len();
        let succeeded = results
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            .count();
        let failed = total - succeeded;
        json!({
            "results": results,
            "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": 0 }
        })
    }

    #[test]
    fn writes_jsonl_and_manifest_fields_correct() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.jsonl");

        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "stats", "result": { "entities": 5, "notes": null } }),
            json!({ "ok": true, "tool": "list",  "result": { "entities": 3, "notes": 2 } }),
        ]);

        let manifest = write_and_manifest(&envelope, &path).unwrap();

        // File must exist.
        assert!(path.exists());

        // rows == number of result entries.
        assert_eq!(manifest["rows"], json!(2));

        // `notes` is null in one row → null count 1.
        assert_eq!(manifest["per_column_null_counts"]["notes"], json!(1));
        // `entities` has no nulls → absent from null counts.
        assert!(manifest["per_column_null_counts"]["entities"].is_null());

        // schema_fingerprint is a non-empty hex string.
        let fp = manifest["schema_fingerprint"].as_str().unwrap();
        assert!(!fp.is_empty());
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));

        // checksum is a non-empty hex string.
        let ck = manifest["checksum"].as_str().unwrap();
        assert!(!ck.is_empty());
        assert!(ck.chars().all(|c| c.is_ascii_hexdigit()));

        // checksum matches sha256 of file content.
        let file_bytes = std::fs::read(&path).unwrap();
        let expected_ck = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&file_bytes);
            format!("{:x}", h.finalize())
        };
        assert_eq!(ck, expected_ck);

        // JSONL: 2 lines.
        let content = String::from_utf8(file_bytes).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line parses as JSON.
        for line in &lines {
            serde_json::from_str::<Value>(line).expect("valid JSON line");
        }
    }

    #[test]
    fn empty_results_produces_valid_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        let envelope = make_envelope(vec![]);
        let manifest = write_and_manifest(&envelope, &path).unwrap();

        assert_eq!(manifest["rows"], json!(0));
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn checksum_stable_across_calls() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stable.jsonl");

        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "get", "result": { "id": "abc", "name": "foo" } }),
        ]);

        let m1 = write_and_manifest(&envelope, &path).unwrap();
        let m2 = write_and_manifest(&envelope, &path).unwrap();
        assert_eq!(m1["checksum"], m2["checksum"]);
        assert_eq!(m1["schema_fingerprint"], m2["schema_fingerprint"]);
    }

    #[test]
    fn extensionless_target_produces_clean_tmp_path() {
        let tmp = TempDir::new().unwrap();
        // A target with no extension must not produce a `..tmp` double-dot path.
        let path = tmp.path().join("outfile");
        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "stats", "result": { "n": 1 } }),
        ]);
        // write_and_manifest must succeed (it uses write_atomic internally).
        write_and_manifest(&envelope, &path).unwrap();
        assert!(path.exists());
        // No double-dot artefact left behind.
        let double_dot = tmp.path().join("outfile..tmp");
        assert!(
            !double_dot.exists(),
            "double-dot tmp path must not be created"
        );
    }

    #[test]
    fn schema_fingerprint_differs_for_different_schemas() {
        let tmp = TempDir::new().unwrap();
        let p1 = tmp.path().join("a.jsonl");
        let p2 = tmp.path().join("b.jsonl");

        let e1 = make_envelope(vec![
            json!({ "ok": true, "tool": "t", "result": { "foo": 1 } }),
        ]);
        let e2 = make_envelope(vec![
            json!({ "ok": true, "tool": "t", "result": { "bar": 1 } }),
        ]);

        let m1 = write_and_manifest(&e1, &p1).unwrap();
        let m2 = write_and_manifest(&e2, &p2).unwrap();
        assert_ne!(m1["schema_fingerprint"], m2["schema_fingerprint"]);
    }
}
