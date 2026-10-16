//! Background live-mirror polling service.
//!
//! `run_mirror_service` is an infinite loop started by `SessionPack::warm()`.
//! It discovers configured CLI transcripts and provider exports, tracks byte
//! offsets, and ingests new content every `poll_interval`.
//!
//! Design principles:
//! - Infallible: a per-file error is logged and skipped; the loop continues.
//! - Cheap when idle: directory listings are cached and cold-file metadata
//!   probes are scheduled through fixed per-tick budgets.
//! - Idempotent: offset tracking + `INSERT OR IGNORE` in `mirror_file` ensure
//!   running multiple times or restarting the daemon is safe.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::ingest::{self, LineTailSource};

fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn discovered_entry_is_link_or_reparse(
    path: &Path,
    file_type: &std::fs::FileType,
) -> io::Result<bool> {
    if file_type.is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        Ok(is_link_or_reparse(&std::fs::symlink_metadata(path)?))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

/// How a discovered file should be ingested.
///
/// Provider exports are `MirrorSource` variants (ADR-080's closed
/// mirror-source set) but deliberately not `LineTailSource` variants: export
/// ingestion is whole-file, not line-tail, so it does not belong in that
/// narrower per-line dispatch enum.
#[derive(Clone, PartialEq, Eq)]
enum DiscoveredKind {
    LineTail {
        source: LineTailSource,
        /// Set for `LineTailSource::Codex`; `None` for `LineTailSource::ClaudeCode`.
        session_id: Option<String>,
    },
    ChatGptExport,
    ClaudeAiExport,
}

const DIRECTORY_PROBES_PER_TICK: usize = 64;
const COLD_FILE_PROBES_PER_TICK: usize = 256;
/// Worst-case number of STALE cold-queue pops per tick. A stale pop is a
/// queue entry that turns out to be reactivated, removed, or already
/// selected — it performs no filesystem work, but lazily invalidated
/// residue must still be bounded so one tick cannot drain an arbitrary
/// backlog. Productive pops remain capped by `COLD_FILE_PROBES_PER_TICK`,
/// independently of this budget (ADR-080 §6).
const COLD_STALE_POPS_PER_TICK: usize = COLD_FILE_PROBES_PER_TICK * 4;
const DIRECTORY_FORCE_RESCAN_PROBES: u16 = 30;
const FILE_UNCHANGED_POLLS_BEFORE_COLD: u8 = 2;
const FILE_COLD_AGE: Duration = Duration::from_secs(5 * 60);
const FILE_UNCHANGED_POLLS_WITHOUT_MTIME: u8 = 30;
/// Consecutive probes on which every source candidate for a file errors
/// before the file is demoted from hot polling to the cold sample. The
/// ordinary cold cadence retries it; no separate retry machinery exists.
const FILE_ERROR_POLLS_BEFORE_COLD: u8 = 3;
/// Consecutive NotFound probes required before a non-pinned file is removed
/// from tracking. A single NotFound can be transient (an atomic replace or
/// a filesystem hiccup); immediate removal would also delete the cursor
/// row, so a file recreated one tick later would be reseeded to EOF
/// (`backfill = false`) and skip the bytes written in between.
const FILE_MISSING_PROBES_BEFORE_REMOVAL: u8 = 2;
/// Bound on cursor-row deletions awaiting retry after a failed cleanup.
const CURSOR_DELETE_RETRY_LIMIT: usize = 1024;
/// Consecutive directory-refresh failures before the failure log escalates
/// from debug to warn. The counter resets on success, so one warn is
/// emitted per failure episode, not per tick.
const DIRECTORY_REFRESH_FAILURES_BEFORE_WARN: u16 = 3;

/// Configuration for the mirror service.
///
/// Loaded from environment variables at daemon boot via `MirrorConfig::from_env`.
pub struct MirrorConfig {
    /// Whether the Claude Code transcript mirror is enabled (default: false — opt-in).
    pub enabled: bool,
    /// Root directory that contains `<project-slug>/<session-uuid>.jsonl` files.
    ///
    /// Defaults to `$HOME/.claude/projects`.
    pub projects_dir: PathBuf,
    /// Whether the Codex CLI transcript mirror is enabled (default: false — opt-in,
    /// independent of `enabled`).
    pub codex_enabled: bool,
    /// Root directory that contains `YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` files.
    ///
    /// Defaults to `$HOME/.codex/sessions`.
    pub codex_sessions_dir: PathBuf,
    /// Whether the ChatGPT export mirror is enabled (default: false — opt-in,
    /// independent of `enabled` and `codex_enabled`).
    pub chatgpt_enabled: bool,
    /// Root directory scanned (recursively) for `conversations.json` export files.
    ///
    /// Defaults to `$HOME/.chatgpt/exports`.
    pub chatgpt_exports_dir: PathBuf,
    /// Whether the claude.ai export mirror is enabled (default: false — opt-in,
    /// independent of the other source flags).
    pub claude_ai_enabled: bool,
    /// Root directory scanned (recursively) for claude.ai `conversations.json`
    /// export files.
    ///
    /// Defaults to `$HOME/.claude/exports`.
    pub claude_ai_exports_dir: PathBuf,
    /// How long to sleep between polling ticks (default: 2 seconds).
    pub poll_interval: Duration,
    /// When true (default), existing files are mirrored from byte offset 0.
    /// When false, newly discovered files start mirroring from their current EOF.
    pub backfill: bool,
}

/// Default poll interval, in seconds, used when `KHIVE_MIRROR_POLL_SECS` is
/// unset, non-numeric, or explicitly zero.
const DEFAULT_MIRROR_POLL_SECS: u64 = 2;

/// Parse `KHIVE_MIRROR_POLL_SECS`, rejecting an explicit `0` (which would
/// otherwise create a hot polling loop) and falling back to
/// `DEFAULT_MIRROR_POLL_SECS` for missing, non-numeric, or zero values.
///
/// Explicit zero and non-numeric input are logged as distinct warnings so an
/// operator can tell which mistake they made.
fn parse_mirror_poll_secs(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_MIRROR_POLL_SECS,
        Some(v) => match v.parse::<u64>() {
            Ok(0) => {
                tracing::warn!(
                    value = v,
                    default_secs = DEFAULT_MIRROR_POLL_SECS,
                    "KHIVE_MIRROR_POLL_SECS must be nonzero; using default"
                );
                DEFAULT_MIRROR_POLL_SECS
            }
            Ok(secs) => secs,
            Err(_) => {
                tracing::warn!(
                    value = v,
                    default_secs = DEFAULT_MIRROR_POLL_SECS,
                    "KHIVE_MIRROR_POLL_SECS is not numeric; using default"
                );
                DEFAULT_MIRROR_POLL_SECS
            }
        },
    }
}

