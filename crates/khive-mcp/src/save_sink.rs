//! File sink for `request` results — writes JSONL and returns a self-describing manifest.
//!
//! See `crates/khive-mcp/docs/save-sink.md` for why the manifest self-reports
//! null counts and why `save_to` is treated as an untrusted, client-supplied
//! filesystem path.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
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

fn resolve_destination(path: &Path, restrict_to_export_root: bool) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("save_to path must not be empty");
    }

    let destination = if restrict_to_export_root {
        let root = export_root()?;
        validate_destination(&root, path)
    } else {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create parent dir {}: {e}", parent.display()))?;
            }
        }
        Ok(path.to_path_buf())
    }?;

    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if !metadata.file_type().is_file() => anyhow::bail!(
            "save_to destination must be absent or an existing regular file: {}",
            destination.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => anyhow::bail!(
            "inspect save_to destination {}: {error}",
            destination.display()
        ),
    }

    Ok(destination)
}

struct DigestingWriter<'a> {
    file: &'a mut std::fs::File,
    digest: &'a mut Sha256,
}

impl std::io::Write for DigestingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buffer)?;
        self.digest.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Incremental form of the ordinary JSONL save sink.
///
/// Rows are serialized once into a securely-created sibling temp file while
/// checksum/schema metadata is accumulated. [`finish`](Self::finish) flushes
/// and atomically renames the complete file over the destination. Dropping an
/// unfinished sink removes the temp file and leaves any old destination intact.
pub struct JsonlSaveSink {
    destination: PathBuf,
    temp: tempfile::NamedTempFile,
    checksum: Sha256,
    rows: usize,
    null_counts: BTreeMap<String, u64>,
    seen_fields: BTreeSet<String>,
}

