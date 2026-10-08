//! Idempotent file tail + upsert into the session mirror tables.
//!
//! `mirror_file` reads new bytes from a JSONL file starting at `start_offset`,
//! parses complete lines via the parser selected by [`LineTailSource`], and
//! writes one bounded chunk (never the whole file at once) to the session
//! mirror tables per call — callers poll repeatedly to drain large deltas.
//! The scoped `(namespace, source, session_id, event id)` key makes replays
//! idempotent without conflating different providers' identifier spaces.
//!
//! See `crates/khive-pack-session/docs/api/mirror-ingest.md` for the full bounded
//! tail-read algorithm, the oversized/unterminated-line handling
//! (PACKSESSION-AUD-003), and the write-path (ADR-099 D5) rationale.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::Utc;
use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlWriter;
use sha2::{Digest, Sha256};

use super::parse;

/// The full ADR-080 mirror-source contract — the closed set of sources
/// `sessions.source` can hold (`docs/adr/ADR-080-session-pack-oss-storage-mechanism.md`,
/// "Mirror sources — closed set"). Adding a source requires amending that ADR
/// section and this enum together.
///
/// This is a superset of [`LineTailSource`]: the provider export variants
/// ingest via whole-file re-parse, not the per-line dispatch
/// `LineTailSource` selects, so they have no `LineTailSource` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    ClaudeCode,
    /// Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
    Codex,
    /// ChatGPT data export (`<exports dir>/**/conversations.json`).
    ChatGptExport,
    /// claude.ai data export (`<exports dir>/**/conversations.json`).
    ClaudeAiExport,
}

impl MirrorSource {
    /// The string written to `sessions.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            MirrorSource::ClaudeCode => "claude_code",
            MirrorSource::Codex => "codex",
            MirrorSource::ChatGptExport => "chatgpt_export",
            MirrorSource::ClaudeAiExport => "claude_ai_export",
        }
    }
}

impl From<LineTailSource> for MirrorSource {
    fn from(source: LineTailSource) -> Self {
        match source {
            LineTailSource::ClaudeCode => MirrorSource::ClaudeCode,
            LineTailSource::Codex => MirrorSource::Codex,
        }
    }
}

/// SHA-256 of a framed optional parsed-text value and the exact raw line.
/// Framing distinguishes absent text from empty text and avoids ambiguity
/// when either field contains a delimiter. The same framing is used by the
/// versioned backfill in `khive-db`.
fn content_hash(text: Option<&str>, raw: &str) -> String {
    let mut hash = Sha256::new();
    match text {
        Some(value) => {
            hash.update([1]);
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        None => hash.update([0]),
    }
    hash.update((raw.len() as u64).to_be_bytes());
    hash.update(raw.as_bytes());
    let digest = hash.finalize();
    format!("{digest:x}")
}

/// Identifies which CLI produced the JSONL file being mirrored, for the
/// purpose of selecting `mirror_file`'s per-line parser.
///
/// This is narrower than [`MirrorSource`]: it covers only the line-tail
/// sources (append-only JSONL, tailed by byte offset). Provider-export
/// ingestion is whole-file re-parse, not line-tail, so those sources have no
/// variants here — see [`mirror_chatgpt_export_file`] and
/// [`mirror_claude_ai_export_file`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTailSource {
    /// Claude Code (`~/.claude/projects/<slug>/<uuid>.jsonl`).
    ClaudeCode,
    /// Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`).
    Codex,
}

/// Statistics returned by a single `mirror_file` call.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct MirrorStats {
    /// Number of new message rows inserted (0 if all were already present).
    pub inserted: u64,
    /// Existing scoped event ids whose persisted content hash differs on replay.
    /// The insert-once row is retained and the cursor may still advance.
    pub replay_mismatches: u64,
    /// Number of complete lines or whole-file events scanned (including duplicates).
    pub scanned: u64,
    /// Byte offset advanced to. Ordinary partial lines are excluded; a known
    /// oversized line may checkpoint a bounded discarded prefix mid-line.
    pub new_offset: u64,
    /// True when this pass's advance discarded bytes from a line that
    /// exceeded `max_line_bytes` (complete or still-unterminated). That
    /// determination is made before any per-source parser ever sees the
    /// line, so the resulting advance is source-independent — no candidate
    /// under overlapping-root dispatch could ever have parsed the same
    /// bytes into rows. Distinct from an ordinary empty advance (blank or
    /// unparseable lines), whose bytes a different source candidate might
    /// still have claimed. The dispatch loop treats the advance as
    /// uncontested only when the same pass also `scanned` no ordinary line.
    pub skipped_oversized_bytes: bool,
    /// Identity of the file handle read for this pass. Persisted with an
    /// advancing cursor so a same-path replacement cannot inherit its offset.
    pub file_identity: Option<String>,
}

/// Stable across appends, but different for a replacement file at the same
/// path. The identity is read from the open handle, so the identity a caller
/// compares is the identity of the object it reads:
///
/// - Unix: device and inode (`unix:<dev>:<ino>`).
/// - Windows: volume serial number and 128-bit file id from `FileIdInfo`
///   (`windows:<volume>:<file id>`, hexadecimal). Creation time is not used
///   there: NTFS file system tunneling can give a file re-created or renamed
///   into a recently vacated name the creation time of the file it replaced.
/// - Other targets, which have neither: file creation time where available.
pub(crate) fn file_identity(file: &std::fs::File) -> std::io::Result<String> {
    #[cfg(unix)]
    {
        let identity = khive_fs::fd_relative::FileIdentity::of(file)?;
        Ok(format!("unix:{}:{}", identity.dev, identity.ino))
    }
    #[cfg(windows)]
    {
        use std::fmt::Write as _;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
        };

        let mut info = FILE_ID_INFO::default();
        // SAFETY: `file` keeps the handle live for the call; `info` is a
        // writable buffer of the exact size FileIdInfo requires.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&raw mut info).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut identity = format!("windows:{:016x}:", info.VolumeSerialNumber);
        for byte in info.FileId.Identifier {
            let _ = write!(identity, "{byte:02x}");
        }
        Ok(identity)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(format!("created:{:?}", file.metadata()?.created().ok()))
    }
}

fn open_source_file(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        return windows_source_open::open_file(path);
    }
    #[cfg(not(windows))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        options.open(path)
    }
}

/// Open a scheduled source through its configured directory, refusing untrusted
/// ancestor links and a linked final file. Each directory handle pins the
/// component used by the next handle-relative open, so replacing a parent during the walk
/// cannot redirect the remaining components outside `root`.
#[derive(Clone, Copy)]
pub(crate) struct TrustedSource<'a> {
    pub(crate) root: &'a Path,
    pub(crate) directory_identities: &'a [String],
}

