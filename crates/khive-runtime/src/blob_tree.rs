//! Tree manifests: `{"schema":"khive-tree/v1","entries":[{path,ref,mode}]}`.
//!
//! A manifest is stored as one blob whose reference is the tree id. The
//! serialization is canonical (entries sorted by path, compact JSON) so two
//! runs that leave identical content produce identical tree references.

use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Bound::{Included, Unbounded};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use khive_storage::{BlobStore, ContentRef};

use crate::{KhiveRuntime, RuntimeError};

pub const TREE_SCHEMA: &str = "khive-tree/v1";

/// Largest manifest the shared load path will read back.
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

/// Validate wire entries in input order: paths, refs, modes, duplicates, and
/// file/directory collisions. `exec.tree_put` uses this for candidate manifests.
pub fn parse_entries(value: &Value) -> Result<Vec<TreeEntry>, RuntimeError> {
    let items = value.as_array().ok_or_else(|| {
        RuntimeError::InvalidInput("entries must be an array of {path, ref, mode}".into())
    })?;
    let mut seen = EntryIndex::default();
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
        insert_entry(
            &mut seen,
            TreeEntry {
                path,
                content_ref: content_ref.to_string(),
                mode: mode as u32,
            },
        )?;
    }
    Ok(seen.into_sorted_entries())
}

/// Validate captured output and loaded manifests with the same rules as wire entries.
fn validate_entries(entries: &[TreeEntry]) -> Result<Vec<TreeEntry>, RuntimeError> {
    let mut seen = EntryIndex::default();
    for entry in entries {
        let path = validate_relative_path(&entry.path, "entry")?;
        ContentRef::from_hex(&entry.content_ref)
            .map_err(|e| RuntimeError::InvalidInput(format!("entry {path:?} ref: {e}")))?;
        if !matches!(entry.mode, 644 | 755 | 120000) {
            return Err(RuntimeError::InvalidInput(format!(
                "entry {path:?} mode must be 644, 755 or 120000; got {}",
                entry.mode
            )));
        }
        insert_entry(&mut seen, entry.clone())?;
    }
    Ok(seen.into_sorted_entries())
}

#[derive(Default)]
struct EntryIndex {
    entries: BTreeMap<String, TreeEntry>,
    prefixes: PrefixIndex,
    work: PrefixWork,
}

impl EntryIndex {
    fn into_sorted_entries(self) -> Vec<TreeEntry> {
        self.entries.into_values().collect()
    }
}

#[derive(Default)]
struct PrefixWork {
    #[cfg(any(test, feature = "test-internals"))]
    compared_bytes: usize,
    #[cfg(any(test, feature = "test-internals"))]
    child_lookups: usize,
    #[cfg(any(test, feature = "test-internals"))]
    ordered_probes: usize,
    #[cfg(any(test, feature = "test-internals"))]
    copied_label_bytes: usize,
}

impl PrefixWork {
    #[inline]
    fn copied_label(&mut self, bytes: usize) {
        #[cfg(any(test, feature = "test-internals"))]
        {
            self.copied_label_bytes += bytes;
        }
        #[cfg(not(any(test, feature = "test-internals")))]
        let _ = (self, bytes);
    }

    #[inline]
    fn comparison(&mut self, bytes: usize) {
        #[cfg(any(test, feature = "test-internals"))]
        {
            self.compared_bytes += bytes;
        }
        #[cfg(not(any(test, feature = "test-internals")))]
        let _ = (self, bytes);
    }

    #[inline]
    fn lookup(&mut self) {
        #[cfg(any(test, feature = "test-internals"))]
        {
            self.child_lookups += 1;
        }
        #[cfg(not(any(test, feature = "test-internals")))]
        let _ = self;
    }

    #[inline]
    fn ordered_probe(&mut self) {
        #[cfg(any(test, feature = "test-internals"))]
        {
            self.ordered_probes += 1;
        }
        #[cfg(not(any(test, feature = "test-internals")))]
        let _ = self;
    }
}

struct PrefixEdge {
    label: String,
    child: usize,
}

#[derive(Default)]
struct PrefixNode {
    children: BTreeMap<char, PrefixEdge>,
    entry: bool,
}

// Arena nodes keep both memory and destruction bounded for a path with many
// components. Edge labels compress a long unbranched path into one allocation.
struct PrefixIndex {
    nodes: Vec<PrefixNode>,
}

impl Default for PrefixIndex {
    fn default() -> Self {
        Self {
            nodes: vec![PrefixNode::default()],
        }
    }
}

