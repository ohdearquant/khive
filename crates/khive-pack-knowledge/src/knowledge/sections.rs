//! Section handlers: edit, import, challenge, adjudicate; markdown parsing helpers.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{
    AdjudicateParams, ChallengeParams, EditParams, ImportParams, Section, SectionType,
};
use super::sections_index::embed_sections;
use super::util::resolve_atom_id;
use super::util::{
    content_hash, deser, new_id, now_us, row_str, sql_err, validate_atom_content,
    validate_section_content,
};
use super::vamana;
use super::KnowledgeHandlers;

// ─── section helpers ──────────────────────────────────────────────────────────

fn count_tokens(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

fn parse_section_type(s: &str) -> Result<SectionType, RuntimeError> {
    SectionType::from_str_loose(s).ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "unknown section_type {s:?}; valid values: {}",
            SectionType::NAMES.join(", ")
        ))
    })
}

/// Deserialise a `knowledge_sections` SQL row into a [`Section`].
/// Returns `None` when the row carries an invalid UUID or unknown `section_type`.
pub(super) fn section_from_row(row: &khive_storage::types::SqlRow) -> Option<Section> {
    let id: Uuid = row_str(row, "id")?.parse().ok()?;
    let st_str = row_str(row, "section_type")?;
    let section_type = SectionType::from_str_loose(&st_str)?;
    Some(Section {
        id,
        atom_id: row_str(row, "atom_id")?,
        namespace: row_str(row, "namespace")?,
        section_type,
        heading: row_str(row, "heading").unwrap_or_default(),
        content: row_str(row, "content").unwrap_or_default(),
        content_hash: row_str(row, "content_hash").unwrap_or_default(),
        status: row_str(row, "status").unwrap_or_else(|| "draft".into()),
        tokens: super::util::row_i64(row, "tokens").unwrap_or(0),
        sort_order: super::util::row_i64(row, "sort_order").unwrap_or(0),
        created_at: super::util::row_i64(row, "created_at").unwrap_or(0),
        updated_at: super::util::row_i64(row, "updated_at").unwrap_or(0),
    })
}