#[cfg(unix)]
mod root_walk;
#[cfg(unix)]
use root_walk::open_source_root;

#[cfg(unix)]
pub(crate) fn open_source_file_beneath(
    root: &Path,
    path: &Path,
    expected_directories: Option<&[String]>,
) -> std::io::Result<(std::fs::File, Vec<String>)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let relative = path.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mirror source is outside its configured root",
        )
    })?;
    let mut components = relative.components().peekable();
    if components.peek().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mirror source does not name a file beneath its configured root",
        ));
    }

    // Retain the configured root's ancestors and every subsequent directory
    // until the leaf opens. Every child open uses the proved parent handle.
    let mut pinned_directories = open_source_root(root)?;
    let mut directory_identities = Vec::new();
    let root_identity = file_identity(
        pinned_directories
            .last()
            .expect("configured root is retained"),
    )?;
    if expected_directories.is_some_and(|expected| expected.first() != Some(&root_identity)) {
        return Err(std::io::Error::other(
            "mirror source root changed after its metadata probe",
        ));
    }
    if expected_directories.is_none() {
        directory_identities.push(root_identity);
    }
    let mut directory_depth = 1;

    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mirror source contains a non-normal path component",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "mirror source contains a NUL byte",
            )
        })?;
        let last = components.peek().is_none();
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        let flags = if last {
            flags
        } else {
            flags | libc::O_DIRECTORY
        };
        let directory = pinned_directories
            .last()
            .expect("source parent is retained");
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let opened = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            if expected_directories.is_some_and(|expected| expected.len() != directory_depth) {
                return Err(std::io::Error::other(
                    "mirror source ancestor count changed after its metadata probe",
                ));
            }
            return Ok((opened, directory_identities));
        }
        let identity = file_identity(&opened)?;
        if expected_directories
            .is_some_and(|expected| expected.get(directory_depth) != Some(&identity))
        {
            return Err(std::io::Error::other(
                "mirror source parent changed after its metadata probe",
            ));
        }
        if expected_directories.is_none() {
            directory_identities.push(identity);
        }
        directory_depth += 1;
        pinned_directories.push(opened);
    }
    unreachable!("nonempty component iterator must return its final file")
}

#[cfg(windows)]
pub(crate) use windows_source_open::open_source_file_beneath;

#[cfg(windows)]
mod windows_source_open {
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::path::{Component, Path};

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Foundation::{
        RtlNtStatusToDosError, HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    use super::file_identity;

    fn invalid(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    fn verify_handle(file: &File, directory: bool) -> io::Result<()> {
        let metadata = file.metadata()?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            return Err(invalid(
                "mirror source component has the wrong kind or is a reparse point",
            ));
        }
        Ok(())
    }

    fn open_root(root: &Path) -> io::Result<Vec<File>> {
        let root = if root.as_os_str().is_empty() {
            Path::new(".")
        } else {
            root
        };
        let absolute = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        let mut pinned = Vec::new();
        // Pin every configured-root ancestor without delete sharing. A later
        // absolute open cannot traverse a swapped-in junction above `root`.
        for ancestor in absolute.ancestors().collect::<Vec<_>>().into_iter().rev() {
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(ancestor)?;
            verify_handle(&file, true)?;
            pinned.push(file);
        }
        if pinned.is_empty() {
            return Err(invalid("mirror source root is empty"));
        }
        Ok(pinned)
    }

    pub(super) fn open_file(path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        verify_handle(&file, false)?;
        Ok(file)
    }

    fn open_child(directory: &File, name: &OsStr, last: bool) -> io::Result<File> {
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide.iter().any(|unit| {
                *unit == 0
                    || *unit == u16::from(b'/')
                    || *unit == u16::from(b'\\')
                    || *unit == u16::from(b':')
            })
        {
            return Err(invalid(
                "mirror source contains an invalid Windows path component",
            ));
        }
        let byte_len = wide
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| invalid("mirror source component is too long"))?;
        let unicode_name = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: directory.as_raw_handle(),
            ObjectName: &raw const unicode_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut io_status = IO_STATUS_BLOCK::default();
        let mut handle: HANDLE = std::ptr::null_mut();
        let desired_access = if last {
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        } else {
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE
        };
        let create_options = if last {
            FILE_NON_DIRECTORY_FILE
        } else {
            FILE_DIRECTORY_FILE
        } | FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT;
        let share_mode = if last {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        };
        // SAFETY: the name buffer, structures, and pinned parent handle remain
        // live for the call. The successful child handle is owned by `File`.
        let status = unsafe {
            NtCreateFile(
                &raw mut handle,
                desired_access,
                &raw const attributes,
                &raw mut io_status,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                share_mode,
                FILE_OPEN,
                create_options,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            // SAFETY: translating a returned NTSTATUS has no preconditions.
            let error = unsafe { RtlNtStatusToDosError(status) };
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        // SAFETY: `handle` was freshly returned by `NtCreateFile` and is
        // transferred exactly once.
        let file = unsafe { File::from_raw_handle(handle as RawHandle) };
        verify_handle(&file, !last)?;
        Ok(file)
    }

    pub(crate) fn open_source_file_beneath(
        root: &Path,
        path: &Path,
        expected_directories: Option<&[String]>,
    ) -> io::Result<(File, Vec<String>)> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid("mirror source is outside its configured root"))?;
        let mut components = relative.components().peekable();
        if components.peek().is_none() {
            return Err(invalid(
                "mirror source does not name a file beneath its configured root",
            ));
        }

        let mut pinned_directories = open_root(root)?;
        let root_identity = file_identity(
            pinned_directories
                .last()
                .ok_or_else(|| invalid("mirror source root is empty"))?,
        )?;
        if expected_directories.is_some_and(|expected| expected.first() != Some(&root_identity)) {
            return Err(io::Error::other(
                "mirror source root changed after its metadata probe",
            ));
        }
        let mut directory_identities = Vec::new();
        if expected_directories.is_none() {
            directory_identities.push(root_identity);
        }
        let mut directory_depth = 1;

        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(invalid(
                    "mirror source contains a non-normal path component",
                ));
            };
            let last = components.peek().is_none();
            let opened = open_child(
                pinned_directories
                    .last()
                    .ok_or_else(|| invalid("mirror source root is empty"))?,
                name,
                last,
            )?;
            if last {
                if expected_directories.is_some_and(|expected| expected.len() != directory_depth) {
                    return Err(io::Error::other(
                        "mirror source ancestor count changed after its metadata probe",
                    ));
                }
                return Ok((opened, directory_identities));
            }
            let identity = file_identity(&opened)?;
            if expected_directories
                .is_some_and(|expected| expected.get(directory_depth) != Some(&identity))
            {
                return Err(io::Error::other(
                    "mirror source parent changed after its metadata probe",
                ));
            }
            if expected_directories.is_none() {
                directory_identities.push(identity);
            }
            directory_depth += 1;
            pinned_directories.push(opened);
        }
        unreachable!("nonempty component iterator must return its final file")
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::OsStr;

        use super::{open_child, open_root};
        use tempfile::TempDir;

        #[test]
        fn mirror_windows_pinned_intermediate_directory_cannot_leave_root() {
            let temp = TempDir::new().expect("tempdir");
            let root = temp.path().join("root");
            let outside = temp.path().join("outside");
            let parent = root.join("staged");
            let moved = outside.join("staged");
            std::fs::create_dir_all(&parent).expect("staged directory");
            std::fs::create_dir_all(&outside).expect("outside directory");
            std::fs::write(parent.join("source.jsonl"), b"inside\n").expect("source file");

            std::fs::rename(&parent, &moved).expect("unheld directory can leave root");
            std::fs::rename(&moved, &parent).expect("restore source directory");

            let directories = open_root(&root).expect("pin root ancestors");
            let directory = open_child(
                directories.last().expect("root handle"),
                OsStr::new("staged"),
                false,
            )
            .expect("pin intermediate directory");
            assert!(
                std::fs::rename(&parent, &moved).is_err(),
                "an opened intermediate directory must not leave its configured root"
            );
            assert!(open_child(&directory, OsStr::new("source.jsonl"), true).is_ok());

            drop(directory);
            std::fs::rename(&parent, &moved).expect("rename succeeds after releasing directory");
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_source_file_beneath(
    _root: &Path,
    _path: &Path,
    _expected_directories: Option<&[String]>,
) -> std::io::Result<(std::fs::File, Vec<String>)> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure directory-relative mirror source opens are unavailable on this platform",
    ))
}

