//! One explicit workspace-tree pass. No poller or filesystem writeback.
//!
//! Main-assigned runtimes only. Each file commits document/lexical index/edge
//! and cursor together; post-commit effect failures remain committed outcomes.
//! Tree reads are synchronous, streamed, and not an adversarial snapshot.

use std::collections::HashSet;
use std::fmt;
#[cfg(not(unix))]
use std::fs;
use std::fs::File;
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
    /// Masked relative paths whose bytes were not UTF-8; no document or cursor was written.
    pub skipped_non_utf8: Vec<PathBuf>,
    /// Masked relative paths whose prior document has no unique current head.
    pub skipped_no_current: Vec<PathBuf>,
    /// Masked relative paths and stable codes for pre-write policy refusals.
    pub refused: Vec<(PathBuf, &'static str)>,
    /// Masked relative paths of links or entries swapped before safe opening.
    pub skipped_entries: Vec<PathBuf>,
}

fn masked_diagnostic(path: &Path) -> PathBuf {
    PathBuf::from(
        khive_runtime::secret_gate::mask_for_redaction_surface(
            khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
            &path.to_string_lossy(),
        )
        .into_owned(),
    )
}

#[derive(Clone, Copy)]
enum PreviousDocument {
    Absent,
    Ambiguous,
    Current(Id128),
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
    /// Masked diagnostic path associated with this error, if filesystem work began.
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
            file: file.map(masked_diagnostic),
            commit_state,
            cause: Box::new(Failure::Runtime(error)),
        }
    }

    fn io(progress: &MirrorPassSummary, file: &Path, error: std::io::Error) -> Self {
        Self {
            progress: progress.clone(),
            file: Some(masked_diagnostic(file)),
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
) -> Result<PreviousDocument, RuntimeError> {
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
        return Ok(PreviousDocument::Absent);
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
        return Ok(PreviousDocument::Ambiguous);
    }
    let id = heads.into_iter().next().expect("one current head");
    Ok(PreviousDocument::Current(Id128::from_bytes(*id.as_bytes())))
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

struct DirectoryFrame {
    path: PathBuf,
    names: std::vec::IntoIter<std::ffi::OsString>,
    #[cfg(unix)]
    handle: File,
}

fn validate_root_spelling(project_root: &Path) -> std::io::Result<()> {
    let raw = project_root.as_os_str().as_encoded_bytes();
    let separator = |byte| byte == b'/' || (cfg!(windows) && byte == b'\\');
    if raw.is_empty()
        || raw.last().is_some_and(|byte| separator(*byte))
        || raw
            .split(|byte| separator(*byte))
            .any(|part| part == b"." || part == b"..")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace mirror root has a refused spelling",
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn validate_root_spelling_for_test(project_root: &Path) -> std::io::Result<()> {
    validate_root_spelling(project_root)
}

#[cfg(unix)]
fn root_frame(
    project_root: &Path,
    #[cfg(test)] before_open: &mut Option<&mut (dyn FnMut(&Path) + Send)>,
) -> std::io::Result<(PathBuf, DirectoryFrame)> {
    use std::os::unix::fs::MetadataExt;
    validate_root_spelling(project_root)?;
    let handle = File::from(khive_fs::directory_walk::open_dir_nofollow(project_root)?);
    #[cfg(test)]
    if let Some(hook) = before_open.as_deref_mut() {
        hook(project_root);
    }
    let root = std::fs::canonicalize(project_root)?;
    let pinned = khive_fs::fd_relative::stat_fd(&handle)?;
    let named = std::fs::metadata(&root)?;
    #[allow(clippy::unnecessary_cast)]
    let pinned_identity = khive_fs::fd_relative::FileIdentity {
        dev: pinned.st_dev as u64,
        ino: pinned.st_ino as u64,
    };
    if pinned_identity.dev != named.dev() || pinned_identity.ino != named.ino() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "workspace mirror root changed while pinning",
        ));
    }
    let names = khive_fs::fd_relative::list_names(&handle)?.into_iter();
    Ok((
        root.clone(),
        DirectoryFrame {
            path: root,
            names,
            handle,
        },
    ))
}

