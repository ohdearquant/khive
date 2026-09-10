//! Tree manifests: `{"schema":"khive-tree/v1","entries":[{path,ref,mode}]}`.
//!
//! A manifest is stored as one blob whose reference is the tree id. The
//! serialization is canonical (entries sorted by path, compact JSON) so two
//! runs that leave identical content produce identical tree references.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{BlobStore, ContentRef};

use crate::vocab::TREE_SCHEMA;

/// Largest manifest the pack will read back.
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

// Preflight has no child timeout: bound both each expansion and retained work.
const MAX_CWD_PATH_BYTES: usize = 64 * 1024;
const MAX_CWD_PATH_COMPONENTS: usize = 1024;
const MAX_CWD_PENDING_BYTES: usize = 256 * 1024;
const MAX_CWD_PENDING_COMPONENTS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    #[serde(rename = "ref")]
    pub content_ref: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema: String,
    entries: Vec<TreeEntry>,
}

/// Validate a relative path: no empty components, no `.`/`..`, no leading
/// `/`, no NUL, no backslash. Returns the normalized path.
pub fn validate_relative_path(path: &str, what: &str) -> Result<String, RuntimeError> {
    if path.is_empty() {
        return Err(RuntimeError::InvalidInput(format!("{what} path is empty")));
    }
    if path.starts_with('/') {
        return Err(RuntimeError::InvalidInput(format!(
            "{what} path {path:?} is absolute; paths are relative to the tree root"
        )));
    }
    if path.contains('\0') || path.contains('\\') {
        return Err(RuntimeError::InvalidInput(format!(
            "{what} path {path:?} contains a forbidden character"
        )));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(RuntimeError::InvalidInput(format!(
                "{what} path {path:?} is not normalized (empty, '.' or '..' component)"
            )));
        }
    }
    Ok(path.to_string())
}

/// Validate a working directory relative to the tree root. `.` and the empty
/// string mean the root; anything else must be a normalized relative path.
pub fn validate_cwd(cwd: &str) -> Result<String, RuntimeError> {
    cwd_component_cost(cwd)?;
    if cwd.is_empty() || cwd == "." {
        return Ok(".".to_string());
    }
    validate_relative_path(cwd, "cwd")
}

fn cwd_component_cost(path: &str) -> Result<(usize, usize), RuntimeError> {
    if path.len() > MAX_CWD_PATH_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "cwd symlink resolution path exceeds {MAX_CWD_PATH_BYTES} bytes"
        )));
    }
    let mut queued_components = 0;
    let mut queued_bytes = 0;
    for (index, part) in path.split('/').enumerate() {
        if index >= MAX_CWD_PATH_COMPONENTS {
            return Err(RuntimeError::InvalidInput(format!(
                "cwd symlink resolution path exceeds {MAX_CWD_PATH_COMPONENTS} components"
            )));
        }
        if !part.is_empty() && part != "." {
            queued_components += 1;
            queued_bytes += part.len();
        }
    }
    Ok((queued_components, queued_bytes))
}

fn prepend_cwd_components(
    pending: &mut VecDeque<String>,
    pending_bytes: &mut usize,
    path: &str,
) -> Result<(), RuntimeError> {
    let (components, bytes) = cwd_component_cost(path)?;
    pending
        .len()
        .checked_add(components)
        .filter(|count| *count <= MAX_CWD_PENDING_COMPONENTS)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "cwd symlink resolution pending component budget exceeds {MAX_CWD_PENDING_COMPONENTS}"
            ))
        })?;
    let total_bytes = pending_bytes
        .checked_add(bytes)
        .filter(|count| *count <= MAX_CWD_PENDING_BYTES)
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "cwd symlink resolution pending byte budget exceeds {MAX_CWD_PENDING_BYTES}"
            ))
        })?;
    // Validate the entire expansion before allocating any owned components.
    for part in path.split('/').rev() {
        if !part.is_empty() && part != "." {
            pending.push_front(part.to_string());
        }
    }
    *pending_bytes = total_bytes;
    Ok(())
}