fn checked_identity(
    file: &std::fs::File,
    metadata: &std::fs::Metadata,
    expected_identity: Option<&str>,
) -> std::io::Result<String> {
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "mirror source is not a regular file",
        ));
    }
    let identity = file_identity(file)?;
    if expected_identity.is_some_and(|expected| expected != identity.as_str()) {
        return Err(std::io::Error::other(
            "mirror source was replaced after its metadata probe",
        ));
    }
    Ok(identity)
}

/// Ceiling on bytes read per `mirror_file` call in production (8 MiB); bounds
/// worst-case memory on a very large accumulated delta. See
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#mirrorlimits--per-pass-caps`.
const MIRROR_MAX_BYTES_PER_PASS: usize = 8 * 1024 * 1024;

/// Ceiling on parsed events collected per `mirror_file` call in production.
const MIRROR_MAX_EVENTS_PER_PASS: usize = 1024;

/// Hard ceiling on a single JSONL line's buffered size, enforced by
/// `read_line_bounded` independently of `max_bytes_per_pass` (PACKSESSION-AUD-003).
const MIRROR_MAX_LINE_BYTES: usize = MIRROR_MAX_BYTES_PER_PASS;

/// Per-call caps on how much of a file's delta `mirror_file` reads/parses
/// before writing a bounded chunk; tests use smaller caps to force multi-pass
/// behavior without giant fixtures.
#[derive(Clone, Copy, Debug)]
struct MirrorLimits {
    max_bytes_per_pass: usize,
    max_events_per_pass: usize,
    max_line_bytes: usize,
}

impl MirrorLimits {
    fn production() -> Self {
        Self {
            max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
            max_line_bytes: MIRROR_MAX_LINE_BYTES,
        }
    }
}

/// Read new bytes of `path` starting at `start_offset`, parse complete lines
/// using the parser selected by `source`, and upsert them idempotently into the
/// session mirror tables.
///
/// For `LineTailSource::Codex`, `codex_session_id` must be the session UUID
/// derived from the filename; it is used both to key the `sessions` row and to
/// synthesise per-line event UUIDs (`"{session_id}:{abs_byte_offset}"`).
/// For `LineTailSource::ClaudeCode`, `codex_session_id` is ignored (the session
/// UUID is embedded in each line).
///
/// Returns stats including the advanced byte offset. An ordinary partial
/// trailing line (no terminating `\n`) is left for the next poll. Once a line
/// is known to exceed the line cap, bounded discarded prefixes are checkpointed
/// until a later poll reaches its terminator.
///
/// One bad file or one bad line does NOT kill the loop: per-file errors propagate
/// to the caller (the service loop logs and continues); per-line parse failures
/// are silently skipped (the parser returns `None`).
pub async fn mirror_file(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_file_with_limits(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        MirrorLimits::production(),
    )
    .await
}

/// Dispatch-loop variant of [`mirror_file`]: identical, except that a pass
/// which consumed bytes but parsed no events does NOT commit the cursor
/// itself. The caller commits it via [`commit_empty_advance`] only when
/// dispatch ends without any candidate inserting rows for the span. This
/// keeps the empty advance's cursor commit atomic with respect to the
/// candidate dispatch order: bytes are never skipped ahead of a later
/// candidate that could parse them, and an interrupt between candidates
/// cannot leave the cursor advanced past bytes no candidate inserted. The
/// service additionally vetoes that commit when ANY candidate errors during
/// the pass, even if another candidate returned an empty advance — except
/// when the empty advance is [`MirrorStats::skipped_oversized_bytes`], which
/// always commits: a known-oversized line was never attributable to a
/// source in the first place, so a sibling's error cannot contest it.
pub async fn mirror_file_deferred(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_file_deferred_checked(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        None,
        None,
    )
    .await
}

pub(crate) async fn mirror_file_deferred_checked(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    expected_identity: Option<&str>,
    trusted_source: Option<TrustedSource<'_>>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_file_inner(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        MirrorLimits::production(),
        false,
        expected_identity,
        trusted_source,
    )
    .await
}

/// Commit the cursor for an empty advance (bytes consumed, zero rows) that
/// [`mirror_file_deferred`] deliberately left uncommitted. Call this only
/// when dispatch for the span ends with no inserting candidate. The commit
/// is a single `INSERT ... ON CONFLICT DO UPDATE` on the cursor row; on
/// failure the error propagates and the in-memory offset is not applied, so
/// the bytes are re-read (bounded, idempotent) on a later pass instead of
/// being silently skipped. The service-level caller also vetoes this commit
/// when ANY candidate errored in the dispatch pass.
pub async fn commit_empty_advance(
    runtime: &KhiveRuntime,
    path: &Path,
    stats: &MirrorStats,
) -> Result<(), RuntimeError> {
    write_cursor_only(
        runtime,
        path,
        &None,
        stats.new_offset,
        stats.file_identity.as_deref(),
    )
    .await
}