/// Serialise a [`Section`] to its wire JSON shape for `knowledge.get` responses.
/// Fields: `id`, `atom_id`, `namespace`, `section_type`, `heading`, `content`,
/// `content_hash`, `status`, `tokens`, `sort_order`, `created_at`, `updated_at`.
pub(super) fn section_to_json(s: &Section) -> Value {
    json!({
        "id": s.id.to_string(),
        "atom_id": s.atom_id,
        "namespace": s.namespace,
        "section_type": s.section_type.as_str(),
        "heading": s.heading,
        "content": s.content,
        "content_hash": s.content_hash,
        "status": s.status,
        "tokens": s.tokens,
        "sort_order": s.sort_order,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

// ─── markdown parsing helpers ─────────────────────────────────────────────────

const MAX_IMPORT_DEPTH: usize = 32;
const MAX_IMPORT_ENTRIES: usize = 100_000;
const MAX_IMPORT_FILES: usize = 10_000;

/// Per-file cap for `knowledge.import` source reads. Sized generously above
/// any legitimate markdown atom (a full research paper rarely exceeds a few
/// hundred KiB of prose) while still bounding worst-case memory for a single
/// file read. Mirrors the crate-wide byte-limit idiom used for
/// `MAX_CARGO_MANIFEST_BYTES` (khive-repo-showcase) and `DAEMON_LOG_MAX_BYTES`
/// (khive-mcp): a `metadata.len()` pre-check plus a `take(cap + 1)` read-time
/// re-check so a file that grows after the pre-check is still caught.
const MAX_IMPORT_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Aggregate cap across every file read by one `knowledge.import` call.
/// `MAX_IMPORT_FILES` alone bounds file *count*, not total bytes — up to
/// 10,000 files each just under `MAX_IMPORT_FILE_BYTES` could otherwise sum
/// to tens of GB of prepared document content in one call. This caps total
/// import size independent of how it's distributed across files.
const MAX_IMPORT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct ImportTraversalLimits {
    max_depth: usize,
    max_entries: usize,
    max_files: usize,
}

const IMPORT_TRAVERSAL_LIMITS: ImportTraversalLimits = ImportTraversalLimits {
    max_depth: MAX_IMPORT_DEPTH,
    max_entries: MAX_IMPORT_ENTRIES,
    max_files: MAX_IMPORT_FILES,
};

#[derive(Debug)]
struct ImportDiscovery {
    files: Vec<PathBuf>,
    entries_visited: usize,
    files_skipped: usize,
}

fn traversal_error(
    path: &Path,
    entries_visited: usize,
    error: impl std::fmt::Display,
) -> RuntimeError {
    RuntimeError::InvalidInput(format!(
        "knowledge.import traversal failed at {:?} after {entries_visited} entries: {error}",
        path
    ))
}

fn traversal_limit_error(
    limit_kind: &str,
    path: &Path,
    depth: usize,
    entries_visited: usize,
    files_discovered: usize,
    limits: ImportTraversalLimits,
) -> RuntimeError {
    RuntimeError::InvalidInput(format!(
        "knowledge.import traversal {limit_kind} limit exceeded at {:?}: \
         depth={depth} max_depth={} entries_visited={entries_visited} max_entries={} \
         files_discovered={files_discovered} max_files={}",
        path, limits.max_depth, limits.max_entries, limits.max_files
    ))
}

/// Rebuild the caller path from lexical components before metadata inspection.
/// In particular, this removes a trailing separator that would otherwise make
/// `symlink_metadata("directory-link/")` dereference the final symlink on Unix.
fn normalize_import_root(path: &Path) -> PathBuf {
    path.components().collect()
}

fn collect_md_files_with_limits(
    root: &Path,
    limits: ImportTraversalLimits,
) -> Result<ImportDiscovery, RuntimeError> {
    let mut directories = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut files = Vec::new();
    let mut entries_visited = 0usize;
    let mut files_skipped = 0usize;

    while let Some((directory, depth)) = directories.pop_front() {
        let read_dir = std::fs::read_dir(&directory)
            .map_err(|error| traversal_error(&directory, entries_visited, error))?;
        let mut entries = Vec::new();
        for entry in read_dir {
            let entry =
                entry.map_err(|error| traversal_error(&directory, entries_visited, error))?;
            let next_entries_visited = entries_visited.saturating_add(1);
            if next_entries_visited > limits.max_entries {
                return Err(traversal_limit_error(
                    "entry",
                    &entry.path(),
                    depth,
                    next_entries_visited,
                    files.len(),
                    limits,
                ));
            }
            entries_visited = next_entries_visited;
            entries.push(entry);
        }
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| traversal_error(&path, entries_visited, error))?;
            if file_type.is_symlink() {
                files_skipped = files_skipped.saturating_add(1);
            } else if file_type.is_dir() {
                let child_depth = depth.saturating_add(1);
                if child_depth > limits.max_depth {
                    return Err(traversal_limit_error(
                        "depth",
                        &path,
                        child_depth,
                        entries_visited,
                        files.len(),
                        limits,
                    ));
                }
                directories.push_back((path, child_depth));
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("md")
            {
                let files_discovered = files.len().saturating_add(1);
                if files_discovered > limits.max_files {
                    return Err(traversal_limit_error(
                        "markdown file",
                        &path,
                        depth,
                        entries_visited,
                        files_discovered,
                        limits,
                    ));
                }
                files.push(path);
            } else {
                files_skipped = files_skipped.saturating_add(1);
            }
        }
    }

    files.sort();
    Ok(ImportDiscovery {
        files,
        entries_visited,
        files_skipped,
    })
}

fn collect_md_files(root: &Path) -> Result<ImportDiscovery, RuntimeError> {
    collect_md_files_with_limits(root, IMPORT_TRAVERSAL_LIMITS)
}

pub(super) fn to_slug(stem: &str) -> String {
    stem.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug)]
struct ImportSourceIdentity {
    slug: String,
    source_path: String,
}

fn import_source_identity(
    root: &Path,
    file: &Path,
    root_is_file: bool,
) -> Result<ImportSourceIdentity, RuntimeError> {
    let relative = if root_is_file {
        file.file_name().map(PathBuf::from).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("import file has no filename: {file:?}"))
        })?
    } else {
        file.strip_prefix(root).map(PathBuf::from).map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "import file {file:?} is outside traversal root {root:?}"
            ))
        })?
    };

    let mut source_components = Vec::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(RuntimeError::InvalidInput(format!(
                "import source path is not root-relative: {relative:?}"
            )));
        };
        let component = component.to_str().ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "import source path is not valid UTF-8: {relative:?}"
            ))
        })?;
        source_components.push(component.to_string());
    }
    let source_path = source_components.join("/");
    let filename = source_components.last().ok_or_else(|| {
        RuntimeError::InvalidInput(format!("import source path is empty: {relative:?}"))
    })?;
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "import source filename has no valid UTF-8 stem: {relative:?}"
            ))
        })?;

    let mut slug_components = source_components[..source_components.len() - 1]
        .iter()
        .map(|component| to_slug(component))
        .collect::<Vec<_>>();
    slug_components.push(to_slug(stem));
    if slug_components.iter().any(String::is_empty) {
        return Err(RuntimeError::InvalidInput(format!(
            "import source path normalizes to an empty slug component: {source_path:?}"
        )));
    }

    Ok(ImportSourceIdentity {
        slug: slug_components.join("--"),
        source_path,
    })
}

#[derive(Debug)]
struct PreparedSection {
    section_type: SectionType,
    heading: String,
    content: String,
}

#[derive(Debug)]
struct PreparedImportFile {
    slug: String,
    name: String,
    atom_content: String,
    properties: serde_json::Map<String, Value>,
    source_uri: String,
    source_type: &'static str,
    sections: Vec<PreparedSection>,
    sections_discovered: usize,
    sections_skipped: usize,
}

/// Open `file` for reading without following a symlink at its final
/// component. Closes the gap between [`collect_md_files_with_limits`]'s
/// traversal-time `file_type()` check (or the root-file `symlink_metadata`
/// check in [`import`](super::KnowledgeHandlers::import)) and the later
/// content read: without `O_NOFOLLOW` an attacker who swaps the path for a
/// symlink or special file in that window would have their read followed
/// transparently. The subsequent [`read_import_file`] call `fstat`s this
/// same handle rather than the path, so the type check and the read
/// observe the same inode.
#[cfg(unix)]
fn open_import_file_handle(file: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(file)
}

