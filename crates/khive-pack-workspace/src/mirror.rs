//! One explicit workspace-tree pass. No poller or filesystem writeback.
//!
//! Main-assigned runtimes only. Each file commits document/lexical index/edge
//! and cursor together; post-commit effect failures remain committed outcomes.
//! Tree reads are synchronous, streamed, and not an adversarial snapshot.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use khive_runtime::{BackendId, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{
    EdgeFilter, EdgeSortField, PageRequest, SortDirection, SortOrder, StorageResult,
};
use khive_storage::{
    EntityFilter, SqlStatement, SqlValue, SqlWriter, StorageCapability, StorageError,
};
use khive_types::{EdgeRelation, Hash32, Id128};

use crate::{cursor, WorkspaceMirrorConfig};

pub(crate) const HISTORY_PAGE_SIZE: u32 = 50;

/// Confirmed commits, unchanged files, and included paths skipped for invalid UTF-8.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MirrorPassSummary {
    /// Files whose document/lexical/cursor transaction was confirmed committed.
    pub committed: usize,
    /// Files whose checksum already matched committed progress.
    pub unchanged: usize,
    /// Canonical-root paths whose bytes were not UTF-8; no document or cursor was written.
    pub skipped_non_utf8: Vec<PathBuf>,
}

/// State of the current file, separate from earlier committed files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileCommitState {
    /// No canonical write attempt for this file was made.
    NotAttempted,
    /// Preserve the native cause; an outer error is not a rollback receipt.
    Unconfirmed,
    /// Document, required lexical writes and cursor committed before effects failed.
    Committed(Id128),
}

#[derive(Debug)]
enum Failure {
    Runtime(RuntimeError),
    Io(std::io::Error),
}

/// Failure retaining progress, file identity and the native error cause.
#[derive(Debug)]
pub struct MirrorPassError {
    /// Earlier confirmed commits, including this file for a post-commit failure.
    pub progress: MirrorPassSummary,
    /// File/root associated with this error, if filesystem work began.
    pub file: Option<PathBuf>,
    /// Do not infer rollback from an unconfirmed outer Runtime error.
    pub commit_state: FileCommitState,
    cause: Box<Failure>,
}

impl MirrorPassError {
    fn runtime(
        progress: &MirrorPassSummary,
        file: Option<&Path>,
        commit_state: FileCommitState,
        error: RuntimeError,
    ) -> Self {
        Self {
            progress: progress.clone(),
            file: file.map(Path::to_owned),
            commit_state,
            cause: Box::new(Failure::Runtime(error)),
        }
    }

    fn io(progress: &MirrorPassSummary, file: &Path, error: std::io::Error) -> Self {
        Self {
            progress: progress.clone(),
            file: Some(file.to_owned()),
            commit_state: FileCommitState::NotAttempted,
            cause: Box::new(Failure::Io(error)),
        }
    }

    /// Retain typed Runtime errors, including the assigned-backend refusal.
    pub fn runtime_error(&self) -> Option<&RuntimeError> {
        match self.cause.as_ref() {
            Failure::Runtime(error) => Some(error),
            Failure::Io(_) => None,
        }
    }
}

impl fmt::Display for MirrorPassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "workspace mirror {:?}: ", self.commit_state)?;
        match self.cause.as_ref() {
            Failure::Runtime(error) => write!(f, "{error}"),
            Failure::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for MirrorPassError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.cause.as_ref() {
            Failure::Runtime(error) => Some(error),
            Failure::Io(error) => Some(error),
        }
    }
}

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidInput(message.to_owned())
}

fn seconds(time: SystemTime) -> Result<i64, RuntimeError> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs())
            .map_err(|_| invalid("workspace mirror timestamp overflow")),
        Err(error) => i64::try_from(error.duration().as_secs())
            .ok()
            .and_then(i64::checked_neg)
            .ok_or_else(|| invalid("workspace mirror timestamp overflow")),
    }
}