/// Store a legacy cursor's first observed identity without changing its offset.
pub async fn adopt_cursor_identity(
    runtime: &KhiveRuntime,
    path: &Path,
    offset: u64,
    identity: &str,
) -> Result<(), RuntimeError> {
    write_cursor_only(runtime, path, &None, offset, Some(identity)).await
}

/// A single bounded read pass: at most `limits.max_bytes_per_pass` bytes and
/// `limits.max_events_per_pass` parsed events, stopping on a line boundary.
struct MirrorChunk {
    events: Vec<parse::ParsedEvent>,
    scanned: u64,
    new_offset: u64,
    /// See [`MirrorStats::skipped_oversized_bytes`].
    skipped_oversized_bytes: bool,
    file_identity: String,
}

/// Outcome of `read_line_bounded` for one line. See
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#lineread--read_line_bounded--the-packsession-aud-003-bound`
/// for the full PACKSESSION-AUD-003 rationale.
#[derive(Debug)]
enum LineRead {
    /// EOF with nothing read at all.
    Eof,
    /// EOF before a terminating `\n`; caller must not advance past it.
    Partial,
    /// A complete line fit within `max_line_bytes`.
    Complete { bytes: usize },
    /// A complete line exceeded `max_line_bytes`; caller must skip it, not
    /// parse `buf` (never fully populated for this case).
    Oversized { bytes: usize },
    /// Exceeded `max_line_bytes` with no `\n` found before this bounded read
    /// stopped. The caller may persist a mid-skip cursor and resume there.
    OversizedUnterminated { bytes: usize },
}

/// Read one line from `reader` into `buf`, never buffering more than
/// `max_line_bytes` or reading more than that cap plus one buffered-reader
/// window regardless of how long the underlying line turns out to be (the
/// PACKSESSION-AUD-003 bound; see the docs guide above for why
/// `BufRead::read_until` alone is unsafe here). `already_oversized` marks a
/// persisted cursor inside a line; bytes are then discarded until its newline.
fn read_line_bounded(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max_line_bytes: usize,
    already_oversized: bool,
) -> std::io::Result<LineRead> {
    let mut total: usize = 0;
    let mut oversized = already_oversized;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if total == 0 {
                LineRead::Eof
            } else {
                LineRead::Partial
            });
        }

        let newline_pos = available.iter().position(|&b| b == b'\n');
        let take = newline_pos.map_or(available.len(), |pos| pos + 1);

        if !oversized {
            if total + take > max_line_bytes {
                oversized = true;
            } else {
                buf.extend_from_slice(&available[..take]);
            }
        }

        total += take;
        reader.consume(take);

        if newline_pos.is_some() {
            return Ok(if oversized {
                LineRead::Oversized { bytes: total }
            } else {
                LineRead::Complete { bytes: total }
            });
        }

        if oversized && total > max_line_bytes {
            // Already over the cap and this fill_buf window had no `\n`:
            // stop here rather than looping onward toward EOF (or forever,
            // if the file keeps growing). See the PACKSESSION-AUD-003 bound
            // above.
            return Ok(LineRead::OversizedUnterminated { bytes: total });
        }
        // No `\n` in this fill_buf window yet, and this call is still under
        // its cap; continue buffering a normal line or discarding a known
        // oversized continuation.
    }
}