/// Best-effort fallback where `O_NOFOLLOW` isn't available: the opened
/// handle is still `fstat`-checked in [`read_import_file`], just without
/// the open-time symlink refusal.
#[cfg(not(unix))]
fn open_import_file_handle(file: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open(file)
}

/// Read one import source through an opened, `fstat`-checked handle and
/// enforce the per-file and running aggregate byte caps. `total_bytes` is
/// threaded across every file in one `knowledge.import` call so the
/// aggregate cap trips regardless of how the total is distributed across
/// files.
fn read_import_file(
    file: &Path,
    source_path: &str,
    total_bytes: &mut u64,
) -> Result<String, RuntimeError> {
    use std::io::Read;

    let mut handle = open_import_file_handle(file).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "failed to open import source {source_path:?}: {error}"
        ))
    })?;
    let metadata = handle.metadata().map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "failed to inspect opened import source {source_path:?}: {error}"
        ))
    })?;
    if !metadata.is_file() {
        return Err(RuntimeError::InvalidInput(format!(
            "import source {source_path:?} is not a regular file on the opened handle \
             (symlink or special-file swap after validation)"
        )));
    }
    if metadata.len() > MAX_IMPORT_FILE_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "import source {source_path:?} is {} bytes, exceeding the per-file cap of \
             {MAX_IMPORT_FILE_BYTES} bytes",
            metadata.len()
        )));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    handle
        .by_ref()
        .take(MAX_IMPORT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            RuntimeError::InvalidInput(format!(
                "failed to read import source {source_path:?}: {error}"
            ))
        })?;
    if bytes.len() as u64 > MAX_IMPORT_FILE_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "import source {source_path:?} grew beyond the per-file cap of \
             {MAX_IMPORT_FILE_BYTES} bytes while being read"
        )));
    }

    *total_bytes = total_bytes.saturating_add(bytes.len() as u64);
    if *total_bytes > MAX_IMPORT_TOTAL_BYTES {
        return Err(RuntimeError::InvalidInput(format!(
            "knowledge.import aggregate size {total_bytes} bytes exceeds the total-import \
             cap of {MAX_IMPORT_TOTAL_BYTES} bytes at {source_path:?}"
        )));
    }

    String::from_utf8(bytes).map_err(|_| {
        RuntimeError::InvalidInput(format!("import source {source_path:?} is not valid UTF-8"))
    })
}