impl MirrorConfig {
    /// Build config from environment variables, falling back to safe defaults.
    ///
    /// | Variable                       | Default                        |
    /// |--------------------------------|--------------------------------|
    /// | `KHIVE_MIRROR_ENABLED`         | `false`                        |
    /// | `KHIVE_MIRROR_PROJECTS_DIR`    | `$HOME/.claude/projects`       |
    /// | `KHIVE_MIRROR_CODEX_ENABLED`   | `false`                        |
    /// | `KHIVE_MIRROR_CODEX_DIR`       | `$HOME/.codex/sessions`        |
    /// | `KHIVE_MIRROR_CHATGPT_ENABLED` | `false`                        |
    /// | `KHIVE_MIRROR_CHATGPT_DIR`     | `$HOME/.chatgpt/exports`       |
    /// | `KHIVE_MIRROR_CLAUDE_AI_ENABLED` | `false`                      |
    /// | `KHIVE_MIRROR_CLAUDE_AI_DIR`   | `$HOME/.claude/exports`        |
    /// | `KHIVE_MIRROR_POLL_SECS`       | `2`                            |
    /// | `KHIVE_MIRROR_BACKFILL`        | `true`                         |
    pub fn from_env() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());

        let enabled = khive_runtime::env_flag("KHIVE_MIRROR_ENABLED", false);

        let projects_dir = std::env::var("KHIVE_MIRROR_PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".claude").join("projects"));

        let codex_enabled = khive_runtime::env_flag("KHIVE_MIRROR_CODEX_ENABLED", false);

        let codex_sessions_dir = std::env::var("KHIVE_MIRROR_CODEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".codex").join("sessions"));

        let chatgpt_enabled = khive_runtime::env_flag("KHIVE_MIRROR_CHATGPT_ENABLED", false);

        let chatgpt_exports_dir = std::env::var("KHIVE_MIRROR_CHATGPT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".chatgpt").join("exports"));

        let claude_ai_enabled = khive_runtime::env_flag("KHIVE_MIRROR_CLAUDE_AI_ENABLED", false);

        let claude_ai_exports_dir = std::env::var("KHIVE_MIRROR_CLAUDE_AI_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".claude").join("exports"));

        let poll_raw = std::env::var("KHIVE_MIRROR_POLL_SECS").ok();
        let poll_secs = parse_mirror_poll_secs(poll_raw.as_deref());

        let backfill = std::env::var("KHIVE_MIRROR_BACKFILL")
            .map(|v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        Self {
            enabled,
            projects_dir,
            codex_enabled,
            codex_sessions_dir,
            chatgpt_enabled,
            chatgpt_exports_dir,
            claude_ai_enabled,
            claude_ai_exports_dir,
            poll_interval: Duration::from_secs(poll_secs),
            backfill,
        }
    }
}

#[cfg(test)]
#[path = "service_config_tests.rs"]
mod config_tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectoryKind {
    ClaudeCodeRoot,
    ClaudeCodeProject,
    Codex,
    ChatGptExport,
    ClaudeAiExport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryFingerprint {
    modified: Option<SystemTime>,
    len: u64,
}

impl DirectoryFingerprint {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

struct TrackedDirectory {
    kinds: Vec<DirectoryKind>,
    fingerprint: Option<DirectoryFingerprint>,
    entries: HashSet<PathBuf>,
    unchanged_probes: u16,
    pinned: bool,
    /// Consecutive refresh failures; drives the one-shot debug→warn
    /// escalation and resets on success.
    refresh_failures: u16,
}

struct TrackedFile {
    kinds: Vec<DiscoveredKind>,
    cold: bool,
    unchanged_polls: u8,
    pinned: bool,
    /// Probes in a row on which every source candidate errored while the
    /// file was hot. Resets on any successful advance; at
    /// `FILE_ERROR_POLLS_BEFORE_COLD` the file is demoted to cold.
    consecutive_error_polls: u8,
    /// Consecutive probes on which `stat` reported the file missing.
    /// Resets on any successful probe; removal requires
    /// `FILE_MISSING_PROBES_BEFORE_REMOVAL` in a row so a transient
    /// NotFound (atomic replace, FS hiccup) gets one grace probe.
    missing_probes: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CursorState {
    byte_offset: u64,
    file_identity: Option<String>,
}

impl CursorState {
    fn reset_if_replaced(&mut self, identity: &str, file_len: u64, backfill: bool) -> bool {
        let prior = self.file_identity.as_deref();
        let identity_changed = prior.is_some_and(|old| old != identity);
        let legacy_without_witness = prior.is_none();
        let truncated = file_len < self.byte_offset;
        if identity_changed || truncated || (legacy_without_witness && backfill) {
            self.byte_offset = 0;
            self.file_identity = Some(identity.to_string());
            return true;
        }
        if legacy_without_witness {
            self.file_identity = Some(identity.to_string());
        }
        false
    }
}

async fn reconcile_cursor_identity(
    runtime: &KhiveRuntime,
    path: &Path,
    cursor: &mut CursorState,
    identity: &str,
    file_len: u64,
    backfill: bool,
) -> Result<bool, RuntimeError> {
    let legacy_without_witness = cursor.file_identity.is_none();
    let restarted = cursor.reset_if_replaced(identity, file_len, backfill);
    if legacy_without_witness && !restarted {
        if let Err(error) =
            ingest::adopt_cursor_identity(runtime, path, cursor.byte_offset, identity).await
        {
            cursor.file_identity = None;
            return Err(error);
        }
    }
    Ok(restarted)
}

struct ScheduledFile {
    path: PathBuf,
    was_cold: bool,
}

#[derive(Default)]
struct CandidateDispatch {
    stats: Option<ingest::MirrorStats>,
    errors: Vec<RuntimeError>,
}

impl CandidateDispatch {
    /// Record one candidate's result. Returns `true` when this candidate
    /// should end dispatch for the file: it advanced the offset past
    /// `start_offset` AND inserted rows.
    ///
    /// Invariant relied on (ADR-080 §6 invariant 4): an `Err` never advances
    /// the cursor — every ingest path commits the cursor only inside the
    /// same successful write that consumed the bytes (`write_events_and_cursor`
    /// is one transaction; `write_cursor_only` failures propagate; export
    /// paths set `new_offset` only after a successful parse+commit). An
    /// advancing candidate has therefore durably consumed its byte range.
    ///
    /// An advance with zero inserts (a cursor-only pass over
    /// blank/unparseable/oversized lines, `mirror_file_with_limits`) is
    /// recorded — the bytes were consumed off the file — but does NOT end
    /// dispatch: under misconfigured overlapping roots, a wrong provider
    /// candidate could otherwise swallow bytes that a later, correct
    /// provider candidate would have parsed into rows. Its cursor commit is
    /// deferred (`mirror_file_deferred`) and committed by the dispatch loop
    /// only when no inserting candidate claims the span and no candidate
    /// errored — except a skip caused by a known-oversized line
    /// (`stats.skipped_oversized_bytes`), which `finalize_dispatch_stats`
    /// commits unconditionally: that determination never depended on which
    /// source parsed the line, so no sibling candidate could ever have
    /// claimed the same bytes.
    ///
    /// Recording precedence: an advancing result (`new_offset >
    /// start_offset`) always replaces a recorded non-advancing one, so a
    /// later empty advance cannot be hidden behind an earlier no-progress
    /// candidate — the in-memory offset must track the durably committed
    /// cursor. Among advancing results the first recorded wins (an empty
    /// advance never overwrites an inserting one, and an inserting
    /// candidate ends dispatch anyway). A non-advancing success is
    /// recorded only when nothing is recorded yet.
    fn record(
        &mut self,
        result: Result<ingest::MirrorStats, RuntimeError>,
        start_offset: u64,
    ) -> bool {
        match result {
            Ok(stats) if stats.new_offset > start_offset && stats.inserted > 0 => {
                self.stats = Some(stats);
                true
            }
            Ok(stats) if stats.new_offset > start_offset => {
                // Empty advance: bytes consumed, but no rows were inserted —
                // fall through to remaining candidates. (The cursor commit is
                // deferred to the end of dispatch by `mirror_file_deferred`;
                // an inserting candidate or an erroring candidate can still
                // veto it.) Always replace a recorded non-advancing result so
                // the in-memory offset follows the furthest consumed byte;
                // keep the first advancing record.
                let recorded_advancing = self
                    .stats
                    .as_ref()
                    .is_some_and(|recorded| recorded.new_offset > start_offset);
                if !recorded_advancing {
                    self.stats = Some(stats);
                }
                false
            }
            Ok(stats) if stats.new_offset >= start_offset => {
                if self.stats.is_none() {
                    self.stats = Some(stats);
                }
                false
            }
            Ok(_) => {
                // The ingest contract guarantees `new_offset >= start_offset`
                // (read_bounded_chunk only advances from start_offset; export
                // paths return start_offset or the file length after a
                // `file_len <= start_offset` guard). Defend against future
                // drift: never record a regressing cursor.
                false
            }
            Err(error) => {
                self.errors.push(error);
                false
            }
        }
    }
}

#[derive(Default)]
struct DirectoryProbeStats {
    metadata_probes: usize,
    walks: usize,
}

#[derive(Default)]
struct DiscoveryIndex {
    directories: HashMap<PathBuf, TrackedDirectory>,
    directory_queue: VecDeque<PathBuf>,
    directory_enqueued: HashSet<PathBuf>,
    files: HashMap<PathBuf, TrackedFile>,
    hot_files: HashSet<PathBuf>,
    priority_cold_queue: VecDeque<PathBuf>,
    priority_cold_enqueued: HashSet<PathBuf>,
    cold_queue: VecDeque<PathBuf>,
    cold_enqueued: HashSet<PathBuf>,
    /// Paths removed from `files` since the last drain, so the service loop
    /// can prune their in-memory offsets and persisted cursor rows.
    removed_files: Vec<PathBuf>,
}

enum ClassifiedEntry {
    Directory(DirectoryKind),
    File(DiscoveredKind),
}

impl DiscoveryIndex {
    fn from_config(config: &MirrorConfig) -> Self {
        let mut discovery = Self::default();

        if config.enabled {
            discovery.add_directory_tree(&config.projects_dir, DirectoryKind::ClaudeCodeRoot, true);
        }
        if config.codex_enabled {
            discovery.add_directory_tree(&config.codex_sessions_dir, DirectoryKind::Codex, true);
        }
        if config.chatgpt_enabled {
            discovery.add_export_root(
                &config.chatgpt_exports_dir,
                DirectoryKind::ChatGptExport,
                DiscoveredKind::ChatGptExport,
            );
        }
        if config.claude_ai_enabled {
            discovery.add_export_root(
                &config.claude_ai_exports_dir,
                DirectoryKind::ClaudeAiExport,
                DiscoveredKind::ClaudeAiExport,
            );
        }

        discovery
    }

    fn add_export_root(
        &mut self,
        path: &Path,
        directory_kind: DirectoryKind,
        file_kind: DiscoveredKind,
    ) {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| is_link_or_reparse(&metadata)) {
            tracing::warn!(
                path = %path.display(),
                "session mirror: refusing a linked configured export root"
            );
            return;
        }
        if path.is_file() {
            if path.file_name().and_then(|name| name.to_str()) == Some("conversations.json") {
                self.add_file(path.to_path_buf(), file_kind, true);
            }
        } else {
            self.add_directory_tree(path, directory_kind, true);
        }
    }

    fn add_directory_tree(&mut self, root: &Path, kind: DirectoryKind, pinned: bool) {
        let mut pending = vec![(root.to_path_buf(), kind, pinned)];

        while let Some((path, directory_kind, is_pinned)) = pending.pop() {
            if let Some(directory) = self.directories.get_mut(&path) {
                directory.pinned |= is_pinned;
                if !directory.kinds.contains(&directory_kind) {
                    directory.kinds.push(directory_kind);
                    directory.fingerprint = None;
                }
                // The queued invariant is non-local (a directory is only
                // guaranteed to be in `directory_queue` while its own probe
                // cycle is intact), so re-assert it on every re-entry.
                self.enqueue_directory(&path);
                continue;
            }
            if self.files.contains_key(&path) {
                self.remove_file(&path, false);
            }

            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if is_link_or_reparse(&metadata) && is_pinned => None,
                Ok(metadata) if is_link_or_reparse(&metadata) => continue,
                Ok(metadata) if metadata.is_dir() => Some(metadata),
                Ok(metadata)
                    if is_pinned
                        && metadata.is_file()
                        && path.file_name().and_then(|name| name.to_str())
                            == Some("conversations.json") =>
                {
                    let file_kind = match directory_kind {
                        DirectoryKind::ChatGptExport => DiscoveredKind::ChatGptExport,
                        DirectoryKind::ClaudeAiExport => DiscoveredKind::ClaudeAiExport,
                        _ => continue,
                    };
                    self.add_file(path, file_kind, true);
                    continue;
                }
                // Keep a pinned configured root as a directory placeholder
                // even when startup finds an unexpected file at that path.
                // The probe path retains the same identity for a runtime
                // directory-to-file transition, allowing a later directory
                // reversion to be discovered.
                Ok(_) if is_pinned => None,
                Ok(_) => continue,
                Err(_) if is_pinned => None,
                Err(_) => continue,
            };

            let mut fingerprint = metadata.as_ref().map(DirectoryFingerprint::from_metadata);
            let mut entries = HashSet::new();
            let mut children = Vec::new();

            if metadata.is_some() {
                match std::fs::read_dir(&path) {
                    Ok(read_dir) => {
                        for entry in read_dir {
                            let Ok(entry) = entry else {
                                fingerprint = None;
                                continue;
                            };
                            let child_path = entry.path();
                            let Ok(file_type) = entry.file_type() else {
                                fingerprint = None;
                                continue;
                            };
                            let linked = match discovered_entry_is_link_or_reparse(
                                &child_path,
                                &file_type,
                            ) {
                                Ok(linked) => linked,
                                Err(_) => {
                                    fingerprint = None;
                                    continue;
                                }
                            };
                            if linked {
                                continue;
                            }
                            let Some(classified) =
                                classify_entry(directory_kind, &child_path, file_type.is_dir())
                            else {
                                continue;
                            };
                            entries.insert(child_path.clone());
                            match classified {
                                ClassifiedEntry::Directory(child_kind) => {
                                    children.push((child_path, child_kind, false));
                                }
                                ClassifiedEntry::File(file_kind) => {
                                    self.add_file(child_path, file_kind, false);
                                }
                            }
                        }
                    }
                    Err(_) => fingerprint = None,
                }
            }

            self.directories.insert(
                path.clone(),
                TrackedDirectory {
                    kinds: vec![directory_kind],
                    fingerprint,
                    entries,
                    unchanged_probes: 0,
                    pinned: is_pinned,
                    refresh_failures: 0,
                },
            );
            self.enqueue_directory(&path);
            pending.extend(children);
        }
    }

    fn add_file(&mut self, path: PathBuf, kind: DiscoveredKind, pinned: bool) {
        if let Some(file) = self.files.get_mut(&path) {
            file.pinned |= pinned;
            if !file.kinds.contains(&kind) {
                file.kinds.push(kind);
            }
            return;
        }

        self.files.insert(
            path.clone(),
            TrackedFile {
                kinds: vec![kind],
                cold: false,
                unchanged_polls: 0,
                pinned,
                consecutive_error_polls: 0,
                missing_probes: 0,
            },
        );
        self.hot_files.insert(path);
    }

    fn probe_directories(&mut self) -> DirectoryProbeStats {
        let mut stats = DirectoryProbeStats::default();
        let scheduled = self.directory_queue.len().min(DIRECTORY_PROBES_PER_TICK);

        for _ in 0..scheduled {
            let Some(path) = self.directory_queue.pop_front() else {
                break;
            };
            self.directory_enqueued.remove(&path);
            let Some(directory) = self.directories.get(&path) else {
                continue;
            };
            let kinds = directory.kinds.clone();
            let stored_fingerprint = directory.fingerprint;
            let force_rescan = directory.unchanged_probes >= DIRECTORY_FORCE_RESCAN_PROBES;
            stats.metadata_probes += 1;

            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if is_link_or_reparse(&metadata) => {
                    self.remove_directory_tree(&path, true);
                }
                Ok(metadata) if metadata.is_dir() => {
                    // A path tracked as a directory that is one again must
                    // not keep a stale file record left by an intervening
                    // file phase (the directory-to-file transition below).
                    if self.files.contains_key(&path) {
                        self.remove_file(&path, false);
                    }
                    let fingerprint = DirectoryFingerprint::from_metadata(&metadata);
                    if force_rescan || Some(fingerprint) != stored_fingerprint {
                        let changed = Some(fingerprint) != stored_fingerprint;
                        if let Err(error) =
                            self.refresh_directory(&path, &kinds, fingerprint, changed)
                        {
                            // Escalate once per failure episode: the counter
                            // crosses the threshold exactly once until a
                            // success resets it, so a wedged directory is
                            // visible without per-tick warn spam.
                            let failures = self.record_refresh_failure(&path);
                            if failures == DIRECTORY_REFRESH_FAILURES_BEFORE_WARN {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %error,
                                    consecutive_failures = failures,
                                    "session mirror: directory refresh keeps failing"
                                );
                            } else {
                                tracing::debug!(
                                    path = %path.display(),
                                    error = %error,
                                    consecutive_failures = failures,
                                    "session mirror: directory refresh failed"
                                );
                            }
                        } else {
                            self.clear_refresh_failures(&path);
                            stats.walks += 1;
                        }
                    } else if let Some(directory) = self.directories.get_mut(&path) {
                        directory.unchanged_probes = directory.unchanged_probes.saturating_add(1);
                    }
                }
                Ok(metadata)
                    if metadata.is_file()
                        && path.file_name().and_then(|name| name.to_str())
                            == Some("conversations.json") =>
                {
                    let pinned = self
                        .directories
                        .get(&path)
                        .is_some_and(|directory| directory.pinned);
                    // Keep pinned identity (as the NotFound/other arms do) so
                    // a configured root that became a file and later reverts to
                    // a directory stays scheduled instead of being dropped.
                    self.remove_directory_tree(&path, true);
                    for kind in kinds {
                        let file_kind = match kind {
                            DirectoryKind::ChatGptExport => Some(DiscoveredKind::ChatGptExport),
                            DirectoryKind::ClaudeAiExport => Some(DiscoveredKind::ClaudeAiExport),
                            _ => None,
                        };
                        if let Some(file_kind) = file_kind {
                            self.add_file(path.clone(), file_kind, pinned);
                        }
                    }
                }
                Ok(_) => {
                    // A stale file record from an intervening file phase must
                    // not shadow the retained directory identity while the
                    // path holds an unrelated entry.
                    if self.files.contains_key(&path) {
                        self.remove_file(&path, false);
                    }
                    self.remove_directory_tree(&path, true);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.remove_directory_tree(&path, true);
                }
                Err(error) => {
                    // A wedged directory must not stay invisible in debug
                    // logs forever: count the failure toward the same
                    // escalation as a failed refresh walk. NotFound is
                    // excluded — it removes the tree above, not an error.
                    let failures = self.record_refresh_failure(&path);
                    if failures == DIRECTORY_REFRESH_FAILURES_BEFORE_WARN {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            consecutive_failures = failures,
                            "session mirror: directory metadata probe keeps failing"
                        );
                    } else {
                        tracing::debug!(
                            path = %path.display(),
                            error = %error,
                            consecutive_failures = failures,
                            "session mirror: directory metadata probe failed"
                        );
                    }
                }
            }

            if self.directories.contains_key(&path) {
                self.enqueue_directory(&path);
            }
        }

        stats
    }

    fn refresh_directory(
        &mut self,
        path: &Path,
        kinds: &[DirectoryKind],
        fingerprint: DirectoryFingerprint,
        prioritize: bool,
    ) -> io::Result<()> {
        let read_dir = std::fs::read_dir(path)?;
        let mut entries = HashSet::new();
        let mut directories = Vec::new();
        let mut files = Vec::new();
        let mut complete = true;

        for entry in read_dir {
            // A faulty entry (raced removal, unreadable metadata) must not
            // abort the whole refresh and freeze discovery of the directory's
            // other files: skip it, and mark the listing incomplete so the
            // stored fingerprint is cleared and the next probe retries —
            // matching `add_directory_tree`'s non-fatal entry handling.
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let child_path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let linked = match discovered_entry_is_link_or_reparse(&child_path, &file_type) {
                Ok(linked) => linked,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if linked {
                continue;
            }
            for kind in kinds {
                let Some(classified) = classify_entry(*kind, &child_path, file_type.is_dir())
                else {
                    continue;
                };
                entries.insert(child_path.clone());
                match classified {
                    ClassifiedEntry::Directory(child_kind) => {
                        directories.push((child_path.clone(), child_kind));
                    }
                    ClassifiedEntry::File(file_kind) => {
                        files.push((child_path.clone(), file_kind));
                    }
                }
            }
        }

        let removed = if complete {
            self.directories
                .get(path)
                .map(|directory| {
                    directory
                        .entries
                        .difference(&entries)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            // An incomplete listing cannot prove entries are gone; defer
            // removals until a complete read (the cleared fingerprint below
            // retries on the next probe).
            Vec::new()
        };

        for removed_path in removed {
            if self.directories.contains_key(&removed_path) {
                self.remove_directory_tree(&removed_path, true);
            } else {
                self.remove_file(&removed_path, true);
            }
        }

        for (file_path, file_kind) in files {
            let already_tracked = self.files.contains_key(&file_path);
            self.add_file(file_path.clone(), file_kind, false);
            // Only a real fingerprint change carries a change signal worth
            // reprioritizing cold files for; a forced rescan of an unchanged
            // directory would otherwise periodically flood the priority queue
            // and starve the ordinary cold round-robin.
            if prioritize && already_tracked {
                self.prioritize_cold(&file_path);
            }
        }
        for (directory_path, directory_kind) in directories {
            self.add_directory_tree(&directory_path, directory_kind, false);
        }

        if let Some(directory) = self.directories.get_mut(path) {
            if complete {
                directory.fingerprint = Some(fingerprint);
                directory.entries = entries;
            } else {
                // Keep the last complete snapshot: a partial listing could
                // drop still-present paths from `entries` and hide their
                // later removal from the difference computation. The cleared
                // fingerprint retries the listing on the next probe.
                directory.fingerprint = None;
            }
            directory.unchanged_probes = 0;
        }

        Ok(())
    }

    fn remove_directory_tree(&mut self, path: &Path, keep_pinned: bool) {
        let Some(directory) = self.directories.get(path) else {
            return;
        };
        let entries = directory.entries.iter().cloned().collect::<Vec<_>>();
        let pinned = directory.pinned;

        for entry in entries {
            if self.directories.contains_key(&entry) {
                self.remove_directory_tree(&entry, true);
            } else {
                self.remove_file(&entry, true);
            }
        }

        if keep_pinned && pinned {
            if let Some(directory) = self.directories.get_mut(path) {
                directory.fingerprint = None;
                directory.entries.clear();
                directory.unchanged_probes = 0;
            }
            // The queued invariant is non-local — this path's queue entry
            // may have been popped earlier in the current tick without a
            // re-enqueue yet — so re-assert it: a retained pinned root must
            // stay scheduled or its later reappearance is never noticed.
            self.enqueue_directory(path);
        } else {
            self.directories.remove(path);
            // Queue residue is invalidated lazily: `probe_directories` skips
            // popped paths that are no longer tracked, and a recursive
            // O(queue) retain per removed node would make deep tree removals
            // quadratic. The enqueue flag is deliberately left set until the
            // residue is popped: it means "an entry is queued", and clearing
            // it early would let a re-add enqueue a duplicate that gets
            // probed twice in one tick.
        }
    }

    fn remove_file(&mut self, path: &Path, preserve_pinned: bool) {
        if preserve_pinned && self.files.get(path).is_some_and(|file| file.pinned) {
            return;
        }
        if self.files.remove(path).is_none() {
            return;
        }
        self.hot_files.remove(path);
        // Cold-queue entries for the removed path are invalidated lazily:
        // `schedule_files` pops them under the counted stale-pop budget and
        // skips them because the path is no longer tracked. An O(queue)
        // `retain` per removal would scan the whole cold ring for residue
        // that drains itself; clearing the enqueue flags is enough for a
        // later re-add to start clean.
        self.cold_enqueued.remove(path);
        self.priority_cold_enqueued.remove(path);
        self.removed_files.push(path.to_path_buf());
    }

    /// Drain paths removed from tracking since the last call so the service
    /// loop can prune their cursors.
    fn take_removed_files(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.removed_files)
    }

    fn reactivate_file(&mut self, path: &Path) {
        // Deliberately does NOT purge `cold_queue` / `priority_cold_queue`:
        // reactivation can race queued entries, and they are invalidated
        // lazily by `schedule_files`' counted stale-pop skip instead of an
        // O(queue) scan here.
        if let Some(file) = self.files.get_mut(path) {
            file.cold = false;
            file.unchanged_polls = 0;
            // A successful growth probe ends the missing-file streak.
            file.missing_probes = 0;
            // ...and starts a fresh error-streak window: "consecutive
            // error ticks" counts between successful probes.
            file.consecutive_error_polls = 0;
            self.hot_files.insert(path.to_path_buf());
        }
    }

    fn schedule_files(&mut self) -> Vec<ScheduledFile> {
        let mut scheduled = self
            .hot_files
            .iter()
            .filter(|path| self.files.contains_key(*path))
            .cloned()
            .map(|path| ScheduledFile {
                path,
                was_cold: false,
            })
            .collect::<Vec<_>>();
        // Productive pops (a still-cold file that gets scheduled and
        // stat'ed) consume `remaining_cold_probes`, keeping the ADR-080 §6
        // "at most 256 cold-file metadata probes" ceiling exact. Stale pops
        // — reactivated, removed, or duplicate entries, invalidated lazily
        // instead of `retain`-purged on every hot/reactivate/remove path —
        // do no filesystem work but are still bounded by
        // `remaining_stale_pops`, so one tick can drain at most a fixed
        // backlog of residue instead of an arbitrary queue.
        let mut remaining_cold_probes = COLD_FILE_PROBES_PER_TICK;
        let mut remaining_stale_pops = COLD_STALE_POPS_PER_TICK;
        let mut selected_cold = HashSet::new();
        let priority_candidates = self.priority_cold_queue.len();

        for _ in 0..priority_candidates {
            if remaining_stale_pops == 0 {
                break;
            }
            let Some(path) = self.priority_cold_queue.pop_front() else {
                break;
            };
            self.priority_cold_enqueued.remove(&path);
            if self.files.get(&path).is_some_and(|file| file.cold)
                && selected_cold.insert(path.clone())
            {
                if remaining_cold_probes == 0 {
                    // Productive entry popped but the probe budget is
                    // spent: hand it back and stop.
                    self.priority_cold_queue.push_front(path.clone());
                    self.priority_cold_enqueued.insert(path);
                    break;
                }
                scheduled.push(ScheduledFile {
                    path,
                    was_cold: true,
                });
                remaining_cold_probes -= 1;
            } else {
                remaining_stale_pops -= 1;
            }
        }

        let cold_candidates = self.cold_queue.len();

        for _ in 0..cold_candidates {
            if remaining_stale_pops == 0 {
                break;
            }
            let Some(path) = self.cold_queue.pop_front() else {
                break;
            };
            self.cold_enqueued.remove(&path);
            if self.files.get(&path).is_some_and(|file| file.cold)
                && selected_cold.insert(path.clone())
            {
                if remaining_cold_probes == 0 {
                    self.cold_queue.push_front(path.clone());
                    self.cold_enqueued.insert(path);
                    break;
                }
                scheduled.push(ScheduledFile {
                    path,
                    was_cold: true,
                });
                remaining_cold_probes -= 1;
            } else {
                remaining_stale_pops -= 1;
            }
        }

        scheduled
    }

    fn record_unchanged(
        &mut self,
        path: &Path,
        was_cold: bool,
        modified: Option<SystemTime>,
        now: SystemTime,
    ) {
        let Some(file) = self.files.get_mut(path) else {
            return;
        };
        // A successful probe ends any missing-file streak.
        file.missing_probes = 0;

        if was_cold || file.cold {
            file.cold = true;
            self.enqueue_cold(path);
            return;
        }

        file.unchanged_polls = file.unchanged_polls.saturating_add(1);
        if should_mark_cold(file.unchanged_polls, modified, now) {
            file.cold = true;
            self.hot_files.remove(path);
            self.enqueue_cold(path);
        }
    }

    fn record_probe_error(&mut self, path: &Path, was_cold: bool, missing: bool) {
        let pinned = self.files.get(path).is_some_and(|file| file.pinned);
        if missing && !pinned {
            // A single NotFound can be transient (an atomic replace or a
            // filesystem hiccup). Removal also queues the cursor row for
            // deletion, after which a recreated file would be reseeded to
            // EOF when `backfill` is off and silently skip the bytes
            // written in between — so require a second consecutive
            // NotFound before removing (one-tick grace).
            let remove = if let Some(file) = self.files.get_mut(path) {
                file.missing_probes = file.missing_probes.saturating_add(1);
                file.missing_probes >= FILE_MISSING_PROBES_BEFORE_REMOVAL
            } else {
                false
            };
            if remove {
                self.remove_file(path, false);
            } else if was_cold {
                self.enqueue_cold(path);
            }
        } else if was_cold {
            self.enqueue_cold(path);
        }
    }

    /// Count a probe on which every source candidate errored. Returns
    /// `true` exactly on the tick whose streak crosses
    /// `FILE_ERROR_POLLS_BEFORE_COLD` — the caller's one-shot warn for the
    /// demotion. A streak of `N` keeps a persistently broken file demoted
    /// on every later reactivated probe without re-warning, and any
    /// successful advance resets the streak via [`Self::clear_error_polls`].
    fn record_error_poll(&mut self, path: &Path) -> bool {
        let Some(file) = self.files.get_mut(path) else {
            return false;
        };
        file.consecutive_error_polls = file.consecutive_error_polls.saturating_add(1);
        let crossed_threshold = file.consecutive_error_polls == FILE_ERROR_POLLS_BEFORE_COLD;
        if file.consecutive_error_polls >= FILE_ERROR_POLLS_BEFORE_COLD && !file.cold {
            // Demote to the cold sample: the ordinary cold cadence retries
            // the file, so no separate retry machinery is needed.
            file.cold = true;
            file.unchanged_polls = 0;
            self.hot_files.remove(path);
            self.enqueue_cold(path);
        }
        crossed_threshold
    }

    /// Reset the consecutive-error streak after a successful cursor advance.
    fn clear_error_polls(&mut self, path: &Path) {
        if let Some(file) = self.files.get_mut(path) {
            file.consecutive_error_polls = 0;
        }
    }

    fn enqueue_cold(&mut self, path: &Path) {
        if self.files.contains_key(path) && self.cold_enqueued.insert(path.to_path_buf()) {
            self.cold_queue.push_back(path.to_path_buf());
        }
    }

    fn prioritize_cold(&mut self, path: &Path) {
        if self.files.get(path).is_some_and(|file| file.cold)
            && self.priority_cold_enqueued.insert(path.to_path_buf())
        {
            self.priority_cold_queue.push_back(path.to_path_buf());
        }
    }

    fn enqueue_directory(&mut self, path: &Path) {
        if self.directories.contains_key(path) && self.directory_enqueued.insert(path.to_path_buf())
        {
            self.directory_queue.push_back(path.to_path_buf());
        }
    }

    /// Bump the consecutive refresh-failure counter for a directory and
    /// return the new value (0 when untracked). The caller escalates to
    /// warn exactly when it crosses `DIRECTORY_REFRESH_FAILURES_BEFORE_WARN`.
    fn record_refresh_failure(&mut self, path: &Path) -> u16 {
        self.directories
            .get_mut(path)
            .map(|directory| {
                directory.refresh_failures = directory.refresh_failures.saturating_add(1);
                directory.refresh_failures
            })
            .unwrap_or(0)
    }

    /// Reset the refresh-failure counter after a successful walk.
    fn clear_refresh_failures(&mut self, path: &Path) {
        if let Some(directory) = self.directories.get_mut(path) {
            directory.refresh_failures = 0;
        }
    }

    fn tracked_files(&self) -> usize {
        self.files.len()
    }
}

fn should_mark_cold(unchanged_polls: u8, modified: Option<SystemTime>, now: SystemTime) -> bool {
    if unchanged_polls < FILE_UNCHANGED_POLLS_BEFORE_COLD {
        return false;
    }

    // A modified time that cannot be compared against `now` (e.g. a
    // future-dated mtime from clock skew) is treated like a missing mtime:
    // fall back to the unchanged-poll threshold instead of keeping the file
    // hot indefinitely.
    match modified.and_then(|modified| now.duration_since(modified).ok()) {
        Some(age) => age >= FILE_COLD_AGE,
        None => unchanged_polls >= FILE_UNCHANGED_POLLS_WITHOUT_MTIME,
    }
}

/// Tally one dispatch pass's outcome against the file's error streak.
/// Returns `true` exactly on the tick whose streak crosses
/// `FILE_ERROR_POLLS_BEFORE_COLD` (the caller's one-shot warn).
///
/// A pass counts as an error poll when no candidate advanced the offset and
/// at least one candidate errored — including mixed passes where one
/// candidate reported a no-progress success (e.g. a whole-file size ceiling
/// below the current file length) while another errored. Treating only
/// all-error passes as error polls would let such a mixed file stay hot and
/// be re-dispatched every tick forever.
fn tally_dispatch_errors(
    discovery: &mut DiscoveryIndex,
    path: &Path,
    advanced: bool,
    had_errors: bool,
) -> bool {
    if advanced {
        discovery.clear_error_polls(path);
        return false;
    }
    had_errors && discovery.record_error_poll(path)
}

/// Finish dispatch bookkeeping for a deferred empty advance. The service
/// vetoes the cursor commit when any candidate errored, unless the advance is
/// a known-oversized-line skip (source-independent — see
/// [`ingest::MirrorStats::skipped_oversized_bytes`]), which always commits. A
/// commit failure itself becomes an error poll so persistent failures reach
/// cold cadence.
async fn finalize_dispatch_stats(
    runtime: &KhiveRuntime,
    path: &Path,
    offset: u64,
    ended_by_inserting: bool,
    stats: Option<ingest::MirrorStats>,
    mut had_errors: bool,
) -> (Option<ingest::MirrorStats>, bool) {
    // Commit a deferred empty advance when dispatch ended with no inserting
    // candidate AND (no candidate error OR the advance skipped bytes from a
    // known-oversized line). An erroring candidate might have parsed an
    // ordinary empty-advance span had it succeeded, so that case holds the
    // cursor at the old offset and re-reads the bytes (bounded and
    // idempotent) on a later pass. A pass that skipped only oversized bytes
    // carries no such ambiguity — the line was too large for ANY parser
    // before one ever ran — so it commits even when a sibling candidate
    // errored on the same span (ADR-080). On commit failure the in-memory offset is
    // likewise NOT applied.
    let stats = match stats {
        Some(stats) if !ended_by_inserting && stats.inserted == 0 && stats.new_offset > offset => {
            // The exemption is exact only when the pass consumed nothing but
            // the oversized bytes (and blank lines): a bounded chunk keeps
            // reading ordinary lines after a terminated oversized line, and
            // those lines an erroring sibling might have parsed, so a mixed
            // pass (`scanned > 0`) keeps the veto.
            let uncontested_skip = stats.skipped_oversized_bytes && stats.scanned == 0;
            if had_errors && !uncontested_skip {
                tracing::debug!(
                    path = %path.display(),
                    new_offset = stats.new_offset,
                    "session mirror: deferring empty-advance cursor commit because a \
                     candidate errored; the span will be re-read on a later pass"
                );
                None
            } else {
                if had_errors {
                    tracing::debug!(
                        path = %path.display(),
                        new_offset = stats.new_offset,
                        "session mirror: committing an oversized-line skip despite a \
                         sibling candidate error — the skip is source-independent"
                    );
                }
                match ingest::commit_empty_advance(runtime, path, &stats).await {
                    Ok(()) => Some(stats),
                    Err(error) => {
                        // A failed deferred cursor commit is an ingest error
                        // for cadence purposes: the file made no durable
                        // progress and must eventually be demoted to the cold
                        // retry cadence just like any other failed pass.
                        had_errors = true;
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            new_offset = stats.new_offset,
                            "session mirror: empty-advance cursor commit failed; \
                             offset held back for a bounded re-read"
                        );
                        None
                    }
                }
            }
        }
        other => other,
    };
    (stats, had_errors)
}

fn classify_entry(
    directory_kind: DirectoryKind,
    path: &Path,
    is_directory: bool,
) -> Option<ClassifiedEntry> {
    // Callers reject symlinks and Windows reparse points before classification,
    // so discovery cannot loop through a linked directory.
    // Traversal depth is therefore bounded by the real on-disk tree depth,
    // and `remove_directory_tree` (which descends only into tracked
    // directories) inherits the same bound — no depth cap is needed.
    if is_directory {
        return match directory_kind {
            DirectoryKind::ClaudeCodeRoot => {
                Some(ClassifiedEntry::Directory(DirectoryKind::ClaudeCodeProject))
            }
            DirectoryKind::ClaudeCodeProject => None,
            DirectoryKind::Codex => Some(ClassifiedEntry::Directory(DirectoryKind::Codex)),
            DirectoryKind::ChatGptExport => {
                Some(ClassifiedEntry::Directory(DirectoryKind::ChatGptExport))
            }
            DirectoryKind::ClaudeAiExport => {
                Some(ClassifiedEntry::Directory(DirectoryKind::ClaudeAiExport))
            }
        };
    }

    match directory_kind {
        DirectoryKind::ClaudeCodeProject
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") =>
        {
            Some(ClassifiedEntry::File(DiscoveredKind::LineTail {
                source: LineTailSource::ClaudeCode,
                session_id: None,
            }))
        }
        DirectoryKind::Codex
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") =>
        {
            extract_codex_session_id(path).map(|session_id| {
                ClassifiedEntry::File(DiscoveredKind::LineTail {
                    source: LineTailSource::Codex,
                    session_id: Some(session_id),
                })
            })
        }
        DirectoryKind::ChatGptExport
            if path.file_name().and_then(|name| name.to_str()) == Some("conversations.json") =>
        {
            Some(ClassifiedEntry::File(DiscoveredKind::ChatGptExport))
        }
        DirectoryKind::ClaudeAiExport
            if path.file_name().and_then(|name| name.to_str()) == Some("conversations.json") =>
        {
            Some(ClassifiedEntry::File(DiscoveredKind::ClaudeAiExport))
        }
        _ => None,
    }
}

fn trusted_root_for_file<'a>(
    config: &'a MirrorConfig,
    path: &Path,
    kinds: &[DiscoveredKind],
) -> Option<&'a Path> {
    kinds.iter().find_map(|kind| {
        let configured = match kind {
            DiscoveredKind::LineTail {
                source: LineTailSource::ClaudeCode,
                ..
            } => config.projects_dir.as_path(),
            DiscoveredKind::LineTail {
                source: LineTailSource::Codex,
                ..
            } => config.codex_sessions_dir.as_path(),
            DiscoveredKind::ChatGptExport => config.chatgpt_exports_dir.as_path(),
            DiscoveredKind::ClaudeAiExport => config.claude_ai_exports_dir.as_path(),
        };
        // A configured export may name conversations.json itself. Anchor its
        // final component at the containing directory in that case.
        let root = if configured == path {
            configured.parent().unwrap_or(Path::new(""))
        } else {
            configured
        };
        path.strip_prefix(root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|_| root)
    })
}

