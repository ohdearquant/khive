//! Idempotent file tail + upsert into the session mirror tables.
//!
//! `mirror_file` reads new bytes from a JSONL file starting at `start_offset`,
//! parses complete lines via the parser selected by [`LineTailSource`], and
//! writes one bounded chunk (never the whole file at once) to the session
//! mirror tables per call — callers poll repeatedly to drain large deltas.
//! `INSERT OR IGNORE` keyed by the event UUID makes replays idempotent.
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
    /// Number of complete lines or whole-file events scanned (including duplicates).
    pub scanned: u64,
    /// Byte offset advanced to (only past complete lines; partial trailing line excluded).
    pub new_offset: u64,
}

/// How the opened file relates to the cursor supplied for this pass.
///
/// The identity is carried in both dispositions so the polling service never
/// separates an offset decision from the exact opened generation that
/// produced it. Only `RestartedAfterTruncation` authorizes a numerically lower
/// cursor: it is emitted when the requested offset was beyond the opened
/// file's EOF, which proves an in-place generation reset after the service's
/// metadata probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MirrorCursorDisposition {
    Continued { file_identity: Option<String> },
    RestartedAfterReplacement { file_identity: Option<String> },
    RestartedAfterTruncation { file_identity: Option<String> },
}

impl MirrorCursorDisposition {
    pub(super) fn file_identity(&self) -> Option<&str> {
        match self {
            Self::Continued { file_identity }
            | Self::RestartedAfterReplacement { file_identity }
            | Self::RestartedAfterTruncation { file_identity } => file_identity.as_deref(),
        }
    }

    fn restarted(&self) -> bool {
        !matches!(self, Self::Continued { .. })
    }

    pub(super) fn restarted_after_truncation(&self) -> bool {
        matches!(self, Self::RestartedAfterTruncation { .. })
    }
}

/// Where the cursor's generation witness came from.
///
/// A persisted witness belongs to the exact byte offset supplied by a
/// previous successful public [`mirror_file`] call. A mismatch means the
/// same path now names a replacement and can safely replay from zero. A
/// service probe is a point-in-time observation made immediately before the
/// ingest open; a mismatch there is a race and must refuse the pass so the
/// service can reconcile the new generation first.
#[derive(Debug)]
enum CursorContinuity {
    Unchecked,
    Persisted(Option<String>),
    Probed(Option<String>),
}

/// Internal pass result used by the polling service to keep the offset and
/// its opened-file continuity disposition together through candidate
/// dispatch and deferred cursor commits.
#[derive(Debug)]
pub(super) struct MirrorPass {
    pub(super) stats: MirrorStats,
    cursor_disposition: MirrorCursorDisposition,
}

impl MirrorPass {
    pub(super) fn into_parts(self) -> (MirrorStats, MirrorCursorDisposition) {
        (self.stats, self.cursor_disposition)
    }

    #[cfg(test)]
    fn file_identity(&self) -> Option<&str> {
        self.cursor_disposition.file_identity()
    }
}

/// Stable identity witness available directly from portable metadata.
///
/// A path is not an identity: editors and log rotators commonly replace a
/// file atomically while retaining its path and length. Unix exposes the
/// device/inode pair through stable `std`; Windows identity instead comes from
/// [`opened_file_identity`] because its corresponding `std::fs::MetadataExt`
/// methods remain unstable.
#[cfg(unix)]
pub(super) fn metadata_file_identity(metadata: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    Some(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
}

/// Stable identity for an already-open file handle. Keeping the handle and
/// its metadata together binds the witness to the same generation whose
/// length and bytes the caller observes.
pub(super) fn opened_file_identity(
    file: &std::fs::File,
    metadata: &std::fs::Metadata,
) -> std::io::Result<Option<String>> {
    #[cfg(unix)]
    {
        let _ = file;
        Ok(metadata_file_identity(metadata))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let _ = metadata;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle for the duration of this call and
        // `info` is the correctly sized writable result structure.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        if ok == 0 {
            // Some remote/custom filesystems do not expose a stable file id.
            // Preserve mirroring with the documented length-only fallback.
            Ok(None)
        } else {
            let file_index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
            // Preserve the pre-existing cursor wire format exactly so moving
            // off unstable `MetadataExt` does not force a one-time replay.
            Ok(Some(format!(
                "windows:{}:{file_index}",
                info.dwVolumeSerialNumber
            )))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        Ok(None)
    }
}

/// Probe one path's length/mtime and identity from a single file generation.
/// Windows must open the file because stable `MetadataExt` lacks file ids;
/// Unix can obtain all three values from one `stat` result.
pub(super) fn probe_file(path: &Path) -> std::io::Result<(std::fs::Metadata, Option<String>)> {
    #[cfg(windows)]
    {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        let identity = opened_file_identity(&file, &metadata)?;
        Ok((metadata, identity))
    }
    #[cfg(unix)]
    {
        let metadata = std::fs::metadata(path)?;
        let identity = metadata_file_identity(&metadata);
        Ok((metadata, identity))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = std::fs::metadata(path)?;
        Ok((metadata, None))
    }
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
/// Returns stats including the advanced byte offset.  A partial trailing line
/// (no terminating `\n`) is left for the next poll — `new_offset` is set to
/// the byte after the last complete `\n`.
///
/// Repeating this public call with the offset returned by its preceding
/// successful pass is generation-safe: the persisted cursor binds that
/// offset to the opened file identity, so a same-path replacement replays
/// from zero even when its length is unchanged. Arbitrary caller-selected
/// replay offsets retain the strict length-decrease fallback.
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
    let continuity = persisted_cursor_continuity(runtime, path, start_offset).await?;
    Ok(mirror_file_inner(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        MirrorLimits::production(),
        true,
        continuity,
    )
    .await?
    .stats)
}

/// Service-only deferred variant which binds an offset to the identity of the
/// file handle that produced it. `expected_file_identity` comes from the
/// service's metadata probe; if the path was replaced before this function
/// opens it, the pass is refused so the next probe can reconcile the new file
/// from byte zero instead of blessing it with its predecessor's offset. If
/// the identity still matches but the requested offset is beyond the opened
/// EOF, the returned `MirrorPass` carries an explicit truncation-reset
/// disposition and that opened identity through service dispatch.
pub(super) async fn mirror_file_deferred_with_witness(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    expected_file_identity: Option<&str>,
) -> Result<MirrorPass, RuntimeError> {
    mirror_file_inner(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        MirrorLimits::production(),
        false,
        CursorContinuity::Probed(expected_file_identity.map(str::to_owned)),
    )
    .await
}

/// Commit a deferred empty advance using the identity captured from the same
/// opened file that produced `new_offset`. This never re-stats the path and
/// therefore cannot associate an old offset with a replacement that landed
/// between the read and commit.
pub(super) async fn commit_empty_advance_with_witness(
    runtime: &KhiveRuntime,
    path: &Path,
    new_offset: u64,
    file_identity: Option<&str>,
) -> Result<(), RuntimeError> {
    write_cursor_only(runtime, path, &None, new_offset, file_identity).await
}

/// Persist a continuity reset before the service takes its EOF fast path.
pub(super) async fn commit_cursor_reset(
    runtime: &KhiveRuntime,
    path: &Path,
    file_identity: Option<&str>,
) -> Result<(), RuntimeError> {
    write_cursor_only(runtime, path, &None, 0, file_identity).await
}

/// Recover the generation witness that belongs to `start_offset`.
///
/// Only an exact offset match is evidence that the stored identity and the
/// caller's offset came from the same successful pass. An arbitrary replay
/// offset remains unchecked and retains the strict length-decrease fallback.
async fn persisted_cursor_continuity(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
) -> Result<CursorContinuity, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|error| RuntimeError::Internal(format!("mirror_file: cursor reader: {error}")))?;
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT byte_offset, file_identity FROM session_mirror_cursor WHERE file_path=?1"
                .into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: Some("mirror_cursor_continuity".into()),
        })
        .await
        .map_err(|error| RuntimeError::Internal(format!("mirror_file: cursor read: {error}")))?;
    let Some(row) = row else {
        return Ok(CursorContinuity::Unchecked);
    };
    let stored_offset = match row.get("byte_offset") {
        Some(SqlValue::Integer(offset)) => u64::try_from(*offset).ok(),
        _ => None,
    };
    if stored_offset != Some(start_offset) {
        return Ok(CursorContinuity::Unchecked);
    }
    let file_identity = match row.get("file_identity") {
        Some(SqlValue::Text(identity)) => Some(identity.clone()),
        _ => None,
    };
    Ok(CursorContinuity::Persisted(file_identity))
}

/// A single bounded read pass: at most `limits.max_bytes_per_pass` bytes and
/// `limits.max_events_per_pass` parsed events, stopping on a line boundary.
struct MirrorChunk {
    events: Vec<parse::ParsedEvent>,
    scanned: u64,
    new_offset: u64,
    cursor_disposition: MirrorCursorDisposition,
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
    /// Exceeded `max_line_bytes` with no `\n` found yet, and NOT at EOF.
    /// Unlike `Oversized`, the caller must not advance past it.
    OversizedUnterminated { bytes: usize },
}

/// Read one line from `reader` into `buf`, never buffering — or reading —
/// more than `max_line_bytes` regardless of how long the underlying line
/// turns out to be (the PACKSESSION-AUD-003 bound; see the docs guide above
/// for why `BufRead::read_until` alone is unsafe here).
fn read_line_bounded(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max_line_bytes: usize,
) -> std::io::Result<LineRead> {
    let mut total: usize = 0;
    let mut oversized = false;

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

        if oversized {
            // Already over the cap and this fill_buf window had no `\n`:
            // stop here rather than looping onward toward EOF (or forever,
            // if the file keeps growing). See the PACKSESSION-AUD-003 bound
            // above.
            return Ok(LineRead::OversizedUnterminated { bytes: total });
        }
        // No `\n` in this fill_buf window yet, and still under the cap;
        // loop for more data, buffering normally.
    }
}

