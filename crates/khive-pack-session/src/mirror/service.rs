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
const DIRECTORY_FORCE_RESCAN_PROBES: u16 = 30;
const FILE_UNCHANGED_POLLS_BEFORE_COLD: u8 = 2;
const FILE_COLD_AGE: Duration = Duration::from_secs(5 * 60);
const FILE_UNCHANGED_POLLS_WITHOUT_MTIME: u8 = 30;

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
}

struct TrackedFile {
    kinds: Vec<DiscoveredKind>,
    cold: bool,
    unchanged_polls: u8,
    pinned: bool,
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
    fn record(
        &mut self,
        result: Result<ingest::MirrorStats, RuntimeError>,
        start_offset: u64,
    ) -> bool {
        match result {
            Ok(stats) if stats.new_offset > start_offset => {
                self.stats = Some(stats);
                true
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
                            tracing::debug!(
                                path = %path.display(),
                                error = %error,
                                "session mirror: directory refresh failed"
                            );
                        } else {
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
                    tracing::debug!(
                        path = %path.display(),
                        error = %error,
                        "session mirror: directory metadata probe failed"
                    );
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
        } else {
            self.directories.remove(path);
            self.directory_queue.retain(|queued| queued != path);
            self.directory_enqueued.remove(path);
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
        // Drop stale scheduler state so a later re-add of this path starts
        // with clean queues instead of being shadowed by leftover entries.
        self.cold_queue.retain(|queued| queued != path);
        self.cold_enqueued.remove(path);
        self.priority_cold_queue.retain(|queued| queued != path);
        self.priority_cold_enqueued.remove(path);
        self.removed_files.push(path.to_path_buf());
    }

    /// Drain paths removed from tracking since the last call so the service
    /// loop can prune their cursors.
    fn take_removed_files(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.removed_files)
    }

    fn reactivate_file(&mut self, path: &Path) {
        if let Some(file) = self.files.get_mut(path) {
            file.cold = false;
            file.unchanged_polls = 0;
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
        let mut remaining_cold_probes = COLD_FILE_PROBES_PER_TICK;
        let mut selected_cold = HashSet::new();
        let priority_candidates = self.priority_cold_queue.len();

        for _ in 0..priority_candidates {
            if remaining_cold_probes == 0 {
                break;
            }
            let Some(path) = self.priority_cold_queue.pop_front() else {
                break;
            };
            self.priority_cold_enqueued.remove(&path);
            if self.files.get(&path).is_some_and(|file| file.cold)
                && selected_cold.insert(path.clone())
            {
                scheduled.push(ScheduledFile {
                    path,
                    was_cold: true,
                });
                remaining_cold_probes -= 1;
            }
        }

        let cold_candidates = self.cold_queue.len();

        for _ in 0..cold_candidates {
            if remaining_cold_probes == 0 {
                break;
            }
            let Some(path) = self.cold_queue.pop_front() else {
                break;
            };
            self.cold_enqueued.remove(&path);
            if self.files.get(&path).is_some_and(|file| file.cold)
                && selected_cold.insert(path.clone())
            {
                scheduled.push(ScheduledFile {
                    path,
                    was_cold: true,
                });
                remaining_cold_probes -= 1;
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
            self.remove_file(path, false);
        } else if was_cold {
            self.enqueue_cold(path);
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

fn classify_entry(
    directory_kind: DirectoryKind,
    path: &Path,
    is_directory: bool,
) -> Option<ClassifiedEntry> {
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
            if let Err(error) = delete_cursors(&runtime, &removed_files).await {
                tracing::debug!(error = %error, "session mirror: cursor cleanup failed");
            }
        }
        let scheduled = discovery.schedule_files();
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
            for kind in kinds {
                let result = match kind {
                    DiscoveredKind::LineTail { source, session_id } => {
                        ingest::mirror_file(
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
                    break;
                }
            }

            let CandidateDispatch { stats, errors } = candidate_dispatch;
            for error in errors {
                tracing::warn!(
                    path = %scheduled_file.path.display(),
                    error = %error,
                    "session mirror: per-file source candidate error"
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
/// logs errors and continues: the in-memory offset map is already pruned, so
/// a failure here only risks a stale row being reloaded on restart, never
/// data loss (ingest is idempotent via `INSERT OR IGNORE`).
async fn delete_cursors(runtime: &KhiveRuntime, paths: &[PathBuf]) -> Result<(), RuntimeError> {
    let sql = runtime.sql();
    let mut writer = sql
        .writer()
        .await
        .map_err(|e| RuntimeError::Internal(format!("mirror: cursor writer: {e}")))?;

    for path in paths {
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM session_mirror_cursor WHERE file_path=?1".into(),
                params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
                label: Some("mirror_cursor_delete".into()),
            })
            .await
            .map_err(|e| RuntimeError::Internal(format!("mirror: cursor delete: {e}")))?;
    }
    Ok(())
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
        COLD_FILE_PROBES_PER_TICK, DIRECTORY_FORCE_RESCAN_PROBES, DIRECTORY_PROBES_PER_TICK,
        FILE_COLD_AGE, FILE_UNCHANGED_POLLS_BEFORE_COLD, FILE_UNCHANGED_POLLS_WITHOUT_MTIME,
    };
    use crate::mirror::ingest::MirrorStats;
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
        assert!(discovery.cold_queue.iter().all(|queued| queued != &path));
        assert!(!discovery.cold_enqueued.contains(&path));
        assert!(discovery
            .priority_cold_queue
            .iter()
            .all(|queued| queued != &path));
        assert!(!discovery.priority_cold_enqueued.contains(&path));

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
        assert!(discovery
            .directory_queue
            .iter()
            .all(|queued| queued != &root));
        assert!(!discovery.directory_enqueued.contains(&root));

        // A re-added directory is probed again instead of waiting for a
        // stale queue entry to drain.
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

        let transient = PathBuf::from("/projects/gone.jsonl");
        discovery.add_file(transient.clone(), DiscoveredKind::ChatGptExport, false);
        discovery.record_probe_error(&transient, false, true);
        assert!(!discovery.files.contains_key(&transient));
        assert!(!discovery.hot_files.contains(&transient));
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
