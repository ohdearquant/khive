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

        let enabled = std::env::var("KHIVE_MIRROR_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let projects_dir = std::env::var("KHIVE_MIRROR_PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".claude").join("projects"));

        let codex_enabled = std::env::var("KHIVE_MIRROR_CODEX_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let codex_sessions_dir = std::env::var("KHIVE_MIRROR_CODEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".codex").join("sessions"));

        let chatgpt_enabled = std::env::var("KHIVE_MIRROR_CHATGPT_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let chatgpt_exports_dir = std::env::var("KHIVE_MIRROR_CHATGPT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".chatgpt").join("exports"));

        let claude_ai_enabled = std::env::var("KHIVE_MIRROR_CLAUDE_AI_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

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
mod config_tests {
    use super::{parse_mirror_poll_secs, DirectoryKind, DiscoveredKind, DiscoveryIndex};

    /// Regression for PACKSESSION-AUD-002: `KHIVE_MIRROR_POLL_SECS=0` used to
    /// produce a hot polling loop via `Duration::from_secs(0)`. Explicit zero
    /// must now be rejected back to the documented default, and the default
    /// must remain distinguishable from a non-numeric value.
    #[test]
    fn poll_secs_zero_is_rejected_and_default_remains_two_seconds() {
        assert_eq!(
            parse_mirror_poll_secs(None),
            2,
            "missing value defaults to 2s"
        );
        assert_eq!(
            parse_mirror_poll_secs(Some("abc")),
            2,
            "non-numeric value defaults to 2s"
        );
        assert_eq!(
            parse_mirror_poll_secs(Some("0")),
            2,
            "explicit zero must be rejected back to the default, not accepted as a hot loop"
        );
        assert_eq!(
            parse_mirror_poll_secs(Some("1")),
            1,
            "valid nonzero value is honored"
        );
        assert_eq!(
            parse_mirror_poll_secs(Some("5")),
            5,
            "valid nonzero value is honored"
        );
    }

    #[test]
    fn claude_ai_scanner_accepts_only_nested_conversations_json() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let nested = temp.path().join("download").join("claude-export");
        std::fs::create_dir_all(&nested).expect("nested export dir");
        let export = nested.join("conversations.json");
        std::fs::write(&export, "[]").expect("export fixture");
        std::fs::write(nested.join("conversations-copy.json"), "[]").expect("non-matching fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeAiExport, true);
        assert_eq!(discovery.files.len(), 1);
        assert!(matches!(
            discovery
                .files
                .get(&export)
                .and_then(|file| file.kinds.first()),
            Some(&DiscoveredKind::ClaudeAiExport)
        ));
    }

    #[test]
    fn overlapping_export_roots_retain_both_parser_candidates() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let export = temp.path().join("conversations.json");
        std::fs::write(&export, "[]").expect("export fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(temp.path(), DirectoryKind::ChatGptExport, true);
        discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeAiExport, true);
        discovery.probe_directories();

        let kinds = &discovery.files.get(&export).expect("tracked export").kinds;
        assert_eq!(kinds.len(), 2);
        assert!(matches!(
            kinds.first(),
            Some(&DiscoveredKind::ChatGptExport)
        ));
        assert!(matches!(
            kinds.get(1),
            Some(&DiscoveredKind::ClaudeAiExport)
        ));
    }
}

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
    /// errored.
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

            let metadata = match std::fs::metadata(&path) {
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

            match std::fs::metadata(&path) {
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
/// vetoes the cursor commit when any candidate errored, and a commit failure
/// itself becomes an error poll so persistent failures reach cold cadence.
async fn finalize_dispatch_stats(
    runtime: &KhiveRuntime,
    path: &Path,
    offset: u64,
    ended_by_inserting: bool,
    stats: Option<ingest::MirrorStats>,
    mut had_errors: bool,
) -> (Option<ingest::MirrorStats>, bool) {
    // Commit a deferred empty advance only when dispatch ended with no
    // inserting candidate AND no candidate error. An erroring candidate might
    // have parsed the span had it succeeded, so the cursor stays at the old
    // offset and a later pass re-reads the bytes (bounded and idempotent)
    // rather than skipping them. On commit failure the in-memory offset is
    // likewise NOT applied.
    let stats = match stats {
        Some(stats) if !ended_by_inserting && stats.inserted == 0 && stats.new_offset > offset => {
            if had_errors {
                tracing::debug!(
                    path = %path.display(),
                    new_offset = stats.new_offset,
                    "session mirror: deferring empty-advance cursor commit because a \
                     candidate errored; the span will be re-read on a later pass"
                );
                None
            } else {
                match ingest::commit_empty_advance(runtime, path, stats.new_offset).await {
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
    // `is_directory` comes from `DirEntry::file_type()`, which does not
    // follow symlinks: a symlinked directory arrives here as NOT a
    // directory and is never queued for traversal by `add_directory_tree`
    // or `refresh_directory`, so discovery cannot loop on symlink cycles.
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

    // Seed in-memory offsets from the persisted cursor table.
    let mut offsets: HashMap<PathBuf, u64> = match load_cursors(&runtime).await {
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
                offsets.remove(removed);
            }
            queue_cursor_deletes(&mut pending_cursor_deletes, &removed_files);
        }
        let blocked_cursor_restores = drain_pending_cursor_deletes(
            &runtime,
            &discovery,
            &mut offsets,
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
            let metadata = match std::fs::metadata(&scheduled_file.path) {
                Ok(metadata) => metadata,
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
            let file_len = metadata.len();
            let modified = metadata.modified().ok();

            let offset = *offsets
                .entry(scheduled_file.path.clone())
                .or_insert(if config.backfill { 0 } else { file_len });

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
                        ingest::mirror_file_deferred(
                            &runtime,
                            &scheduled_file.path,
                            offset,
                            source,
                            session_id.as_deref(),
                        )
                        .await
                    }
                    DiscoveredKind::ChatGptExport => {
                        ingest::mirror_chatgpt_export_file(&runtime, &scheduled_file.path, offset)
                            .await
                    }
                    DiscoveredKind::ClaudeAiExport => {
                        ingest::mirror_claude_ai_export_file(&runtime, &scheduled_file.path, offset)
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
                    offsets.insert(scheduled_file.path.clone(), stats.new_offset);
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

/// Load persisted `(file_path, byte_offset)` pairs from `session_mirror_cursor`.
///
/// Missing table (e.g. schema not yet applied) returns an empty map rather
/// than an error — the service self-bootstraps on the first successful write.
async fn load_cursors(runtime: &KhiveRuntime) -> Result<HashMap<PathBuf, u64>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: cursor reader: {e}")))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT file_path, byte_offset FROM session_mirror_cursor".into(),
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
                map.insert(file_path, byte_offset);
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
    offsets: &mut HashMap<PathBuf, u64>,
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
        if offsets.contains_key(path) {
            continue;
        }
        match read_cursor_offset(runtime, path).await {
            Ok(Some(offset)) => {
                offsets.insert(path.clone(), offset);
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

/// Read one persisted cursor offset. `Ok(None)` means the query succeeded but
/// no row exists; an acquisition or query failure is returned so a cancelled
/// delete can remain pending instead of allowing an EOF seed.
async fn read_cursor_offset(
    runtime: &KhiveRuntime,
    path: &Path,
) -> Result<Option<u64>, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql.reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path=?1".into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: Some("mirror_cursor_read".into()),
        })
        .await?;
    Ok(match rows.first().and_then(|row| row.get("byte_offset")) {
        Some(SqlValue::Integer(offset)) => Some(*offset as u64),
        _ => None,
    })
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
mod discovery_tests {
    use super::{
        should_mark_cold, CandidateDispatch, DirectoryKind, DiscoveredKind, DiscoveryIndex,
        TrackedDirectory, COLD_FILE_PROBES_PER_TICK, COLD_STALE_POPS_PER_TICK,
        DIRECTORY_FORCE_RESCAN_PROBES, DIRECTORY_PROBES_PER_TICK,
        DIRECTORY_REFRESH_FAILURES_BEFORE_WARN, FILE_COLD_AGE, FILE_ERROR_POLLS_BEFORE_COLD,
        FILE_UNCHANGED_POLLS_BEFORE_COLD, FILE_UNCHANGED_POLLS_WITHOUT_MTIME,
    };
    use crate::mirror::ingest::MirrorStats;
    use std::collections::HashSet;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn mark_cold(discovery: &mut DiscoveryIndex, path: &Path) {
        let file = discovery.files.get_mut(path).expect("tracked file");
        file.cold = true;
        file.unchanged_polls = FILE_UNCHANGED_POLLS_BEFORE_COLD;
        discovery.hot_files.remove(path);
        discovery.enqueue_cold(path);
    }

    #[test]
    fn cold_scheduler_has_a_fixed_per_tick_file_probe_ceiling() {
        let mut discovery = DiscoveryIndex::default();
        let count = COLD_FILE_PROBES_PER_TICK * 8;
        for index in 0..count {
            let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
            discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
            mark_cold(&mut discovery, &path);
        }

        let scheduled = discovery.schedule_files();
        assert_eq!(scheduled.len(), COLD_FILE_PROBES_PER_TICK);
        assert!(scheduled.iter().all(|file| file.was_cold));
        assert_eq!(discovery.tracked_files(), count);
    }

    #[test]
    fn divergent_provider_limit_no_progress_does_not_block_next_candidate() {
        let start_offset = 41;
        let mut dispatch = CandidateDispatch::default();

        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset,
            }),
            start_offset,
        ));
        assert!(dispatch.record(
            Ok(MirrorStats {
                inserted: 2,
                scanned: 2,
                new_offset: 97,
            }),
            start_offset,
        ));

        let stats = dispatch.stats.expect("progressing provider candidate");
        assert_eq!(stats.new_offset, 97);
        assert_eq!(stats.inserted, 2);
    }

    #[test]
    fn directory_scheduler_has_a_fixed_per_tick_metadata_ceiling() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let mut discovery = DiscoveryIndex::default();
        for index in 0..DIRECTORY_PROBES_PER_TICK * 4 {
            discovery.add_directory_tree(
                &temp.path().join(format!("missing-{index}")),
                DirectoryKind::Codex,
                true,
            );
        }

        let stats = discovery.probe_directories();
        assert_eq!(stats.metadata_probes, DIRECTORY_PROBES_PER_TICK);
        assert_eq!(stats.walks, 0);
    }

    #[test]
    fn parent_mtime_change_prioritizes_a_cold_growing_file_in_that_cycle() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let project = temp.path().join("project-slug");
        std::fs::create_dir(&project).expect("project dir");
        let transcript = project.join("session.jsonl");
        std::fs::write(&transcript, "first\n").expect("transcript fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeCodeRoot, true);
        mark_cold(&mut discovery, &transcript);

        OpenOptions::new()
            .append(true)
            .open(&transcript)
            .expect("open transcript")
            .write_all(b"second\n")
            .expect("grow transcript");
        discovery
            .directories
            .get_mut(&project)
            .expect("tracked project directory")
            .fingerprint = None;

        let stats = discovery.probe_directories();
        assert_eq!(stats.walks, 1);
        assert!(!discovery.hot_files.contains(&transcript));
        assert!(discovery
            .schedule_files()
            .iter()
            .any(|file| file.path == transcript && file.was_cold));
    }

    #[test]
    fn cold_round_robin_is_a_fallback_when_append_does_not_change_parent_mtime() {
        let mut discovery = DiscoveryIndex::default();
        let count = COLD_FILE_PROBES_PER_TICK * 3 + 1;
        let target = PathBuf::from(format!("/cold/session-{}.jsonl", count - 1));

        for index in 0..count {
            let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
            discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
            mark_cold(&mut discovery, &path);
        }

        let mut found_on_cycle = None;
        for cycle in 1..=4 {
            let scheduled = discovery.schedule_files();
            if scheduled.iter().any(|file| file.path == target) {
                found_on_cycle = Some(cycle);
                break;
            }
            for file in scheduled {
                discovery.record_unchanged(
                    &file.path,
                    file.was_cold,
                    Some(UNIX_EPOCH),
                    SystemTime::now(),
                );
            }
        }

        assert_eq!(found_on_cycle, Some(4));
    }

    #[test]
    fn missing_configured_root_stays_tracked_until_it_appears() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let root = temp.path().join("codex-sessions");
        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(&root, DirectoryKind::Codex, true);

        discovery.probe_directories();
        assert!(discovery.directories.contains_key(&root));

        let day = root.join("2026/08/02");
        std::fs::create_dir_all(&day).expect("date tree");
        let transcript =
            day.join("rollout-2026-08-02T12-00-00-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
        std::fs::write(&transcript, "{}\n").expect("codex transcript");

        discovery.probe_directories();
        assert!(discovery.files.contains_key(&transcript));
        assert!(discovery.hot_files.contains(&transcript));
    }

    #[test]
    fn nested_configured_root_keeps_identity_across_ancestor_remove_and_recreate() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let ancestor = temp.path().join("exports");
        let nested = ancestor.join("claude-ai");
        std::fs::create_dir_all(&nested).expect("nested export root");
        let export = nested.join("conversations.json");
        std::fs::write(&export, "[]").expect("export fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(&ancestor, DirectoryKind::ChatGptExport, true);
        discovery.add_directory_tree(&nested, DirectoryKind::ClaudeAiExport, true);

        std::fs::remove_dir_all(&nested).expect("remove nested configured root");
        discovery
            .directories
            .get_mut(&ancestor)
            .expect("tracked ancestor")
            .fingerprint = None;
        discovery.probe_directories();

        let missing_root = discovery
            .directories
            .get(&nested)
            .expect("configured nested root remains scheduled");
        assert!(missing_root.pinned);
        assert!(missing_root.kinds.contains(&DirectoryKind::ClaudeAiExport));

        std::fs::create_dir_all(&nested).expect("recreate nested configured root");
        std::fs::write(&export, "[]").expect("recreated export fixture");
        discovery
            .directories
            .get_mut(&ancestor)
            .expect("tracked ancestor")
            .fingerprint = None;
        discovery.probe_directories();

        let kinds = &discovery
            .files
            .get(&export)
            .expect("recreated export discovered")
            .kinds;
        assert!(kinds.contains(&DiscoveredKind::ClaudeAiExport));
    }

    #[test]
    fn old_unchanged_files_become_cold_only_after_the_threshold() {
        let now = SystemTime::now();
        let old = now.checked_sub(FILE_COLD_AGE).expect("old timestamp");
        assert!(!should_mark_cold(
            FILE_UNCHANGED_POLLS_BEFORE_COLD - 1,
            Some(old),
            now
        ));
        assert!(should_mark_cold(
            FILE_UNCHANGED_POLLS_BEFORE_COLD,
            Some(old),
            now
        ));
        assert!(!should_mark_cold(
            FILE_UNCHANGED_POLLS_BEFORE_COLD,
            Some(now),
            now
        ));
    }

    #[test]
    fn future_mtime_falls_back_to_unchanged_poll_threshold() {
        let now = SystemTime::now();
        let future = now
            .checked_add(Duration::from_secs(3600))
            .expect("future timestamp");
        assert!(
            !should_mark_cold(FILE_UNCHANGED_POLLS_WITHOUT_MTIME - 1, Some(future), now),
            "below the no-mtime threshold an unusable mtime must not mark cold"
        );
        assert!(
            should_mark_cold(FILE_UNCHANGED_POLLS_WITHOUT_MTIME, Some(future), now),
            "a future-dated mtime must fall back to the unchanged-poll threshold"
        );
    }

    #[test]
    fn regressing_candidate_offset_is_never_recorded() {
        let start_offset = 100;
        let mut dispatch = CandidateDispatch::default();

        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 4,
                scanned: 4,
                new_offset: start_offset - 60,
            }),
            start_offset,
        ));
        assert!(
            dispatch.stats.is_none(),
            "a regressing cursor must not be recorded"
        );

        assert!(dispatch.record(
            Ok(MirrorStats {
                inserted: 1,
                scanned: 1,
                new_offset: start_offset + 50,
            }),
            start_offset,
        ));
        assert_eq!(
            dispatch
                .stats
                .expect("advancing candidate recorded")
                .new_offset,
            150
        );
    }

    #[test]
    fn remove_file_clears_scheduler_state_and_readd_starts_clean() {
        let mut discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/cold/session.jsonl");
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
        assert!(discovery.cold_enqueued.contains(&path));

        discovery.remove_file(&path, false);
        // Queue residue is invalidated lazily: the entry may still sit in
        // the cold queue, but it must never schedule again, and the enqueue
        // flags are cleared so a re-add starts clean.
        assert!(!discovery.cold_enqueued.contains(&path));
        assert!(!discovery.priority_cold_enqueued.contains(&path));
        assert!(discovery
            .schedule_files()
            .iter()
            .all(|file| file.path != path));

        // A re-added path is scheduled on the next tick instead of being
        // shadowed by leftover queue state.
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
        assert!(discovery
            .schedule_files()
            .iter()
            .any(|file| file.path == path && file.was_cold));
    }

    #[test]
    fn remove_directory_tree_clears_directory_schedule_state() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let root = temp.path().join("projects");
        std::fs::create_dir_all(&root).expect("root dir");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(&root, DirectoryKind::ClaudeCodeRoot, false);
        assert!(discovery.directory_enqueued.contains(&root));

        discovery.remove_directory_tree(&root, false);
        // Queue residue is invalidated lazily: the entry may still sit in
        // the queue, but an untracked path is skipped on pop, and the
        // enqueue flag stays set until that pop so a re-add cannot enqueue
        // a duplicate.
        assert!(discovery.directory_enqueued.contains(&root));

        // A re-added directory is probed exactly once on the next tick
        // instead of twice (once per queue entry).
        discovery.add_directory_tree(&root, DirectoryKind::ClaudeCodeRoot, false);
        let stats = discovery.probe_directories();
        assert_eq!(stats.metadata_probes, 1);
    }

    #[test]
    fn removed_files_are_reported_once_for_cursor_cleanup() {
        let mut discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/exports/conversations.json");
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

        discovery.remove_file(&path, false);
        assert_eq!(discovery.take_removed_files(), vec![path.clone()]);
        assert!(discovery.take_removed_files().is_empty());

        // A pinned file that survives removal is not reported as removed.
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, true);
        discovery.remove_file(&path, true);
        assert!(discovery.files.contains_key(&path));
        assert!(discovery.take_removed_files().is_empty());
    }

    #[test]
    fn pinned_missing_file_stays_tracked_and_hot_for_retry() {
        let mut discovery = DiscoveryIndex::default();
        let pinned = PathBuf::from("/exports/conversations.json");
        discovery.add_file(pinned.clone(), DiscoveredKind::ChatGptExport, true);

        discovery.record_probe_error(&pinned, false, true);
        assert!(discovery.files.contains_key(&pinned));
        assert!(discovery.hot_files.contains(&pinned));
        assert!(discovery
            .schedule_files()
            .iter()
            .any(|file| file.path == pinned && !file.was_cold));
    }

    #[test]
    fn transient_not_found_gets_one_grace_probe_before_removal() {
        let mut discovery = DiscoveryIndex::default();
        let transient = PathBuf::from("/projects/gone.jsonl");
        discovery.add_file(transient.clone(), DiscoveredKind::ChatGptExport, false);

        // One NotFound (an atomic replace or FS hiccup) keeps the file and
        // its cursor so a promptly recreated path resumes from its old
        // offset instead of being reseeded to EOF.
        discovery.record_probe_error(&transient, false, true);
        assert!(discovery.files.contains_key(&transient));
        assert!(discovery.hot_files.contains(&transient));
        assert!(discovery.take_removed_files().is_empty());

        // A successful probe in between resets the streak entirely.
        discovery.record_unchanged(&transient, false, None, SystemTime::now());
        discovery.record_probe_error(&transient, false, true);
        assert!(discovery.files.contains_key(&transient));

        // A second consecutive NotFound removes the file and queues cursor
        // cleanup.
        discovery.record_probe_error(&transient, false, true);
        assert!(!discovery.files.contains_key(&transient));
        assert!(!discovery.hot_files.contains(&transient));
        assert_eq!(discovery.take_removed_files(), vec![transient.clone()]);
    }

    #[test]
    fn stale_cold_queue_residue_is_pop_bounded_and_hot_files_still_scheduled() {
        let mut discovery = DiscoveryIndex::default();

        // A large backlog of stale entries at the queue head: files that
        // were cold, enqueued, then reactivated (e.g. growth noticed via a
        // directory priority sweep) — their queue entries are invalidated
        // lazily instead of by an O(queue) purge on reactivation.
        let stale_count = COLD_STALE_POPS_PER_TICK * 5;
        for index in 0..stale_count {
            let path = PathBuf::from(format!("/stale/session-{index}.jsonl"));
            discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
            mark_cold(&mut discovery, &path);
            discovery.reactivate_file(&path);
        }
        // Fresh cold files behind the residue.
        let fresh_count = COLD_FILE_PROBES_PER_TICK * 3;
        let first_fresh = PathBuf::from("/cold/session-0.jsonl");
        for index in 0..fresh_count {
            let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
            discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
            mark_cold(&mut discovery, &path);
        }
        // A hot file must still be scheduled in the same tick.
        let hot = PathBuf::from("/hot/session.jsonl");
        discovery.add_file(hot.clone(), DiscoveredKind::ChatGptExport, false);

        let queue_before = discovery.cold_queue.len();
        assert_eq!(queue_before, stale_count + fresh_count);

        let mut scheduled = discovery.schedule_files();
        assert!(
            scheduled
                .iter()
                .any(|file| file.path == hot && !file.was_cold),
            "hot scheduling is unaffected by the residue backlog"
        );
        // One tick cannot drain the arbitrary stale backlog: with residue
        // at the queue head, total pops are bounded by the stale-pop budget
        // plus the productive probe budget.
        let drained = queue_before - discovery.cold_queue.len();
        assert!(drained <= COLD_STALE_POPS_PER_TICK + COLD_FILE_PROBES_PER_TICK);
        assert!(
            discovery.cold_queue.len() > fresh_count / 2,
            "most of the backlog survives one tick"
        );

        // Bounded lag, no starvation: the residue drains at the stale-pop
        // rate and the fresh cold files are scheduled once it clears.
        let mut ticks = 1;
        while !scheduled.iter().any(|file| file.path == first_fresh) {
            scheduled = discovery.schedule_files();
            ticks += 1;
            assert!(
                ticks <= (stale_count / COLD_STALE_POPS_PER_TICK) + 2,
                "stale residue must drain at the bounded per-tick rate"
            );
        }
        assert!(
            scheduled.iter().filter(|file| file.was_cold).count() <= COLD_FILE_PROBES_PER_TICK,
            "the productive metadata-probe ceiling holds on the catch-up tick"
        );
    }

    #[test]
    fn persistently_erroring_hot_file_is_demoted_to_cold_and_recovers() {
        let mut discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/broken/session.jsonl");
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        assert!(discovery.hot_files.contains(&path));

        // Below the threshold the file stays hot.
        for _tick in 1..FILE_ERROR_POLLS_BEFORE_COLD {
            assert!(!discovery.record_error_poll(&path));
            assert!(discovery.hot_files.contains(&path));
            assert!(!discovery.files[&path].cold);
        }
        // Crossing the threshold demotes to cold exactly once (the one-shot
        // warn tick) and enqueues it for the ordinary cold cadence.
        assert!(discovery.record_error_poll(&path));
        assert!(!discovery.hot_files.contains(&path));
        assert!(discovery.files[&path].cold);
        assert!(discovery.cold_enqueued.contains(&path));

        // Further error ticks keep it cold without re-crossing.
        assert!(!discovery.record_error_poll(&path));

        // Recovery reactivates through the normal hot path and resets the
        // streak.
        discovery.reactivate_file(&path);
        assert!(discovery.hot_files.contains(&path));
        assert!(!discovery.files[&path].cold);
        for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
            assert!(!discovery.record_error_poll(&path));
        }
        assert!(discovery.record_error_poll(&path));
    }

    #[test]
    fn empty_advance_candidate_does_not_mask_inserting_provider() {
        let start_offset = 10;
        let mut dispatch = CandidateDispatch::default();

        // A wrong provider can advance the cursor while inserting nothing
        // (a cursor-only pass over unparseable lines); that must not end
        // dispatch and hide the provider that parses the same bytes.
        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset + 40,
            }),
            start_offset,
        ));
        assert!(dispatch.record(
            Ok(MirrorStats {
                inserted: 3,
                scanned: 3,
                new_offset: start_offset + 40,
            }),
            start_offset,
        ));
        let stats = dispatch.stats.expect("inserting provider recorded");
        assert_eq!(stats.inserted, 3);
        assert_eq!(stats.new_offset, start_offset + 40);
    }

    /// Regression for the recording-precedence finding: a no-progress
    /// candidate recorded first must not hide a later empty advance, or the
    /// in-memory offset stays behind a durably committed cursor.
    #[test]
    fn later_empty_advance_replaces_earlier_no_progress_record() {
        let start_offset = 100;
        let mut dispatch = CandidateDispatch::default();

        // Candidate A: no progress (whole-file ceiling below the current
        // length). Recorded only because nothing else is recorded yet.
        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset,
            }),
            start_offset,
        ));

        // Candidate B: empty advance past start_offset. Must replace A's
        // non-advancing record so the in-memory offset follows the cursor.
        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset + 64,
            }),
            start_offset,
        ));

        let stats = dispatch.stats.expect("empty advance recorded");
        assert_eq!(
            stats.new_offset,
            start_offset + 64,
            "the advancing result wins over the earlier no-progress record"
        );
        assert_eq!(stats.inserted, 0);
    }

    /// An advancing record is kept against a later empty advance (first
    /// advancing record wins; an inserting candidate ends dispatch anyway).
    #[test]
    fn first_advancing_record_is_kept_against_later_empty_advance() {
        let start_offset = 50;
        let mut dispatch = CandidateDispatch::default();

        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset + 30,
            }),
            start_offset,
        ));
        assert!(!dispatch.record(
            Ok(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: start_offset + 90,
            }),
            start_offset,
        ));

        assert_eq!(
            dispatch
                .stats
                .expect("first advancing record kept")
                .new_offset,
            start_offset + 30
        );
    }

    /// Item 6: a mixed poll — one candidate's no-progress success plus
    /// another candidate's error — must count toward demotion; otherwise the
    /// file stays hot forever.
    #[test]
    fn mixed_no_progress_and_error_polls_count_toward_demotion() {
        let mut discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/projects/mixed.jsonl");
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

        for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
            assert!(
                !super::tally_dispatch_errors(&mut discovery, &path, false, true),
                "below the threshold no demotion"
            );
            assert!(!discovery.files[&path].cold);
        }
        assert!(
            super::tally_dispatch_errors(&mut discovery, &path, false, true),
            "the crossing tick reports the one-shot demotion warn"
        );
        assert!(
            discovery.files[&path].cold,
            "the mixed no-progress+error file is demoted to cold"
        );

        // A successful advance resets the streak.
        super::tally_dispatch_errors(&mut discovery, &path, true, false);
        assert_eq!(discovery.files[&path].consecutive_error_polls, 0);
    }

    #[test]
    fn refresh_failure_counter_crosses_the_warn_threshold_once_per_episode() {
        let mut discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/projects/wedged");
        discovery.directories.insert(
            path.clone(),
            TrackedDirectory {
                kinds: vec![DirectoryKind::ClaudeCodeRoot],
                fingerprint: None,
                entries: HashSet::new(),
                unchanged_probes: 0,
                pinned: false,
                refresh_failures: 0,
            },
        );

        for _ in 1..DIRECTORY_REFRESH_FAILURES_BEFORE_WARN {
            assert_ne!(
                discovery.record_refresh_failure(&path),
                DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
                "below the threshold the log stays at debug"
            );
        }
        assert_eq!(
            discovery.record_refresh_failure(&path),
            DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
            "exactly one escalation per episode"
        );
        assert_ne!(
            discovery.record_refresh_failure(&path),
            DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
            "subsequent failures in the same episode do not re-escalate"
        );

        discovery.clear_refresh_failures(&path);
        assert_eq!(
            discovery.directories[&path].refresh_failures, 0,
            "a successful walk resets the episode"
        );
    }

    #[test]
    fn export_directory_becoming_a_file_keeps_pinned_identity_for_reversion() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let root = temp.path().join("conversations.json");
        std::fs::create_dir(&root).expect("root directory");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(&root, DirectoryKind::ChatGptExport, true);

        // The configured root is replaced by a regular export file.
        std::fs::remove_dir(&root).expect("remove root directory");
        std::fs::write(&root, "[]").expect("export file fixture");
        discovery.probe_directories();

        let file = discovery.files.get(&root).expect("export file tracked");
        assert!(file.pinned);
        assert!(file.kinds.contains(&DiscoveredKind::ChatGptExport));
        let directory = discovery
            .directories
            .get(&root)
            .expect("directory identity retained while the path is a file");
        assert!(directory.pinned);
        assert!(directory.kinds.contains(&DirectoryKind::ChatGptExport));

        // The path reverts to a directory containing an export file.
        std::fs::remove_file(&root).expect("remove export file");
        std::fs::create_dir(&root).expect("recreate root directory");
        std::fs::write(root.join("conversations.json"), "[]").expect("nested export fixture");
        discovery.probe_directories();

        let directory = discovery
            .directories
            .get(&root)
            .expect("configured root still scheduled after reversion");
        assert!(directory.pinned);
        assert!(directory.kinds.contains(&DirectoryKind::ChatGptExport));
        assert!(
            !discovery.files.contains_key(&root),
            "the stale file record from the file phase must not shadow the directory"
        );
        assert!(discovery
            .files
            .contains_key(&root.join("conversations.json")));
    }

    #[test]
    fn pinned_file_placeholder_at_startup_reverts_to_a_directory() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let root = temp.path().join("codex-sessions");
        std::fs::write(&root, "unexpected regular file").expect("startup file fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(&root, DirectoryKind::Codex, true);

        let placeholder = discovery
            .directories
            .get(&root)
            .expect("pinned file gets a directory placeholder");
        assert!(placeholder.pinned);
        assert!(placeholder.kinds.contains(&DirectoryKind::Codex));
        assert!(!discovery.files.contains_key(&root));

        std::fs::remove_file(&root).expect("remove startup file");
        std::fs::create_dir(&root).expect("recreate configured root as directory");
        let day = root.join("2026/08/05");
        std::fs::create_dir_all(&day).expect("date directory");
        let transcript =
            day.join("rollout-2026-08-05T12-00-00-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
        std::fs::write(&transcript, "{}\n").expect("reversion transcript");

        discovery.probe_directories();

        let directory = discovery
            .directories
            .get(&root)
            .expect("placeholder remains scheduled after reversion");
        assert!(directory.pinned);
        assert!(discovery.files.contains_key(&transcript));
    }

    #[test]
    fn force_rescan_without_fingerprint_change_does_not_reprioritize_cold_files() {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let project = temp.path().join("project-slug");
        std::fs::create_dir(&project).expect("project dir");
        let transcript = project.join("session.jsonl");
        std::fs::write(&transcript, "first\n").expect("transcript fixture");

        let mut discovery = DiscoveryIndex::default();
        discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeCodeRoot, true);
        mark_cold(&mut discovery, &transcript);

        // Drain the startup enqueue with an unchanged probe.
        discovery.probe_directories();

        discovery
            .directories
            .get_mut(&project)
            .expect("tracked project directory")
            .unchanged_probes = DIRECTORY_FORCE_RESCAN_PROBES;

        let stats = discovery.probe_directories();
        assert_eq!(stats.walks, 1);
        assert!(
            discovery.priority_cold_queue.is_empty(),
            "a forced rescan of an unchanged directory carries no change signal"
        );
        assert!(discovery.cold_enqueued.contains(&transcript));
    }
}