/// Read at most one bounded chunk of `path` starting at `start_offset`. A
/// complete line whose buffered size exceeds `limits.max_line_bytes` is
/// skipped outright (bytes counted, offset advances past it, `tracing::warn!`
/// names the file/offset — PACKSESSION-AUD-003, no silent coercion); a
/// partial trailing line is left for the next call.
fn read_bounded_chunk(
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
    continuity: &CursorContinuity,
) -> std::io::Result<MirrorChunk> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    let file_len = metadata.len();
    let file_identity = opened_file_identity(&file, &metadata)?;
    let replaced = match continuity {
        CursorContinuity::Persisted(previous_identity) => file_identity
            .as_ref()
            .is_some_and(|identity| previous_identity.as_ref() != Some(identity)),
        CursorContinuity::Probed(Some(expected_identity))
            if file_identity.as_ref() != Some(expected_identity) =>
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "file identity changed between metadata probe and ingest open",
            ));
        }
        CursorContinuity::Unchecked
        | CursorContinuity::Persisted(_)
        | CursorContinuity::Probed(_) => false,
    };
    // An offset beyond EOF proves in-place truncation (or a shorter same-path
    // replacement). Replay from the beginning; equality remains ordinary EOF
    // when the opened file is the generation the service expected.
    let cursor_disposition = if replaced {
        MirrorCursorDisposition::RestartedAfterReplacement {
            file_identity: file_identity.clone(),
        }
    } else if start_offset > file_len {
        MirrorCursorDisposition::RestartedAfterTruncation {
            file_identity: file_identity.clone(),
        }
    } else {
        MirrorCursorDisposition::Continued {
            file_identity: file_identity.clone(),
        }
    };
    let start_offset = if cursor_disposition.restarted() {
        0
    } else {
        start_offset
    };
    if start_offset == file_len {
        return Ok(MirrorChunk {
            events: Vec::new(),
            scanned: 0,
            new_offset: start_offset,
            cursor_disposition,
        });
    }

    file.seek(SeekFrom::Start(start_offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut scanned: u64 = 0;
    let mut lines_consumed: u64 = 0;
    let mut new_offset = start_offset;
    let mut bytes_this_pass: usize = 0;

    loop {
        if lines_consumed > 0
            && (bytes_this_pass >= limits.max_bytes_per_pass
                || events.len() >= limits.max_events_per_pass)
        {
            break;
        }

        line.clear();
        let line_offset = new_offset;

        match read_line_bounded(&mut reader, &mut line, limits.max_line_bytes)? {
            LineRead::Eof => break,
            LineRead::Partial => break, // leave partial trailing line for next pass
            LineRead::OversizedUnterminated { bytes } => {
                // Already over max_line_bytes with no `\n` found in this
                // bounded read (see `read_line_bounded`'s bound above):
                // do NOT advance new_offset past line_offset. The next call
                // re-reads from the same line_offset and is bounded the
                // same way — cheap and repeatable, whether the file is
                // still growing (a later pass will eventually see the
                // terminator and fall into the `Oversized` skip-and-advance
                // arm below) or genuinely corrupt/truncated (every later
                // poll or daemon restart repeats this same bounded read,
                // never the unbounded tail scan PACKSESSION-AUD-003 flagged).
                tracing::warn!(
                    path = %path.display(),
                    offset = line_offset,
                    line_bytes = bytes,
                    max_line_bytes = limits.max_line_bytes,
                    "session mirror: oversized JSONL line has no terminator in this bounded read; \
                     leaving cursor at line start for a bounded retry"
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
        cursor_disposition,
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
    let continuity = persisted_cursor_continuity(runtime, path, start_offset).await?;
    Ok(mirror_file_inner(
        runtime,
        path,
        start_offset,
        source,
        codex_session_id,
        limits,
        true,
        continuity,
    )
    .await?
    .stats)
}

/// Implementation of [`mirror_file`]. When `commit_empty_cursor` is false,
/// a pass that consumed bytes but parsed no events does NOT commit the
/// cursor — the dispatch loop commits it with the opened-file witness only
/// if no later candidate inserts rows for the same span. When true (every
/// non-dispatch caller and test), the cursor is committed immediately.
async fn mirror_file_inner(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    source: LineTailSource,
    codex_session_id: Option<&str>,
    limits: MirrorLimits,
    commit_empty_cursor: bool,
    continuity: CursorContinuity,
) -> Result<MirrorPass, RuntimeError> {
    let chunk = read_bounded_chunk(
        path,
        start_offset,
        source,
        codex_session_id,
        limits,
        &continuity,
    )
    .map_err(|e| {
        RuntimeError::Internal(format!(
            "mirror_file: failed to read {:?} at offset {start_offset}: {e}",
            path
        ))
    })?;

    let restarted = chunk.cursor_disposition.restarted();
    if chunk.new_offset == start_offset && !restarted {
        // Nothing was consumed this pass (EOF, or only a partial trailing
        // line was seen) — there is no advanced cursor to persist.
        return Ok(MirrorPass {
            stats: MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: chunk.new_offset,
            },
            cursor_disposition: chunk.cursor_disposition,
        });
    }

    if chunk.events.is_empty() {
        // Bytes were consumed, or an opened-file truncation proved the
        // supplied cursor belongs to an earlier generation, but nothing
        // parsed — e.g. a chunk made entirely of blank lines, unparseable
        // lines, or skipped oversized lines. With `commit_empty_cursor`,
        // apply the cursor update immediately so we don't re-read the same
        // bytes on the next call. Without it (the dispatch loop), the commit
        // is deferred to
        // the witnessed cursor-only helper, which the loop calls only when no later
        // candidate inserted rows for the span — so the cursor never moves
        // past bytes another provider candidate might still parse, and an
        // interrupt between candidates cannot strand a committed empty
        // advance ahead of unconsumed rows. A failure here must propagate —
        // silently swallowing it would let the cursor and the
        // already-consumed bytes drift apart.
        if commit_empty_cursor {
            write_cursor_only(
                runtime,
                path,
                &None,
                chunk.new_offset,
                chunk.cursor_disposition.file_identity(),
            )
            .await?;
        }
        return Ok(MirrorPass {
            stats: MirrorStats {
                inserted: 0,
                scanned: chunk.scanned,
                new_offset: chunk.new_offset,
            },
            cursor_disposition: chunk.cursor_disposition,
        });
    }

    let file_identity = chunk.cursor_disposition.file_identity().map(str::to_owned);
    let stats = write_events_and_cursor(
        runtime,
        path,
        MirrorSource::from(source).as_str(),
        &[],
        &chunk.events,
        chunk.scanned,
        chunk.new_offset,
        file_identity,
    )
    .await?;
    Ok(MirrorPass {
        stats,
        cursor_disposition: chunk.cursor_disposition,
    })
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
pub(super) async fn mirror_chatgpt_export_file(
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
    mirror_whole_file_export(runtime, path, start_offset, max_bytes, CHATGPT_EXPORT_SPEC).await
}

/// Read a whole claude.ai export `conversations.json`, parse its
/// `chat_messages` arrays via [`parse::parse_claude_ai_export`], and commit
/// the resulting sessions, messages, and cursor atomically.
pub(super) async fn mirror_claude_ai_export_file(
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
    )
    .await
}

async fn mirror_whole_file_export(
    runtime: &KhiveRuntime,
    path: &Path,
    start_offset: u64,
    max_bytes: u64,
    spec: WholeFileExportSpec,
) -> Result<MirrorStats, RuntimeError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        RuntimeError::Internal(format!("{}: failed to open {path:?}: {e}", spec.operation))
    })?;
    let metadata = file.metadata().map_err(|e| {
        RuntimeError::Internal(format!("{}: failed to stat {path:?}: {e}", spec.operation))
    })?;
    let file_len = metadata.len();
    let file_identity = opened_file_identity(&file, &metadata).map_err(|e| {
        RuntimeError::Internal(format!(
            "{}: failed to identify {path:?}: {e}",
            spec.operation
        ))
    })?;

    if file_len <= start_offset {
        return Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset,
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
            scanned: 0,
            new_offset: start_offset,
        });
    }

    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| {
        RuntimeError::Internal(format!("{}: failed to read {path:?}: {e}", spec.operation))
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
        file_identity,
    )
    .await
}