fn probe_source_file(
    root: &Path,
    path: &Path,
) -> io::Result<(std::fs::Metadata, String, Vec<String>)> {
    ingest::open_source_file_beneath(root, path, None).and_then(|(file, directory_identities)| {
        let metadata = file.metadata()?;
        let identity = ingest::file_identity(&file)?;
        Ok((metadata, identity, directory_identities))
    })
}

/// Infinite background polling loop.  Returns only on a fatal setup error.
///
/// Seed state from the `session_mirror_cursor` table and one initial discovery
/// pass at startup, then loop: probe changed directories and active files,
/// sample a bounded cold-file set, tail new bytes, sleep.
///
/// Every source is independent: each is enabled by its own flag and indexed
/// under its own configured root.
///
/// Per-file errors are logged with `tracing::warn!` and do NOT stop the loop.
pub async fn run_mirror_service(runtime: KhiveRuntime, config: MirrorConfig) {
    tracing::info!(
        projects_dir = %config.projects_dir.display(),
        codex_sessions_dir = %config.codex_sessions_dir.display(),
        chatgpt_exports_dir = %config.chatgpt_exports_dir.display(),
        claude_ai_exports_dir = %config.claude_ai_exports_dir.display(),
        poll_interval_ms = config.poll_interval.as_millis(),
        backfill = config.backfill,
        cc_enabled = config.enabled,
        codex_enabled = config.codex_enabled,
        chatgpt_enabled = config.chatgpt_enabled,
        claude_ai_enabled = config.claude_ai_enabled,
        "session mirror service starting"
    );

    // Seed in-memory offsets and file identities from the persisted cursor table.
    let mut cursors: HashMap<PathBuf, CursorState> = match load_cursors(&runtime).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(error = %e, "session mirror: failed to load cursors (starting from empty)");
            HashMap::new()
        }
    };
    let mut discovery = DiscoveryIndex::from_config(&config);
    // Cursor rows whose deletion failed, retried on later ticks. A stale
    // row that survives to a daemon restart is reloaded by `load_cursors`,
    // and a recreated same-path file would then resume from the old offset
    // and silently skip bytes — so failed deletions are retried on every
    // later tick. The retry set itself is bounded by
    // `CURSOR_DELETE_RETRY_LIMIT`: beyond it the oldest entry is evicted
    // with an ERROR log naming that residual risk (see
    // `queue_cursor_deletes`), rather than letting the set grow without
    // bound during a store outage.
    let mut pending_cursor_deletes: VecDeque<PathBuf> = VecDeque::new();

    loop {
        let directory_work = discovery.probe_directories();
        // Files removed from discovery must not leave stale cursors behind:
        // a recreated same-path file would otherwise inherit its
        // predecessor's offset and skip early data.
        let removed_files = discovery.take_removed_files();
        if !removed_files.is_empty() {
            for removed in &removed_files {
                cursors.remove(removed);
            }
            queue_cursor_deletes(&mut pending_cursor_deletes, &removed_files);
        }
        let blocked_cursor_restores = drain_pending_cursor_deletes(
            &runtime,
            &discovery,
            &mut cursors,
            &mut pending_cursor_deletes,
        )
        .await;
        let scheduled = discovery
            .schedule_files()
            .into_iter()
            .filter(|scheduled_file| {
                !blocked_cursor_restores.contains(scheduled_file.path.as_path())
            })
            .collect::<Vec<_>>();
        let total_tracked = discovery.tracked_files();
        let mut files_mirrored: u64 = 0;
        let mut rows_inserted: u64 = 0;

        for scheduled_file in scheduled {
            let Some(root) = discovery
                .files
                .get(&scheduled_file.path)
                .and_then(|file| trusted_root_for_file(&config, &scheduled_file.path, &file.kinds))
            else {
                discovery.record_probe_error(&scheduled_file.path, scheduled_file.was_cold, false);
                tracing::warn!(
                    path = %scheduled_file.path.display(),
                    "session mirror: scheduled file is outside its configured root"
                );
                continue;
            };
            let (metadata, observed_identity, directory_identities) =
                match probe_source_file(root, &scheduled_file.path) {
                    Ok(probe) => probe,
                    Err(e) => {
                        let missing = e.kind() == io::ErrorKind::NotFound;
                        discovery.record_probe_error(
                            &scheduled_file.path,
                            scheduled_file.was_cold,
                            missing,
                        );
                        if !missing {
                            tracing::warn!(
                                path = %scheduled_file.path.display(),
                                error = %e,
                                "session mirror: stat failed"
                            );
                        } else {
                            tracing::debug!(
                                path = %scheduled_file.path.display(),
                                error = %e,
                                "session mirror: file missing during probe"
                            );
                        }
                        continue;
                    }
                };
            if !metadata.is_file() {
                discovery.record_probe_error(&scheduled_file.path, scheduled_file.was_cold, false);
                tracing::warn!(
                    path = %scheduled_file.path.display(),
                    "session mirror: refusing a non-regular file or symlink"
                );
                continue;
            }
            let file_len = metadata.len();
            let modified = metadata.modified().ok();

            let cursor = cursors
                .entry(scheduled_file.path.clone())
                .or_insert_with(|| CursorState {
                    byte_offset: if config.backfill { 0 } else { file_len },
                    file_identity: Some(observed_identity.clone()),
                });
            let previous_offset = cursor.byte_offset;
            let restarted = match reconcile_cursor_identity(
                &runtime,
                &scheduled_file.path,
                cursor,
                &observed_identity,
                file_len,
                config.backfill,
            )
            .await
            {
                Ok(restarted) => restarted,
                Err(error) => {
                    tracing::warn!(
                        path = %scheduled_file.path.display(),
                        error = %error,
                        "session mirror: could not persist legacy cursor identity"
                    );
                    continue;
                }
            };
            if restarted {
                tracing::info!(
                    path = %scheduled_file.path.display(),
                    previous_offset,
                    file_len,
                    "session mirror: file replacement or truncation; restarting at byte zero"
                );
            }
            let offset = cursor.byte_offset;

            if file_len <= offset {
                discovery.record_unchanged(
                    &scheduled_file.path,
                    scheduled_file.was_cold,
                    modified,
                    SystemTime::now(),
                );
                continue;
            }
            discovery.reactivate_file(&scheduled_file.path);

            // Tail line sources or re-read provider exports whole.
            let Some(kinds) = discovery
                .files
                .get(&scheduled_file.path)
                .map(|file| file.kinds.clone())
            else {
                continue;
            };
            let mut candidate_dispatch = CandidateDispatch::default();
            let mut ended_by_inserting = false;
            let trusted_source = ingest::TrustedSource {
                root,
                directory_identities: &directory_identities,
            };
            for kind in kinds {
                let result = match kind {
                    DiscoveredKind::LineTail { source, session_id } => {
                        // Deferred variant: an empty advance (bytes consumed,
                        // zero rows) does NOT commit its cursor inline, so the
                        // commit cannot race ahead of a later candidate that
                        // would parse the same span, and an interrupt between
                        // candidates cannot strand a committed cursor past
                        // uninserted rows. The commit happens below, only when
                        // dispatch ends without an inserting candidate.
                        ingest::mirror_file_deferred_checked(
                            &runtime,
                            &scheduled_file.path,
                            offset,
                            source,
                            session_id.as_deref(),
                            Some(&observed_identity),
                            Some(trusted_source),
                        )
                        .await
                    }
                    DiscoveredKind::ChatGptExport => {
                        ingest::mirror_chatgpt_export_file_checked(
                            &runtime,
                            &scheduled_file.path,
                            offset,
                            &observed_identity,
                            trusted_source,
                        )
                        .await
                    }
                    DiscoveredKind::ClaudeAiExport => {
                        ingest::mirror_claude_ai_export_file_checked(
                            &runtime,
                            &scheduled_file.path,
                            offset,
                            &observed_identity,
                            trusted_source,
                        )
                        .await
                    }
                };
                if candidate_dispatch.record(result, offset) {
                    ended_by_inserting = true;
                    break;
                }
            }

            let CandidateDispatch { stats, errors } = candidate_dispatch;
            let had_errors = !errors.is_empty();
            for error in errors {
                tracing::warn!(
                    path = %scheduled_file.path.display(),
                    error = %error,
                    "session mirror: per-file source candidate error"
                );
            }

            let (stats, had_errors) = finalize_dispatch_stats(
                &runtime,
                &scheduled_file.path,
                offset,
                ended_by_inserting,
                stats,
                had_errors,
            )
            .await;

            // A successful advance ends the error streak; a tick on which
            // no candidate advanced and at least one errored grows it toward
            // demotion. A no-progress success from one candidate does not
            // make the file healthy while another candidate errors: without
            // this, a misconfigured file with one capped-out provider and
            // one broken provider would stay hot forever.
            let advanced = stats
                .as_ref()
                .is_some_and(|stats| stats.new_offset > offset);
            if tally_dispatch_errors(&mut discovery, &scheduled_file.path, advanced, had_errors) {
                tracing::warn!(
                    path = %scheduled_file.path.display(),
                    consecutive_error_polls = FILE_ERROR_POLLS_BEFORE_COLD,
                    "session mirror: demoting persistently erroring file to cold"
                );
            }

            if let Some(stats) = stats {
                // `CandidateDispatch::record` already rejects regressing
                // cursors; guard the write side too so the stored offset can
                // only advance or hold.
                if stats.new_offset >= offset {
                    cursors.insert(
                        scheduled_file.path.clone(),
                        CursorState {
                            byte_offset: stats.new_offset,
                            file_identity: stats.file_identity,
                        },
                    );
                }
                if stats.inserted > 0 || stats.new_offset > offset {
                    files_mirrored += 1;
                    rows_inserted += stats.inserted;
                    tracing::debug!(
                        path = %scheduled_file.path.display(),
                        inserted = stats.inserted,
                        scanned = stats.scanned,
                        new_offset = stats.new_offset,
                        "session mirror: tailed file"
                    );
                }
            }
        }

        if files_mirrored > 0 || rows_inserted > 0 {
            tracing::info!(
                files_mirrored,
                rows_inserted,
                total_tracked,
                directory_metadata_probes = directory_work.metadata_probes,
                directory_walks = directory_work.walks,
                "session mirror tick"
            );
        } else {
            tracing::debug!(
                total_tracked,
                directory_metadata_probes = directory_work.metadata_probes,
                directory_walks = directory_work.walks,
                "session mirror: quiet tick"
            );
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

/// Load persisted offsets and file identities from `session_mirror_cursor`.
///
/// Missing table (e.g. schema not yet applied) returns an empty map rather
/// than an error — the service self-bootstraps on the first successful write.
async fn load_cursors(
    runtime: &KhiveRuntime,
) -> Result<HashMap<PathBuf, CursorState>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: cursor reader: {e}")))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT file_path, byte_offset, file_identity FROM session_mirror_cursor".into(),
            params: vec![],
            label: Some("mirror_load_cursors".into()),
        })
        .await;

    match rows {
        Err(e) => {
            // Table may not exist yet (schema applied lazily at first warm tick).
            tracing::debug!(error = %e, "mirror: cursor table not yet available");
            Ok(HashMap::new())
        }
        Ok(rows) => {
            let mut map = HashMap::with_capacity(rows.len());
            for row in rows {
                let file_path = match row.get("file_path") {
                    Some(SqlValue::Text(s)) => PathBuf::from(s),
                    _ => continue,
                };
                let byte_offset = match row.get("byte_offset") {
                    Some(SqlValue::Integer(n)) => *n as u64,
                    _ => 0,
                };
                let file_identity = match row.get("file_identity") {
                    Some(SqlValue::Text(identity)) => Some(identity.clone()),
                    _ => None,
                };
                map.insert(
                    file_path,
                    CursorState {
                        byte_offset,
                        file_identity,
                    },
                );
            }
            Ok(map)
        }
    }
}