/// Resolve cwd against the immutable manifest before materialization. Only
/// directory prefixes in the manifest can be working directories. Expand each
/// link before interpreting subsequent `..` components, as path lookup does.
pub async fn resolve_cwd(
    rt: &KhiveRuntime,
    entries: &[TreeEntry],
    cwd: &str,
) -> Result<String, RuntimeError> {
    let cwd = validate_cwd(cwd)?;
    if cwd == "." {
        return Ok(cwd);
    }
    let by_path: BTreeMap<&str, &TreeEntry> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let directories: BTreeSet<&str> = entries
        .iter()
        .flat_map(|entry| {
            entry
                .path
                .match_indices('/')
                .map(move |(i, _)| &entry.path[..i])
        })
        .collect();
    let store = blob_store(rt)?;
    let mut pending = VecDeque::new();
    let mut pending_bytes = 0;
    prepend_cwd_components(&mut pending, &mut pending_bytes, &cwd)?;
    let mut resolved: Vec<String> = Vec::new();
    let mut expansions = 0;
    while let Some(component) = pending.pop_front() {
        pending_bytes -= component.len();
        if component == ".." {
            if resolved.pop().is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd {cwd:?} symlink target escapes the tree root"
                )));
            }
            continue;
        }
        let path = if resolved.is_empty() {
            component.clone()
        } else {
            format!("{}/{component}", resolved.join("/"))
        };
        if let Some(entry) = by_path.get(path.as_str()) {
            if entry.mode != 120000 {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd {cwd:?} traverses non-directory entry {path:?}"
                )));
            }
            expansions += 1;
            if expansions > 40 {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd {cwd:?} symlink chain exceeds 40 expansions (possible cycle)"
                )));
            }
            let reference = ContentRef::from_hex(&entry.content_ref)
                .map_err(|e| RuntimeError::InvalidInput(format!("cwd entry {path:?}: {e}")))?;
            let bytes = store
                .get_bounded_verified(&reference, MAX_CWD_PATH_BYTES as u64)
                .await
                .map_err(|e| {
                    RuntimeError::InvalidInput(format!(
                        "cwd symlink target at {path:?} cannot be read within {MAX_CWD_PATH_BYTES} bytes: {e}"
                    ))
                })?;
            let target = std::str::from_utf8(&bytes).map_err(|_| {
                RuntimeError::InvalidInput(format!(
                    "cwd entry {path:?} target cannot name a UTF-8 manifest directory"
                ))
            })?;
            if target.starts_with('/') {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd {cwd:?} symlink target at {path:?} is absolute and escapes the tree root"
                )));
            }
            if target.is_empty() || target.contains('\0') {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd entry {path:?} has an empty or NUL-containing symlink target"
                )));
            }
            // A relative target starts in the link's parent, not at the link.
            prepend_cwd_components(&mut pending, &mut pending_bytes, target)?;
        } else if directories.contains(path.as_str()) {
            resolved.push(component);
        } else {
            return Err(RuntimeError::InvalidInput(format!(
                "cwd {cwd:?} directory {path:?} is absent from the manifest"
            )));
        }
    }
    Ok(if resolved.is_empty() {
        ".".to_string()
    } else {
        resolved.join("/")
    })
}