#[cfg(test)]
mod cursor_retry_tests {
    use super::{
        delete_cursors, drain_pending_cursor_deletes, finalize_dispatch_stats,
        queue_cursor_deletes, tally_dispatch_errors, DiscoveredKind, DiscoveryIndex,
        CURSOR_DELETE_RETRY_LIMIT, FILE_ERROR_POLLS_BEFORE_COLD,
    };
    use crate::mirror::ingest::{mirror_file, LineTailSource, MirrorStats};
    use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;
    use khive_runtime::{AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig};
    use khive_storage::types::{SqlStatement, SqlValue};
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::{NamedTempFile, TempDir};

    /// File-backed runtime WITHOUT the session schema applied — cursor DML
    /// fails with "no such table", which doubles as the fault injection for
    /// the retry path. Caller keeps the `TempDir` alive.
    fn runtime_without_schema() -> (KhiveRuntime, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            brain_split: None,
            db_path: Some(db_path),
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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
        (rt, dir)
    }

    async fn apply_session_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        for stmt in &SESSION_SCHEMA_PLAN_STMTS {
            writer
                .execute_script(stmt.to_string())
                .await
                .expect("schema stmt");
        }
    }

    async fn insert_cursor_row(rt: &KhiveRuntime, path: &std::path::Path, offset: i64) {
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
                      VALUES(?1, NULL, ?2, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text(path.to_string_lossy().into_owned()),
                    SqlValue::Integer(offset),
                ],
                label: None,
            })
            .await
            .expect("cursor insert");
    }

    async fn cursor_row_exists(rt: &KhiveRuntime, path: &std::path::Path) -> bool {
        let mut reader = rt.sql().reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path = ?1".into(),
                params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
                label: None,
            })
            .await
            .expect("cursor query");
        !rows.is_empty()
    }

    #[tokio::test]
    async fn failed_cursor_delete_stays_pending_and_is_retried_next_tick() {
        let (rt, _dir) = runtime_without_schema();
        let discovery = DiscoveryIndex::default();
        let path = PathBuf::from("/projects/gone.jsonl");

        let mut pending = VecDeque::new();
        queue_cursor_deletes(&mut pending, std::slice::from_ref(&path));
        assert_eq!(pending.len(), 1);
        let mut offsets = HashMap::new();

        // No cursor table yet: the delete fails and the entry must stay
        // pending — dropping it would let a stale row survive a daemon
        // restart and skip bytes on a recreated file.
        drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
        assert_eq!(
            pending.len(),
            1,
            "failed delete must remain in the retry set"
        );

        // The table appears (schema applied) and a row exists for the path:
        // the next tick's retry deletes it and drains the set.
        apply_session_schema(&rt).await;
        insert_cursor_row(&rt, &path, 4096).await;
        drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
        assert!(pending.is_empty(), "successful retry drains the set");
        assert!(
            !cursor_row_exists(&rt, &path).await,
            "stale row deleted on retry"
        );
    }

    #[tokio::test]
    async fn retracked_path_cancels_its_pending_cursor_delete() {
        let (rt, _dir) = runtime_without_schema();
        apply_session_schema(&rt).await;

        let path = PathBuf::from("/projects/recreated.jsonl");
        insert_cursor_row(&rt, &path, 512).await;

        let mut discovery = DiscoveryIndex::default();
        let mut pending = VecDeque::new();
        queue_cursor_deletes(&mut pending, std::slice::from_ref(&path));
        let mut offsets = HashMap::new();

        // The file is re-discovered before the delete drains: its fresh
        // cursor row must not be deleted out from under it.
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
        assert!(pending.is_empty(), "re-tracked path cancels its delete");
        assert!(cursor_row_exists(&rt, &path).await, "fresh row preserved");
        assert_eq!(
            offsets.get(&path),
            Some(&512),
            "canceling the delete restores the in-memory offset from the preserved row"
        );
    }

    /// Item 3 regression: remove + re-add in the same pass. The removal
    /// drops the in-memory offset and queues the cursor delete; the re-add
    /// cancels the delete. The cancel must restore the offset from the
    /// preserved cursor row — otherwise the next seed falls back to
    /// `file_len` (`backfill=false`) and skips bytes the preserved row
    /// proves were already mirrored.
    #[tokio::test]
    async fn same_pass_remove_and_readd_restores_offset_from_preserved_cursor_row() {
        let (rt, _dir) = runtime_without_schema();
        apply_session_schema(&rt).await;

        let path = PathBuf::from("/projects/flap.jsonl");
        insert_cursor_row(&rt, &path, 4096).await;

        let mut discovery = DiscoveryIndex::default();
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

        // Same-pass removal: discovery reports the file gone, the service
        // loop drops the in-memory offset and queues the cursor delete.
        discovery.remove_file(&path, false);
        let removed = discovery.take_removed_files();
        assert_eq!(removed, vec![path.clone()]);
        let mut offsets = HashMap::from([(path.clone(), 4096u64)]);
        for removed_path in &removed {
            offsets.remove(removed_path);
        }
        let mut pending = VecDeque::new();
        queue_cursor_deletes(&mut pending, &removed);
        assert!(offsets.is_empty(), "removal dropped the in-memory offset");

        // Same-pass re-discovery: the file is back before the delete drains.
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;

        assert!(
            pending.is_empty(),
            "re-added path cancels its pending delete"
        );
        assert!(
            cursor_row_exists(&rt, &path).await,
            "the preserved cursor row survives the cancel"
        );
        assert_eq!(
            offsets.get(&path),
            Some(&4096),
            "the cancel restores the in-memory offset from the preserved row, \
             so backfill=false seeding never falls back to EOF and skips bytes"
        );
    }

    #[tokio::test]
    async fn failed_cancelled_cursor_restore_blocks_eof_seed_until_retry() {
        let (rt, _dir) = runtime_without_schema();
        let mut file = NamedTempFile::new().expect("tmpfile");
        let prefix = b"already mirrored\n";
        let new_line = br#"{"uuid":"uuid-restored","sessionId":"sess-restored","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"restored"}}"#;
        file.write_all(prefix).expect("prefix");
        file.write_all(new_line).expect("new line");
        file.write_all(b"\n").expect("line terminator");
        let path = file.path().to_path_buf();
        let true_offset = prefix.len() as u64;
        let file_len = std::fs::metadata(&path).expect("file metadata").len();

        let mut discovery = DiscoveryIndex::default();
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        let gone = path.with_file_name("gone.jsonl");
        let mut pending = VecDeque::new();
        queue_cursor_deletes(&mut pending, &[path.clone(), gone.clone()]);
        let mut offsets = HashMap::new();

        // The missing schema injects a cursor-read failure. The cancellation
        // remains pending and the tracked path is explicitly blocked from the
        // scheduling pass, so backfill=false cannot seed it at EOF.
        let blocked =
            drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
        assert!(blocked.contains(path.as_path()));
        assert!(pending.contains(&path));
        assert!(
            pending.contains(&gone),
            "an unrelated failed delete remains pending"
        );
        let scheduled = discovery
            .schedule_files()
            .into_iter()
            .filter(|file| !blocked.contains(file.path.as_path()))
            .collect::<Vec<_>>();
        assert!(scheduled.is_empty(), "failed restore must block this tick");
        assert!(
            !offsets.contains_key(&path),
            "failed restore must not insert an EOF seed before retry"
        );

        // Once the schema and preserved row are available, the next drain
        // restores the true offset and removes the pending cancellation.
        apply_session_schema(&rt).await;
        insert_cursor_row(&rt, &path, true_offset as i64).await;
        let blocked =
            drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
        assert!(blocked.is_empty());
        assert!(pending.is_empty());
        assert_eq!(offsets.get(&path), Some(&true_offset));

        // The restored offset points at the new line, proving the retry does
        // not skip bytes that followed the already mirrored prefix.
        let restored_offset = *offsets.entry(path.clone()).or_insert(file_len);
        assert_eq!(restored_offset, true_offset);
        let stats = mirror_file(
            &rt,
            &path,
            restored_offset,
            LineTailSource::ClaudeCode,
            None,
        )
        .await
        .expect("mirror bytes after restored cursor");
        assert_eq!(
            stats.inserted, 1,
            "bytes after the preserved cursor are mirrored"
        );
    }

    #[tokio::test]
    async fn failed_empty_advance_commit_counts_as_error_for_cold_demotion() {
        let (rt, _dir) = runtime_without_schema();
        let path = PathBuf::from("/projects/blank.jsonl");
        let mut discovery = DiscoveryIndex::default();
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

        let (stats, had_errors) = finalize_dispatch_stats(
            &rt,
            &path,
            0,
            false,
            Some(MirrorStats {
                inserted: 0,
                scanned: 0,
                new_offset: 64,
            }),
            false,
        )
        .await;
        assert!(stats.is_none(), "failed commit must hold back the offset");
        assert!(had_errors, "failed commit must become an error poll");

        for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
            assert!(!tally_dispatch_errors(
                &mut discovery,
                &path,
                false,
                had_errors
            ));
        }
        assert!(tally_dispatch_errors(
            &mut discovery,
            &path,
            false,
            had_errors
        ));
        assert!(discovery.files[&path].cold);
    }

    #[tokio::test]
    async fn queue_cursor_deletes_deduplicates_and_is_bounded() {
        let mut pending = VecDeque::new();
        let path = PathBuf::from("/projects/gone.jsonl");
        queue_cursor_deletes(&mut pending, &[path.clone(), path.clone()]);
        assert_eq!(pending.len(), 1, "duplicate removals collapse to one entry");

        for index in 0..(CURSOR_DELETE_RETRY_LIMIT + 8) {
            let extra = PathBuf::from(format!("/projects/extra-{index}.jsonl"));
            queue_cursor_deletes(&mut pending, &[extra]);
        }
        assert_eq!(
            pending.len(),
            CURSOR_DELETE_RETRY_LIMIT,
            "the retry set is bounded; oldest entries are evicted"
        );
        assert!(
            !pending.contains(&path),
            "the oldest entry is the one evicted under the bound"
        );
    }

    #[tokio::test]
    async fn delete_cursors_reports_per_path_failures_without_aborting_the_batch() {
        let (rt, _dir) = runtime_without_schema();
        let path = PathBuf::from("/projects/gone.jsonl");
        let failed = delete_cursors(&rt, std::slice::from_ref(&path))
            .await
            .expect("writer acquisition succeeds even when the table is missing");
        assert_eq!(
            failed,
            vec![path],
            "the failing path is reported for retry instead of aborting the batch"
        );
    }

    /// Item 7 regression: a failing DELETE must not block later paths in the
    /// batch (head-of-line blocking). With the cursor table missing, every
    /// path fails — and every path is still attempted and reported, rather
    /// than the batch aborting at the first failure.
    #[tokio::test]
    async fn head_of_line_cursor_delete_failure_does_not_block_the_batch() {
        let (rt, _dir) = runtime_without_schema();
        let first = PathBuf::from("/projects/first.jsonl");
        let second = PathBuf::from("/projects/second.jsonl");
        let third = PathBuf::from("/projects/third.jsonl");
        let failed = delete_cursors(&rt, &[first.clone(), second.clone(), third.clone()])
            .await
            .expect("writer acquisition succeeds even when the table is missing");
        assert_eq!(
            failed,
            vec![first, second, third],
            "every path is attempted past the first failure; the failed paths \
             are retained for retry"
        );
    }

    /// The happy path still drains the whole batch and reports nothing.
    #[tokio::test]
    async fn delete_cursors_drains_the_batch_when_every_delete_succeeds() {
        let (rt, _dir) = runtime_without_schema();
        apply_session_schema(&rt).await;
        let first = PathBuf::from("/projects/a.jsonl");
        let second = PathBuf::from("/projects/b.jsonl");
        insert_cursor_row(&rt, &first, 10).await;
        insert_cursor_row(&rt, &second, 20).await;

        let failed = delete_cursors(&rt, &[first.clone(), second.clone()])
            .await
            .expect("delete batch");
        assert!(failed.is_empty(), "no per-path failures on the happy path");
        assert!(!cursor_row_exists(&rt, &first).await);
        assert!(!cursor_row_exists(&rt, &second).await);
    }
}