/// Delete persisted cursor rows for files that left discovery.
///
/// A recreated same-path file must start from a fresh cursor instead of
/// inheriting its predecessor's offset (and skipping early data). The caller
/// retries failures via the pending-delete set: the in-memory offset map is
/// already pruned, so an *unretried* failure here is worse than wasted work —
/// `load_cursors` reloads the stale row on the next daemon restart, and a
/// recreated same-path file then resumes from the old offset and silently
/// skips the bytes written in between. Idempotent `INSERT OR IGNORE` does
/// not cover this case because the skipped bytes are never read at all.
///
/// Returns the paths whose DELETE failed (empty on full success); `Err`
/// only when the writer cannot be acquired at all. A failing path must not
/// block the rest of the batch — head-of-line blocking would let one bad
/// row stall every later deletion, and the failed paths are retried on
/// later ticks via the pending-delete set either way.
///
/// The DELETE keys on `path.to_string_lossy()` — the same lossy text the
/// ingest cursor upserts (`upsert_cursor_on_writer`, `write_cursor_only`)
/// use as the row key — so a delete always targets the row the inserts
/// wrote, even for non-UTF-8 paths.
async fn delete_cursors(
    runtime: &KhiveRuntime,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: cursor writer: {e}")))?;

    let mut failed = Vec::new();
    for path in paths {
        if let Err(error) = writer
            .execute(SqlStatement {
                sql: "DELETE FROM session_mirror_cursor WHERE file_path=?1".into(),
                params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
                label: Some("mirror_cursor_delete".into()),
            })
            .await
        {
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "session mirror: cursor delete failed for one path; continuing batch"
            );
            failed.push(path.clone());
        }
    }
    Ok(failed)
}