fn prepare_import_file(
    file: &Path,
    identity: ImportSourceIdentity,
    chunk_strategy: &str,
    total_bytes: &mut u64,
) -> Result<PreparedImportFile, RuntimeError> {
    let content = read_import_file(file, &identity.source_path, total_bytes)?;
    let (atom_name, atom_body, parsed_sections) = parse_atlas_md(&content);
    let atlas_id = extract_atlas_id(&content);
    let name = if atom_name.is_empty() {
        identity
            .slug
            .rsplit("--")
            .next()
            .unwrap_or(&identity.slug)
            .replace('-', " ")
    } else {
        atom_name
    };
    let atom_content = if chunk_strategy == "atom" {
        content
    } else if atom_body.split_whitespace().count() >= super::util::MIN_ATOM_CONTENT_WORDS {
        atom_body
    } else {
        parsed_sections
            .iter()
            .map(|(_, _, body)| body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    validate_atom_content(&atom_content)?;

    let citation_count = parsed_sections
        .iter()
        .filter(|(section_type, _, _)| *section_type == SectionType::References)
        .map(|(_, _, body)| body.lines().filter(|line| !line.trim().is_empty()).count())
        .sum::<usize>();
    let source_uri = atlas_id
        .as_ref()
        .map(|id| format!("atlas:{id}"))
        .unwrap_or_else(|| format!("file:{}", identity.source_path));
    let source_type = if citation_count > 0 {
        "paper"
    } else {
        "imported"
    };
    let mut properties = serde_json::Map::new();
    properties.insert(
        "source_path".to_string(),
        Value::String(identity.source_path.clone()),
    );
    if let Some(id) = atlas_id {
        properties.insert("atlas_id".to_string(), Value::String(id));
    }

    let sections_discovered = parsed_sections.len();
    let (sections, sections_skipped) = if chunk_strategy == "section" {
        let mut prepared = Vec::new();
        let mut skipped = 0usize;
        for (section_type, heading, content) in parsed_sections {
            if content.len() < super::util::MIN_SECTION_CONTENT_LEN {
                skipped = skipped.saturating_add(1);
                continue;
            }
            validate_section_content(&content)?;
            khive_runtime::secret_gate::check(&heading)?;
            khive_runtime::secret_gate::check(&content)?;
            prepared.push(PreparedSection {
                section_type,
                heading,
                content,
            });
        }
        (prepared, skipped)
    } else {
        (Vec::new(), 0)
    };

    khive_runtime::secret_gate::check(&identity.slug)?;
    khive_runtime::secret_gate::check(&name)?;
    khive_runtime::secret_gate::check(&atom_content)?;
    khive_runtime::secret_gate::check_json(&Value::Object(properties.clone()))?;
    khive_runtime::secret_gate::check(&source_uri)?;
    khive_runtime::secret_gate::check(source_type)?;

    Ok(PreparedImportFile {
        slug: identity.slug,
        name,
        atom_content,
        properties,
        source_uri,
        source_type,
        sections,
        sections_discovered,
        sections_skipped,
    })
}

fn extract_atlas_id(content: &str) -> Option<String> {
    content.lines().take(32).find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("atlas_id:")
            .or_else(|| trimmed.strip_prefix("atlas-id:"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn parse_atlas_md(content: &str) -> (String, String, Vec<(SectionType, String, String)>) {
    let mut name = String::new();
    let mut pre_body = String::new();
    let mut sections: Vec<(SectionType, String, String)> = Vec::new();

    let mut current_heading: Option<(SectionType, String)> = None;
    let mut current_body = String::new();
    let mut in_pre = true;

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if name.is_empty() && current_heading.is_none() && current_body.trim().is_empty() {
                name = rest.trim().to_string();
                continue;
            }
        }

        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((stype, heading)) = current_heading.take() {
                sections.push((stype, heading, current_body.trim_end().to_string()));
                current_body.clear();
            } else if in_pre {
                pre_body = current_body.trim_end().to_string();
                current_body.clear();
                in_pre = false;
            }
            let heading_text = rest.trim().to_string();
            let stype = SectionType::from_str_loose(&heading_text).unwrap_or(SectionType::Other);
            current_heading = Some((stype, heading_text));
            continue;
        }
        current_body.push_str(line);
        current_body.push('\n');
    }

    if let Some((stype, heading)) = current_heading {
        sections.push((stype, heading, current_body.trim_end().to_string()));
    } else {
        pre_body = current_body.trim_end().to_string();
    }

    (name, pre_body, sections)
}

// ─── handler impls ────────────────────────────────────────────────────────────

impl KnowledgeHandlers {
    pub(crate) async fn edit(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: EditParams = deser(params)?;
        if p.sections.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "sections must not be empty".into(),
            ));
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let atom_id = {
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit atom reader", e))?;
            let id = p.id.trim().to_string();
            let row = if id.parse::<Uuid>().is_ok() {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE id = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by id", e))?
            } else {
                reader
                    .query_row(SqlStatement {
                        sql: "SELECT id FROM knowledge_atoms WHERE slug = ?1 AND namespace = ?2 AND deleted_at IS NULL LIMIT 1".into(),
                        params: vec![SqlValue::Text(id.clone()), SqlValue::Text(ns.clone())],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit atom lookup by slug", e))?
            };
            row.and_then(|r| row_str(&r, "id"))
                .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {:?}", p.id)))?
        };

        let now = now_us();
        let mut upserted = 0usize;
        let mut section_results: Vec<Value> = Vec::with_capacity(p.sections.len());

        for su in &p.sections {
            let stype = parse_section_type(&su.section_type)?;
            validate_section_content(&su.content)?;
            // Secret gate: scan section content and heading before any write.
            khive_runtime::secret_gate::check(&su.content)?;
            if let Some(ref h) = su.heading {
                khive_runtime::secret_gate::check(h)?;
            }
            let heading = su.heading.as_deref().unwrap_or(stype.as_str()).to_string();
            let tokens = count_tokens(&su.content);
            let sort_order = su.sort_order.unwrap_or_else(|| {
                SectionType::ALL
                    .iter()
                    .position(|&t| t == stype)
                    .unwrap_or(9) as i64
            });
            let hash = content_hash(&su.content);

            // Sections are content-addressed: the dedup key is (atom_id, content_hash),
            // matching the UNIQUE constraint. Identical content is an idempotent
            // metadata refresh; distinct content inserts a new row, so repeated
            // section types with differing content coexist as sibling rows.
            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("edit section reader", e))?;
            let existing_section = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_sections \
                          WHERE atom_id = ?1 AND content_hash = ?2 LIMIT 1"
                        .into(),
                    params: vec![
                        SqlValue::Text(atom_id.clone()),
                        SqlValue::Text(hash.clone()),
                    ],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("edit section lookup", e))?;

            let section_id = existing_section
                .as_ref()
                .and_then(|r| row_str(r, "id"))
                .unwrap_or_else(new_id);

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("edit section writer", e))?;

            if existing_section.is_some() {
                // Identical content already stored: refresh metadata only. Content
                // is unchanged, so the embedding and verification status stay valid.
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE knowledge_sections SET \
                              heading=?1, tokens=?2, sort_order=?3, updated_at=?4 \
                              WHERE id=?5"
                            .into(),
                        params: vec![
                            SqlValue::Text(heading.clone()),
                            SqlValue::Integer(tokens),
                            SqlValue::Integer(sort_order),
                            SqlValue::Integer(now),
                            SqlValue::Text(section_id.clone()),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit section update", e))?;
            } else {
                // New content: insert a fresh row, leaving any sibling sections
                // (including verified ones of the same type) untouched.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO knowledge_sections \
                              (id, atom_id, namespace, section_type, heading, content, \
                               content_hash, tokens, sort_order, created_at, updated_at) \
                              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
                            .into(),
                        params: vec![
                            SqlValue::Text(section_id.clone()),
                            SqlValue::Text(atom_id.clone()),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Text(stype.as_str().to_string()),
                            SqlValue::Text(heading.clone()),
                            SqlValue::Text(su.content.clone()),
                            SqlValue::Text(hash.clone()),
                            SqlValue::Integer(tokens),
                            SqlValue::Integer(sort_order),
                            SqlValue::Integer(now),
                            SqlValue::Integer(now),
                        ],
                        label: None,
                    })
                    .await
                    .map_err(|e| sql_err("edit section insert", e))?;
            }

            upserted += 1;
            section_results.push(json!({
                "id": section_id,
                "atom_id": atom_id,
                "section_type": stype.as_str(),
                "heading": heading,
                "tokens": tokens,
                "content_hash": hash,
            }));
        }

        // Inline re-embed: newly-inserted section rows (embedding IS NULL) are embedded
        // via the shared embed_sections path so the hybrid section-cosine read path
        // (ADR-051) is fresh without a manual reindex. Byte-identical sections go
        // through the metadata-only UPDATE branch above and keep their existing vector.
        // The Vamana ANN snapshot rebuild is deferred (per-edit cost too high);
        // approximate ANN recall over new vectors lags until the next kkernel reindex.
        // Missing embedder: embed_sections returns zero counters and an empty
        // truncation report immediately, so edit succeeds.
        let (_, _, _, section_truncation) =
            embed_sections(runtime, token, false, 32, None, Some(&atom_id)).await?;

        // Refresh this atom's vector-store entry (knowledge.atom field) so atom-granularity
        // semantic recall is also fresh. rebuild_ann=false writes the vector immediately
        // while deferring the Vamana ANN snapshot build — the same ANN-deferral tradeoff
        // taken for sections above.
        // Missing embedder: index early-returns {failed:0, reason:"no embedding model
        // configured"} — failed==0 but nothing was written, so check for reason too.
        let (atom_vector_refreshed, mut truncation_by_model) = {
            let atom_params = serde_json::json!({
                "ids": [atom_id],
                "rebuild_ann": false,
                "insert_only": false,
            });
            let result = KnowledgeHandlers::index(runtime, token, atom_params, ann, None).await?;
            let failed = result.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
            let skipped_no_embedder = result.get("reason").is_some();
            if failed > 0 {
                tracing::warn!(
                    atom_id = %atom_id,
                    failed = failed,
                    "knowledge.edit: atom vector refresh failed; \
                    hybrid recall for this atom may be stale until next reindex"
                );
            }
            let truncation_by_model = match result.get("truncation_by_model") {
                Some(value) => serde_json::from_value::<
                    BTreeMap<String, khive_runtime::retrieval::EmbeddingTruncationReport>,
                >(value.clone())
                .map_err(|e| {
                    RuntimeError::Internal(format!(
                        "knowledge.edit: invalid knowledge.index truncation report: {e}"
                    ))
                })?,
                None => BTreeMap::new(),
            };
            (failed == 0 && !skipped_no_embedder, truncation_by_model)
        };

        // The section pass uses the default embedder. Merge its actual outcome
        // into the atom pass's per-model report before deriving the advisory,
        // so one response accounts for every embedding input this edit sent.
        let default_model_name = runtime.default_embedder_name();
        if !default_model_name.is_empty() {
            truncation_by_model
                .entry(default_model_name.to_string())
                .or_default()
                .merge(section_truncation);
        }
        let any_truncated = truncation_by_model
            .values()
            .any(khive_runtime::retrieval::EmbeddingTruncationReport::any_truncated);

        let mut response = json!({
            "atom_id": atom_id,
            "upserted": upserted,
            "sections": section_results,
            "atom_vector_refreshed": atom_vector_refreshed,
            "truncation_by_model": truncation_by_model,
        });
        if any_truncated {
            response
                .as_object_mut()
                .expect("edit response is an object")
                .insert(
                    "warnings".to_string(),
                    json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
                );
        }
        Ok(response)
    }

    pub(crate) async fn import(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        ann: &vamana::SharedAnn,
    ) -> Result<Value, RuntimeError> {
        let p: ImportParams = deser(params)?;
        let path_str = p.path.trim().to_string();
        if path_str.is_empty() {
            return Err(RuntimeError::InvalidInput("path must not be empty".into()));
        }

        let chunk_strategy = p
            .chunk_strategy
            .as_deref()
            .unwrap_or("section")
            .to_ascii_lowercase();
        if !["section", "atom"].contains(&chunk_strategy.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown chunk_strategy {:?}; valid: section | atom",
                chunk_strategy
            )));
        }
        let format = p.format.as_deref().unwrap_or("atlas_md");
        if format != "atlas_md" {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown format {format:?}; only \"atlas_md\" is supported"
            )));
        }

        let normalized_md_path = normalize_import_root(Path::new(&path_str));
        let md_path = normalized_md_path.as_path();
        let metadata = std::fs::symlink_metadata(md_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::NotFound(format!("path does not exist: {path_str:?}"))
            } else {
                RuntimeError::InvalidInput(format!(
                    "failed to inspect import path {path_str:?}: {error}"
                ))
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(RuntimeError::InvalidInput(format!(
                "import path must not be a symbolic link: {path_str:?}"
            )));
        }

        let root_is_file = metadata.is_file();
        let discovery = if root_is_file {
            if md_path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                return Err(RuntimeError::InvalidInput(format!(
                    "import file must have a .md extension: {path_str:?}"
                )));
            }
            ImportDiscovery {
                files: vec![md_path.to_path_buf()],
                entries_visited: 1,
                files_skipped: 0,
            }
        } else if metadata.is_dir() {
            collect_md_files(md_path)?
        } else {
            return Err(RuntimeError::InvalidInput(format!(
                "path is not a regular file or directory: {path_str:?}"
            )));
        };

        let mut identities = Vec::with_capacity(discovery.files.len());
        let mut sources_by_slug = BTreeMap::new();
        for file in &discovery.files {
            let identity = import_source_identity(md_path, file, root_is_file)?;
            if let Some(previous_source) =
                sources_by_slug.insert(identity.slug.clone(), identity.source_path.clone())
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "normalized slug collision for {:?}: {:?} and {:?}",
                    identity.slug, previous_source, identity.source_path
                )));
            }
            identities.push((file.clone(), identity));
        }

        let mut prepared_files = Vec::with_capacity(identities.len());
        let mut total_bytes = 0u64;
        for (file, identity) in identities {
            prepared_files.push(prepare_import_file(
                &file,
                identity,
                &chunk_strategy,
                &mut total_bytes,
            )?);
        }

        let sections_discovered = prepared_files
            .iter()
            .map(|prepared| prepared.sections_discovered)
            .sum::<usize>();
        let sections_skipped = prepared_files
            .iter()
            .map(|prepared| prepared.sections_skipped)
            .sum::<usize>();
        let mut imported_atoms = 0usize;
        let mut imported_sections = 0usize;

        for prepared in &prepared_files {
            let upsert_params = serde_json::json!({
                "atoms": [{
                    "slug": prepared.slug,
                    "name": prepared.name,
                    "content": prepared.atom_content,
                    "properties": Value::Object(prepared.properties.clone()),
                    "source_uri": prepared.source_uri,
                    "source_type": prepared.source_type,
                    "finalized": true,
                }]
            });
            KnowledgeHandlers::upsert_import_atoms(runtime, token, upsert_params).await?;
            imported_atoms += 1;

            if !prepared.sections.is_empty() {
                let section_updates = prepared
                    .sections
                    .iter()
                    .map(|section| {
                        json!({
                            "section_type": section.section_type.as_str(),
                            "heading": section.heading,
                            "content": section.content,
                        })
                    })
                    .collect::<Vec<_>>();
                let edit_params = json!({
                    "id": prepared.slug,
                    "sections": section_updates,
                });
                let result = KnowledgeHandlers::edit(runtime, token, edit_params, ann).await?;
                if let Some(count) = result.get("upserted").and_then(Value::as_u64) {
                    imported_sections += count as usize;
                }
            }
        }

        Ok(json!({
            "imported_atoms": imported_atoms,
            "imported_sections": imported_sections,
            "files_processed": imported_atoms,
            "entries_visited": discovery.entries_visited,
            "files_discovered": discovery.files.len(),
            "files_skipped": discovery.files_skipped,
            "traversal_errors": 0,
            "sections_discovered": sections_discovered,
            "sections_skipped": sections_skipped,
        }))
    }

    pub(crate) async fn challenge(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ChallengeParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;
        let hash = p
            .content_hash
            .as_ref()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());

        // Same-type sibling sections are valid (UNIQUE(atom_id, content_hash)),
        // so section_type alone no longer identifies one section. Resolve the
        // single eligible target before mutating: a content_hash pins it
        // exactly, otherwise there must be exactly one eligible section.
        let target_hash = Self::resolve_section_hash(
            runtime,
            &atom_id,
            stype,
            hash.as_deref(),
            "status NOT IN ('disputed','deprecated')",
            "section not found, already disputed, or deprecated",
        )
        .await?;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("challenge writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE knowledge_sections SET status='disputed' \
                      WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 \
                      AND status NOT IN ('disputed','deprecated')"
                    .into(),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                    SqlValue::Text(target_hash.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found, already disputed, or deprecated".into(),
            ));
        }

        // json_set targets the fixed nested path `$.dispute_count` only; no caller
        // input reaches this statement, so it cannot create or replace the
        // top-level reserved property key.
        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',coalesce(json_extract(properties,'$.dispute_count'),0)+{affected}) WHERE id=?1 AND namespace=?2"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("challenge dispute_count increment", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "content_hash": target_hash,
            "disputed": affected,
            "reason": p.reason,
        }))
    }

    /// Resolve the single section of `stype` on `atom_id` that the lifecycle
    /// verbs should act on. `hash` pins an exact sibling; without it there must
    /// be exactly one section matching `status_filter`, otherwise the call is
    /// ambiguous and is rejected. Returns the target `content_hash`.
    async fn resolve_section_hash(
        runtime: &KhiveRuntime,
        atom_id: &str,
        stype: SectionType,
        hash: Option<&str>,
        status_filter: &str,
        not_found_msg: &str,
    ) -> Result<String, RuntimeError> {
        let sql = runtime.sql();
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("section resolve reader", e))?;

        let mut query = format!(
            "SELECT content_hash FROM knowledge_sections \
             WHERE atom_id=?1 AND section_type=?2 AND {status_filter}"
        );
        let mut params = vec![
            SqlValue::Text(atom_id.to_owned()),
            SqlValue::Text(stype.as_str().to_string()),
        ];
        if let Some(h) = hash {
            query.push_str(" AND content_hash=?3");
            params.push(SqlValue::Text(h.to_owned()));
        }

        let rows = reader
            .query_all(SqlStatement {
                sql: query,
                params,
                label: None,
            })
            .await
            .map_err(|e| sql_err("section resolve", e))?;

        if rows.is_empty() {
            return Err(RuntimeError::InvalidInput(not_found_msg.to_owned()));
        }
        if hash.is_none() && rows.len() > 1 {
            return Err(RuntimeError::InvalidInput(format!(
                "atom has {} '{}' sections matching; specify content_hash to target one",
                rows.len(),
                stype.as_str(),
            )));
        }
        rows.first()
            .and_then(|r| row_str(r, "content_hash"))
            .ok_or_else(|| RuntimeError::Internal("section row missing content_hash".into()))
    }

    pub(crate) async fn adjudicate(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: AdjudicateParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();

        let resolution = p.resolution.trim().to_ascii_lowercase();
        if resolution != "accept" && resolution != "reject" {
            return Err(RuntimeError::InvalidInput(
                "resolution must be \"accept\" or \"reject\"".into(),
            ));
        }

        let atom_id = resolve_atom_id(runtime, &ns, &p.atom_id).await?;
        let stype = parse_section_type(&p.section_type)?;
        let hash = p
            .content_hash
            .as_ref()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());

        let new_status = if resolution == "accept" {
            "verified"
        } else {
            "reviewed"
        };

        // Target a single disputed section — same-type siblings can be disputed
        // independently, so resolve one before resolving its lifecycle.
        let target_hash = Self::resolve_section_hash(
            runtime,
            &atom_id,
            stype,
            hash.as_deref(),
            "status='disputed'",
            "section not found or not in disputed state",
        )
        .await?;

        let mut writer = sql
            .writer()
            .await
            .map_err(|e| sql_err("adjudicate writer", e))?;

        let affected = writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_sections SET status='{new_status}' \
                     WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 AND status='disputed'"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(stype.as_str().to_string()),
                    SqlValue::Text(target_hash.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate section status", e))?;

        if affected == 0 {
            return Err(RuntimeError::InvalidInput(
                "section not found or not in disputed state".into(),
            ));
        }

        // json_set targets the fixed nested path `$.dispute_count` only; no caller
        // input reaches this statement, so it cannot create or replace the
        // top-level reserved property key.
        writer
            .execute(SqlStatement {
                sql: format!(
                    "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',CASE WHEN coalesce(json_extract(properties,'$.dispute_count'),0) >= {affected} THEN coalesce(json_extract(properties,'$.dispute_count'),0)-{affected} ELSE 0 END) WHERE id=?1 AND namespace=?2"
                ),
                params: vec![
                    SqlValue::Text(atom_id.clone()),
                    SqlValue::Text(ns.clone()),
                ],
                label: None,
            })
            .await
            .map_err(|e| sql_err("adjudicate dispute_count decrement", e))?;

        Ok(json!({
            "atom_id": atom_id,
            "section_type": stype.as_str(),
            "content_hash": target_hash,
            "resolution": resolution,
            "new_status": new_status,
            "resolved": affected,
        }))
    }
}

