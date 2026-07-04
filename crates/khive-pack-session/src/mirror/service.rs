//! Background live-mirror polling service.
//!
//! `run_mirror_service` is an infinite loop started by `SessionPack::warm()`.
//! It discovers `*.jsonl` files under the Claude Code projects directory,
//! tracks byte offsets, and tails new content every `poll_interval`.
//!
//! Design principles:
//! - Infallible: a per-file error is logged and skipped; the loop continues.
//! - Cheap when idle: each tick performs only `metadata().len()` stat calls;
//!   actual reads happen only when the file has grown.
//! - Idempotent: offset tracking + `INSERT OR IGNORE` in `mirror_file` ensure
//!   running multiple times or restarting the daemon is safe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};

use super::ingest::{self, LineTailSource};

/// How a discovered file should be ingested.
///
/// `ChatGptExport` is a `MirrorSource` variant (ADR-080's closed mirror-source
/// set) but deliberately not a `LineTailSource` variant: ChatGPT export
/// ingestion is whole-file (`mirror_chatgpt_export_file`), not line-tail, so
/// it does not belong in that narrower per-line dispatch enum.
enum DiscoveredKind {
    LineTail {
        source: LineTailSource,
        /// Set for `LineTailSource::Codex`; `None` for `LineTailSource::ClaudeCode`.
        session_id: Option<String>,
    },
    ChatGptExport,
}

/// A discovered file together with how it should be ingested.
struct DiscoveredFile {
    path: PathBuf,
    kind: DiscoveredKind,
}

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
            poll_interval: Duration::from_secs(poll_secs),
            backfill,
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::parse_mirror_poll_secs;

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
}

