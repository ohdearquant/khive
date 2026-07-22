//! File sink for `request` results — writes JSONL and returns a self-describing manifest.
//!
//! See `crates/khive-mcp/docs/save-sink.md` for why the manifest self-reports
//! null counts and why `save_to` is treated as an untrusted, client-supplied
//! filesystem path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Environment override for the allowed `save_to` export root.
const EXPORT_ROOT_ENV: &str = "KHIVE_SAVE_TO_ROOT";

/// Resolve (and create) the allowed export root for `save_to` destinations.
/// Defaults to `~/.khive/exports`; overridable via `KHIVE_SAVE_TO_ROOT`.
fn export_root() -> anyhow::Result<PathBuf> {
    let root = match std::env::var(EXPORT_ROOT_ENV) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".khive").join("exports")
        }
    };
    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("create export root {}: {e}", root.display()))?;
    root.canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize export root {}: {e}", root.display()))
}

/// Validate a client-supplied `save_to` path against the allowed export `root`
/// and return the canonicalized destination. Rejects `..` traversal, a
/// resolved parent outside `root`, and an existing symlink at the
/// destination. See `crates/khive-mcp/docs/save-sink.md`.
fn validate_destination(root: &Path, requested: &Path) -> anyhow::Result<PathBuf> {
    if requested.as_os_str().is_empty() {
        anyhow::bail!("save_to path must not be empty");
    }
    if requested
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "save_to path must not contain '..' traversal components: {}",
            requested.display()
        );
    }

    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let parent = joined.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(p) => p,
        None => anyhow::bail!("save_to path has no parent directory: {}", joined.display()),
    };

    // Containment must be proven BEFORE any directory creation: walk up to the
    // deepest existing ancestor and canonicalize that. `..` components were
    // already rejected above, so the not-yet-existing suffix can only descend
    // beneath the ancestor — if the ancestor is inside the root, the parent is.
    let mut existing = parent;
    while !existing.exists() {
        existing = match existing.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => p,
            None => anyhow::bail!(
                "save_to path has no existing ancestor: {}",
                joined.display()
            ),
        };
    }
    let canonical_existing = existing.canonicalize().map_err(|e| {
        anyhow::anyhow!("canonicalize save_to ancestor {}: {e}", existing.display())
    })?;
    if !canonical_existing.starts_with(root) {
        anyhow::bail!(
            "save_to path escapes the allowed export root ({}): {}",
            root.display(),
            joined.display()
        );
    }

    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("create save_to parent dir {}: {e}", parent.display()))?;

    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize save_to parent {}: {e}", parent.display()))?;

    if !canonical_parent.starts_with(root) {
        anyhow::bail!(
            "save_to path escapes the allowed export root ({}): {}",
            root.display(),
            joined.display()
        );
    }

    let file_name = joined
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("save_to path has no file name: {}", joined.display()))?;
    let dest = canonical_parent.join(file_name);

    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "save_to destination must not be a symlink: {}",
                dest.display()
            );
        }
    }

    Ok(dest)
}

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
/// `restrict_to_export_root` gates the destination policy (root containment,
/// `..` traversal rejection, symlink-destination rejection): `true` for the
/// agent-facing MCP `request` tool, where `path` is a client-supplied string
/// reaching the filesystem; `false` for the trusted operator CLI path
/// (`kkernel exec --save-file`, `from_wire = false`), which may write anywhere
/// the operator points it, matching its documented behavior.
///
/// Errors are propagated as `anyhow::Error` so callers can convert to their preferred
/// error type (`McpError::internal_error` on the MCP path; `anyhow::bail!` on the CLI path).
pub fn write_and_manifest(
    results_envelope: &Value,
    path: &Path,
    restrict_to_export_root: bool,
) -> anyhow::Result<Value> {
    let dest = if restrict_to_export_root {
        let root = export_root()?;
        validate_destination(&root, path)?
    } else {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create parent dir {}: {e}", parent.display()))?;
            }
        }
        path.to_path_buf()
    };
    let path = dest.as_path();

    let results_arr = results_envelope
        .get("results")
        .and_then(Value::as_array)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let mut jsonl_bytes: Vec<u8> = Vec::new();
    for row in results_arr {
        let line =
            serde_json::to_vec(row).map_err(|e| anyhow::anyhow!("serialize result row: {e}"))?;
        jsonl_bytes.extend_from_slice(&line);
        jsonl_bytes.push(b'\n');
    }

    write_atomic(path, &jsonl_bytes)?;

    let rows = results_arr.len();

    // Only object-shaped `.result` values contribute to the column schema;
    // scalar/array results have no named fields to count.
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

    let schema_input = seen_fields.iter().cloned().collect::<Vec<_>>().join("|");
    let schema_fingerprint = hex_sha256(schema_input.as_bytes());
    let checksum = hex_sha256(&jsonl_bytes);
    let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let null_counts_val: Value = serde_json::to_value(&null_counts).unwrap_or_else(|_| json!({}));

    // Carry the envelope's op-outcome summary through: the manifest is the
    // only output the caller sees on the save path, and without the counts a
    // strict caller (`kkernel exec --strict --save-file`) cannot distinguish
    // an all-green batch from one whose failures are sitting in the file.
    let summary = results_envelope.get("summary").cloned().unwrap_or_else(|| {
        let succeeded = results_arr
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            .count();
        json!({
            "total": rows,
            "succeeded": succeeded,
            "failed": rows - succeeded,
            "aborted": 0,
        })
    });

    Ok(json!({
        "path": abs_path.to_string_lossy(),
        "rows": rows,
        "per_column_null_counts": null_counts_val,
        "schema_fingerprint": schema_fingerprint,
        "checksum": checksum,
        "summary": summary,
    }))
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Write `data` to `path` via a securely-created, randomly-named temp file in
/// the same directory, then rename over the destination (same-filesystem,
/// atomic). See `crates/khive-mcp/docs/save-sink.md` for the
/// symlink/predictable-path race this closes vs. a sibling `.tmp` file.
fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut tmp = tempfile::Builder::new()
        .prefix(".khive-save-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| anyhow::anyhow!("create temp file in {}: {e}", parent.display()))?;

    tmp.write_all(data)
        .map_err(|e| anyhow::anyhow!("write temp file: {e}"))?;
    tmp.flush()
        .map_err(|e| anyhow::anyhow!("flush temp file: {e}"))?;

    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("persist temp file to {}: {}", path.display(), e.error))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::TempDir;

    /// Scope `KHIVE_SAVE_TO_ROOT` to `root` for the duration of `f`.
    ///
    /// Tests are `#[serial]` because `EXPORT_ROOT_ENV` is process-global state.
    fn with_root<R>(root: &Path, f: impl FnOnce() -> R) -> R {
        std::env::set_var(EXPORT_ROOT_ENV, root);
        let result = f();
        std::env::remove_var(EXPORT_ROOT_ENV);
        result
    }

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
    fn manifest_carries_the_envelope_summary() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.jsonl");

        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "stats", "result": {} }),
            json!({ "ok": false, "tool": "get", "error": "not found" }),
        ]);

        let manifest = write_and_manifest(&envelope, &path, false).unwrap();
        assert_eq!(manifest["summary"]["total"], json!(2));
        assert_eq!(manifest["summary"]["succeeded"], json!(1));
        assert_eq!(manifest["summary"]["failed"], json!(1));
    }

    #[test]
    fn writes_jsonl_and_manifest_fields_correct() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.jsonl");

        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "stats", "result": { "entities": 5, "notes": null } }),
            json!({ "ok": true, "tool": "list",  "result": { "entities": 3, "notes": 2 } }),
        ]);

        let manifest = write_and_manifest(&envelope, &path, false).unwrap();

        assert!(path.exists());
        assert_eq!(manifest["rows"], json!(2));
        assert_eq!(manifest["per_column_null_counts"]["notes"], json!(1));
        assert!(manifest["per_column_null_counts"]["entities"].is_null());

        let fp = manifest["schema_fingerprint"].as_str().unwrap();
        assert!(!fp.is_empty());
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));

        let ck = manifest["checksum"].as_str().unwrap();
        assert!(!ck.is_empty());
        assert!(ck.chars().all(|c| c.is_ascii_hexdigit()));

        let file_bytes = std::fs::read(&path).unwrap();
        let expected_ck = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&file_bytes);
            format!("{:x}", h.finalize())
        };
        assert_eq!(ck, expected_ck);

        let content = String::from_utf8(file_bytes).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            serde_json::from_str::<Value>(line).expect("valid JSON line");
        }
    }

    #[test]
    fn empty_results_produces_valid_manifest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        let envelope = make_envelope(vec![]);
        let manifest = write_and_manifest(&envelope, &path, false).unwrap();

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

        let m1 = write_and_manifest(&envelope, &path, false).unwrap();
        let m2 = write_and_manifest(&envelope, &path, false).unwrap();
        assert_eq!(m1["checksum"], m2["checksum"]);
        assert_eq!(m1["schema_fingerprint"], m2["schema_fingerprint"]);
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

        let m1 = write_and_manifest(&e1, &p1, false).unwrap();
        let m2 = write_and_manifest(&e2, &p2, false).unwrap();
        assert_ne!(m1["schema_fingerprint"], m2["schema_fingerprint"]);
    }

    #[test]
    #[serial]
    fn happy_path_relative_and_absolute_inside_root_both_succeed() {
        let tmp = TempDir::new().unwrap();
        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "t", "result": { "n": 1 } }),
        ]);

        with_root(tmp.path(), || {
            // Relative path is joined under the root.
            let m1 = write_and_manifest(&envelope, Path::new("nested/rel.jsonl"), true).unwrap();
            assert!(tmp.path().join("nested/rel.jsonl").exists());
            assert_eq!(m1["rows"], json!(1));

            // Absolute path that resolves inside the root also succeeds.
            let abs = tmp.path().join("abs.jsonl");
            let m2 = write_and_manifest(&envelope, &abs, true).unwrap();
            assert!(abs.exists());
            assert_eq!(m2["rows"], json!(1));
        });
    }

    #[test]
    #[serial]
    fn traversal_component_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let envelope = make_envelope(vec![json!({ "ok": true, "tool": "t", "result": {} })]);

        with_root(tmp.path(), || {
            let err =
                write_and_manifest(&envelope, Path::new("../escape.jsonl"), true).unwrap_err();
            assert!(
                err.to_string().contains("traversal"),
                "expected traversal error, got: {err}"
            );
        });
    }

    #[test]
    #[serial]
    fn outside_root_missing_parent_is_rejected_without_creating_directories() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let envelope = make_envelope(vec![json!({ "ok": true, "tool": "t", "result": {} })]);

        with_root(tmp.path(), || {
            let missing_parent = outside.path().join("no").join("such").join("dir");
            let dest = missing_parent.join("escape.jsonl");
            let err = write_and_manifest(&envelope, &dest, true).unwrap_err();
            assert!(
                err.to_string().contains("escapes the allowed export root"),
                "expected escape error, got: {err}"
            );
            assert!(
                !missing_parent.exists() && !outside.path().join("no").exists(),
                "outside-root parent directories must not be created"
            );
        });
    }

    #[test]
    #[serial]
    fn absolute_path_outside_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let envelope = make_envelope(vec![json!({ "ok": true, "tool": "t", "result": {} })]);

        with_root(tmp.path(), || {
            let target = outside.path().join("outside.jsonl");
            let err = write_and_manifest(&envelope, &target, true).unwrap_err();
            assert!(
                err.to_string().contains("escapes"),
                "expected escape error, got: {err}"
            );
            assert!(!target.exists());
        });
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn symlinked_destination_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let real_target = outside.path().join("real.txt");
        std::fs::write(&real_target, b"pre-existing").unwrap();

        let link_path = tmp.path().join("link.jsonl");
        std::os::unix::fs::symlink(&real_target, &link_path).unwrap();

        let envelope = make_envelope(vec![json!({ "ok": true, "tool": "t", "result": {} })]);

        with_root(tmp.path(), || {
            let err = write_and_manifest(&envelope, &link_path, true).unwrap_err();
            assert!(
                err.to_string().contains("symlink"),
                "expected symlink error, got: {err}"
            );
        });

        // The symlink target must be untouched.
        assert_eq!(std::fs::read(&real_target).unwrap(), b"pre-existing");
    }

    #[test]
    #[serial]
    fn overwrite_of_existing_regular_file_inside_root_succeeds() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("overwrite.jsonl");
        std::fs::write(&path, b"stale content").unwrap();

        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "t", "result": { "n": 2 } }),
        ]);

        with_root(tmp.path(), || {
            let manifest = write_and_manifest(&envelope, &path, true).unwrap();
            assert_eq!(manifest["rows"], json!(1));
        });

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"n\":2"));
        assert!(!content.contains("stale content"));
    }

    #[test]
    fn unrestricted_path_outside_any_root_still_succeeds() {
        // Trusted operator path (`kkernel exec --save-file`, from_wire = false):
        // no root containment is enforced, matching documented CLI behavior.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("cli.jsonl");
        let envelope = make_envelope(vec![
            json!({ "ok": true, "tool": "t", "result": { "n": 3 } }),
        ]);

        let manifest = write_and_manifest(&envelope, &path, false).unwrap();
        assert_eq!(manifest["rows"], json!(1));
        assert!(path.exists());
    }
}