#[cfg(not(unix))]
fn root_frame(
    project_root: &Path,
    #[cfg(test)] _before_open: &mut Option<&mut (dyn FnMut(&Path) + Send)>,
) -> std::io::Result<(PathBuf, DirectoryFrame)> {
    validate_root_spelling(project_root)?;
    let metadata = fs::symlink_metadata(project_root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace mirror root must be a real directory",
        ));
    }
    let root = fs::canonicalize(project_root)?;
    let names = portable_names(&root)?.into_iter();
    Ok((root.clone(), DirectoryFrame { path: root, names }))
}

#[cfg(not(unix))]
fn portable_names(path: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    let mut names = fs::read_dir(path)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
    Ok(names)
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
    #[cfg(test)]
    {
        mirror_directory_once_inner(runtime, registry, token, project_root, config, None).await
    }
    #[cfg(not(test))]
    {
        mirror_directory_once_inner(runtime, registry, token, project_root, config).await
    }
}

#[cfg(test)]
pub(crate) async fn mirror_directory_once_with_hook(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    project_root: &Path,
    config: &WorkspaceMirrorConfig,
    hook: &mut (dyn FnMut(&Path) + Send),
) -> Result<MirrorPassSummary, MirrorPassError> {
    mirror_directory_once_inner(runtime, registry, token, project_root, config, Some(hook)).await
}