/// Infinite background polling loop.  Returns only on a fatal setup error.
///
/// Seed state from the `session_mirror_cursor` table at startup, then loop:
/// stat each discovered file, tail any new bytes, sleep.
///
/// Claude Code and Codex mirrors are independent: each is enabled by its own
/// flag and scanned separately each tick.
///
/// Per-file errors are logged with `tracing::warn!` and do NOT stop the loop.
pub async fn run_mirror_service(runtime: KhiveRuntime, config: MirrorConfig) {
    tracing::info!(
        projects_dir = %config.projects_dir.display(),
        codex_sessions_dir = %config.codex_sessions_dir.display(),
        poll_interval_ms = config.poll_interval.as_millis(),
        backfill = config.backfill,
        cc_enabled = config.enabled,
        codex_enabled = config.codex_enabled,
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

    loop {
        // Collect all files to process this tick.
        let mut discovered: Vec<DiscoveredFile> = Vec::new();

        if config.enabled {
            for path in scan_cc_jsonl_files(&config.projects_dir) {
                discovered.push(DiscoveredFile {
                    path,
                    kind: DiscoveredKind::LineTail {
                        source: LineTailSource::ClaudeCode,
                        session_id: None,
                    },
                });
            }
        }

        if config.codex_enabled {
            for item in scan_codex_jsonl_files(&config.codex_sessions_dir) {
                discovered.push(item);
            }
        }

        if config.chatgpt_enabled {
            for item in scan_chatgpt_conversations_files(&config.chatgpt_exports_dir) {
                discovered.push(item);
            }
        }

        let total_tracked = discovered.len();
        let mut files_mirrored: u64 = 0;
        let mut rows_inserted: u64 = 0;

        for item in &discovered {
            // Seed offset for newly discovered files.
            if !offsets.contains_key(&item.path) {
                let start = if config.backfill {
                    0
                } else {
                    std::fs::metadata(&item.path).map(|m| m.len()).unwrap_or(0)
                };
                offsets.insert(item.path.clone(), start);
            }

            let offset = *offsets.get(&item.path).unwrap_or(&0);

            // Fast path: skip if file hasn't grown.
            let file_len = match std::fs::metadata(&item.path).map(|m| m.len()) {
                Ok(len) => len,
                Err(e) => {
                    tracing::warn!(path = %item.path.display(), error = %e, "session mirror: stat failed");
                    continue;
                }
            };

            if file_len <= offset {
                continue;
            }

            // Tail (line-tail sources) or whole-file re-read (ChatGPT export).
            let result = match &item.kind {
                DiscoveredKind::LineTail { source, session_id } => {
                    ingest::mirror_file(
                        &runtime,
                        &item.path,
                        offset,
                        *source,
                        session_id.as_deref(),
                    )
                    .await
                }
                DiscoveredKind::ChatGptExport => {
                    ingest::mirror_chatgpt_export_file(&runtime, &item.path, offset).await
                }
            };

            match result {
                Ok(stats) => {
                    offsets.insert(item.path.clone(), stats.new_offset);
                    if stats.inserted > 0 || stats.new_offset > offset {
                        files_mirrored += 1;
                        rows_inserted += stats.inserted;
                        tracing::debug!(
                            path = %item.path.display(),
                            inserted = stats.inserted,
                            scanned = stats.scanned,
                            new_offset = stats.new_offset,
                            "session mirror: tailed file"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %item.path.display(),
                        error = %e,
                        "session mirror: per-file error (skipping)"
                    );
                }
            }
        }

        if files_mirrored > 0 || rows_inserted > 0 {
            tracing::info!(
                files_mirrored,
                rows_inserted,
                total_tracked,
                "session mirror tick"
            );
        } else {
            tracing::debug!(total_tracked, "session mirror: quiet tick");
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

/// Scan `projects_dir` for Claude Code `*.jsonl` files one level deep.
///
/// Expects the layout: `<projects_dir>/<project-slug>/<session-uuid>.jsonl`.
/// Silently skips unreadable subdirectories.
fn scan_cc_jsonl_files(projects_dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    let Ok(top_entries) = std::fs::read_dir(projects_dir) else {
        return files;
    };

    for top_entry in top_entries.flatten() {
        let slug_dir = top_entry.path();
        if !slug_dir.is_dir() {
            continue;
        }
        let Ok(sub_entries) = std::fs::read_dir(&slug_dir) else {
            continue;
        };
        for sub_entry in sub_entries.flatten() {
            let path = sub_entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    files
}

/// Recursively scan `sessions_dir` for Codex `rollout-*.jsonl` files.
///
/// Expects the date-nested layout:
/// `<sessions_dir>/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
/// The session UUID is parsed from the filename stem.
/// Silently skips unreadable directories or filenames that do not match the
/// expected `rollout-…-<uuid>` pattern.
fn scan_codex_jsonl_files(sessions_dir: &std::path::Path) -> Vec<DiscoveredFile> {
    let mut files = Vec::new();
    scan_codex_dir_recursive(sessions_dir, &mut files);
    files
}

/// Recursive helper for `scan_codex_jsonl_files`.
fn scan_codex_dir_recursive(dir: &std::path::Path, out: &mut Vec<DiscoveredFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_codex_dir_recursive(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(session_id) = extract_codex_session_id(&path) {
                out.push(DiscoveredFile {
                    path,
                    kind: DiscoveredKind::LineTail {
                        source: LineTailSource::Codex,
                        session_id: Some(session_id),
                    },
                });
            }
        }
    }
}

/// Find `conversations.json` ChatGPT export files under `path`.
///
/// If `path` is itself a file, it is accepted only when its basename is
/// exactly `conversations.json`; otherwise `path` is scanned recursively.
/// Silently skips unreadable directories, like the other scanners.
fn scan_chatgpt_conversations_files(path: &std::path::Path) -> Vec<DiscoveredFile> {
    let mut files = Vec::new();
    if path.is_file() {
        if path.file_name().and_then(|n| n.to_str()) == Some("conversations.json") {
            files.push(DiscoveredFile {
                path: path.to_path_buf(),
                kind: DiscoveredKind::ChatGptExport,
            });
        }
        return files;
    }
    scan_chatgpt_dir_recursive(path, &mut files);
    files
}

/// Recursive helper for `scan_chatgpt_conversations_files`.
fn scan_chatgpt_dir_recursive(dir: &std::path::Path, out: &mut Vec<DiscoveredFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_chatgpt_dir_recursive(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("conversations.json") {
            out.push(DiscoveredFile {
                path,
                kind: DiscoveredKind::ChatGptExport,
            });
        }
    }
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
        // Regression for Finding 2: a stem with no UUID suffix has 4 hyphens
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
