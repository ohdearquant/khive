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

use super::ingest;

/// Configuration for the mirror service.
///
/// Loaded from environment variables at daemon boot via `MirrorConfig::from_env`.
pub struct MirrorConfig {
    /// Whether the mirror service is enabled (default: false — opt-in).
    pub enabled: bool,
    /// Root directory that contains `<project-slug>/<session-uuid>.jsonl` files.
    ///
    /// Defaults to `$HOME/.claude/projects`.
    pub projects_dir: PathBuf,
    /// How long to sleep between polling ticks (default: 2 seconds).
    pub poll_interval: Duration,
    /// When true (default), existing files are mirrored from byte offset 0.
    /// When false, newly discovered files start mirroring from their current EOF.
    pub backfill: bool,
}

impl MirrorConfig {
    /// Build config from environment variables, falling back to safe defaults.
    ///
    /// | Variable                  | Default                   |
    /// |---------------------------|---------------------------|
    /// | `KHIVE_MIRROR_ENABLED`    | `false`                   |
    /// | `KHIVE_MIRROR_PROJECTS_DIR` | `$HOME/.claude/projects` |
    /// | `KHIVE_MIRROR_POLL_SECS`  | `2`                       |
    /// | `KHIVE_MIRROR_BACKFILL`   | `true`                    |
    pub fn from_env() -> Self {
        let enabled = std::env::var("KHIVE_MIRROR_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let projects_dir = std::env::var("KHIVE_MIRROR_PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
                PathBuf::from(home).join(".claude").join("projects")
            });

        let poll_secs = std::env::var("KHIVE_MIRROR_POLL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2);

        let backfill = std::env::var("KHIVE_MIRROR_BACKFILL")
            .map(|v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        Self {
            enabled,
            projects_dir,
            poll_interval: Duration::from_secs(poll_secs),
            backfill,
        }
    }
}

/// Infinite background polling loop.  Returns only on a fatal setup error.
///
/// Seed state from the `session_mirror_cursor` table at startup, then loop:
/// stat each discovered file, tail any new bytes, sleep.
///
/// Per-file errors are logged with `tracing::warn!` and do NOT stop the loop.
pub async fn run_mirror_service(runtime: KhiveRuntime, config: MirrorConfig) {
    tracing::info!(
        projects_dir = %config.projects_dir.display(),
        poll_interval_ms = config.poll_interval.as_millis(),
        backfill = config.backfill,
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
        let files = scan_jsonl_files(&config.projects_dir);

        let mut files_mirrored: u64 = 0;
        let mut rows_inserted: u64 = 0;

        for path in &files {
            // Seed offset for newly discovered files.
            if !offsets.contains_key(path) {
                let start = if config.backfill {
                    0
                } else {
                    // Skip existing content — start at current EOF.
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                };
                offsets.insert(path.clone(), start);
            }

            let offset = *offsets.get(path).unwrap_or(&0);

            // Fast path: skip if file hasn't grown.
            let file_len = match std::fs::metadata(path).map(|m| m.len()) {
                Ok(len) => len,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "session mirror: stat failed");
                    continue;
                }
            };

            if file_len <= offset {
                continue;
            }

            // Tail the new bytes.
            match ingest::mirror_file(&runtime, path, offset).await {
                Ok(stats) => {
                    offsets.insert(path.clone(), stats.new_offset);
                    if stats.inserted > 0 || stats.new_offset > offset {
                        files_mirrored += 1;
                        rows_inserted += stats.inserted;
                        tracing::debug!(
                            path = %path.display(),
                            inserted = stats.inserted,
                            scanned = stats.scanned,
                            new_offset = stats.new_offset,
                            "session mirror: tailed file"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
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
                total_tracked = files.len(),
                "session mirror tick"
            );
        } else {
            tracing::debug!(total_tracked = files.len(), "session mirror: quiet tick");
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

/// Recursively scan `projects_dir` for `*.jsonl` files one level deep.
///
/// Expects the layout: `<projects_dir>/<project-slug>/<session-uuid>.jsonl`.
/// Silently skips unreadable subdirectories.
fn scan_jsonl_files(projects_dir: &std::path::Path) -> Vec<PathBuf> {
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