/// Read at most one bounded chunk of `path` starting at `start_offset`. A
/// complete line whose buffered size exceeds `limits.max_line_bytes` is
/// skipped outright (bytes counted, offset advances past it, `tracing::warn!`
/// names the file/offset — PACKSESSION-AUD-003, no silent coercion). A known
/// oversized line checkpoints bounded discarded prefixes until its newline;
/// an ordinary partial trailing line is left for the next call.
fn read_bounded_chunk(
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
    expected_identity: Option<&str>,
    trusted_source: Option<TrustedSource<'_>>,
) -> std::io::Result<MirrorChunk> {
    let mut file = match trusted_source {
        Some(source) => {
            open_source_file_beneath(source.root, path, Some(source.directory_identities))?.0
        }
        None => open_source_file(path)?,
    };
    let metadata = file.metadata()?;
    let identity = checked_identity(&file, &metadata, expected_identity)?;
    let file_len = metadata.len();
    if start_offset >= file_len {
        return Ok(MirrorChunk {
            events: Vec::new(),
            scanned: 0,
            new_offset: start_offset,
            skipped_oversized_bytes: false,
            file_identity: identity,
        });
    }

    // Normal cursors point just after `\n`. A non-newline predecessor marks
    // a persisted cursor inside an oversized line, so its suffix must stay
    // in discard mode rather than being parsed as a standalone JSON record.
    let mut skipping_oversized_line = if start_offset == 0 {
        false
    } else {
        file.seek(SeekFrom::Start(start_offset - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] != b'\n'
    };
    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut scanned: u64 = 0;
    let mut lines_consumed: u64 = 0;
    let mut new_offset = start_offset;
    let mut bytes_this_pass: usize = 0;
    let mut skipped_oversized_bytes = false;

    loop {
        if lines_consumed > 0
            && (bytes_this_pass >= limits.max_bytes_per_pass
                || events.len() >= limits.max_events_per_pass)
        {
            break;
        }

        line.clear();
        let line_offset = new_offset;

        match read_line_bounded(
            &mut reader,
            &mut line,
            limits.max_line_bytes,
            skipping_oversized_line,
        )? {
            LineRead::Eof => break,
            LineRead::Partial => break, // leave partial trailing line for next pass
            LineRead::OversizedUnterminated { bytes } => {
                new_offset += bytes as u64;
                skipped_oversized_bytes = true;
                tracing::warn!(
                    path = %path.display(),
                    offset = line_offset,
                    next_offset = new_offset,
                    line_bytes = bytes,
                    max_line_bytes = limits.max_line_bytes,
                    "session mirror: oversized JSONL line has no terminator in this bounded read; \
                     advancing the cursor to continue the bounded skip"
                );
                break;
            }
            LineRead::Oversized { bytes } => {
                tracing::warn!(
                    path = %path.display(),
                    offset = line_offset,
                    line_bytes = bytes,
                    max_line_bytes = limits.max_line_bytes,
                    "session mirror: skipping oversized JSONL line"
                );
                new_offset += bytes as u64;
                bytes_this_pass += bytes;
                lines_consumed += 1;
                skipping_oversized_line = false;
                skipped_oversized_bytes = true;
            }
            LineRead::Complete { bytes } => {
                new_offset += bytes as u64;
                bytes_this_pass += bytes;
                lines_consumed += 1;

                let raw = String::from_utf8_lossy(&line[..line.len() - 1]);
                if raw.is_empty() {
                    continue; // blank line: bytes consumed, but not counted as scanned
                }

                match source {
                    LineTailSource::ClaudeCode => {
                        if let Some(ev) = parse::parse_cc_line(&raw) {
                            events.push(ev);
                        }
                    }
                    LineTailSource::Codex => {
                        let sid = codex_session_id.unwrap_or("");
                        if let Some(ev) = parse::parse_codex_line(&raw, sid, line_offset) {
                            events.push(ev);
                        }
                    }
                }
                scanned += 1;
            }
        }
    }

    Ok(MirrorChunk {
        events,
        scanned,
        new_offset,
        skipped_oversized_bytes,
        file_identity: identity,
    })
}

/// Read, parse, and write one bounded chunk starting at `start_offset`.
async fn mirror_file_with_limits(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
) -> Result<MirrorStats, RuntimeError> {
    mirror_file_inner(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        limits,
        true,
        None,
        None,
    )
    .await
}

/// Implementation of [`mirror_file`]. When `commit_empty_advance` is false,
/// a pass that consumed bytes but parsed no events does NOT commit the
/// cursor — the dispatch loop commits it via [`commit_empty_advance`] only
/// if no later candidate inserts rows for the same span (see
/// `candidate_dispatch` in `service.rs`). When true (every non-dispatch
/// caller and test), the cursor is committed immediately as before.
#[allow(clippy::too_many_arguments)]
async fn mirror_file_inner(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
    commit_empty_advance: bool,
    expected_identity: Option<&str>,
    trusted_source: Option<TrustedSource<'_>>,
) -> Result<MirrorStats, RuntimeError> {
    let chunk = read_bounded_chunk(
        path,
        start_offset,
        source,
        codex_session_id,
        limits,
        expected_identity,
        trusted_source,
    )
    .map_err(|e| {
        RuntimeError::Internal(format!(
            "mirror_file: failed to read {:?} at offset {start_offset}: {e}",
            path
        ))
    })?;

    if chunk.new_offset == start_offset {
        // Nothing was consumed this pass (EOF, or only a partial trailing
        // line was seen) — there is no advanced cursor to persist.
        return Ok(MirrorStats {
            inserted: 0,
            replay_mismatches: 0,
            scanned: 0,
            new_offset: chunk.new_offset,
            skipped_oversized_bytes: false,
            file_identity: Some(chunk.file_identity),
        });
    }

    if chunk.events.is_empty() {
        // Bytes were consumed (`chunk.new_offset > start_offset` here,
        // checked above) but nothing parsed — e.g. a chunk made entirely of
        // blank lines, unparseable lines, or skipped oversized lines. With
        // `commit_empty_advance`, apply the cursor update immediately so we
        // don't re-read the same bytes on the next call. Without it (the
        // dispatch loop), the commit is deferred to
        // [`commit_empty_advance`], which the loop calls only when no later
        // candidate inserted rows for the span — so the cursor never moves
        // past bytes another provider candidate might still parse, and an
        // interrupt between candidates cannot strand a committed empty
        // advance ahead of unconsumed rows. A failure here must propagate —
        // silently swallowing it would let the cursor and the
        // already-consumed bytes drift apart.
        if commit_empty_advance {
            write_cursor_only(
                runtime,
                path,
                &None,
                chunk.new_offset,
                Some(&chunk.file_identity),
            )
            .await?;
        }
        return Ok(MirrorStats {
            inserted: 0,
            replay_mismatches: 0,
            scanned: chunk.scanned,
            new_offset: chunk.new_offset,
            skipped_oversized_bytes: chunk.skipped_oversized_bytes,
            file_identity: Some(chunk.file_identity),
        });
    }

    write_events_and_cursor(
        runtime,
        path,
        MirrorSource::from(source).as_str(),
        &[],
        &chunk.events,
        chunk.scanned,
        chunk.new_offset,
        &chunk.file_identity,
    )
    .await
}

/// Default ceiling (256 MiB) on a ChatGPT export `conversations.json` file
/// read in one [`mirror_chatgpt_export_file`] pass — a ceiling on the entire
/// file, not a per-pass delta (unlike the JSONL line-tail sources). An export
/// over this size is skipped (warn-logged) and the cursor is left untouched
/// so it is retried on every later tick rather than dropped (PACKSESSION-AUD-003).
/// See `crates/khive-pack-session/docs/api/mirror-ingest.md#chatgpt-export-whole-file-re-parse-mirror_chatgpt_export_file`.
const DEFAULT_CHATGPT_MAX_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_CLAUDE_AI_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct WholeFileExportSpec {
    source: MirrorSource,
    parser: fn(&str) -> Option<parse::ParsedExport>,
    operation: &'static str,
    format_name: &'static str,
    max_bytes_env: &'static str,
}

fn parse_chatgpt_export_for_ingest(content: &str) -> Option<parse::ParsedExport> {
    parse::parse_chatgpt_export(content).map(|events| parse::ParsedExport {
        sessions: Vec::new(),
        events,
    })
}

const CHATGPT_EXPORT_SPEC: WholeFileExportSpec = WholeFileExportSpec {
    source: MirrorSource::ChatGptExport,
    parser: parse_chatgpt_export_for_ingest,
    operation: "mirror_chatgpt_export_file",
    format_name: "ChatGPT",
    max_bytes_env: "KHIVE_MIRROR_CHATGPT_MAX_BYTES",
};

const CLAUDE_AI_EXPORT_SPEC: WholeFileExportSpec = WholeFileExportSpec {
    source: MirrorSource::ClaudeAiExport,
    parser: parse::parse_claude_ai_export_with_sessions,
    operation: "mirror_claude_ai_export_file",
    format_name: "claude.ai",
    max_bytes_env: "KHIVE_MIRROR_CLAUDE_AI_MAX_BYTES",
};

/// Resolve the ChatGPT export size ceiling from `KHIVE_MIRROR_CHATGPT_MAX_BYTES`,
/// falling back to [`DEFAULT_CHATGPT_MAX_BYTES`] for missing, non-numeric, or
/// zero values (zero would skip every export unconditionally, so it is
/// treated the same as unset).
fn chatgpt_max_bytes() -> u64 {
    export_max_bytes(CHATGPT_EXPORT_SPEC.max_bytes_env, DEFAULT_CHATGPT_MAX_BYTES)
}

fn claude_ai_max_bytes() -> u64 {
    export_max_bytes(
        CLAUDE_AI_EXPORT_SPEC.max_bytes_env,
        DEFAULT_CLAUDE_AI_MAX_BYTES,
    )
}

fn export_max_bytes(variable: &str, default: u64) -> u64 {
    std::env::var(variable)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Read the whole ChatGPT export `conversations.json` at `path`, parse every
/// conversation's mapping tree via [`parse::parse_chatgpt_export`], and
/// upsert every message-bearing event idempotently into the session mirror
/// tables in a single transaction. Unlike `mirror_file`, this always
/// re-reads and re-parses the whole file (a ChatGPT export has no stable
/// "new bytes" boundary to tail) — `start_offset` is only a cheap
/// re-poll guard: if the file has not grown past it, nothing is read.
///
/// `new_offset` is set to the whole file's byte length only after a
/// successful parse and commit; any IO, parse, or DB error leaves the
/// persisted cursor untouched, so a partially-downloaded export is retried
/// whole on the next tick, never half-consumed. An export over
/// `chatgpt_max_bytes` is skipped (warn-logged) without ever calling
/// `read_to_string`. See the docs guide linked above `chatgpt_max_bytes`
/// for the full rationale.
pub async fn mirror_chatgpt_export_file(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
) -> Result<MirrorStats, RuntimeError> {
    mirror_chatgpt_export_file_with_max_bytes(runtime, path, start_offset, chatgpt_max_bytes())
        .await
}

/// Implementation behind [`mirror_chatgpt_export_file`], taking an explicit
/// `max_bytes` ceiling so tests can exercise the oversized-skip path without
/// a giant fixture or racing on process-global environment variables.
async fn mirror_chatgpt_export_file_with_max_bytes(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    max_bytes: u64,
) -> Result<MirrorStats, RuntimeError> {
    mirror_whole_file_export(
        runtime,
        path,
        start_offset,
        max_bytes,
        CHATGPT_EXPORT_SPEC,
        None,
        None,
    )
    .await
}

pub(crate) async fn mirror_chatgpt_export_file_checked(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    expected_identity: &str,
    trusted_source: TrustedSource<'_>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_whole_file_export(
        runtime,
        path,
        start_offset,
        chatgpt_max_bytes(),
        CHATGPT_EXPORT_SPEC,
        Some(expected_identity),
        Some(trusted_source),
    )
    .await
}

/// Read a whole claude.ai export `conversations.json`, parse its
/// `chat_messages` arrays via [`parse::parse_claude_ai_export`], and commit
/// the resulting sessions, messages, and cursor atomically.
pub async fn mirror_claude_ai_export_file(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
) -> Result<MirrorStats, RuntimeError> {
    mirror_claude_ai_export_file_with_max_bytes(runtime, path, start_offset, claude_ai_max_bytes())
        .await
}

async fn mirror_claude_ai_export_file_with_max_bytes(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    max_bytes: u64,
) -> Result<MirrorStats, RuntimeError> {
    mirror_whole_file_export(
        runtime,
        path,
        start_offset,
        max_bytes,
        CLAUDE_AI_EXPORT_SPEC,
        None,
        None,
    )
    .await
}

pub(crate) async fn mirror_claude_ai_export_file_checked(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    expected_identity: &str,
    trusted_source: TrustedSource<'_>,
) -> Result<MirrorStats, RuntimeError> {
    mirror_whole_file_export(
        runtime,
        path,
        start_offset,
        claude_ai_max_bytes(),
        CLAUDE_AI_EXPORT_SPEC,
        Some(expected_identity),
        Some(trusted_source),
    )
    .await
}

async fn mirror_whole_file_export(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    max_bytes: u64,
    spec: WholeFileExportSpec,
    expected_identity: Option<&str>,
    trusted_source: Option<TrustedSource<'_>>,
) -> Result<MirrorStats, RuntimeError> {
    let file = match trusted_source {
        Some(source) => {
            open_source_file_beneath(source.root, path, Some(source.directory_identities))
                .map(|(file, _)| file)
        }
        None => open_source_file(path),
    }
    .map_err(|e| {
        RuntimeError::Internal(format!("{}: failed to open {path:?}: {e}", spec.operation))
    })?;
    let metadata = file.metadata().map_err(|e| {
        RuntimeError::Internal(format!("{}: failed to stat {path:?}: {e}", spec.operation))
    })?;
    let identity = checked_identity(&file, &metadata, expected_identity).map_err(|e| {
        RuntimeError::Internal(format!(
            "{}: failed to verify {path:?}: {e}",
            spec.operation
        ))
    })?;
    let file_len = metadata.len();

    if file_len <= start_offset {
        return Ok(MirrorStats {
            inserted: 0,
            replay_mismatches: 0,
            scanned: 0,
            new_offset: start_offset,
            skipped_oversized_bytes: false,
            file_identity: Some(identity),
        });
    }

    if file_len > max_bytes {
        tracing::warn!(
            path = %path.display(),
            source = spec.source.as_str(),
            file_bytes = file_len,
            max_bytes,
            max_bytes_env = spec.max_bytes_env,
            "session mirror: skipping oversized whole-file export"
        );
        return Ok(MirrorStats {
            inserted: 0,
            replay_mismatches: 0,
            scanned: 0,
            new_offset: start_offset,
            skipped_oversized_bytes: false,
            file_identity: Some(identity),
        });
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| {
            RuntimeError::Internal(format!("{}: failed to read {path:?}: {e}", spec.operation))
        })?;
    if bytes.len() as u64 != file_len {
        return Err(RuntimeError::Internal(format!(
            "{}: {path:?} changed size during read; retrying",
            spec.operation
        )));
    }
    let content = String::from_utf8(bytes).map_err(|e| {
        RuntimeError::Internal(format!(
            "{}: invalid UTF-8 in {path:?}: {e}",
            spec.operation
        ))
    })?;

    let parsed = (spec.parser)(&content).ok_or_else(|| {
        RuntimeError::Internal(format!(
            "{}: {path:?} is not a valid {} export (expected its conversations.json array shape)",
            spec.operation, spec.format_name
        ))
    })?;

    let scanned = parsed.events.len() as u64;

    write_events_and_cursor(
        runtime,
        path,
        spec.source.as_str(),
        &parsed.sessions,
        &parsed.events,
        scanned,
        file_len,
        &identity,
    )
    .await
}

/// Upsert explicit sessions, `events`, and the mirror cursor for `path` in one
/// transaction.
/// Shared by `mirror_file`'s line-tail path and both provider-export
/// whole-file paths. See
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#write-path-write_events_and_cursor-and-friends-adr-099-d5`
/// for the ADR-099 D5 suspension-free rationale.
#[allow(clippy::too_many_arguments)]
async fn write_events_and_cursor(
    runtime: &KhiveRuntime,
    path: &Path,
    source_value: &'static str,
    sessions: &[parse::ParsedSession],
    events: &[parse::ParsedEvent],
    scanned: u64,
    new_offset: u64,
    file_identity: &str,
) -> Result<MirrorStats, RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let sql = runtime.sql();

    let sessions_owned: Vec<parse::ParsedSession> = sessions.to_vec();
    let events_owned: Vec<parse::ParsedEvent> = events.to_vec();
    let path_owned: PathBuf = path.to_path_buf();
    let identity_owned = file_identity.to_string();

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path_owned,
                source_value,
                &sessions_owned,
                &events_owned,
                MirrorWriteProgress {
                    scanned,
                    new_offset,
                    now_us,
                    file_identity: &identity_owned,
                },
            )
            .await
            .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "session_mirror_write_events_and_cursor",
                    e,
                )
            })
        })
    });

    let boxed = sql
        .atomic_unit(op)
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: atomic_unit: {e}")))?;

    Ok(*boxed.downcast::<MirrorStats>().unwrap_or_else(|_| {
        panic!("atomic_unit op for write_events_and_cursor must return MirrorStats")
    }))
}