impl PrefixIndex {
    fn first_parent<'a>(&self, path: &'a str, work: &mut PrefixWork) -> Option<&'a str> {
        self.first_prefix(path, false, work)
    }

    fn first_prefix<'a>(
        &self,
        path: &'a str,
        include_exact: bool,
        work: &mut PrefixWork,
    ) -> Option<&'a str> {
        let mut node = 0;
        let mut consumed = 0;
        loop {
            let remaining = &path[consumed..];
            if self.nodes[node].entry
                && (remaining.starts_with('/') || (include_exact && remaining.is_empty()))
            {
                return Some(&path[..consumed]);
            }
            let first = remaining.chars().next()?;
            work.lookup();
            let edge = self.nodes[node].children.get(&first)?;
            let common = common_prefix_bytes(remaining, &edge.label, work);
            if common != edge.label.len() {
                return None;
            }
            consumed += common;
            node = edge.child;
        }
    }

    fn insert(&mut self, path: &str, work: &mut PrefixWork) {
        let mut node = 0;
        let mut consumed = 0;
        loop {
            let remaining = &path[consumed..];
            let Some(first) = remaining.chars().next() else {
                self.nodes[node].entry = true;
                return;
            };
            work.lookup();
            let Some(edge) = self.nodes[node].children.get(&first) else {
                work.copied_label(remaining.len());
                let child = self.nodes.len();
                self.nodes.push(PrefixNode {
                    entry: true,
                    ..PrefixNode::default()
                });
                self.nodes[node].children.insert(
                    first,
                    PrefixEdge {
                        label: remaining.to_string(),
                        child,
                    },
                );
                return;
            };
            let common = common_prefix_bytes(remaining, &edge.label, work);
            if common == edge.label.len() {
                consumed += common;
                node = edge.child;
                continue;
            }

            // Split only at a UTF-8 character boundary. The old path keeps
            // its child; the new path either ends here or gains a sibling.
            let mut edge = self.nodes[node]
                .children
                .remove(&first)
                .expect("edge exists");
            work.copied_label(edge.label.len() - common);
            let old_suffix = edge.label.split_off(common);
            let middle = self.nodes.len();
            let mut middle_node = PrefixNode::default();
            middle_node.children.insert(
                old_suffix.chars().next().expect("nonempty suffix"),
                PrefixEdge {
                    label: old_suffix,
                    child: edge.child,
                },
            );
            self.nodes.push(middle_node);
            edge.child = middle;
            self.nodes[node].children.insert(first, edge);
            if common == remaining.len() {
                self.nodes[middle].entry = true;
            } else {
                let suffix = &remaining[common..];
                work.copied_label(suffix.len());
                let child = self.nodes.len();
                self.nodes.push(PrefixNode {
                    entry: true,
                    ..PrefixNode::default()
                });
                self.nodes[middle].children.insert(
                    suffix.chars().next().expect("nonempty suffix"),
                    PrefixEdge {
                        label: suffix.to_string(),
                        child,
                    },
                );
            }
            return;
        }
    }
}

/// Exact-or-directory-prefix coverage, independent of manifest collision rules.
/// Declarations are used as given; this does not validate or normalize them.
pub struct PathCoverage {
    prefixes: PrefixIndex,
    #[cfg(any(test, feature = "test-internals"))]
    build_work: PrefixWorkSnapshot,
}

/// Work performed by the shared prefix index, exposed only for test observers.
#[cfg(any(test, feature = "test-internals"))]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PrefixWorkSnapshot {
    pub compared_bytes: usize,
    pub child_lookups: usize,
    pub copied_label_bytes: usize,
}

#[cfg(any(test, feature = "test-internals"))]
impl PrefixWork {
    fn snapshot(&self) -> PrefixWorkSnapshot {
        PrefixWorkSnapshot {
            compared_bytes: self.compared_bytes,
            child_lookups: self.child_lookups,
            copied_label_bytes: self.copied_label_bytes,
        }
    }
}

impl PathCoverage {
    pub fn new(paths: &[String]) -> Self {
        let mut ordered: Vec<&str> = paths.iter().map(String::as_str).collect();
        // Insertion only affects representation, never the boolean predicate.
        // A later split cannot copy an older suffix longer than its input.
        ordered.sort_by_key(|path| path.len());
        let mut prefixes = PrefixIndex::default();
        let mut build = PrefixWork::default();
        for path in ordered {
            prefixes.insert(path, &mut build);
        }
        Self {
            prefixes,
            #[cfg(any(test, feature = "test-internals"))]
            build_work: build.snapshot(),
        }
    }

    pub fn covers(&self, path: &str) -> bool {
        self.prefixes
            .first_prefix(path, true, &mut PrefixWork::default())
            .is_some()
    }