#[cfg(test)]
mod import_traversal_tests {
    use super::{collect_md_files_with_limits, ImportTraversalLimits};
    use tempfile::TempDir;

    #[test]
    fn traversal_surfaces_root_read_errors() {
        let root = TempDir::new().expect("temp root");
        let missing = root.path().join("missing");
        let error = collect_md_files_with_limits(
            &missing,
            ImportTraversalLimits {
                max_depth: 8,
                max_entries: 100,
                max_files: 100,
            },
        )
        .expect_err("missing traversal root must be surfaced");
        assert!(error.to_string().contains("traversal failed"));
    }

    #[test]
    fn traversal_enforces_depth_limit() {
        let root = TempDir::new().expect("temp root");
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).expect("nested directory fixture");
        let error = collect_md_files_with_limits(
            root.path(),
            ImportTraversalLimits {
                max_depth: 0,
                max_entries: 100,
                max_files: 100,
            },
        )
        .expect_err("depth over limit must fail closed");
        let message = error.to_string();
        assert!(message.contains("depth limit"), "{message}");
        assert!(
            message.contains(nested.to_string_lossy().as_ref()),
            "{message}"
        );
        assert!(message.contains("depth=1"), "{message}");
        assert!(message.contains("max_depth=0"), "{message}");
        assert!(message.contains("entries_visited=1"), "{message}");
        assert!(message.contains("max_entries=100"), "{message}");
        assert!(message.contains("files_discovered=0"), "{message}");
        assert!(message.contains("max_files=100"), "{message}");
    }

    #[test]
    fn traversal_enforces_entry_limit() {
        let root = TempDir::new().expect("temp root");
        let first = root.path().join("a.md");
        std::fs::write(&first, "a").expect("fixture");
        let error = collect_md_files_with_limits(
            root.path(),
            ImportTraversalLimits {
                max_depth: 8,
                max_entries: 0,
                max_files: 100,
            },
        )
        .expect_err("entry over limit must fail closed");
        let message = error.to_string();
        assert!(message.contains("entry limit"), "{message}");
        assert!(
            message.contains(first.to_string_lossy().as_ref()),
            "{message}"
        );
        assert!(message.contains("entries_visited=1"), "{message}");
        assert!(message.contains("max_entries=0"), "{message}");
        assert!(message.contains("files_discovered=0"), "{message}");
        assert!(message.contains("max_files=100"), "{message}");
        assert!(message.contains("depth=0"), "{message}");
        assert!(message.contains("max_depth=8"), "{message}");
    }

    #[test]
    fn traversal_enforces_markdown_file_limit() {
        let root = TempDir::new().expect("temp root");
        std::fs::write(root.path().join("a.md"), "a").expect("first fixture");
        let second = root.path().join("b.md");
        std::fs::write(&second, "b").expect("second fixture");
        let error = collect_md_files_with_limits(
            root.path(),
            ImportTraversalLimits {
                max_depth: 8,
                max_entries: 100,
                max_files: 1,
            },
        )
        .expect_err("file over limit must fail closed");
        let message = error.to_string();
        assert!(message.contains("markdown file limit"), "{message}");
        assert!(
            message.contains(second.to_string_lossy().as_ref()),
            "{message}"
        );
        assert!(message.contains("files_discovered=2"), "{message}");
        assert!(message.contains("max_files=1"), "{message}");
        assert!(message.contains("entries_visited=2"), "{message}");
        assert!(message.contains("max_entries=100"), "{message}");
        assert!(message.contains("depth=0"), "{message}");
        assert!(message.contains("max_depth=8"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn traversal_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("temp root");
        let markdown = root.path().join("source.md");
        std::fs::write(&markdown, "source").expect("source fixture");
        symlink(&markdown, root.path().join("alias.md")).expect("symlink fixture");
        let discovery = collect_md_files_with_limits(
            root.path(),
            ImportTraversalLimits {
                max_depth: 8,
                max_entries: 100,
                max_files: 100,
            },
        )
        .expect("traversal");

        assert_eq!(discovery.files, vec![markdown]);
        assert_eq!(discovery.files_skipped, 1);
    }
}

#[cfg(test)]
mod import_read_tests {
    use super::{read_import_file, MAX_IMPORT_FILE_BYTES, MAX_IMPORT_TOTAL_BYTES};
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn symlink_swap_at_read_time_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("temp root");
        let outside = TempDir::new().expect("outside root");
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "outside content").expect("secret fixture");

        // Simulates the post-validation swap: the path traversal accepted
        // as a regular file is, by read time, a symlink pointing outside
        // the import root.
        let swapped = root.path().join("swapped.md");
        symlink(&secret, &swapped).expect("swap symlink fixture");

        let mut total_bytes = 0u64;
        let error = read_import_file(&swapped, "swapped.md", &mut total_bytes)
            .expect_err("a symlink swapped in after validation must be refused at read time");
        assert!(
            error.to_string().contains("failed to open import source"),
            "{error}"
        );
        assert_eq!(
            total_bytes, 0,
            "a rejected read must not count toward the aggregate cap"
        );
    }

    #[test]
    fn oversize_single_file_is_refused() {
        let root = TempDir::new().expect("temp root");
        let big = root.path().join("big.md");
        let file = std::fs::File::create(&big).expect("create big fixture");
        file.set_len(MAX_IMPORT_FILE_BYTES + 1)
            .expect("grow big fixture past the per-file cap");
        drop(file);

        let mut total_bytes = 0u64;
        let error = read_import_file(&big, "big.md", &mut total_bytes)
            .expect_err("a file over the per-file cap must be refused");
        assert!(
            error.to_string().contains("exceeding the per-file cap"),
            "{error}"
        );
        assert_eq!(
            total_bytes, 0,
            "a rejected read must not count toward the aggregate cap"
        );
    }

    #[test]
    fn aggregate_cap_is_refused_once_tripped() {
        let root = TempDir::new().expect("temp root");
        let small = root.path().join("small.md");
        std::fs::write(
            &small,
            "a small file that pushes the running total over the cap",
        )
        .expect("small fixture");

        // Seed the running total just under the aggregate cap so this one
        // small read is what tips it over, without needing to write a
        // 256 MiB fixture to disk.
        let mut total_bytes = MAX_IMPORT_TOTAL_BYTES - 5;
        let error = read_import_file(&small, "small.md", &mut total_bytes)
            .expect_err("a read that crosses the aggregate cap must be refused");
        assert!(
            error.to_string().contains("exceeds the total-import cap"),
            "{error}"
        );
    }

    #[test]
    fn clean_read_under_both_caps_succeeds() {
        let root = TempDir::new().expect("temp root");
        let clean = root.path().join("clean.md");
        std::fs::write(&clean, "# Clean\n\nWell under every cap.").expect("clean fixture");

        let mut total_bytes = 0u64;
        let content = read_import_file(&clean, "clean.md", &mut total_bytes)
            .expect("a file under both caps must read successfully");
        assert_eq!(content, "# Clean\n\nWell under every cap.");
        assert_eq!(total_bytes, content.len() as u64);
    }
}