/// Percent-encoded canonical absolute path; Unix backslashes remain literal bytes.
pub(crate) fn file_uri(path: &Path) -> Option<String> {
    let raw = path.to_str()?;
    let normalized;
    let raw = if cfg!(windows) {
        normalized = raw
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!("//{}", rest.replace('\\', "/")))
            .unwrap_or_else(|| raw.strip_prefix(r"\\?\").unwrap_or(raw).replace('\\', "/"));
        normalized.as_str()
    } else {
        raw
    };
    let mut uri = if cfg!(windows) && raw.starts_with("//") {
        String::from("file:")
    } else if raw.starts_with('/') {
        String::from("file://")
    } else {
        String::from("file:///")
    };
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    Some(uri)
}

/// Cursor slots also partition by namespace; source_uri itself stays a file identity.
pub(crate) fn cursor_key(namespace: &str, source_uri: &str) -> String {
    format!("{}:{namespace}{source_uri}", namespace.len())
}

fn next_offset(offset: u64, count: usize) -> Result<u64, RuntimeError> {
    offset
        .checked_add(
            u64::try_from(count)
                .map_err(|_| invalid("workspace mirror history offset overflow"))?,
        )
        .filter(|next| *next <= i64::MAX as u64)
        .ok_or_else(|| invalid("workspace mirror history offset overflow"))
}

/// Lookup work is linear in this source URI's history, per file, per pass.
async fn previous_document(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    uri: &str,
) -> Result<Option<Id128>, RuntimeError> {
    let namespace = token.namespace().as_str();
    let store = runtime.entities(token)?;
    let filter = EntityFilter {
        kinds: vec!["document".to_owned()],
        namespaces: vec![namespace.to_owned()],
        ..EntityFilter::default()
    }
    .property_eq(
        "$.source_uri",
        khive_storage::types::SqlValue::Text(uri.to_owned()),
    );
    let mut history = HashSet::new();
    let mut offset = 0;
    loop {
        let page = store
            .query_entities(
                namespace,
                filter.clone(),
                PageRequest {
                    offset,
                    limit: HISTORY_PAGE_SIZE,
                },
            )
            .await?;
        if page.items.is_empty() {
            break;
        }
        offset = next_offset(offset, page.items.len())?;
        history.extend(page.items.into_iter().map(|entity| entity.id));
    }
    if history.is_empty() {
        return Ok(None);
    }
    let targets: Vec<_> = history.iter().copied().collect();
    let mut heads = history;
    let graph = runtime.graph(token)?;
    let namespaces = vec![namespace.to_owned()];
    for chunk in targets.chunks(HISTORY_PAGE_SIZE as usize) {
        let filter = EdgeFilter {
            ids: vec![],
            source_ids: vec![],
            target_ids: chunk.to_vec(),
            relations: vec![EdgeRelation::Supersedes],
            min_weight: None,
            max_weight: None,
            created_at: None,
        };
        let mut offset = 0;
        loop {
            let page = graph
                .query_edges_in_namespaces(
                    &namespaces,
                    filter.clone(),
                    vec![SortOrder {
                        field: EdgeSortField::CreatedAt,
                        direction: SortDirection::Asc,
                    }],
                    PageRequest {
                        offset,
                        limit: HISTORY_PAGE_SIZE,
                    },
                )
                .await?;
            if page.items.is_empty() {
                break;
            }
            offset = next_offset(offset, page.items.len())?;
            for edge in page.items {
                heads.remove(&edge.target_id);
            }
        }
    }
    if heads.len() != 1 {
        return Err(invalid(
            "workspace mirror source URI has no unique current document",
        ));
    }
    Ok(heads
        .into_iter()
        .next()
        .map(|id| Id128::from_bytes(*id.as_bytes())))
}

struct CursorAdvance {
    key: String,
    namespace: String,
    uri: String,
    expected: Option<cursor::CursorRow>,
    hash: String,
    mtime: i64,
    synced: i64,
}

fn advance_cursor<'w>(
    writer: &'w mut dyn SqlWriter,
    update: CursorAdvance,
    created_id: Id128,
) -> Pin<Box<dyn Future<Output = StorageResult<()>> + Send + 'w>> {
    Box::pin(async move {
        let current = cursor::get_on_writer(writer, &update.key).await?;
        if current != update.expected {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: "workspace mirror cursor fence".into(),
                message: "workspace mirror cursor changed before commit".to_owned(),
            });
        }
        let head = writer
            .query_scalar(SqlStatement {
                sql: khive_runtime::sql!("workspace_mirror_current_head").into(),
                params: vec![
                    SqlValue::Text(update.namespace),
                    SqlValue::Text("$.source_uri".into()),
                    SqlValue::Text(update.uri),
                    SqlValue::Text(created_id.to_string()),
                    SqlValue::Text(EdgeRelation::Supersedes.to_string()),
                ],
                label: None,
            })
            .await?;
        if !matches!(head, Some(SqlValue::Integer(1))) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: "workspace mirror head fence".into(),
                message: "workspace mirror source URI head changed before commit".to_owned(),
            });
        }
        cursor::upsert_on_writer(
            writer,
            &update.key,
            Some(update.mtime),
            Some(&update.hash),
            Some(update.synced),
        )
        .await
    })
}