/// Add removed paths to the pending cursor-delete set, deduplicating and
/// evicting (ERROR-logged) beyond `CURSOR_DELETE_RETRY_LIMIT` so an outage
/// cannot grow the set without bound. Eviction is the escape valve, not a
/// neutral log: an evicted path's stale cursor row can survive a daemon
/// restart and skip bytes if the path is recreated — the error names that
/// consequence.
fn queue_cursor_deletes(pending: &mut VecDeque<PathBuf>, removed: &[PathBuf]) {
    for path in removed {
        if pending.contains(path) {
            continue;
        }
        if pending.len() >= CURSOR_DELETE_RETRY_LIMIT {
            let dropped = pending.pop_front();
            tracing::error!(
                dropped = %dropped.map(|p| p.display().to_string()).unwrap_or_default(),
                limit = CURSOR_DELETE_RETRY_LIMIT,
                "session mirror: cursor-delete retry set full; evicting oldest entry — \
                 its stale cursor row can survive a daemon restart and skip bytes if \
                 the path is recreated"
            );
        }
        pending.push_back(path.clone());
    }
}

/// Drain the pending cursor-delete set against the store. A path re-tracked
/// since its removal seeds a fresh in-memory offset and rewrites its cursor
/// row on the next successful ingest, so its pending delete is cancelled
/// instead of removing the fresh row out from under it. When the removal
/// already dropped the in-memory offset, the cancel restores it from the
/// preserved cursor row — otherwise the next seed falls back to `file_len`
/// (`backfill=false`) and silently skips bytes the preserved row proves
/// were already mirrored. A failed restore keeps the pending entry and
/// returns the path in the blocked set so this tick cannot seed or schedule
/// it; the cursor row remains authoritative until a successful read restores
/// the offset. Paths deleted before a failure are retried independently.
async fn drain_pending_cursor_deletes(
    runtime: &KhiveRuntime,
    discovery: &DiscoveryIndex,
    cursors: &mut HashMap<PathBuf, CursorState>,
    pending: &mut VecDeque<PathBuf>,
) -> HashSet<PathBuf> {
    let mut blocked = HashSet::new();
    if pending.is_empty() {
        return blocked;
    }
    let mut cancelled = Vec::new();
    pending.retain(|path| {
        if discovery.files.contains_key(path) {
            cancelled.push(path.clone());
            false
        } else {
            true
        }
    });
    let mut restore_failed = Vec::new();
    for path in &cancelled {
        if cursors.contains_key(path) {
            continue;
        }
        match read_cursor_state(runtime, path).await {
            Ok(Some(cursor)) => {
                cursors.insert(path.clone(), cursor);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "session mirror: failed to restore cancelled cursor; retrying before scheduling"
                );
                restore_failed.push(path.clone());
                blocked.insert(path.clone());
            }
        }
    }
    // A failed restore remains pending, but must not be sent through the
    // delete batch while the path is tracked: its preserved row is still
    // authoritative, and scheduling it with no offset would seed at EOF.
    pending.extend(restore_failed);
    if pending.is_empty() {
        return blocked;
    }
    let paths: Vec<PathBuf> = pending
        .iter()
        .filter(|path| !blocked.contains(path.as_path()))
        .cloned()
        .collect();
    if paths.is_empty() {
        return blocked;
    }
    let blocked_pending = blocked.iter().cloned().collect::<VecDeque<_>>();
    match delete_cursors(runtime, &paths).await {
        Ok(failed) if failed.is_empty() => *pending = blocked_pending,
        Ok(failed) => {
            let mut remaining = blocked_pending;
            remaining.extend(failed.iter().cloned());
            tracing::warn!(
                failed = failed.len(),
                remaining = remaining.len(),
                "session mirror: cursor cleanup partially failed; retrying failed paths next tick"
            );
            *pending = remaining;
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                remaining = pending.len(),
                "session mirror: cursor cleanup failed before any delete; retrying next tick"
            );
        }
    }
    blocked
}