    #[cfg(any(test, feature = "test-internals"))]
    #[doc(hidden)]
    pub fn build_work(&self) -> PrefixWorkSnapshot {
        self.build_work
    }

    #[cfg(any(test, feature = "test-internals"))]
    #[doc(hidden)]
    pub fn covers_with_work(&self, path: &str) -> (bool, PrefixWorkSnapshot) {
        let mut query = PrefixWork::default();
        let covered = self.prefixes.first_prefix(path, true, &mut query).is_some();
        (covered, query.snapshot())
    }
}

fn common_prefix_bytes(left: &str, right: &str, work: &mut PrefixWork) -> usize {
    let mut common = 0;
    for (a, b) in left.chars().zip(right.chars()) {
        work.comparison(a.len_utf8().max(b.len_utf8()));
        if a != b {
            break;
        }
        common += a.len_utf8();
    }
    common
}

/// Insert an already validated entry. Its parents and first ordered descendant
/// find every file/directory collision without scanning all prior entries.
fn insert_entry(seen: &mut EntryIndex, entry: TreeEntry) -> Result<(), RuntimeError> {
    let path = &entry.path;
    seen.work.ordered_probe();
    if seen.entries.contains_key(path) {
        return Err(RuntimeError::InvalidInput(format!(
            "duplicate entry path {path:?}"
        )));
    }
    // Neither a file nor a symlink can be a directory prefix of another entry.
    if let Some(existing) = seen.prefixes.first_parent(path, &mut seen.work) {
        return Err(RuntimeError::InvalidInput(format!(
            "entry {path:?} conflicts with entry {existing:?} (file and directory at one path)"
        )));
    }
    let prefix = format!("{path}/");
    seen.work.ordered_probe();
    if let Some((existing, _)) = seen
        .entries
        .range::<str, _>((Included(prefix.as_str()), Unbounded))
        .next()
    {
        if existing.starts_with(&prefix) {
            return Err(RuntimeError::InvalidInput(format!(
                "entry {path:?} conflicts with entry {existing:?} (file and directory at one path)"
            )));
        }
    }
    seen.prefixes.insert(path, &mut seen.work);
    seen.work.ordered_probe();
    seen.entries.insert(entry.path.clone(), entry);
    Ok(())
}

/// Serialize only a manifest that the load path can read back. The writer
/// refuses as soon as the 8 MiB budget is crossed, before retaining more.
fn canonical_bytes(entries: &[TreeEntry]) -> Result<Vec<u8>, RuntimeError> {
    let mut sorted: Vec<TreeEntry> = entries.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        schema: TREE_SCHEMA.to_string(),
        entries: sorted,
    };
    struct BoundedWriter {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for BoundedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len().saturating_add(buf.len()) > MAX_MANIFEST_BYTES as usize {
                self.exceeded = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "tree manifest exceeds load limit",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        exceeded: false,
    };
    serde_json::to_writer(&mut writer, &manifest).map_err(|error| {
        if writer.exceeded {
            RuntimeError::InvalidInput(format!(
                "tree manifest exceeds {MAX_MANIFEST_BYTES}-byte load limit"
            ))
        } else {
            RuntimeError::InvalidInput(format!("tree manifest serialization failed: {error}"))
        }
    })?;
    Ok(writer.bytes)
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

/// Validate and store entries from any producer; returns a loadable tree ref.
pub async fn store(rt: &KhiveRuntime, entries: &[TreeEntry]) -> Result<String, RuntimeError> {
    let entries = validate_entries(entries)?;
    let bytes = canonical_bytes(&entries)?;
    let store = blob_store(rt)?;
    let content_ref = store.put(bytes).await?;
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
    let hydrator = rt
        .blob_hydrator()
        .ok_or_else(|| RuntimeError::Unconfigured("exec blob hydrator is not installed".into()))?;
    let bytes = hydrator
        .hydrate_verified(&content_ref, MAX_MANIFEST_BYTES)
        .await?;
    let manifest: Manifest = serde_json::from_slice(bytes.bytes()).map_err(|e| {
        RuntimeError::InvalidInput(format!("tree {tree_ref} is not a khive-tree manifest: {e}"))
    })?;
    drop(bytes);
    if manifest.schema != TREE_SCHEMA {
        return Err(RuntimeError::InvalidInput(format!(
            "tree {tree_ref} has schema {:?}; expected {TREE_SCHEMA:?}",
            manifest.schema
        )));
    }
    let entries = validate_entries(&manifest.entries)?;
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

#[cfg(test)]
#[path = "blob_tree/tests.rs"]
mod tests;