/// Create one session row without mutating an existing session. This accepts
/// metadata separately from [`parse::ParsedEvent`] so whole-file exports can
/// persist valid zero-message conversations.
#[allow(clippy::too_many_arguments)]
async fn ensure_session_on_writer(
    writer: &mut dyn SqlWriter,
    source_value: &'static str,
    session_id: &str,
    cwd: Option<&str>,
    git_branch: Option<&str>,
    slug: Option<&str>,
    created_at_micros: i64,
    now_us: i64,
) -> khive_storage::types::StorageResult<()> {
    let created_at = if created_at_micros != 0 {
        created_at_micros
    } else {
        now_us
    };

    writer
        .execute(SqlStatement {
            sql: "INSERT INTO sessions \
                  (id, provider_session_id, source, cwd, git_branch, slug, \
                   message_count, first_seen_at, last_seen_at, namespace) \
                  VALUES(?1, ?1, ?2, ?3, ?4, ?5, 0, ?6, ?6, 'local') \
                  ON CONFLICT(namespace, source, provider_session_id) DO NOTHING"
                .into(),
            params: vec![
                SqlValue::Text(session_id.to_string()),
                SqlValue::Text(source_value.to_string()),
                cwd.map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                git_branch
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                slug.map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(created_at),
            ],
            label: Some("session_mirror_create_session".into()),
        })
        .await
        .map_err(|e| {
            khive_storage::StorageError::driver(
                khive_storage::StorageCapability::Sql,
                "mirror: session create",
                e,
            )
        })?;
    Ok(())
}