/// Read one persisted cursor. `Ok(None)` means the query succeeded but
/// no row exists; an acquisition or query failure is returned so a cancelled
/// delete can remain pending instead of allowing an EOF seed.
async fn read_cursor_state(
    runtime: &KhiveRuntime,
    path: &Path,
) -> Result<Option<CursorState>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset, file_identity FROM session_mirror_cursor WHERE file_path=?1"
                .into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: Some("mirror_cursor_read".into()),
        })
        .await?;
    Ok(rows.first().map(|row| CursorState {
        byte_offset: match row.get("byte_offset") {
            Some(SqlValue::Integer(offset)) => *offset as u64,
            _ => 0,
        },
        file_identity: match row.get("file_identity") {
            Some(SqlValue::Text(identity)) => Some(identity.clone()),
            _ => None,
        },
    }))
}

/// Extract the session UUID from a Codex filename of the form
/// `rollout-<timestamp>-<uuid>.jsonl`.
///
/// Returns `None` for files whose name does not match the expected pattern or
/// whose derived candidate is not a valid UUID.  Files whose stem looks like
/// `rollout-2025-11-11T08-32-36` (no UUID suffix) are rejected here and
/// silently skipped by the caller.
fn extract_codex_session_id(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if !stem.starts_with("rollout-") {
        return None;
    }
    // A standard UUID has 5 hyphen-delimited groups (8-4-4-4-12).
    // Split the stem and take the last 5 segments as the UUID candidate.
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 6 {
        return None;
    }
    let candidate = parts[parts.len() - 5..].join("-");
    // Validate structurally: reject timestamp-shaped junk like "2025-11-11T08-32-36"
    // that also happens to have 4 hyphens.  uuid::Uuid::parse_str enforces the
    // 8-4-4-4-12 hex-character layout.
    match uuid::Uuid::parse_str(&candidate) {
        Ok(_) => Some(candidate),
        Err(_) => {
            tracing::debug!(
                path = %path.display(),
                candidate,
                "session mirror: codex filename did not yield a valid UUID — skipping"
            );
            None
        }
    }
}

#[cfg(test)]
#[path = "service_discovery_tests.rs"]
mod discovery_tests;

#[cfg(test)]
#[path = "service_cursor_retry_tests.rs"]
mod cursor_retry_tests;

#[cfg(test)]
#[path = "service_codex_filename_tests.rs"]
mod codex_filename_tests;