/// Mirror one tree sequentially, retaining at most one file's body at a time.
///
/// All body bytes are read; valid UTF-8 is hashed even when mtime is unchanged.
/// Invalid UTF-8 files are skipped and named in the summary, without writes.
/// Paths must be Unicode and symlink entries are skipped. This does not lock the tree or
/// promise ABA-proof history serialization. Earlier files may already commit
/// before a later error; inspect progress and commit_state before retrying.
pub async fn mirror_directory_once(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    project_root: &Path,
    config: &WorkspaceMirrorConfig,
) -> Result<MirrorPassSummary, MirrorPassError> {
    let mut progress = MirrorPassSummary::default();
    if runtime.backend_id().as_str() != BackendId::MAIN {
        return Err(MirrorPassError::runtime(
            &progress,
            None,
            FileCommitState::NotAttempted,
            invalid(&format!(
                "workspace mirror requires main backend; assigned backend is {}",
                runtime.backend_id()
            )),
        ));
    }
    let metadata = fs::symlink_metadata(project_root)
        .map_err(|error| MirrorPassError::io(&progress, project_root, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MirrorPassError::runtime(
            &progress,
            Some(project_root),
            FileCommitState::NotAttempted,
            invalid("workspace mirror root must be a real directory"),
        ));
    }
    let root = fs::canonicalize(project_root)
        .map_err(|error| MirrorPassError::io(&progress, project_root, error))?;
    let first =
        fs::read_dir(&root).map_err(|error| MirrorPassError::io(&progress, &root, error))?;
    let mut directories = vec![(root.clone(), first)];
    while let Some((directory, entries)) = directories.last_mut() {
        let (directory, next) = (directory.clone(), entries.next());
        let Some(entry) = next else {
            directories.pop();
            continue;
        };
        let entry = entry.map_err(|error| MirrorPassError::io(&progress, &directory, error))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            let entries = fs::read_dir(&path)
                .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            directories.push((path, entries));
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let relative = path.strip_prefix(&root).map_err(|_| {
            MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror path escaped its root"),
            )
        })?;
        if !config.includes(relative) {
            continue;
        }
        let name = relative.to_str().ok_or_else(|| {
            MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror paths must be Unicode"),
            )
        })?;
        let uri = file_uri(&path).ok_or_else(|| {
            MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror paths must be Unicode"),
            )
        })?;
        let key = cursor_key(token.namespace().as_str(), &uri);
        let mut file =
            File::open(&path).map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        let before = file
            .metadata()
            .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        if !before.is_file() {
            continue;
        }
        let modified = before
            .modified()
            .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        let after = file
            .metadata()
            .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
        if before.len() != after.len()
            || modified
                != after
                    .modified()
                    .map_err(|error| MirrorPassError::io(&progress, &path, error))?
        {
            return Err(MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror file changed while reading"),
            ));
        }
        let raw = match String::from_utf8(raw) {
            Ok(raw) => raw,
            Err(_) => {
                progress.skipped_non_utf8.push(path);
                continue;
            }
        };
        let checksum = Hash32::from_blake3(raw.as_bytes()).to_string();
        let masked = khive_runtime::secret_gate::mask_secrets(&raw);
        let expected = {
            let mut reader = runtime.sql().reader().await.map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    error.into(),
                )
            })?;
            cursor::get(&mut *reader, &key).await.map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    error.into(),
                )
            })?
        };
        let previous = previous_document(runtime, token, &uri)
            .await
            .map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    error,
                )
            })?;
        if expected
            .as_ref()
            .is_some_and(|row| row.last_hash.as_deref() == Some(checksum.as_str()))
        {
            if previous.is_none() {
                return Err(MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    invalid("workspace mirror cursor has no current document"),
                ));
            }
            progress.unchanged = progress.unchanged.checked_add(1).ok_or_else(|| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    invalid("workspace mirror counter overflow"),
                )
            })?;
            continue;
        }
        let source_type = match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension)
                if extension.eq_ignore_ascii_case("md")
                    || extension.eq_ignore_ascii_case("markdown") =>
            {
                "text/markdown"
            }
            _ => "text/plain",
        };
        let update = CursorAdvance {
            key,
            namespace: token.namespace().as_str().to_owned(),
            uri: uri.clone(),
            expected,
            hash: checksum.clone(),
            mtime: seconds(modified).map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    error,
                )
            })?,
            synced: seconds(SystemTime::now()).map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::NotAttempted,
                    error,
                )
            })?,
        };
        let committed = progress.committed.checked_add(1).ok_or_else(|| {
            MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror counter overflow"),
            )
        })?;
        let (id, effects) = runtime
            .create_workspace_document_with_cursor(
                registry,
                token,
                (name, masked.as_ref()),
                (&uri, source_type, &checksum),
                previous,
                move |writer, created_id| advance_cursor(writer, update, created_id),
            )
            .await
            .map_err(|error| {
                MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::Unconfirmed,
                    error,
                )
            })?;
        progress.committed = committed;
        if let Err(error) = effects {
            return Err(MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::Committed(id),
                error,
            ));
        }
    }
    Ok(progress)
}