/// The synchronous-DML body of `write_events_and_cursor`, run inside one
/// `atomic_unit` closure. Takes a plain `&mut dyn SqlWriter` (not `&mut dyn
/// SqlTransaction`) because `atomic_unit` owns the transaction boundary
/// entirely — this function must not, and does not, issue its own
/// `BEGIN`/`COMMIT`/`ROLLBACK`.
#[derive(Clone, Copy)]
struct MirrorWriteProgress<'a> {
    scanned: u64,
    new_offset: u64,
    now_us: i64,
    file_identity: &'a str,
}

async fn write_events_and_cursor_on_writer(
    writer: &mut dyn SqlWriter,
    path: &Path,
    source_value: &'static str,
    sessions: &[parse::ParsedSession],
    events: &[parse::ParsedEvent],
    progress: MirrorWriteProgress<'_>,
) -> khive_storage::types::StorageResult<MirrorStats> {
    let MirrorWriteProgress {
        scanned,
        new_offset,
        now_us,
        file_identity,
    } = progress;
    let mut inserted: u64 = 0;
    let mut replay_mismatches: u64 = 0;
    let mut last_session_id: Option<String> = None;
    let mut ensured_session_ids = std::collections::HashSet::new();

    // Whole-file parsers can identify a valid conversation independently of
    // whether any of its messages produce displayable events. Create those
    // session rows first, within the same atomic unit as messages and cursor.
    for session in sessions {
        if !ensured_session_ids.insert(session.session_id.clone()) {
            continue;
        }
        ensure_session_on_writer(
            writer,
            source_value,
            &session.session_id,
            session.cwd.as_deref(),
            session.git_branch.as_deref(),
            session.slug.as_deref(),
            session.created_at_micros,
            now_us,
        )
        .await?;
    }

    for ev in events {
        let created_at = if ev.created_at_micros != 0 {
            ev.created_at_micros
        } else {
            now_us
        };
        let event_hash = content_hash(ev.text.as_deref(), &ev.raw);

        // sessions row: create-only (see docs guide — replay is a no-op via
        // `DO NOTHING`; `last_seen_at` advances below only on a new message).
        if ensured_session_ids.insert(ev.session_id.clone()) {
            ensure_session_on_writer(
                writer,
                source_value,
                &ev.session_id,
                ev.cwd.as_deref(),
                ev.git_branch.as_deref(),
                ev.slug.as_deref(),
                ev.created_at_micros,
                now_us,
            )
            .await?;
        }

        // session_messages insert, idempotent only on the full scoped event
        // identity. Unrelated constraint failures must remain visible.
        let affected = writer
            .execute(SqlStatement {
                sql: "INSERT INTO session_messages \
                      (id, session_id, seq, parent_uuid, is_sidechain, role, \
                       msg_type, text, raw, created_at, namespace, source, content_hash) \
                      VALUES(?1, ?2, \
                        (SELECT COALESCE(MAX(seq),-1)+1 FROM session_messages \
                         WHERE namespace='local' AND source=?3 AND session_id=?2), \
                        ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'local', ?3, ?11) \
                      ON CONFLICT(namespace, source, session_id, id) DO NOTHING"
                    .into(),
                params: vec![
                    SqlValue::Text(ev.uuid.clone()),
                    SqlValue::Text(ev.session_id.clone()),
                    SqlValue::Text(source_value.to_string()),
                    ev.parent_uuid
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Integer(i64::from(ev.is_sidechain)),
                    ev.role
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Text(ev.msg_type.clone()),
                    ev.text
                        .as_deref()
                        .map(|s| SqlValue::Text(s.to_string()))
                        .unwrap_or(SqlValue::Null),
                    SqlValue::Text(ev.raw.clone()),
                    SqlValue::Integer(created_at),
                    SqlValue::Text(event_hash.clone()),
                ],
                label: Some("session_mirror_insert_message".into()),
            })
            .await
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "mirror: message insert",
                    e,
                )
            })?;

        if affected == 0 {
            let stored_hash = writer
                .query_scalar(SqlStatement {
                    sql: "SELECT content_hash FROM session_messages \
                          WHERE namespace='local' AND source=?1 AND session_id=?2 AND id=?3"
                        .into(),
                    params: vec![
                        SqlValue::Text(source_value.to_string()),
                        SqlValue::Text(ev.session_id.clone()),
                        SqlValue::Text(ev.uuid.clone()),
                    ],
                    label: Some("session_mirror_replay_hash".into()),
                })
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "mirror: replay hash lookup",
                        e,
                    )
                })?;
            match stored_hash {
                Some(SqlValue::Text(hash)) if hash == event_hash => {}
                Some(SqlValue::Text(_)) => replay_mismatches += 1,
                _ => {
                    return Err(khive_storage::StorageError::Conflict {
                        capability: khive_storage::StorageCapability::Sql,
                        operation: "mirror: message replay".into(),
                        message: "duplicate scoped event id has no stored content hash".into(),
                    });
                }
            }
        }

        // Advance session metadata only when a new message landed — keeps
        // last_seen_at monotonic (MAX) and backfills NULL metadata; a pure
        // replay (affected == 0) touches nothing (see docs guide).
        if affected > 0 {
            writer
                .execute(SqlStatement {
                    sql: "UPDATE sessions SET \
                            last_seen_at=MAX(last_seen_at, ?2), \
                            cwd=COALESCE(cwd, ?3), \
                            git_branch=COALESCE(git_branch, ?4), \
                            slug=COALESCE(slug, ?5) \
                          WHERE namespace='local' AND source=?6 AND provider_session_id=?1"
                        .into(),
                    params: vec![
                        SqlValue::Text(ev.session_id.clone()),
                        SqlValue::Integer(created_at),
                        ev.cwd
                            .as_deref()
                            .map(|s| SqlValue::Text(s.to_string()))
                            .unwrap_or(SqlValue::Null),
                        ev.git_branch
                            .as_deref()
                            .map(|s| SqlValue::Text(s.to_string()))
                            .unwrap_or(SqlValue::Null),
                        ev.slug
                            .as_deref()
                            .map(|s| SqlValue::Text(s.to_string()))
                            .unwrap_or(SqlValue::Null),
                        SqlValue::Text(source_value.to_string()),
                    ],
                    label: Some("session_mirror_touch_session".into()),
                })
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "mirror: session touch",
                        e,
                    )
                })?;
        }

        inserted += affected;
        last_session_id = Some(ev.session_id.clone());
    }

    // Refresh message_count for each distinct session touched; skipped on a
    // pure replay (inserted == 0) since counts cannot have changed.
    if inserted > 0 {
        let mut seen_sessions: Vec<String> = events
            .iter()
            .map(|e| e.session_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        seen_sessions.sort(); // deterministic order for tests

        for sid in &seen_sessions {
            writer
                .execute(SqlStatement {
                    sql: "UPDATE sessions SET message_count=\
                          (SELECT COUNT(*) FROM session_messages \
                           WHERE namespace='local' AND source=?2 AND session_id=?1) \
                          WHERE namespace='local' AND source=?2 AND provider_session_id=?1"
                        .into(),
                    params: vec![
                        SqlValue::Text(sid.clone()),
                        SqlValue::Text(source_value.to_string()),
                    ],
                    label: Some("session_mirror_refresh_count".into()),
                })
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "mirror: count refresh",
                        e,
                    )
                })?;
        }
    }

    upsert_cursor_on_writer(
        writer,
        path,
        last_session_id.as_deref(),
        new_offset,
        now_us,
        Some(file_identity),
    )
    .await?;
    if replay_mismatches > 0 {
        tracing::warn!(
            source = source_value,
            path = %path.display(),
            replay_mismatches,
            "session mirror retained changed-content replay under an existing scoped event id"
        );
    }

    // No explicit COMMIT: `atomic_unit` owns the transaction boundary and
    // commits on `Ok` / rolls back the whole unit on `Err`.
    Ok(MirrorStats {
        inserted,
        replay_mismatches,
        scanned,
        new_offset,
        skipped_oversized_bytes: false,
        file_identity: Some(file_identity.to_string()),
    })
}