#[cfg(test)]
mod codex_filename_tests {
    use super::extract_codex_session_id;
    use std::path::Path;

    #[test]
    fn real_codex_filename_yields_uuid() {
        let path =
            Path::new("rollout-2025-11-11T08-32-36-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
        assert_eq!(
            extract_codex_session_id(path).as_deref(),
            Some("019a731e-4a58-71b1-a71f-a8d2f9782113")
        );
    }

    #[test]
    fn timestamp_only_stem_is_rejected() {
        // Regression: a stem with no UUID suffix has 4 hyphens
        // in its trailing segments and must NOT be accepted as a session id.
        let path = Path::new("rollout-2025-11-11T08-32-36.jsonl");
        assert_eq!(extract_codex_session_id(path), None);
    }

    #[test]
    fn invalid_hex_suffix_is_rejected() {
        let path =
            Path::new("rollout-2025-11-11T08-32-36-zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz.jsonl");
        assert_eq!(extract_codex_session_id(path), None);
    }

    #[test]
    fn too_short_suffix_is_rejected() {
        let path = Path::new("rollout-2025-11-11T08-32-36-aaaa-bbbb-cccc-dddd.jsonl");
        assert_eq!(extract_codex_session_id(path), None);
    }

    #[test]
    fn non_rollout_filename_is_rejected() {
        let path = Path::new("not-a-rollout-file.jsonl");
        assert_eq!(extract_codex_session_id(path), None);
    }
}
