//! Tree manifests: `{"schema":"khive-tree/v1","entries":[{path,ref,mode}]}`.
//!
//! A manifest is stored as one blob whose reference is the tree id. The
//! serialization is canonical (entries sorted by path, compact JSON) so two
//! runs that leave identical content produce identical tree references.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{BlobStore, ContentRef};

use crate::vocab::TREE_SCHEMA;

/// Largest manifest the pack will read back.
pub const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

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
    if cwd.is_empty() || cwd == "." {
        return Ok(".".to_string());
    }
    validate_relative_path(cwd, "cwd")
}

/// The one entry validator: paths, modes, duplicates, ref format, and the rule that a file
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
        let mode = item
            .get("mode")
            .and_then(Value::as_u64)
            .ok_or_else(|| RuntimeError::InvalidInput("entry.mode must be 644 or 755".into()))?;
        if mode != 644 && mode != 755 {
            return Err(RuntimeError::InvalidInput(format!(
                "entry {path:?} mode must be 644 or 755; got {mode} (symlinks and other modes are refused)"
            )));
        }
        // A file cannot also be a directory prefix of another entry.
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
    fn rejects_duplicates_and_modes() {
        let r = digest_hex(b"x");
        let dup =
            json!([{"path": "a", "ref": r, "mode": 644}, {"path": "a", "ref": r, "mode": 644}]);
        assert!(parse_entries(&dup).is_err());
        let mode = json!([{"path": "a", "ref": r, "mode": 777}]);
        assert!(parse_entries(&mode).is_err());
        let link = json!([{"path": "a", "ref": r, "mode": 0o120777}]);
        assert!(parse_entries(&link).is_err());
        let nested =
            json!([{"path": "a", "ref": r, "mode": 644}, {"path": "a/b", "ref": r, "mode": 644}]);
        assert!(parse_entries(&nested).is_err());
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