/// The one entry validator: paths, modes, duplicates, ref format, and the rule that an entry
/// cannot also be a directory prefix of another entry. Exposed so `exec.tree_put` validates
/// a whole candidate manifest through this function rather than reimplementing its rules.
pub(crate) fn parse_entries(value: &Value) -> Result<Vec<TreeEntry>, RuntimeError> {
    let items = value.as_array().ok_or_else(|| {
        RuntimeError::InvalidInput("entries must be an array of {path, ref, mode}".into())
    })?;
    let mut seen: BTreeMap<String, TreeEntry> = BTreeMap::new();
    for item in items {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("entry.path must be a string".into()))?;
        let path = validate_relative_path(path, "entry")?;
        let content_ref = item
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("entry.ref must be a string".into()))?;
        ContentRef::from_hex(content_ref)
            .map_err(|e| RuntimeError::InvalidInput(format!("entry {path:?} ref: {e}")))?;
        let mode = item.get("mode").and_then(Value::as_u64).ok_or_else(|| {
            RuntimeError::InvalidInput("entry.mode must be 644, 755 or 120000".into())
        })?;
        if !matches!(mode, 644 | 755 | 120000) {
            return Err(RuntimeError::InvalidInput(format!(
                "entry {path:?} mode must be 644, 755 or 120000; got {mode}"
            )));
        }
        // Neither a file nor a symlink can be a directory prefix of another entry.
        for existing in seen.keys() {
            if existing.starts_with(&format!("{path}/"))
                || path.starts_with(&format!("{existing}/"))
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "entry {path:?} conflicts with entry {existing:?} (file and directory at one path)"
                )));
            }
        }
        if seen
            .insert(
                path.clone(),
                TreeEntry {
                    path: path.clone(),
                    content_ref: content_ref.to_string(),
                    mode: mode as u32,
                },
            )
            .is_some()
        {
            return Err(RuntimeError::InvalidInput(format!(
                "duplicate entry path {path:?}"
            )));
        }
    }
    Ok(seen.into_values().collect())
}

fn canonical_bytes(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<TreeEntry> = entries.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        schema: TREE_SCHEMA.to_string(),
        entries: sorted,
    };
    serde_json::to_vec(&manifest).expect("manifest serializes")
}

pub fn blob_store(rt: &KhiveRuntime) -> Result<std::sync::Arc<dyn BlobStore>, RuntimeError> {
    rt.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured("no blob store is configured for this runtime".into())
    })
}

/// Validate and store a manifest built from wire entries; returns the tree ref.
pub async fn store_from_value(rt: &KhiveRuntime, entries: &Value) -> Result<String, RuntimeError> {
    let entries = parse_entries(entries)?;
    store(rt, &entries).await
}

/// Store already-validated entries; returns the tree ref.
pub async fn store(rt: &KhiveRuntime, entries: &[TreeEntry]) -> Result<String, RuntimeError> {
    let store = blob_store(rt)?;
    let content_ref = store.put(canonical_bytes(entries)).await?;
    Ok(content_ref.as_str().to_string())
}

/// Load a manifest by reference. An unknown reference or a non-manifest
/// object is an error naming the reference.
pub async fn load(rt: &KhiveRuntime, tree_ref: &str) -> Result<Vec<TreeEntry>, RuntimeError> {
    let content_ref = ContentRef::from_hex(tree_ref)
        .map_err(|e| RuntimeError::InvalidInput(format!("tree ref {tree_ref:?}: {e}")))?;
    let store = blob_store(rt)?;
    if !store.exists(&content_ref).await? {
        return Err(RuntimeError::NotFound(format!(
            "tree {tree_ref} is not in the blob store"
        )));
    }
    let bytes = store
        .get_bounded_verified(&content_ref, MAX_MANIFEST_BYTES)
        .await?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
        RuntimeError::InvalidInput(format!("tree {tree_ref} is not a khive-tree manifest: {e}"))
    })?;
    if manifest.schema != TREE_SCHEMA {
        return Err(RuntimeError::InvalidInput(format!(
            "tree {tree_ref} has schema {:?}; expected {TREE_SCHEMA:?}",
            manifest.schema
        )));
    }
    let entries = parse_entries(&serde_json::to_value(&manifest.entries).unwrap_or(Value::Null))?;
    Ok(entries)
}

/// Every entry's blob must exist before materialization starts.
pub async fn verify_blobs(rt: &KhiveRuntime, entries: &[TreeEntry]) -> Result<(), RuntimeError> {
    let store = blob_store(rt)?;
    for entry in entries {
        let content_ref = ContentRef::from_hex(&entry.content_ref)
            .map_err(|e| RuntimeError::InvalidInput(format!("entry {:?} ref: {e}", entry.path)))?;
        if !store.exists(&content_ref).await? {
            return Err(RuntimeError::NotFound(format!(
                "entry {:?} references blob {} which is not in the store",
                entry.path, entry.content_ref
            )));
        }
    }
    Ok(())
}