/// Upsert explicit sessions, `events`, and the mirror cursor for `path` in one
/// transaction.
/// Shared by `mirror_file`'s line-tail path and both provider-export
/// whole-file paths. See
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#write-path-write_events_and_cursor-and-friends-adr-099-d5`
/// for the ADR-099 D5 suspension-free rationale.
async fn write_events_and_cursor(
    runtime: &KhiveRuntime,
    path: &Path,
    source_value: &'static str,
    sessions: &[parse::ParsedSession],
    events: &[parse::ParsedEvent],
    scanned: u64,
    new_offset: u64,
    file_identity: Option<String>,
) -> Result<MirrorStats, RuntimeError> {
    let now_us = Utc::now().timestamp_micros();
    let sql = runtime.sql();

    let sessions_owned: Vec<parse::ParsedSession> = sessions.to_vec();
    let events_owned: Vec<parse::ParsedEvent> = events.to_vec();
    let path_owned: PathBuf = path.to_path_buf();

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path_owned,
                source_value,
                &sessions_owned,
                &events_owned,
                file_identity.as_deref(),
                MirrorWriteProgress {
                    scanned,
                    new_offset,
                    now_us,
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
            sql: format!(
                "INSERT INTO sessions \
                  (id, provider_session_id, source, cwd, git_branch, slug, \
                   message_count, first_seen_at, last_seen_at, namespace) \
                  VALUES(?1, ?1, '{}', ?2, ?3, ?4, 0, ?5, ?5, 'local') \
                  ON CONFLICT(id) DO NOTHING",
                source_value
            ),
            params: vec![
                SqlValue::Text(session_id.to_string()),
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
struct MirrorWriteProgress {
    scanned: u64,
    new_offset: u64,
    now_us: i64,
}

async fn write_events_and_cursor_on_writer(
    writer: &mut dyn SqlWriter,
    path: &Path,
    source_value: &'static str,
    sessions: &[parse::ParsedSession],
    events: &[parse::ParsedEvent],
    file_identity: Option<&str>,
    progress: MirrorWriteProgress,
) -> khive_storage::types::StorageResult<MirrorStats> {
    let MirrorWriteProgress {
        scanned,
        new_offset,
        now_us,
    } = progress;
    let mut inserted: u64 = 0;
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

        // session_messages insert, idempotent via INSERT OR IGNORE.
        let affected = writer
            .execute(SqlStatement {
                sql: "INSERT OR IGNORE INTO session_messages \
                      (id, session_id, seq, parent_uuid, is_sidechain, role, \
                       msg_type, text, raw, created_at, namespace) \
                      VALUES(?1, ?2, \
                        (SELECT COALESCE(MAX(seq),-1)+1 FROM session_messages WHERE session_id=?2), \
                        ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'local')"
                    .into(),
                params: vec![
                    SqlValue::Text(ev.uuid.clone()),
                    SqlValue::Text(ev.session_id.clone()),
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
                          WHERE id=?1"
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
                          (SELECT COUNT(*) FROM session_messages WHERE session_id=?1) \
                          WHERE id=?1"
                        .into(),
                    params: vec![SqlValue::Text(sid.clone())],
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
        file_identity,
        now_us,
    )
    .await?;

    // No explicit COMMIT: `atomic_unit` owns the transaction boundary and
    // commits on `Ok` / rolls back the whole unit on `Err`.
    Ok(MirrorStats {
        inserted,
        scanned,
        new_offset,
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
    file_identity: Option<&str>,
    now_us: i64,
) -> khive_storage::types::StorageResult<()> {
    let path_str = path.to_string_lossy().into_owned();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO session_mirror_cursor(\
                    file_path, session_id, byte_offset, file_identity, updated_at\
                 ) VALUES(?1, ?2, ?3, ?4, ?5) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=excluded.session_id, \
                byte_offset=excluded.byte_offset, \
                file_identity=COALESCE(\
                    excluded.file_identity, session_mirror_cursor.file_identity\
                ), \
                updated_at=excluded.updated_at"
                .into(),
            params: vec![
                SqlValue::Text(path_str),
                session_id
                    .map(|s| SqlValue::Text(s.to_string()))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(new_offset as i64),
                file_identity
                    .map(|identity| SqlValue::Text(identity.to_string()))
                    .unwrap_or(SqlValue::Null),
                SqlValue::Integer(now_us),
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
        sql: "INSERT INTO session_mirror_cursor(\
                file_path, session_id, byte_offset, file_identity, updated_at\
              ) VALUES(?1, ?2, ?3, ?4, ?5) \
              ON CONFLICT(file_path) DO UPDATE SET \
                session_id=COALESCE(excluded.session_id, session_mirror_cursor.session_id), \
                byte_offset=excluded.byte_offset, \
                file_identity=COALESCE(\
                    excluded.file_identity, session_mirror_cursor.file_identity\
                ), \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(path_str),
            session_id
                .as_deref()
                .map(|s| SqlValue::Text(s.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(new_offset as i64),
            file_identity
                .map(|identity| SqlValue::Text(identity.to_string()))
                .unwrap_or(SqlValue::Null),
            SqlValue::Integer(now_us),
        ],
        label: Some("session_mirror_cursor_only".into()),
    })
    .await
    .map_err(|e| RuntimeError::Internal(format!("mirror_file: cursor write: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use khive_runtime::{
        AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    };
    use khive_storage::types::{SqlStatement, SqlValue};
    use tempfile::{NamedTempFile, TempDir};

    use super::*;
    use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

    #[cfg(windows)]
    #[test]
    fn windows_identity_comes_from_a_stable_open_handle_api() {
        let file = NamedTempFile::new().expect("temporary transcript");
        let first_handle = std::fs::File::open(file.path()).expect("open first handle");
        let first_metadata = first_handle.metadata().expect("first metadata");
        let first = opened_file_identity(&first_handle, &first_metadata)
            .expect("Windows handle identity query");

        let second_handle = std::fs::File::open(file.path()).expect("open second handle");
        let second_metadata = second_handle.metadata().expect("second metadata");
        let second = opened_file_identity(&second_handle, &second_metadata)
            .expect("second Windows handle identity query");

        assert_eq!(first, second, "two handles to one file must agree");
        if let Some(first) = first {
            assert!(first.starts_with("windows:"), "unexpected witness: {first}");
            assert_eq!(first.split(':').count(), 3, "unexpected witness: {first}");
        }
    }

    /// Build a file-backed runtime (exercises the real `atomic_unit`
    /// single-writer path) and apply the session schema. Caller must keep
    /// the returned `TempDir` alive.
    async fn setup() -> (KhiveRuntime, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(db_path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("file-backed runtime");
        apply_session_schema(&rt).await;
        (rt, dir)
    }

    async fn apply_session_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        for stmt in &SESSION_SCHEMA_PLAN_STMTS {
            w.execute_script(stmt.to_string())
                .await
                .expect("schema stmt");
        }
        // w dropped here — releases the writer connection.
    }

    /// Count rows in a table.
    async fn count_rows(rt: &KhiveRuntime, table: &str) -> i64 {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: format!("SELECT COUNT(*) FROM {table}"),
                params: vec![],
                label: None,
            })
            .await
            .expect("count query")
            .expect("count row");
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => *n,
            _ => 0,
        }
    }

    /// Retrieve the stored byte_offset for a file path.
    async fn cursor_offset(rt: &KhiveRuntime, path_str: &str) -> Option<i64> {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path=?1".into(),
                params: vec![SqlValue::Text(path_str.to_string())],
                label: None,
            })
            .await
            .expect("cursor query")?;
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }
    }

    /// Retrieve the file-identity witness stored with a cursor.
    async fn cursor_file_identity(rt: &KhiveRuntime, path_str: &str) -> Option<String> {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT file_identity FROM session_mirror_cursor WHERE file_path=?1".into(),
                params: vec![SqlValue::Text(path_str.to_string())],
                label: None,
            })
            .await
            .expect("cursor identity query")?;
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Text(identity)) => Some(identity.clone()),
            _ => None,
        }
    }

    fn user_line(uuid: &str, session_id: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","timestamp":"2026-06-29T10:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// A user line with NO `timestamp` field — `created_at` falls back to `now_us`.
    fn user_line_no_ts(uuid: &str, session_id: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    /// Retrieve the stored `last_seen_at` for a session id.
    async fn last_seen_at(rt: &KhiveRuntime, session_id: &str) -> Option<i64> {
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT last_seen_at FROM sessions WHERE id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("last_seen query")?;
        match row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => Some(*n),
            _ => None,
        }
    }

    #[tokio::test]
    async fn test_mirror_three_lines_and_idempotency() {
        let (rt, _dir) = setup().await;

        // Build a fixture JSONL with 3 lines, all ending in '\n'.
        let line1 = user_line("uuid-1", "sess-A", "Hello");
        let line2 = user_line("uuid-2", "sess-A", "World");
        let line3 = user_line("uuid-3", "sess-A", "Done");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line1}").unwrap();
        writeln!(file, "{line2}").unwrap();
        writeln!(file, "{line3}").unwrap();

        let path = file.path().to_path_buf();

        // First call: should insert all 3 rows.
        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror_file first call");
        assert_eq!(stats.inserted, 3, "should insert 3 new messages");
        assert_eq!(stats.scanned, 3, "should scan 3 lines");
        assert!(stats.new_offset > 0, "offset should advance");

        let msg_count = count_rows(&rt, "session_messages").await;
        assert_eq!(msg_count, 3, "3 messages in DB");

        let session_count = count_rows(&rt, "sessions").await;
        assert_eq!(session_count, 1, "1 session row");

        // Idempotency: second call over the SAME range inserts 0 rows.
        let stats2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror_file second call");
        assert_eq!(stats2.inserted, 0, "second pass must insert 0 rows");
        assert_eq!(count_rows(&rt, "session_messages").await, 3);

        // Offset-aware: calling from the advanced offset finds nothing new.
        let stats3 = mirror_file(
            &rt,
            &path,
            stats.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("mirror_file from new_offset");
        assert_eq!(stats3.inserted, 0, "no new data past advanced offset");
        assert_eq!(stats3.new_offset, stats.new_offset);

        // Cursor was recorded.
        let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
        assert!(stored_offset.is_some(), "cursor should be recorded");
        assert_eq!(stored_offset.unwrap(), stats.new_offset as i64);

        assert_eq!(
            cursor_file_identity(&rt, &path.to_string_lossy()).await,
            probe_file(&path).expect("transcript identity").1,
            "cursor must persist the identity witness from the opened transcript"
        );
    }

    #[tokio::test]
    async fn mirror_file_restarts_after_same_path_truncation() {
        let (rt, _dir) = setup().await;
        let path = _dir.path().join("truncated.jsonl");
        let old = format!(
            "{}\n{}\n",
            user_line("uuid-truncate-old-1", "sess-truncate", "old one"),
            user_line("uuid-truncate-old-2", "sess-truncate", "old two")
        );
        std::fs::write(&path, old).expect("write original transcript");

        let first = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror original transcript");
        let replacement = format!(
            "{}\n",
            user_line("uuid-truncate-new", "sess-truncate", "new")
        );
        std::fs::write(&path, replacement).expect("truncate and rewrite transcript");
        let replacement_len = std::fs::metadata(&path)
            .expect("replacement metadata")
            .len();
        assert!(replacement_len < first.new_offset);

        let second = mirror_file(
            &rt,
            &path,
            first.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("mirror truncated transcript from stale offset");

        assert_eq!(second.inserted, 1, "new prefix must not be skipped");
        assert_eq!(second.new_offset, replacement_len);
        assert_eq!(count_rows(&rt, "session_messages").await, 3);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(replacement_len as i64)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_public_mirror_file_call_replays_same_length_replacement() {
        let (rt, dir) = setup().await;
        let path = dir.path().join("same-length-public.jsonl");
        let replacement_path = dir.path().join("same-length-public.next.jsonl");
        let old = format!(
            "{}\n",
            user_line("uuid-public-old", "sess-public-replace", "old")
        );
        let new = format!(
            "{}\n",
            user_line("uuid-public-new", "sess-public-replace", "new")
        );
        assert_eq!(old.len(), new.len());
        std::fs::write(&path, old).expect("original transcript");

        let first = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror original generation");
        assert_eq!(first.inserted, 1);

        std::fs::write(&replacement_path, new).expect("replacement transcript");
        std::fs::rename(&replacement_path, &path).expect("same-path atomic replacement");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("replacement metadata")
                .len(),
            first.new_offset
        );

        let second = mirror_file(
            &rt,
            &path,
            first.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("mirror replacement generation");
        assert_eq!(
            second.inserted, 1,
            "the public API must bind its offset to the persisted generation witness"
        );
        assert_eq!(second.new_offset, first.new_offset);
        assert_eq!(count_rows(&rt, "session_messages").await, 2);
    }

    #[tokio::test]
    async fn mirror_file_respects_low_test_cap_and_advances_over_multiple_passes() {
        // PACKSESSION-AUD-003 regression: multi-pass bounded reads (see docs guide).
        let (rt, _dir) = setup().await;

        let lines: Vec<String> = (0..6)
            .map(|i| user_line(&format!("uuid-cap-{i}"), "sess-CAP", &format!("line{i}")))
            .collect();

        let mut file = NamedTempFile::new().expect("tmpfile");
        for line in &lines {
            writeln!(file, "{line}").unwrap();
        }
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        // All 6 fixture lines are byte-identical in length, so capping at
        // exactly two lines' worth of bytes forces a 2-line-per-pass split
        // without needing a giant fixture.
        let cap_bytes = (lines[0].len() + 1) + (lines[1].len() + 1);
        let limits = MirrorLimits {
            max_bytes_per_pass: cap_bytes,
            max_events_per_pass: 1024,
            max_line_bytes: MIRROR_MAX_LINE_BYTES,
        };

        let stats1 =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("first bounded pass");
        assert_eq!(
            stats1.inserted, 2,
            "first pass must stop at the byte cap, not read the whole file"
        );
        assert_eq!(stats1.scanned, 2);
        assert!(
            stats1.new_offset < file_len,
            "new_offset {new} must be less than file_len {file_len} for a bounded pass",
            new = stats1.new_offset
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(stats1.new_offset as i64),
            "cursor must be committed after the first bounded pass"
        );

        let stats2 = mirror_file_with_limits(
            &rt,
            &path,
            stats1.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("second bounded pass");
        assert_eq!(stats2.inserted, 2);
        assert!(stats2.new_offset > stats1.new_offset);
        assert!(stats2.new_offset < file_len);

        let stats3 = mirror_file_with_limits(
            &rt,
            &path,
            stats2.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("third bounded pass");
        assert_eq!(stats3.inserted, 2);
        assert_eq!(stats3.new_offset, file_len, "final pass must reach EOF");

        // All 6 rows landed across 3 bounded passes, and the cursor reflects
        // the full file — no pass allocated or inserted the entire file at
        // once.
        assert_eq!(count_rows(&rt, "session_messages").await, 6);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64)
        );

        // A pass with no remaining bytes is a clean no-op.
        let stats4 = mirror_file_with_limits(
            &rt,
            &path,
            stats3.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("fourth pass at EOF");
        assert_eq!(stats4.inserted, 0);
        assert_eq!(stats4.scanned, 0);
    }

    #[tokio::test]
    async fn test_oversized_line_is_skipped_and_offset_advances() {
        // PACKSESSION-AUD-003 regression: oversized complete line (see docs guide).
        let (rt, _dir) = setup().await;

        let small1 = user_line("uuid-small1", "sess-OV", "ok");
        let huge_text = "x".repeat(2000);
        let huge = user_line("uuid-huge", "sess-OV", &huge_text);
        let small2 = user_line("uuid-small2", "sess-OV", "after");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{small1}").unwrap();
        writeln!(file, "{huge}").unwrap();
        writeln!(file, "{small2}").unwrap();
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let max_line_bytes: usize = 256;
        assert!(
            huge.len() + 1 > max_line_bytes,
            "fixture huge line must exceed the cap"
        );
        assert!(
            small1.len() + 1 < max_line_bytes && small2.len() + 1 < max_line_bytes,
            "fixture small lines must fit under the cap"
        );

        let limits = MirrorLimits {
            max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
            max_line_bytes,
        };

        let stats =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("mirror with a small line cap");

        assert_eq!(stats.inserted, 2, "only the two small lines are inserted");
        assert_eq!(
            stats.scanned, 2,
            "the oversized line must not count toward scanned"
        );
        assert_eq!(
            stats.new_offset, file_len,
            "offset must advance past the oversized line, not wedge on it"
        );
        assert_eq!(count_rows(&rt, "session_messages").await, 2);
    }

    #[tokio::test]
    async fn test_line_just_under_cap_then_oversized_next_line_is_bounded() {
        // PACKSESSION-AUD-003 regression: under-cap line followed by an
        // oversized one (see docs guide).
        let (rt, _dir) = setup().await;

        let max_line_bytes: usize = 256;
        let shell_len = user_line("uuid-a", "sess-BND", "").len() + 1; // + '\n'
        let pad = max_line_bytes.saturating_sub(shell_len).saturating_sub(4);
        let text_a = "y".repeat(pad);
        let line_a = user_line("uuid-a", "sess-BND", &text_a);

        let huge_text = "z".repeat(max_line_bytes * 4);
        let line_b = user_line("uuid-b", "sess-BND", &huge_text);

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line_a}").unwrap();
        writeln!(file, "{line_b}").unwrap();
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        assert!(
            line_a.len() + 1 < max_line_bytes,
            "fixture line A must land just under the cap"
        );
        assert!(
            line_b.len() + 1 > max_line_bytes,
            "fixture line B must land over the cap"
        );

        let limits = MirrorLimits {
            max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
            max_line_bytes,
        };

        let stats =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("mirror with a boundary line cap");

        assert_eq!(stats.inserted, 1, "only the under-cap line is inserted");
        assert_eq!(
            stats.scanned, 1,
            "the oversized line must not count toward scanned"
        );
        assert_eq!(
            stats.new_offset, file_len,
            "offset must advance past both lines, including the skipped oversized one"
        );
        assert_eq!(count_rows(&rt, "session_messages").await, 1);
    }

    /// Counts every byte pulled through `Read::read`, for asserting a hard
    /// ceiling on `read_line_bounded`'s reads independent of buffer size.
    struct CountingReader<R> {
        inner: R,
        total_read: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl<R: std::io::Read> std::io::Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.total_read.set(self.total_read.get() + n);
            Ok(n)
        }
    }

    #[test]
    fn test_read_line_bounded_oversized_unterminated_reads_are_capped_per_call() {
        // PACKSESSION-AUD-003 regression: oversized-unterminated reads are
        // capped per call, not just per buffered byte (see docs guide).
        let max_line_bytes: usize = 64;
        let buf_capacity: usize = 256;
        // Far larger than max_line_bytes + a handful of buffer refills, and
        // containing NO '\n' anywhere — the pathological unterminated case.
        let data = vec![b'x'; 200_000];

        let total_read = std::rc::Rc::new(std::cell::Cell::new(0));
        let counting = CountingReader {
            inner: std::io::Cursor::new(data),
            total_read: total_read.clone(),
        };
        let mut reader = std::io::BufReader::with_capacity(buf_capacity, counting);
        let mut buf = Vec::new();

        let outcome =
            read_line_bounded(&mut reader, &mut buf, max_line_bytes).expect("read must not error");

        match outcome {
            LineRead::OversizedUnterminated { bytes } => {
                assert!(
                    bytes > max_line_bytes,
                    "must have detected the crossing of the cap, got {bytes}"
                );
            }
            other => panic!("expected OversizedUnterminated, got {other:?}"),
        }
        assert!(
            buf.is_empty(),
            "buf must never buffer anything once the line is flagged oversized"
        );

        // The load-bearing assertion: total bytes ever pulled from the
        // underlying 200,000-byte source must be bounded to roughly
        // max_line_bytes plus a small, constant number of buffer refills —
        // never anywhere close to scanning the whole remaining file.
        let read_bytes = total_read.get();
        assert!(
            read_bytes <= max_line_bytes + buf_capacity * 4,
            "read_line_bounded pulled {read_bytes} bytes from the source for an \
             unterminated oversized line — expected at most {} (bounded to the \
             cap plus a few buffer refills), not an unbounded scan toward EOF",
            max_line_bytes + buf_capacity * 4
        );
    }

    #[tokio::test]
    async fn test_oversized_unterminated_line_leaves_cursor_at_line_start_and_is_bounded_on_retry()
    {
        // PACKSESSION-AUD-003 regression: unterminated-oversized-line cursor
        // handling and bounded retry (see docs guide).
        let (rt, _dir) = setup().await;

        let max_line_bytes: usize = 256;
        // One line, far larger than the cap, with no terminating '\n' at all.
        let huge_unterminated = "z".repeat(max_line_bytes * 20);

        let mut file = NamedTempFile::new().expect("tmpfile");
        file.write_all(huge_unterminated.as_bytes())
            .expect("write unterminated line");
        let path = file.path().to_path_buf();

        let limits = MirrorLimits {
            max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
            max_line_bytes,
        };

        // First pass: the oversized-unterminated line must not advance the
        // cursor at all (same policy as an ordinary `Partial`).
        let stats1 =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("first pass over an unterminated oversized line");
        assert_eq!(
            stats1.new_offset, 0,
            "cursor must stay at the line start — nothing was durably consumed"
        );
        assert_eq!(stats1.scanned, 0);
        assert_eq!(stats1.inserted, 0);
        assert_eq!(
            count_rows(&rt, "session_messages").await,
            0,
            "no partial/garbage row may be written for an unterminated oversized line"
        );

        // Second pass from the persisted (unchanged) offset behaves
        // identically — a durable, bounded retry, never a wedge that grows
        // unboundedly worse, and no replay of previously-seen bytes as new
        // events (there were none).
        let stats2 = mirror_file_with_limits(
            &rt,
            &path,
            stats1.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("second pass (simulated daemon restart) over the same unterminated line");
        assert_eq!(stats2.new_offset, 0);
        assert_eq!(stats2.scanned, 0);
        assert_eq!(stats2.inserted, 0);
        assert_eq!(count_rows(&rt, "session_messages").await, 0);

        // Now the line completes (append a terminating '\n' and a bit more,
        // simulating the file finishing its write): it must be recognized
        // as the ordinary complete-oversized-line skip, advance past it, and
        // ingest anything that follows normally.
        let small_after = user_line("uuid-after-huge", "sess-UNTERM", "after");
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("reopen for append");
            writeln!(f).unwrap(); // terminate the huge line
            writeln!(f, "{small_after}").unwrap();
        }
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stats3 = mirror_file_with_limits(
            &rt,
            &path,
            stats2.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("third pass once the huge line terminates");
        assert_eq!(
            stats3.new_offset, file_len,
            "once terminated, the skip-and-advance path must clear past the whole \
             oversized line plus the following valid line"
        );
        assert_eq!(stats3.scanned, 1, "only the small trailing line is scanned");
        assert_eq!(stats3.inserted, 1);
        assert_eq!(count_rows(&rt, "session_messages").await, 1);
    }

    #[tokio::test]
    async fn test_still_growing_partial_line_under_cap_is_unaffected() {
        // Guard: still-growing partial line under the cap must not regress
        // from the oversized-unterminated handling (see docs guide).
        let (rt, _dir) = setup().await;

        let small1 = user_line("uuid-g1", "sess-GROW", "first");
        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{small1}").unwrap();
        // Partial trailing line: valid JSON-shaped prefix, no newline yet.
        let partial_prefix = user_line("uuid-g2", "sess-GROW", "second");
        file.write_all(partial_prefix.as_bytes())
            .expect("write partial line, no trailing newline");
        let path = file.path().to_path_buf();

        let limits = MirrorLimits::production();

        let stats1 =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("first pass: complete line + partial trailing line");
        assert_eq!(stats1.scanned, 1, "only the complete first line is scanned");
        assert_eq!(stats1.inserted, 1);
        assert_eq!(
            stats1.new_offset,
            (small1.len() + 1) as u64,
            "cursor must stop right after the first complete line, not consume the partial tail"
        );

        // The file "grows": the trailing line now gets its newline.
        writeln!(file).unwrap();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stats2 = mirror_file_with_limits(
            &rt,
            &path,
            stats1.new_offset,
            LineTailSource::ClaudeCode,
            None,
            limits,
        )
        .await
        .expect("second pass: the previously-partial line now completes");
        assert_eq!(stats2.new_offset, file_len);
        assert_eq!(stats2.scanned, 1);
        assert_eq!(stats2.inserted, 1);
        assert_eq!(count_rows(&rt, "session_messages").await, 2);
    }

    #[tokio::test]
    async fn test_large_run_of_blank_lines_is_bounded_and_persists_cursor() {
        // PACKSESSION-AUD-003 regression: blank-line runs are bounded and the
        // cursor persists on every durable advance (see docs guide).
        let (rt, _dir) = setup().await;

        let mut file = NamedTempFile::new().expect("tmpfile");
        for _ in 0..500 {
            writeln!(file).unwrap(); // blank line: just "\n"
        }
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(file_len, 500, "500 one-byte blank lines");

        // A tiny per-pass byte cap forces the blank-line run across multiple
        // passes instead of reading straight to EOF in one call.
        let limits = MirrorLimits {
            max_bytes_per_pass: 50,
            max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
            max_line_bytes: MIRROR_MAX_LINE_BYTES,
        };

        let stats1 =
            mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("first blank-line pass");

        assert_eq!(stats1.inserted, 0);
        assert_eq!(stats1.scanned, 0, "blank lines never count toward scanned");
        assert!(
            stats1.new_offset > 0,
            "the pass cap must trip after at least one blank line, not read unbounded"
        );
        assert!(
            stats1.new_offset < file_len,
            "a bounded pass over an all-blank file must not reach EOF in one call"
        );

        // The cursor must be durably persisted even though `scanned == 0`.
        let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
        assert_eq!(
            stored_offset,
            Some(stats1.new_offset as i64),
            "cursor must be persisted even when the pass scanned zero events"
        );

        // Repeated calls continue from the persisted offset (not from 0) and
        // eventually reach EOF, never re-reading already-consumed blanks.
        let mut offset = stats1.new_offset;
        loop {
            let stats = mirror_file_with_limits(
                &rt,
                &path,
                offset,
                LineTailSource::ClaudeCode,
                None,
                limits,
            )
            .await
            .expect("subsequent blank-line pass");
            assert_eq!(stats.inserted, 0);
            if stats.new_offset == offset {
                break; // EOF reached, no further progress
            }
            offset = stats.new_offset;
        }
        assert_eq!(
            offset, file_len,
            "all blank lines eventually consumed to EOF"
        );
    }

    /// Regression for the multi-candidate atomicity finding: the deferred
    /// dispatch variant must NOT commit the cursor on an empty advance —
    /// the commit is the dispatch loop's final step, vetoable until then.
    #[tokio::test]
    async fn deferred_empty_advance_leaves_cursor_uncommitted_until_witnessed_commit() {
        let (rt, _dir) = setup().await;

        let mut file = NamedTempFile::new().expect("tmpfile");
        for _ in 0..16 {
            writeln!(file).unwrap(); // blank line: just "\n"
        }
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let expected_identity = probe_file(&path).expect("transcript probe").1;
        let pass = mirror_file_deferred_with_witness(
            &rt,
            &path,
            0,
            LineTailSource::ClaudeCode,
            None,
            expected_identity.as_deref(),
        )
        .await
        .expect("deferred blank-line pass");
        let stats = &pass.stats;
        assert_eq!(stats.inserted, 0);
        assert_eq!(
            stats.new_offset, file_len,
            "bytes were consumed off the file"
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            None,
            "the deferred pass must not commit the cursor itself"
        );

        // Dispatch ends without an inserting candidate: the loop commits.
        commit_empty_advance_with_witness(&rt, &path, stats.new_offset, pass.file_identity())
            .await
            .expect("end-of-dispatch commit");
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64),
            "the commit lands exactly where the deferred pass consumed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_cursor_witness_survives_probe_and_commit_replacement_races() {
        let (rt, dir) = setup().await;
        let path = dir.path().join("witnessed-empty.jsonl");
        let replacement = dir.path().join("witnessed-empty.next.jsonl");
        let replacement_line = format!(
            "{}\n",
            user_line("uuid-deferred-replacement", "sess-deferred", "replacement")
        );
        let original_line = format!("{}\n", "x".repeat(replacement_line.len() - 1));
        assert_eq!(original_line.len(), replacement_line.len());
        std::fs::write(&path, original_line).expect("original unparseable transcript");
        let original_identity =
            metadata_file_identity(&std::fs::metadata(&path).expect("original metadata"));

        let stale_probe = mirror_file_deferred_with_witness(
            &rt,
            &path,
            0,
            LineTailSource::ClaudeCode,
            None,
            Some("unix:stale:probe"),
        )
        .await
        .expect_err("a probe/open identity mismatch must refuse the pass");
        assert!(
            stale_probe.to_string().contains("identity changed"),
            "{stale_probe}"
        );
        assert_eq!(cursor_offset(&rt, &path.to_string_lossy()).await, None);

        let pass = mirror_file_deferred_with_witness(
            &rt,
            &path,
            0,
            LineTailSource::ClaudeCode,
            None,
            original_identity.as_deref(),
        )
        .await
        .expect("witnessed deferred read");
        assert_eq!(pass.stats.inserted, 0);
        assert_eq!(pass.stats.new_offset, replacement_line.len() as u64);

        std::fs::write(&replacement, replacement_line).expect("replacement transcript");
        std::fs::rename(&replacement, &path).expect("atomic replacement before cursor commit");
        let replacement_identity =
            metadata_file_identity(&std::fs::metadata(&path).expect("replacement metadata"));
        assert_ne!(replacement_identity, original_identity);

        commit_empty_advance_with_witness(&rt, &path, pass.stats.new_offset, pass.file_identity())
            .await
            .expect("commit opened-file witness");
        assert_eq!(
            cursor_file_identity(&rt, &path.to_string_lossy()).await,
            original_identity,
            "the cursor must not bless the replacement with its predecessor's offset"
        );

        let replay = mirror_file(
            &rt,
            &path,
            pass.stats.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("replay replacement after deferred commit");
        assert_eq!(
            replay.inserted, 1,
            "the next public call must detect the old witness and replay the replacement"
        );
        assert_eq!(
            cursor_file_identity(&rt, &path.to_string_lossy()).await,
            replacement_identity
        );
    }

    /// A deferred pass that DOES parse events commits rows and cursor in one
    /// transaction, exactly like the immediate variant.
    #[tokio::test]
    async fn deferred_pass_with_events_commits_rows_and_cursor_together() {
        let (rt, _dir) = setup().await;

        let mut file = NamedTempFile::new().expect("tmpfile");
        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        for index in 0..3 {
            writeln!(
                file,
                "{}",
                user_line(&format!("u{index}"), session_id, "hello")
            )
            .unwrap();
        }
        let path = file.path().to_path_buf();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let expected_identity = probe_file(&path).expect("transcript probe").1;
        let pass = mirror_file_deferred_with_witness(
            &rt,
            &path,
            0,
            LineTailSource::ClaudeCode,
            None,
            expected_identity.as_deref(),
        )
        .await
        .expect("deferred event pass");
        let stats = pass.stats;
        assert_eq!(stats.inserted, 3);
        assert_eq!(stats.new_offset, file_len);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64),
            "an inserting pass commits its cursor inside the same atomic unit"
        );
        assert_eq!(count_rows(&rt, "session_messages").await, 3);
    }

    #[tokio::test]
    async fn test_partial_trailing_line_not_consumed() {
        let (rt, _dir) = setup().await;

        let line1 = user_line("uuid-p1", "sess-B", "Complete");
        // Write one complete line + a partial line without trailing '\n'.
        let partial = r#"{"uuid":"uuid-p2","sessionId":"sess-B","type":"user""#;

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line1}").unwrap(); // complete line (has \n)
        write!(file, "{partial}").unwrap(); // partial — NO trailing \n

        let path = file.path().to_path_buf();
        let full_len = std::fs::metadata(&path).unwrap().len();

        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .expect("mirror_file partial");

        // Only the complete line should be consumed.
        assert_eq!(stats.inserted, 1, "only 1 complete line inserted");
        assert!(
            stats.new_offset < full_len,
            "new_offset {new} must be less than file_len {full_len}",
            new = stats.new_offset
        );

        // The partial bytes remain; calling again from new_offset finds no complete lines.
        let stats2 = mirror_file(
            &rt,
            &path,
            stats.new_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("second call");
        assert_eq!(
            stats2.inserted, 0,
            "partial line must not be consumed on re-poll"
        );
        assert_eq!(
            stats2.new_offset, stats.new_offset,
            "offset must not advance on partial-only content"
        );
    }

    #[tokio::test]
    async fn test_duplicate_uuid_across_two_calls() {
        let (rt, _dir) = setup().await;

        let line = user_line("uuid-dup", "sess-C", "First");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line}").unwrap();

        let path = file.path().to_path_buf();

        // First call inserts.
        let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);

        // Append same uuid again.
        writeln!(file, "{line}").unwrap();

        // Second call from offset 0 should see both lines but insert 0 new rows.
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "duplicate uuid must not be re-inserted");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental: call from first call's new_offset; the second line is the dup.
        let s3 = mirror_file(&rt, &path, s1.new_offset, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s3.inserted, 0, "incremental dup must also insert 0");
    }

    #[tokio::test]
    async fn test_replay_does_not_mutate_session_metadata() {
        // Replay-idempotency regression (see docs guide): a pure replay must
        // not advance last_seen_at.
        let (rt, _dir) = setup().await;

        let line = user_line_no_ts("uuid-nts", "sess-NTS", "no timestamp here");
        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{line}").unwrap();
        let path = file.path().to_path_buf();

        let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);
        let seen_after_first = last_seen_at(&rt, "sess-NTS")
            .await
            .expect("session row exists");

        // Replay from offset 0: re-scans the same line, inserts 0, and must
        // leave last_seen_at byte-identical even though now_us has advanced.
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "replay must insert 0 rows");
        let seen_after_replay = last_seen_at(&rt, "sess-NTS").await.unwrap();
        assert_eq!(
            seen_after_first, seen_after_replay,
            "replay must not advance last_seen_at for a timestamp-missing event"
        );
    }

    #[tokio::test]
    async fn test_empty_file_is_a_no_op() {
        let (rt, _dir) = setup().await;

        let file = NamedTempFile::new().expect("tmpfile");
        let path = file.path().to_path_buf();

        let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();
        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.new_offset, 0);
    }

    #[tokio::test]
    async fn test_missing_file_returns_error() {
        let (rt, _dir) = setup().await;
        let bad_path = std::path::PathBuf::from("/nonexistent/path/session.jsonl");
        let result = mirror_file(&rt, &bad_path, 0, LineTailSource::ClaudeCode, None).await;
        assert!(
            matches!(result, Err(RuntimeError::Internal(_))),
            "missing file should return Internal error"
        );
    }

    // ── Codex source integration tests ────────────────────────────────────────

    /// Build a minimal Codex response_item/message line (`input_text` for
    /// user, `output_text` for assistant — the generic `text` type does not
    /// occur in real Codex transcripts).
    fn codex_message_line(role: &str, text: &str) -> String {
        let block_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        format!(
            r#"{{"type":"response_item","timestamp":"2026-06-30T08:00:00Z","payload":{{"type":"message","role":"{role}","content":[{{"type":"{block_type}","text":"{text}"}}]}}}}"#
        )
    }

    /// Build a minimal Codex session_meta line.
    fn codex_meta_line(session_id: &str, cwd: &str, branch: &str) -> String {
        format!(
            r#"{{"type":"session_meta","timestamp":"2026-06-30T08:00:00Z","payload":{{"id":"{session_id}","cwd":"{cwd}","git":{{"branch":"{branch}","commit_hash":"abc","repository_url":"https://github.com/example/repo"}}}}}}"#
        )
    }

    /// Build a Codex event_msg line (should be skipped).
    fn codex_event_msg_line() -> String {
        r#"{"type":"event_msg","timestamp":"2026-06-30T08:00:00Z","payload":{"type":"user_message","content":"should be skipped"}}"#.to_string()
    }

    #[tokio::test]
    async fn test_codex_mirror_inserts_with_source_codex() {
        let (rt, _dir) = setup().await;

        let session_id = "cdx-sess-0001-0001-0001-000000000001";
        let meta = codex_meta_line(session_id, "/home/lion/proj", "feat-x");
        let user_msg = codex_message_line("user", "Hello from Codex");
        let asst_msg = codex_message_line("assistant", "Hello back from Codex");
        let skip_msg = codex_event_msg_line();

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{meta}").unwrap();
        writeln!(file, "{user_msg}").unwrap();
        writeln!(file, "{asst_msg}").unwrap();
        writeln!(file, "{skip_msg}").unwrap();

        let path = file.path().to_path_buf();

        // Mirror the file as Codex source.
        let stats = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
            .await
            .expect("codex mirror_file");

        // session_meta + 2 response_item/message rows = 3 parseable, event_msg skipped.
        assert_eq!(stats.inserted, 3, "meta + 2 messages inserted");
        assert_eq!(
            stats.scanned, 4,
            "4 lines total (including skipped event_msg)"
        );
        assert!(stats.new_offset > 0);

        // Session row exists with source='codex'.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let session_row = r
            .query_row(SqlStatement {
                sql: "SELECT source FROM sessions WHERE id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("session row must exist");
        match session_row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Text(s)) => assert_eq!(s, "codex", "source must be 'codex'"),
            other => panic!("unexpected source value: {other:?}"),
        }

        // All 3 message rows are stored.
        assert_eq!(count_rows(&rt, "session_messages").await, 3);

        // The two response_item/message rows carry their real input_text/
        // output_text content through to session_messages.text — not just a
        // row count, but the actual extracted string for each role.
        let mut r2 = sql.reader().await.expect("reader");
        let rows = r2
            .query_all(SqlStatement {
                sql: "SELECT role, text FROM session_messages \
                      WHERE session_id=?1 AND role IS NOT NULL ORDER BY seq"
                    .into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok");
        let texts: Vec<(String, String)> = rows
            .iter()
            .map(|row| {
                let role = match row.get("role") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected role value: {other:?}"),
                };
                let text = match row.get("text") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected text value: {other:?}"),
                };
                (role, text)
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                ("user".to_string(), "Hello from Codex".to_string()),
                ("assistant".to_string(), "Hello back from Codex".to_string()),
            ],
            "input_text/output_text blocks must round-trip to session_messages.text by role"
        );
    }

    #[tokio::test]
    async fn test_codex_event_id_is_stable_and_idempotent() {
        // Verifies that: (a) synthetic uuid format is "{session_id}:{offset}",
        // and (b) a second mirror_file pass over the same bytes inserts 0 rows.
        let (rt, _dir) = setup().await;

        let session_id = "cdx-sess-idem-0001-0001-000000000002";
        let user_msg = codex_message_line("user", "Idempotency test");

        let mut file = NamedTempFile::new().expect("tmpfile");
        writeln!(file, "{user_msg}").unwrap();

        let path = file.path().to_path_buf();

        // First pass.
        let s1 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
            .await
            .unwrap();
        assert_eq!(s1.inserted, 1);

        // Verify the stored id matches the expected synthetic format.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let msg_row = r
            .query_row(SqlStatement {
                sql: "SELECT id FROM session_messages WHERE session_id=?1".into(),
                params: vec![SqlValue::Text(session_id.to_string())],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("message row must exist");
        let stored_id = match msg_row.columns.first().map(|c| &c.value) {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected id type: {other:?}"),
        };
        // The line starts at byte offset 0.
        let expected_id = format!("{session_id}:0");
        assert_eq!(
            stored_id, expected_id,
            "synthetic uuid must be {{session_id}}:{{offset}}"
        );

        // Second pass from offset 0: same lines, 0 new rows (idempotent).
        let s2 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
            .await
            .unwrap();
        assert_eq!(s2.inserted, 0, "second pass must be idempotent");
        assert_eq!(count_rows(&rt, "session_messages").await, 1);

        // Incremental pass from advanced offset: no new data.
        let s3 = mirror_file(
            &rt,
            &path,
            s1.new_offset,
            LineTailSource::Codex,
            Some(session_id),
        )
        .await
        .unwrap();
        assert_eq!(s3.inserted, 0, "incremental pass finds nothing new");
    }

    #[tokio::test]
    async fn test_codex_and_cc_are_independent_sessions() {
        // Both sources can coexist in the same DB; source column distinguishes them.
        let (rt, _dir) = setup().await;

        let cc_line = user_line("cc-uuid-1", "cc-sess-1", "from claude code");
        let mut cc_file = NamedTempFile::new().expect("cc tmpfile");
        writeln!(cc_file, "{cc_line}").unwrap();

        let cdx_session_id = "cdx-sess-coex-0001-0001-000000000003";
        let cdx_msg = codex_message_line("user", "from codex");
        let mut cdx_file = NamedTempFile::new().expect("cdx tmpfile");
        writeln!(cdx_file, "{cdx_msg}").unwrap();

        mirror_file(&rt, cc_file.path(), 0, LineTailSource::ClaudeCode, None)
            .await
            .unwrap();

        mirror_file(
            &rt,
            cdx_file.path(),
            0,
            LineTailSource::Codex,
            Some(cdx_session_id),
        )
        .await
        .unwrap();

        assert_eq!(count_rows(&rt, "sessions").await, 2);
        assert_eq!(count_rows(&rt, "session_messages").await, 2);

        // Verify sources are distinct.
        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT source FROM sessions ORDER BY source".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        let sources: Vec<String> = rows
            .iter()
            .filter_map(|row| match row.get("source") {
                Some(SqlValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sources, vec!["claude_code", "codex"]);
    }

    // ── ChatGPT export whole-file ingest tests ────────────────────────────────
    //
    // All fixtures below are hand-authored synthetic JSON, not real export
    // content. Node ids are set equal to their own `message.id` so that
    // `parent_uuid` (which threads through the mapping node id, per
    // `parse::build_chatgpt_event`) resolves to the expected message uuid.

    use serde_json::json;

    fn write_export_file(content: &str) -> (NamedTempFile, std::path::PathBuf) {
        let mut file = NamedTempFile::new().expect("tmpfile");
        write!(file, "{content}").unwrap();
        let path = file.path().to_path_buf();
        (file, path)
    }

    fn chatgpt_happy_export_json() -> String {
        let conv = json!({
            "id": "conv-happy",
            "title": "Synthetic Happy",
            "create_time": 1_751_462_400.0,
            "current_node": "msg-happy-assistant",
            "mapping": {
                "root-happy": {
                    "id": "root-happy",
                    "message": null,
                    "parent": null,
                    "children": ["msg-happy-user"]
                },
                "msg-happy-user": {
                    "id": "msg-happy-user",
                    "parent": "root-happy",
                    "children": ["msg-happy-assistant"],
                    "message": {
                        "id": "msg-happy-user",
                        "author": {"role": "user"},
                        "create_time": 1_751_462_400.0,
                        "content": {"content_type": "text", "parts": ["Hello synthetic"]}
                    }
                },
                "msg-happy-assistant": {
                    "id": "msg-happy-assistant",
                    "parent": "msg-happy-user",
                    "children": [],
                    "message": {
                        "id": "msg-happy-assistant",
                        "author": {"role": "assistant"},
                        "create_time": 1_751_462_401.0,
                        "content": {"content_type": "text", "parts": ["Hi synthetic"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_happy_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_happy_export_json());
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("happy path ingest");
        assert_eq!(stats.inserted, 2, "2 message-bearing nodes");
        assert_eq!(stats.scanned, 2, "2 events parsed");
        assert_eq!(stats.new_offset, file_len, "whole-file cursor-at-length");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT source, slug, cwd, git_branch FROM sessions WHERE id='conv-happy'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("session row must exist");
        match row.get("source") {
            Some(SqlValue::Text(s)) => assert_eq!(s, "chatgpt_export"),
            other => panic!("unexpected source: {other:?}"),
        }
        match row.get("slug") {
            Some(SqlValue::Text(s)) => assert_eq!(s, "Synthetic Happy"),
            other => panic!("unexpected slug: {other:?}"),
        }
        assert!(
            matches!(row.get("cwd"), Some(SqlValue::Null) | None),
            "chatgpt export never carries a cwd"
        );
        assert!(
            matches!(row.get("git_branch"), Some(SqlValue::Null) | None),
            "chatgpt export never carries a git branch"
        );

        let mut r2 = sql.reader().await.expect("reader");
        let rows = r2
            .query_all(SqlStatement {
                sql: "SELECT seq, role FROM session_messages \
                      WHERE session_id='conv-happy' ORDER BY seq"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        let roles: Vec<(i64, String)> = rows
            .iter()
            .map(|row| {
                let seq = match row.get("seq") {
                    Some(SqlValue::Integer(n)) => *n,
                    other => panic!("unexpected seq: {other:?}"),
                };
                let role = match row.get("role") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => panic!("unexpected role: {other:?}"),
                };
                (seq, role)
            })
            .collect();
        assert_eq!(
            roles,
            vec![(0, "user".to_string()), (1, "assistant".to_string())]
        );
    }

    fn chatgpt_idempotency_export_json() -> String {
        let conv = json!({
            "id": "conv-idem",
            "title": "Synthetic Idempotency",
            "current_node": "msg-idem-assistant",
            "mapping": {
                "root-idem": {
                    "id": "root-idem",
                    "message": null,
                    "parent": null,
                    "children": ["msg-idem-user"]
                },
                "msg-idem-user": {
                    "id": "msg-idem-user",
                    "parent": "root-idem",
                    "children": ["msg-idem-assistant"],
                    "message": {
                        "id": "msg-idem-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Same question again"]}
                    }
                },
                "msg-idem-assistant": {
                    "id": "msg-idem-assistant",
                    "parent": "msg-idem-user",
                    "children": [],
                    "message": {
                        "id": "msg-idem-assistant",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Same answer again"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_reingest_idempotency_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_idempotency_export_json());

        let s1 = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("first ingest");
        assert_eq!(s1.inserted, 2);

        let seen_after_first = last_seen_at(&rt, "conv-idem")
            .await
            .expect("session row exists");

        // Re-ingest from offset 0 (the service always re-reads the whole file
        // for this source): same event uuids, INSERT OR IGNORE must dedup.
        let s2 = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("second ingest");
        assert_eq!(s2.inserted, 0, "re-ingest must insert 0 new rows");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let count = r
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) FROM session_messages WHERE session_id='conv-idem'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("count row");
        match count.columns.first().map(|c| &c.value) {
            Some(SqlValue::Integer(n)) => assert_eq!(*n, 2, "message count stays at 2"),
            other => panic!("unexpected count: {other:?}"),
        }

        let seen_after_replay = last_seen_at(&rt, "conv-idem")
            .await
            .expect("session row still exists");
        assert_eq!(
            seen_after_first, seen_after_replay,
            "pure replay must not advance last_seen_at"
        );
    }

    fn chatgpt_branch_sidechain_export_json() -> String {
        let conv = json!({
            "id": "conv-branch",
            "title": "Synthetic Branch",
            "current_node": "msg-branch-main",
            "mapping": {
                "root-branch": {
                    "id": "root-branch",
                    "message": null,
                    "parent": null,
                    "children": ["msg-branch-user"]
                },
                "msg-branch-user": {
                    "id": "msg-branch-user",
                    "parent": "root-branch",
                    "children": ["msg-branch-main", "msg-branch-alt"],
                    "message": {
                        "id": "msg-branch-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": ["Branch question"]}
                    }
                },
                "msg-branch-main": {
                    "id": "msg-branch-main",
                    "parent": "msg-branch-user",
                    "children": [],
                    "message": {
                        "id": "msg-branch-main",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Main answer"]}
                    }
                },
                "msg-branch-alt": {
                    "id": "msg-branch-alt",
                    "parent": "msg-branch-user",
                    "children": [],
                    "message": {
                        "id": "msg-branch-alt",
                        "author": {"role": "assistant"},
                        "content": {"content_type": "text", "parts": ["Alternate answer"]}
                    }
                }
            }
        });
        serde_json::to_string(&json!([conv])).unwrap()
    }

    #[tokio::test]
    async fn test_chatgpt_branch_sidechain_conversations_json() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&chatgpt_branch_sidechain_export_json());

        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("branch ingest");
        assert_eq!(stats.inserted, 3, "user + main + alt all stored");

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let rows = r
            .query_all(SqlStatement {
                sql: "SELECT id, is_sidechain, text FROM session_messages \
                      WHERE session_id='conv-branch' ORDER BY id"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok");
        assert_eq!(rows.len(), 3);

        for row in &rows {
            let id = match row.get("id") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected id: {other:?}"),
            };
            let is_sidechain = match row.get("is_sidechain") {
                Some(SqlValue::Integer(n)) => *n,
                other => panic!("unexpected is_sidechain: {other:?}"),
            };
            let text = match row.get("text") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected text: {other:?}"),
            };
            match id.as_str() {
                "msg-branch-user" | "msg-branch-main" => {
                    assert_eq!(is_sidechain, 0, "{id} is on the current-node path")
                }
                "msg-branch-alt" => {
                    assert_eq!(is_sidechain, 1, "alt branch is off the current-node path");
                    assert_eq!(
                        text, "Alternate answer",
                        "sidechain content must be preserved, not dropped"
                    );
                }
                other => panic!("unexpected message id: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn test_chatgpt_malformed_conversations_json_cursor_does_not_advance() {
        let (rt, _dir) = setup().await;

        // Seed the path with a valid (if empty) export and record its cursor.
        let (mut file, path) = write_export_file("[]");
        let seeded_stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("seeding with an empty array is a valid parse");
        assert_eq!(seeded_stats.inserted, 0);
        let seeded_offset = seeded_stats.new_offset;

        let seeded_sessions = count_rows(&rt, "sessions").await;
        let seeded_messages = count_rows(&rt, "session_messages").await;

        // Overwrite with a longer, malformed (valid-JSON-but-not-an-array) body.
        let malformed = r#"{"oops": "not a chatgpt export array"}"#;
        file.as_file_mut().set_len(0).expect("truncate");
        std::io::Seek::seek(file.as_file_mut(), std::io::SeekFrom::Start(0)).unwrap();
        write!(file, "{malformed}").unwrap();

        let result = mirror_chatgpt_export_file(&rt, &path, seeded_offset).await;
        assert!(
            matches!(result, Err(RuntimeError::Internal(_))),
            "malformed export must return Internal error, got {result:?}"
        );

        let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
        assert_eq!(
            stored_offset,
            Some(seeded_offset as i64),
            "cursor must remain at the pre-error value"
        );
        assert_eq!(
            count_rows(&rt, "sessions").await,
            seeded_sessions,
            "no new session rows on parse failure"
        );
        assert_eq!(
            count_rows(&rt, "session_messages").await,
            seeded_messages,
            "no new message rows on parse failure"
        );
    }

    #[tokio::test]
    async fn test_chatgpt_export_over_max_bytes_is_skipped_without_reading() {
        // PACKSESSION-AUD-003 regression: oversized ChatGPT exports are
        // skipped without reading (see docs guide).
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file("[]");

        let file_len = std::fs::metadata(&path).unwrap().len();
        let max_bytes = 1u64; // smaller than even an empty-array export
        assert!(
            file_len > max_bytes,
            "fixture export must exceed the tiny ceiling"
        );

        let stats = mirror_chatgpt_export_file_with_max_bytes(&rt, &path, 0, max_bytes)
            .await
            .expect("an oversized export must be skipped, not error");

        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.scanned, 0);
        assert_eq!(
            stats.new_offset, 0,
            "cursor must not advance past a skipped oversized export"
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            None,
            "no cursor row should be written for a skipped pass"
        );
        assert_eq!(count_rows(&rt, "sessions").await, 0);
        assert_eq!(count_rows(&rt, "session_messages").await, 0);
    }

    #[tokio::test]
    async fn test_chatgpt_secret_bearing_conversations_json_is_masked() {
        // Assembled from fragments at runtime so no credential-shaped literal
        // is committed to the repo; matches the AWS-key shape already covered
        // by `khive_runtime::secret_gate`'s own detector tests.
        let secret_fragment_a = "AKIA";
        let secret_fragment_b = "FAKEKEY1234567890";
        let secret = format!("{secret_fragment_a}{secret_fragment_b}");
        let user_text = format!("here is my key: {secret}");

        let conv = json!({
            "id": "conv-secret",
            "title": "Synthetic Secret",
            "current_node": "msg-secret-user",
            "mapping": {
                "root-secret": {
                    "id": "root-secret",
                    "message": null,
                    "parent": null,
                    "children": ["msg-secret-user"]
                },
                "msg-secret-user": {
                    "id": "msg-secret-user",
                    "parent": "root-secret",
                    "children": [],
                    "message": {
                        "id": "msg-secret-user",
                        "author": {"role": "user"},
                        "content": {"content_type": "text", "parts": [user_text]}
                    }
                }
            }
        });
        let content = serde_json::to_string(&json!([conv])).unwrap();
        let (_file, path) = write_export_file(&content);

        let (rt, _dir) = setup().await;
        let stats = mirror_chatgpt_export_file(&rt, &path, 0)
            .await
            .expect("secret-bearing content must still ingest, only masked");
        assert_eq!(stats.inserted, 1);

        let sql = rt.sql();
        let mut r = sql.reader().await.expect("reader");
        let row = r
            .query_row(SqlStatement {
                sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("message row must exist");
        let (stored_text, stored_raw) = match (row.get("text"), row.get("raw")) {
            (Some(SqlValue::Text(t)), Some(SqlValue::Text(r))) => (t.clone(), r.clone()),
            other => panic!("unexpected text/raw shape: {other:?}"),
        };

        assert!(
            !stored_text.contains(&secret),
            "stored text must not contain the raw secret"
        );
        assert!(
            !stored_raw.contains(&secret),
            "stored raw must not contain the raw secret"
        );
        assert!(
            stored_text.contains("***MASKED***"),
            "stored text must carry the secret_gate redaction marker"
        );
        assert!(
            stored_raw.contains("***MASKED***"),
            "stored raw must carry the secret_gate redaction marker"
        );
    }

    fn claude_ai_happy_export_json() -> String {
        serde_json::to_string(&json!([{
            "uuid": "claude-conv-happy",
            "name": "Synthetic Claude.ai Export",
            "created_at": "2026-07-31T10:00:00Z",
            "current_leaf_message_uuid": "claude-msg-main",
            "chat_messages": [
                {
                    "uuid": "claude-msg-user",
                    "sender": "human",
                    "index": 0,
                    "parent_message_uuid": "00000000-0000-4000-8000-000000000000",
                    "created_at": "2026-07-31T10:00:01Z",
                    "content": [{"type": "text", "text": "Question"}]
                },
                {
                    "uuid": "claude-msg-main",
                    "sender": "assistant",
                    "index": 1,
                    "parent_message_uuid": "claude-msg-user",
                    "created_at": "2026-07-31T10:00:02Z",
                    "content": [{"type": "text", "text": "Current answer"}]
                },
                {
                    "uuid": "claude-msg-alt",
                    "sender": "assistant",
                    "index": 2,
                    "parent_message_uuid": "claude-msg-user",
                    "created_at": "2026-07-31T10:00:03Z",
                    "content": [{"type": "text", "text": "Alternate answer"}]
                }
            ]
        }]))
        .unwrap()
    }

    #[tokio::test]
    async fn test_claude_ai_export_ingest_is_idempotent_and_preserves_branches() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file(&claude_ai_happy_export_json());
        let file_len = std::fs::metadata(&path).unwrap().len();

        let first = mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("claude.ai export ingest");
        assert_eq!(first.inserted, 3);
        assert_eq!(first.scanned, 3);
        assert_eq!(first.new_offset, file_len);

        let replay = mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("idempotent replay");
        assert_eq!(replay.inserted, 0);
        assert_eq!(count_rows(&rt, "sessions").await, 1);
        assert_eq!(count_rows(&rt, "session_messages").await, 3);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64)
        );

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let session = reader
            .query_row(SqlStatement {
                sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-happy'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query ok")
            .expect("session row");
        assert!(matches!(
            session.get("provider_session_id"),
            Some(SqlValue::Text(value)) if value == "claude-conv-happy"
        ));
        assert!(matches!(
            session.get("source"),
            Some(SqlValue::Text(value)) if value == "claude_ai_export"
        ));
        assert!(matches!(
            session.get("slug"),
            Some(SqlValue::Text(value)) if value == "Synthetic Claude.ai Export"
        ));
        assert!(matches!(
            session.get("message_count"),
            Some(SqlValue::Integer(3))
        ));

        let messages = reader
            .query_all(SqlStatement {
                sql: "SELECT id, parent_uuid, is_sidechain FROM session_messages \
                      WHERE session_id='claude-conv-happy' ORDER BY seq"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("message query");
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].get("parent_uuid"),
            Some(SqlValue::Null) | None
        ));
        assert!(matches!(
            messages[1].get("is_sidechain"),
            Some(SqlValue::Integer(0))
        ));
        assert!(matches!(
            messages[2].get("id"),
            Some(SqlValue::Text(value)) if value == "claude-msg-alt"
        ));
        assert!(matches!(
            messages[2].get("parent_uuid"),
            Some(SqlValue::Text(value)) if value == "claude-msg-user"
        ));
        assert!(matches!(
            messages[2].get("is_sidechain"),
            Some(SqlValue::Integer(1))
        ));
    }

    #[tokio::test]
    async fn test_claude_ai_empty_conversation_creates_session_and_advances_cursor() {
        let (rt, _dir) = setup().await;
        let export = serde_json::to_string(&json!([{
            "uuid": "claude-conv-empty",
            "name": "Empty Claude Conversation",
            "created_at": "2026-07-31T10:00:00Z",
            "chat_messages": []
        }]))
        .unwrap();
        let (_file, path) = write_export_file(&export);
        let file_len = std::fs::metadata(&path).unwrap().len();

        let first = mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("empty claude.ai conversation ingest");
        assert_eq!(first.inserted, 0);
        assert_eq!(first.scanned, 0);
        assert_eq!(first.new_offset, file_len);
        assert_eq!(count_rows(&rt, "sessions").await, 1);
        assert_eq!(count_rows(&rt, "session_messages").await, 0);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64)
        );

        let replay = mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("empty conversation replay");
        assert_eq!(replay.inserted, 0);
        assert_eq!(count_rows(&rt, "sessions").await, 1);

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let session = reader
            .query_row(SqlStatement {
                sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-empty'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query empty session")
            .expect("empty session row");
        assert!(matches!(
            session.get("provider_session_id"),
            Some(SqlValue::Text(value)) if value == "claude-conv-empty"
        ));
        assert!(matches!(
            session.get("source"),
            Some(SqlValue::Text(value)) if value == "claude_ai_export"
        ));
        assert!(matches!(
            session.get("slug"),
            Some(SqlValue::Text(value)) if value == "Empty Claude Conversation"
        ));
        assert!(matches!(
            session.get("message_count"),
            Some(SqlValue::Integer(0))
        ));
    }

    #[tokio::test]
    async fn test_claude_ai_unsupported_only_conversation_creates_session() {
        let (rt, _dir) = setup().await;
        let export = serde_json::to_string(&json!([{
            "uuid": "claude-conv-internal-only",
            "summary": "Internal-only Claude Conversation",
            "updated_at": "2026-07-31T10:00:00Z",
            "chat_messages": [{
                "uuid": "claude-msg-internal-only",
                "sender": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "not display text"},
                    {"type": "provider_internal", "payload": {"hidden": true}}
                ]
            }]
        }]))
        .unwrap();
        let (_file, path) = write_export_file(&export);
        let file_len = std::fs::metadata(&path).unwrap().len();

        let stats = mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("unsupported-only claude.ai conversation ingest");
        assert_eq!(stats.inserted, 0);
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.new_offset, file_len);
        assert_eq!(count_rows(&rt, "sessions").await, 1);
        assert_eq!(count_rows(&rt, "session_messages").await, 0);
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            Some(file_len as i64)
        );

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let session = reader
            .query_row(SqlStatement {
                sql: "SELECT slug, message_count FROM sessions \
                      WHERE id='claude-conv-internal-only'"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query unsupported-only session")
            .expect("unsupported-only session row");
        assert!(matches!(
            session.get("slug"),
            Some(SqlValue::Text(value)) if value == "Internal-only Claude Conversation"
        ));
        assert!(matches!(
            session.get("message_count"),
            Some(SqlValue::Integer(0))
        ));
    }

    #[tokio::test]
    async fn test_claude_ai_export_secret_bearing_title_is_masked_in_stored_slug() {
        let (rt, _dir) = setup().await;
        let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
        let export = serde_json::to_string(&json!([{
            "uuid": "claude-conv-secret-title",
            "name": format!("prod creds {secret}"),
            "created_at": "2026-07-31T10:00:00Z",
            "chat_messages": []
        }]))
        .unwrap();
        let (_file, path) = write_export_file(&export);

        mirror_claude_ai_export_file(&rt, &path, 0)
            .await
            .expect("secret-title claude.ai conversation ingest");
        assert_eq!(count_rows(&rt, "sessions").await, 1);

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let session = reader
            .query_row(SqlStatement {
                sql: "SELECT slug FROM sessions WHERE id='claude-conv-secret-title'".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("query secret-title session")
            .expect("secret-title session row");
        let Some(SqlValue::Text(slug)) = session.get("slug") else {
            panic!("stored slug must be text");
        };
        assert!(!slug.contains(&secret));
        assert!(slug.contains("***MASKED***"));
    }

    #[tokio::test]
    async fn test_claude_ai_export_over_max_bytes_leaves_cursor_untouched() {
        let (rt, _dir) = setup().await;
        let (_file, path) = write_export_file("[]");

        let stats = mirror_claude_ai_export_file_with_max_bytes(&rt, &path, 0, 1)
            .await
            .expect("oversized export is skipped");
        assert_eq!(stats, MirrorStats::default());
        assert_eq!(cursor_offset(&rt, &path.to_string_lossy()).await, None);
    }

    /// SS6 invariants #4/#5 (error never advances cursor; one transaction per
    /// pass) — see
    /// `crates/khive-pack-session/docs/api/mirror-ingest.md#test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced`
    /// for why this drives `atomic_unit` directly instead of through crafted event data.
    #[tokio::test]
    async fn test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced() {
        let (rt, _dir) = setup().await;
        let sql = rt.sql();
        let path = std::path::Path::new("/synthetic/mid-tx-probe.json");
        let path_owned = path.to_path_buf();

        let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
            Box::pin(async move {
                // First write succeeds — mirrors event 1's session row in a
                // multi-event file pass.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO sessions \
                              (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
                              VALUES('mid-tx-session', 'mid-tx-session', 'chatgpt_export', 0, 1, 1, 'local')"
                            .into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;

                // Cursor advance succeeds too — mirrors `upsert_cursor_on_writer`
                // running near the end of `write_events_and_cursor_on_writer`.
                upsert_cursor_on_writer(
                    writer,
                    &path_owned,
                    Some("mid-tx-session"),
                    999,
                    Some("test:identity"),
                    1,
                )
                .await?;

                // Third write fails with a genuine (non-suppressed) SQL error —
                // mirrors a mid-loop DB failure on a later event in the same file.
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO no_such_table_mid_tx_probe(a) VALUES(1)".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;

                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            })
        });

        // `atomic_unit` itself must surface the error and roll back the
        // whole unit — no explicit `commit()`/`drop()` orchestration is the
        // caller's job anymore; the seam owns it.
        let result = sql.atomic_unit(op).await;
        assert!(
            result.is_err(),
            "atomic_unit must propagate the forced third-write failure"
        );

        assert_eq!(
            count_rows(&rt, "sessions").await,
            0,
            "session write must not survive a later error in the same atomic unit"
        );
        assert_eq!(
            cursor_offset(&rt, &path.to_string_lossy()).await,
            None,
            "cursor must not advance when a later write in the same atomic unit fails"
        );
    }

    /// Build a bare, file-backed, write-queue-enabled `SqlAccess` handle
    /// (sidesteps the process-global `KHIVE_WRITE_QUEUE` env var race — see
    /// docs guide ADR-099 D5 notes).
    fn write_queue_pool(db_path: std::path::PathBuf) -> Arc<khive_db::ConnectionPool> {
        let pool_cfg = khive_db::PoolConfig {
            path: Some(db_path),
            write_queue_enabled: Some(true),
            ..khive_db::PoolConfig::default()
        };
        let pool = Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
        {
            let w_conn = pool.writer().expect("writer");
            for stmt in &SESSION_SCHEMA_PLAN_STMTS {
                w_conn
                    .conn()
                    .execute_batch(stmt)
                    .expect("session schema stmt");
            }
        }
        pool
    }

    /// ADR-099 D5 acceptance: `write_events_and_cursor_on_writer` is
    /// suspension-free under `atomic_unit` on the real single-writer path
    /// (see docs guide) — a suspending closure would fail `block_on_sync`
    /// instead of returning `Ok`.
    #[tokio::test]
    async fn write_events_and_cursor_is_suspension_free_under_single_writer() {
        let dir = TempDir::new().expect("tempdir");
        let pool = write_queue_pool(dir.path().join("suspend_free.db"));
        let sql: Arc<dyn khive_storage::SqlAccess> =
            Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

        pool.writer_task_handle()
            .unwrap()
            .expect("writer task must be spawned with the flag on for a file-backed pool");

        let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-1","sessionId":"suspend-free-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];

        let path = std::path::Path::new("/synthetic/suspend-free.jsonl").to_path_buf();
        let now_us = Utc::now().timestamp_micros();
        let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
            Box::pin(async move {
                write_events_and_cursor_on_writer(
                    writer,
                    &path,
                    "claude_code",
                    &[],
                    &events,
                    Some("test:identity"),
                    MirrorWriteProgress {
                        scanned: 1,
                        new_offset: 100,
                        now_us,
                    },
                )
                .await
                .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "test_write_events_and_cursor",
                        e,
                    )
                })
            })
        });

        let boxed = sql
            .atomic_unit(op)
            .await
            .expect("a suspension-free closure must not hit block_on_sync's Pending error");
        let stats = *boxed
            .downcast::<MirrorStats>()
            .expect("op must return MirrorStats");

        assert_eq!(stats.inserted, 1, "the one event must be inserted");

        let mut reader = sql.reader().await.expect("reader");
        let row = khive_storage::SqlReader::query_scalar(
            reader.as_mut(),
            SqlStatement {
                sql: "SELECT COUNT(*) FROM sessions".into(),
                params: vec![],
                label: None,
            },
        )
        .await
        .expect("count query")
        .expect("count row");
        match row {
            SqlValue::Integer(1) => {}
            other => panic!("the session row must be committed, got COUNT(*) = {other:?}"),
        }
    }

    /// ADR-099 D5 acceptance ("single-writer concurrency, mandatory"):
    /// session-mirror ingest must route through the shared writer task, not
    /// open its own standalone `BEGIN IMMEDIATE` (see docs guide for the
    /// queue-depth + occupier-parked-on-oneshot technique).
    #[tokio::test]
    async fn session_ingest_routes_through_writer_task_when_flag_enabled() {
        let dir = TempDir::new().expect("tempdir");
        let pool = write_queue_pool(dir.path().join("concurrency.db"));
        let sql: Arc<dyn khive_storage::SqlAccess> =
            Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

        let writer_task = pool
            .writer_task_handle()
            .unwrap()
            .expect("writer task must be spawned with the flag on for a file-backed pool");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), khive_storage::StorageError>(())
                    })
                    .await
            })
        };

        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-concurrency-1","sessionId":"concurrency-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];
        let path = std::path::Path::new("/synthetic/concurrency.jsonl").to_path_buf();
        let now_us = Utc::now().timestamp_micros();
        let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
            Box::pin(async move {
                write_events_and_cursor_on_writer(
                    writer,
                    &path,
                    "claude_code",
                    &[],
                    &events,
                    Some("test:identity"),
                    MirrorWriteProgress {
                        scanned: 1,
                        new_offset: 100,
                        now_us,
                    },
                )
                .await
                .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "test_session_ingest_concurrency",
                        e,
                    )
                })
            })
        });

        let sql_for_ingest = Arc::clone(&sql);
        let ingest_task = tokio::spawn(async move { sql_for_ingest.atomic_unit(op).await });

        let mut saw_enqueued = false;
        for _ in 0..100 {
            if writer_task.queue_depth() >= 1 {
                saw_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            saw_enqueued,
            "session-ingest's atomic_unit request never appeared in the writer task's \
             channel while the occupier held the single drain slot — the converted ingest \
             path is not routing through the shared writer task (a standalone `begin_tx` \
             connection would never show up here at all)"
        );

        release_tx
            .send(())
            .expect("occupier must still be waiting on the release signal");
        occupier
            .await
            .expect("occupier task must not panic")
            .expect("occupier write must succeed");

        let boxed = ingest_task
            .await
            .expect("ingest task must not panic")
            .expect("ingest atomic_unit must succeed once the occupier releases the slot");
        let stats = *boxed
            .downcast::<MirrorStats>()
            .expect("op must return MirrorStats");
        assert_eq!(stats.inserted, 1, "the ingest event must be committed");
    }

    /// ADR-099 revert-companion test: the pre-conversion shape (a closure
    /// issuing its own `BEGIN IMMEDIATE` inside `atomic_unit`) must fail
    /// deterministically with a nested-transaction error — proves the
    /// suspension-free assertions above are non-vacuous (see docs guide).
    #[tokio::test]
    async fn old_shape_manual_begin_immediate_inside_atomic_unit_fails() {
        let dir = TempDir::new().expect("tempdir");
        let pool = write_queue_pool(dir.path().join("old_shape_begin_immediate.db"));
        let sql: Arc<dyn khive_storage::SqlAccess> =
            Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

        pool.writer_task_handle()
            .unwrap()
            .expect("writer task must be spawned with the flag on for a file-backed pool");

        let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
            Box::pin(async move {
                // `atomic_unit` already has an open transaction around this
                // closure — issuing a second `BEGIN IMMEDIATE` here is
                // exactly the old `begin_tx`-shaped mistake this ADR
                // retires: a caller managing its own transaction control
                // inside a seam that already owns the transaction boundary.
                writer
                    .execute(SqlStatement {
                        sql: "BEGIN IMMEDIATE".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            })
        });

        let err = sql.atomic_unit(op).await.expect_err(
            "a closure that issues its own BEGIN IMMEDIATE inside atomic_unit must fail with a \
             nested-transaction error, not silently succeed",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("cannot start a transaction within a transaction"),
            "expected the deterministic nested-transaction failure (SQLite's own message for a \
             second BEGIN issued inside an already-open transaction), got: {msg}"
        );
    }
}