impl JsonlSaveSink {
    /// Validate/preflight the destination and create the sibling temp file.
    pub fn new(path: &Path, restrict_to_export_root: bool) -> anyhow::Result<Self> {
        let destination = resolve_destination(path, restrict_to_export_root)?;
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temp = tempfile::Builder::new()
            .prefix(".khive-save-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|error| {
                anyhow::anyhow!("create temp file in {}: {error}", parent.display())
            })?;
        Ok(Self {
            destination,
            temp,
            checksum: Sha256::new(),
            rows: 0,
            null_counts: BTreeMap::new(),
            seen_fields: BTreeSet::new(),
        })
    }

    /// Append one ordered result envelope row.
    pub fn write_row(&mut self, row: &Value) -> anyhow::Result<()> {
        if let Some(Value::Object(object)) = row.get("result") {
            for (key, value) in object {
                self.seen_fields.insert(key.clone());
                if value.is_null() {
                    *self.null_counts.entry(key.clone()).or_insert(0) += 1;
                }
            }
        }

        let mut writer = DigestingWriter {
            file: self.temp.as_file_mut(),
            digest: &mut self.checksum,
        };
        serde_json::to_writer(&mut writer, row)
            .map_err(|error| anyhow::anyhow!("serialize result row: {error}"))?;
        writer
            .write_all(b"\n")
            .map_err(|error| anyhow::anyhow!("write result row: {error}"))?;
        self.rows = self
            .rows
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("save sink row count overflow"))?;
        Ok(())
    }

    /// Write every result row from an ordinary request envelope, then publish
    /// it with the envelope summary. Constructing the sink separately lets a
    /// caller preflight the destination before performing external writes.
    pub fn write_envelope(mut self, results_envelope: &Value) -> anyhow::Result<Value> {
        let results = results_envelope
            .get("results")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let rows = results.len();
        let summary = results_envelope.get("summary").cloned().unwrap_or_else(|| {
            let succeeded = results
                .iter()
                .filter(|row| row.get("ok").and_then(Value::as_bool) == Some(true))
                .count();
            json!({
                "total": rows,
                "succeeded": succeeded,
                "failed": rows - succeeded,
                "aborted": 0,
            })
        });
        let failures = summary
            .get("failures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| failure_projection(results));
        for row in results {
            self.write_row(row)?;
        }
        self.finish_with_failures(summary, failures)
    }

    /// Publish the complete JSONL file and return its ordinary manifest.
    pub fn finish(self, summary: Value) -> anyhow::Result<Value> {
        let failures = summary
            .get("failures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.finish_with_failures(summary, failures)
    }

    fn finish_with_failures(
        mut self,
        summary: Value,
        failures: Vec<Value>,
    ) -> anyhow::Result<Value> {
        self.temp
            .as_file_mut()
            .flush()
            .map_err(|error| anyhow::anyhow!("flush temp file: {error}"))?;

        let schema_input = self
            .seen_fields
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("|");
        let schema_fingerprint = hex_sha256(schema_input.as_bytes());
        let checksum = format!("{:x}", self.checksum.clone().finalize());
        let rows = self.rows;
        let null_counts = serde_json::to_value(&self.null_counts).unwrap_or_else(|_| json!({}));
        let destination = self.destination.clone();

        self.temp.persist(&destination).map_err(|error| {
            anyhow::anyhow!(
                "persist temp file to {}: {}",
                destination.display(),
                error.error
            )
        })?;
        let absolute =
            std::fs::canonicalize(&destination).unwrap_or_else(|_| destination.to_path_buf());
        let mut manifest = json!({
            "path": absolute.to_string_lossy(),
            "rows": rows,
            "per_column_null_counts": null_counts,
            "schema_fingerprint": schema_fingerprint,
            "checksum": checksum,
            "summary": summary,
        });
        if !failures.is_empty() {
            manifest["failures"] = Value::Array(failures);
        }
        Ok(manifest)
    }
}

fn failure_projection(results: &[Value]) -> Vec<Value> {
    results
        .iter()
        .enumerate()
        .filter(|(_, row)| row.get("ok").and_then(Value::as_bool) == Some(false))
        .map(|(op_index, row)| {
            let mut failure = json!({
                "op_index": row
                    .get("op_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(op_index as u64),
                "tool": row.get("tool").and_then(Value::as_str).unwrap_or("?"),
                "error": row
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| Value::String("unknown error".to_string())),
            });
            if let Some(reason) = row.get("reason").and_then(Value::as_str) {
                failure["reason"] = Value::String(reason.to_string());
            }
            failure
        })
        .collect()
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
///   "checksum": "<sha256 of file bytes>",
///   "summary": { ... },
///   "failures": [ {"op_index": 0, "tool": "...", "error": ..., "reason": "..."} ]
/// }
/// ```
///
/// `failures` is omitted for an all-successful result. It is a compact
/// projection rather than the full result payload: callers still need stable
/// refusal metadata in the stdout manifest while canonical per-op rows remain
/// in the JSONL file. Incremental callers may provide an already-bounded
/// `summary.failures` projection; that bound is preserved in the manifest.
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
    JsonlSaveSink::new(path, restrict_to_export_root)?.write_envelope(results_envelope)
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
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
            json!({
                "ok": false,
                "tool": "get",
                "error": "not found",
                "reason": "entity-not-found"
            }),
        ]);

        let manifest = write_and_manifest(&envelope, &path, false).unwrap();
        assert_eq!(manifest["summary"]["total"], json!(2));
        assert_eq!(manifest["summary"]["succeeded"], json!(1));
        assert_eq!(manifest["summary"]["failed"], json!(1));
        assert_eq!(manifest["failures"][0]["op_index"], json!(1));
        assert_eq!(manifest["failures"][0]["tool"], json!("get"));
        assert_eq!(manifest["failures"][0]["error"], json!("not found"));
        assert_eq!(manifest["failures"][0]["reason"], json!("entity-not-found"));
    }

    #[test]
    fn incremental_sink_preserves_bounded_summary_failures() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bounded.jsonl");
        let mut sink = JsonlSaveSink::new(&path, false).unwrap();
        sink.write_row(&json!({
            "ok": false,
            "tool": "get",
            "error": "full canonical row",
            "reason": "entity-not-found"
        }))
        .unwrap();
        let manifest = sink
            .finish(json!({
                "total": 2_000,
                "succeeded": 0,
                "failed": 2_000,
                "aborted": 0,
                "failures": [{
                    "op_index": 0,
                    "tool": "get",
                    "error": "bounded detail",
                    "reason": "entity-not-found"
                }],
                "failure_details_omitted": 1_999
            }))
            .unwrap();
        assert_eq!(manifest["failures"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["failures"][0]["error"], "bounded detail");
        assert_eq!(manifest["summary"]["failure_details_omitted"], 1_999);
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
    fn unfinished_incremental_sink_leaves_existing_destination_unchanged() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("existing.jsonl");
        std::fs::write(&path, b"old-complete-output\n").unwrap();

        {
            let mut sink = JsonlSaveSink::new(&path, false).unwrap();
            sink.write_row(&json!({
                "ok": true,
                "tool": "get",
                "result": {"id": "new"}
            }))
            .unwrap();
            // Simulate a later chunk/dispatch failure: no finish/publish.
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"old-complete-output\n");
        let siblings: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(siblings, vec![std::ffi::OsString::from("existing.jsonl")]);
    }

    #[test]
    fn deterministic_invalid_operator_destinations_fail_during_preflight() {
        let tmp = TempDir::new().unwrap();

        let empty_error = JsonlSaveSink::new(Path::new(""), false)
            .err()
            .expect("empty path must fail");
        assert!(empty_error.to_string().contains("must not be empty"));

        let directory_error = JsonlSaveSink::new(tmp.path(), false)
            .err()
            .expect("directory destination must fail");
        assert!(directory_error
            .to_string()
            .contains("absent or an existing regular file"));
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
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