/// Upsert the `session_mirror_cursor` row for `path` inside the open
/// `atomic_unit` transaction — issues only the one cursor DML statement, no
/// transaction control of its own.
///
/// The row is keyed by `path.to_string_lossy()` — the same lossy text
/// that [`write_cursor_only`] and the mirror service's `delete_cursors`
/// use for their DELETE, so an insert and a later delete target the same
/// row even when the path is not valid UTF-8. Do not switch one side to a
/// stricter keying without switching all three.
async fn upsert_cursor_on_writer(
    writer: &mut dyn SqlWriter,
    path: &Path,
    session_id: Option<&str>,
    new_offset: u64,
    now_us: i64,
    file_identity: Option<&str>,
) -> khive_storage::types::StorageResult<()> {
    let path_str = path.to_string_lossy().into_owned();
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at, file_identity) \
              VALUES(?1, ?2, ?3, ?4, ?5) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=excluded.session_id, \
                byte_offset=excluded.byte_offset, \
                updated_at=excluded.updated_at, \
                file_identity=excluded.file_identity"
                    .into(),
            params: vec![
                SqlValue::Text(path_str),
                session_id
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(new_offset as i64),
                SqlValue::Integer(now_us),
                file_identity
                    .map(|identity| SqlValue::Text(identity.to_string()))
                    .unwrap_or(SqlValue::Null),
            ],
            label: Some("session_mirror_cursor_upsert".into()),
        })
        .await
        .map_err(|e| {
            khive_storage::StorageError::driver(
                khive_storage::StorageCapability::Sql,
                "mirror: cursor upsert",
                e,
            )
        })?;
    Ok(())
}

/// Write only the cursor row (no events); used when a pass consumed bytes
/// but produced no parseable events, so the offset still advances past
/// blank/unparseable content. A failure here must propagate — see
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#write-path-write_events_and_cursor-and-friends-adr-099-d5`.
///
/// The row is keyed by `path.to_string_lossy()`, the same lossy text used
/// by [`upsert_cursor_on_writer`] (the insert path) and by the mirror
/// service's `delete_cursors`, so insert and delete always target the same
/// row even for non-UTF-8 paths.
async fn write_cursor_only(
    runtime: &KhiveRuntime,
    path: &Path,
    session_id: &Option<String>,
    new_offset: u64,
    file_identity: Option<&str>,
) -> Result<(), RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let path_str = path.to_string_lossy().into_owned();
    let sql = runtime.sql();
    let mut w = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor writer: {e}")))?;
    w.execute(SqlStatement {
        sql: "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at, file_identity) \
              VALUES(?1, ?2, ?3, ?4, ?5) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=COALESCE(excluded.session_id, session_mirror_cursor.session_id), \
                byte_offset=excluded.byte_offset, \
                updated_at=excluded.updated_at, \
                file_identity=excluded.file_identity"
            .into(),
        params: vec![
            SqlValue::Text(path_str),
            session_id
                .as_deref()
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(new_offset as i64),
            SqlValue::Integer(now_us),
            file_identity
                .map(|identity| SqlValue::Text(identity.to_string()))
                .unwrap_or(SqlValue::Null),
        ],
        label: Some("session_mirror_cursor_only".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor write: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod tests;
