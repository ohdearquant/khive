//! Exec tree helpers: shared manifests plus cwd resolution and diff presentation.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound::{Included, Unbounded};

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::ContentRef;

pub(crate) use khive_runtime::blob_tree::parse_entries;
pub use khive_runtime::blob_tree::{
    blob_store, entries_json, load, store, store_from_value, validate_relative_path, verify_blobs,
    TreeEntry, MAX_MANIFEST_BYTES,
};

// Preflight has no child timeout: bound both each expansion and retained work.
const MAX_CWD_PATH_BYTES: usize = 64 * 1024;
const MAX_CWD_PATH_COMPONENTS: usize = 1024;
const MAX_CWD_PENDING_BYTES: usize = 256 * 1024;
const MAX_CWD_PENDING_COMPONENTS: usize = 4096;

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
    let hydrator = rt
        .blob_hydrator()
        .ok_or_else(|| RuntimeError::Unconfigured("exec blob hydrator is not installed".into()))?;
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
            let bytes = hydrator
                .hydrate_verified(&reference, MAX_CWD_PATH_BYTES as u64)
                .await
                .map_err(|e| {
                    if matches!(
                        &e,
                        RuntimeError::Storage(
                            khive_storage::StorageError::Timeout { .. }
                                | khive_storage::StorageError::AdmissionTimeout { .. }
                        ) | RuntimeError::Internal(_) | RuntimeError::Unconfigured(_)
                    ) {
                        e
                    } else {
                        RuntimeError::InvalidInput(format!(
                            "cwd symlink target at {path:?} cannot be read within {MAX_CWD_PATH_BYTES} bytes: {e}"
                        ))
                    }
                })?;
            let target = std::str::from_utf8(bytes.bytes()).map_err(|_| {
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
        } else {
            // Seek the slash boundary so lexical siblings cannot mask descendants.
            let prefix = format!("{path}/");
            let is_directory = by_path
                .range::<str, _>((Included(prefix.as_str()), Unbounded))
                .next()
                .is_some_and(|(entry_path, _)| entry_path.starts_with(&prefix));
            if !is_directory {
                return Err(RuntimeError::InvalidInput(format!(
                    "cwd {cwd:?} directory {path:?} is absent from the manifest"
                )));
            }
            resolved.push(component);
        }
    }
    Ok(if resolved.is_empty() {
        ".".to_string()
    } else {
        resolved.join("/")
    })
}

/// Boolean output coverage over already validated declarations, independent of
/// the manifest's file/directory collision policy.
#[cfg(any(unix, test))]
pub(crate) struct DeclaredCoverage {
    prefixes: khive_runtime::blob_tree::PathCoverage,
    #[cfg(test)]
    work: crate::membership::MembershipWork,
    #[cfg(test)]
    pub(crate) declarations: Vec<String>,
}

#[cfg(any(unix, test))]
impl DeclaredCoverage {
    pub(crate) fn new(paths: &[String], work: crate::membership::MembershipWork) -> Self {
        let prefixes = khive_runtime::blob_tree::PathCoverage::new(paths);
        #[cfg(test)]
        {
            let build = prefixes.build_work();
            work.declared_build(
                build.compared_bytes,
                build.child_lookups,
                build.copied_label_bytes,
            );
        }
        #[cfg(not(test))]
        let _ = work;
        Self {
            prefixes,
            #[cfg(test)]
            work,
            #[cfg(test)]
            declarations: paths.to_vec(),
        }
    }

    pub(crate) fn covers(&self, path: &str) -> bool {
        #[cfg(test)]
        {
            let (covered, query) = self.prefixes.covers_with_work(path);
            self.work
                .declared_query(query.compared_bytes, query.child_lookups);
            covered
        }
        #[cfg(not(test))]
        self.prefixes.covers(path)
    }
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
#[path = "tree/tests.rs"]
mod tests;