pub fn entries_json(entries: &[TreeEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|e| json!({"path": e.path, "ref": e.content_ref, "mode": e.mode}))
            .collect(),
    )
}

/// One changed path between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub op: &'static str,
    pub content_ref: Option<String>,
    pub base_ref: Option<String>,
}

impl Change {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "op": self.op,
            "ref": self.content_ref,
            "base_ref": self.base_ref,
        })
    }
}

/// Diff two entry lists by path: added, modified (content or mode), deleted.
pub fn diff(base: &[TreeEntry], head: &[TreeEntry]) -> Vec<Change> {
    let base_map: BTreeMap<&str, &TreeEntry> = base.iter().map(|e| (e.path.as_str(), e)).collect();
    let head_map: BTreeMap<&str, &TreeEntry> = head.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut out = Vec::new();
    for (path, entry) in &head_map {
        match base_map.get(path) {
            None => out.push(Change {
                path: path.to_string(),
                op: "added",
                content_ref: Some(entry.content_ref.clone()),
                base_ref: None,
            }),
            Some(old) if old.content_ref != entry.content_ref || old.mode != entry.mode => out
                .push(Change {
                    path: path.to_string(),
                    op: "modified",
                    content_ref: Some(entry.content_ref.clone()),
                    base_ref: Some(old.content_ref.clone()),
                }),
            Some(_) => {}
        }
    }
    for (path, entry) in &base_map {
        if !head_map.contains_key(path) {
            out.push(Change {
                path: path.to_string(),
                op: "deleted",
                content_ref: None,
                base_ref: Some(entry.content_ref.clone()),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// BLAKE3 hex of `bytes`, the same digest a blob reference carries.
pub fn digest_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_paths() {
        for bad in ["../x", "/abs", "a/../b", "", "a//b", "./a", "a\\b"] {
            assert!(validate_relative_path(bad, "entry").is_err(), "{bad:?}");
        }
        assert_eq!(
            validate_relative_path("a/b.txt", "entry").unwrap(),
            "a/b.txt"
        );
        assert_eq!(validate_cwd("").unwrap(), ".");
        assert!(validate_cwd("package/../package").is_err());
    }

    #[test]
    fn cwd_component_cost_bounds_raw_paths_before_skipping_noops() {
        let bytes = "a".repeat(MAX_CWD_PATH_BYTES);
        assert_eq!(cwd_component_cost(&bytes).unwrap(), (1, bytes.len()));
        assert!(cwd_component_cost(&(bytes + "a")).is_err());

        let components = "/".repeat(MAX_CWD_PATH_COMPONENTS - 1);
        assert_eq!(cwd_component_cost(&components).unwrap(), (0, 0));
        assert!(cwd_component_cost(&(components + "/")).is_err());
    }

    #[test]
    fn cwd_queue_bounds_components_before_mutation() {
        let mut pending = VecDeque::new();
        let mut bytes = 0;
        let path = vec!["a"; MAX_CWD_PATH_COMPONENTS].join("/");
        for _ in 0..MAX_CWD_PENDING_COMPONENTS / MAX_CWD_PATH_COMPONENTS {
            prepend_cwd_components(&mut pending, &mut bytes, &path).unwrap();
        }
        assert_eq!(pending.len(), MAX_CWD_PENDING_COMPONENTS);
        assert_eq!(bytes, MAX_CWD_PENDING_COMPONENTS);
        let before = pending.clone();
        let error = prepend_cwd_components(&mut pending, &mut bytes, "a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("pending component budget"), "{error}");
        assert_eq!(pending, before);
        assert_eq!(bytes, MAX_CWD_PENDING_COMPONENTS);
    }

    #[test]
    fn cwd_queue_bounds_bytes_before_mutation() {
        let mut pending = VecDeque::new();
        let mut bytes = 0;
        let path = "a".repeat(MAX_CWD_PATH_BYTES);
        for _ in 0..MAX_CWD_PENDING_BYTES / MAX_CWD_PATH_BYTES {
            prepend_cwd_components(&mut pending, &mut bytes, &path).unwrap();
        }
        assert_eq!(bytes, MAX_CWD_PENDING_BYTES);
        let before = pending.clone();
        let error = prepend_cwd_components(&mut pending, &mut bytes, "a")
            .unwrap_err()
            .to_string();
        assert!(error.contains("pending byte budget"), "{error}");
        assert_eq!(pending, before);
        assert_eq!(bytes, MAX_CWD_PENDING_BYTES);
    }

    #[test]
    fn cwd_queue_skips_noops_without_reordering_parent_components() {
        let mut pending = VecDeque::new();
        let mut bytes = 0;
        prepend_cwd_components(&mut pending, &mut bytes, "tail").unwrap();
        prepend_cwd_components(&mut pending, &mut bytes, "first//./../second/.").unwrap();
        assert_eq!(
            pending,
            VecDeque::from(["first", "..", "second", "tail"].map(str::to_string))
        );
        assert_eq!(bytes, 17);
    }

    #[test]
    fn accepts_regular_executable_and_symlink_modes() {
        let r = digest_hex(b"target");
        let entries = parse_entries(&json!([
            {"path": "a", "ref": r, "mode": 644},
            {"path": "b", "ref": r, "mode": 755},
            {"path": "c", "ref": r, "mode": 120000}
        ]))
        .unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.mode).collect::<Vec<_>>(),
            vec![644, 755, 120000]
        );
    }

    #[test]
    fn rejects_duplicates_and_modes() {
        let r = digest_hex(b"x");
        let dup =
            json!([{"path": "a", "ref": r, "mode": 644}, {"path": "a", "ref": r, "mode": 644}]);
        assert!(parse_entries(&dup).is_err());
        for mode in [0, 777, 0o120777, 0o120000, 100644, 100755, 120001, u64::MAX] {
            let entries = json!([{"path": "a", "ref": r, "mode": mode}]);
            assert!(parse_entries(&entries).is_err(), "mode {mode}");
        }
    }

    #[test]
    fn rejects_entries_beneath_files_and_symlinks_in_either_order() {
        let r = digest_hex(b"target");
        for mode in [644, 755, 120000] {
            let mut entries = vec![
                json!({"path": "a", "ref": r, "mode": mode}),
                json!({"path": "a/b", "ref": r, "mode": 644}),
            ];
            for _ in 0..2 {
                assert!(parse_entries(&json!(entries)).is_err(), "mode {mode}");
                entries.reverse();
            }
        }
    }

    #[test]
    fn canonical_bytes_are_order_independent() {
        let r = digest_hex(b"x");
        let a = vec![
            TreeEntry {
                path: "b".into(),
                content_ref: r.clone(),
                mode: 644,
            },
            TreeEntry {
                path: "a".into(),
                content_ref: r.clone(),
                mode: 755,
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
    }

    #[test]
    fn diff_classifies() {
        let r1 = digest_hex(b"1");
        let r2 = digest_hex(b"2");
        let base = vec![
            TreeEntry {
                path: "keep".into(),
                content_ref: r1.clone(),
                mode: 644,
            },
            TreeEntry {
                path: "mod".into(),
                content_ref: r1.clone(),
                mode: 644,
            },
            TreeEntry {
                path: "del".into(),
                content_ref: r1.clone(),
                mode: 644,
            },
        ];
        let head = vec![
            TreeEntry {
                path: "keep".into(),
                content_ref: r1.clone(),
                mode: 644,
            },
            TreeEntry {
                path: "mod".into(),
                content_ref: r2.clone(),
                mode: 644,
            },
            TreeEntry {
                path: "add".into(),
                content_ref: r2.clone(),
                mode: 755,
            },
        ];
        let ops: Vec<(String, &str)> = diff(&base, &head)
            .into_iter()
            .map(|c| (c.path, c.op))
            .collect();
        assert_eq!(
            ops,
            vec![
                ("add".into(), "added"),
                ("del".into(), "deleted"),
                ("mod".into(), "modified")
            ]
        );
    }
}