async fn mirror_directory_once_inner(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    project_root: &Path,
    config: &WorkspaceMirrorConfig,
    #[cfg(test)] mut before_open: Option<&mut (dyn FnMut(&Path) + Send)>,
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
    #[cfg(test)]
    let (root, first) = root_frame(project_root, &mut before_open)
        .map_err(|error| MirrorPassError::io(&progress, project_root, error))?;
    #[cfg(not(test))]
    let (root, first) = root_frame(project_root)
        .map_err(|error| MirrorPassError::io(&progress, project_root, error))?;
    let mut directories = vec![first];
    while let Some(frame) = directories.last_mut() {
        let Some(name) = frame.names.next() else {
            directories.pop();
            continue;
        };
        let path = frame.path.join(&name);
        let relative = path.strip_prefix(&root).map_err(|_| {
            MirrorPassError::runtime(
                &progress,
                Some(&path),
                FileCommitState::NotAttempted,
                invalid("workspace mirror path escaped its root"),
            )
        })?;

        #[cfg(unix)]
        let (mut file, before) = {
            use khive_fs::fd_relative::{open_at, open_dir_at, stat_at, stat_fd, FileIdentity};
            let observed = stat_at(&frame.handle, &name)
                .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            let kind = observed.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                progress.skipped_entries.push(masked_diagnostic(relative));
                continue;
            }
            if kind != libc::S_IFDIR && kind != libc::S_IFREG {
                continue;
            }
            if kind == libc::S_IFREG && !config.includes(relative) {
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = before_open.as_deref_mut() {
                hook(&path);
            }
            let opened = if kind == libc::S_IFDIR {
                open_dir_at(&frame.handle, &name)
            } else {
                open_at(&frame.handle, &name, false)
            };
            let handle = match opened {
                Ok(handle) => handle,
                Err(error) => {
                    if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                        progress.skipped_entries.push(masked_diagnostic(relative));
                        continue;
                    }
                    return Err(MirrorPassError::io(&progress, &path, error));
                }
            };
            let actual =
                stat_fd(&handle).map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            #[allow(clippy::unnecessary_cast)]
            let observed_identity = FileIdentity {
                dev: observed.st_dev as u64,
                ino: observed.st_ino as u64,
            };
            #[allow(clippy::unnecessary_cast)]
            let actual_identity = FileIdentity {
                dev: actual.st_dev as u64,
                ino: actual.st_ino as u64,
            };
            if observed_identity != actual_identity {
                progress.skipped_entries.push(masked_diagnostic(relative));
                continue;
            }
            if kind == libc::S_IFDIR {
                if actual.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    progress.skipped_entries.push(masked_diagnostic(relative));
                    continue;
                }
                let names = khive_fs::fd_relative::list_names(&handle)
                    .map_err(|error| MirrorPassError::io(&progress, &path, error))?
                    .into_iter();
                directories.push(DirectoryFrame {
                    path,
                    names,
                    handle,
                });
                continue;
            }
            let before = handle
                .metadata()
                .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            if actual.st_mode & libc::S_IFMT != libc::S_IFREG || !before.is_file() {
                progress.skipped_entries.push(masked_diagnostic(relative));
                continue;
            }
            (handle, before)
        };

        #[cfg(not(unix))]
        let (mut file, before) = {
            let observed = fs::symlink_metadata(&path)
                .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            if observed.file_type().is_symlink() {
                progress.skipped_entries.push(masked_diagnostic(relative));
                continue;
            }
            if observed.is_dir() {
                #[cfg(test)]
                if let Some(hook) = before_open.as_deref_mut() {
                    hook(&path);
                }
                let names = portable_names(&path)
                    .map_err(|error| MirrorPassError::io(&progress, &path, error))?
                    .into_iter();
                directories.push(DirectoryFrame { path, names });
                continue;
            }
            if !observed.is_file() || !config.includes(relative) {
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = before_open.as_deref_mut() {
                hook(&path);
            }
            let file = match khive_fs::opened_file::open_regular_file_within(&root, &path) {
                Ok(file) => file,
                Err(
                    khive_fs::opened_file::ContainedOpenError::NotRegular
                    | khive_fs::opened_file::ContainedOpenError::Resolve(_)
                    | khive_fs::opened_file::ContainedOpenError::Escapes { .. },
                ) => {
                    progress.skipped_entries.push(masked_diagnostic(relative));
                    continue;
                }
                Err(
                    khive_fs::opened_file::ContainedOpenError::Open(error)
                    | khive_fs::opened_file::ContainedOpenError::Metadata(error),
                ) => {
                    return Err(MirrorPassError::io(&progress, &path, error));
                }
            };
            let before = file
                .metadata()
                .map_err(|error| MirrorPassError::io(&progress, &path, error))?;
            (file, before)
        };

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
                progress.skipped_non_utf8.push(masked_diagnostic(relative));
                continue;
            }
        };
        let checksum = Hash32::from_blake3(raw.as_bytes()).to_string();
        let masked = khive_runtime::secret_gate::mask_for_redaction_surface(
            khive_runtime::secret_gate::RedactionSurface::WorkspaceMirror,
            &raw,
        );
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
        let previous = match previous {
            PreviousDocument::Ambiguous => {
                progress
                    .skipped_no_current
                    .push(masked_diagnostic(relative));
                continue;
            }
            PreviousDocument::Absent if expected.is_some() => {
                progress
                    .skipped_no_current
                    .push(masked_diagnostic(relative));
                continue;
            }
            PreviousDocument::Absent => None,
            PreviousDocument::Current(id) => Some(id),
        };
        if expected
            .as_ref()
            .is_some_and(|row| row.last_hash.as_deref() == Some(checksum.as_str()))
        {
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
        let result = runtime
            .create_workspace_document_with_cursor(
                registry,
                token,
                (name, masked.as_ref()),
                (&uri, source_type, &checksum),
                previous,
                move |writer, created_id| advance_cursor(writer, update, created_id),
            )
            .await;
        let (id, effects) = match result {
            Ok(result) => result,
            Err(RuntimeError::CanonicalAdmission {
                op_index: 0,
                source,
            }) if matches!(source.as_ref(), RuntimeError::SecretDetected(_)) => {
                progress
                    .refused
                    .push((masked_diagnostic(relative), "SecretDetected"));
                continue;
            }
            Err(RuntimeError::SecretDetected(_)) => {
                progress
                    .refused
                    .push((masked_diagnostic(relative), "SecretDetected"));
                continue;
            }
            Err(error) => {
                return Err(MirrorPassError::runtime(
                    &progress,
                    Some(&path),
                    FileCommitState::Unconfirmed,
                    error,
                ));
            }
        };
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
