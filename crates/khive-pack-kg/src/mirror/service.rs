//! Background workspace-mirror polling service (ADR-087).
//!
//! `run_mirror_service` is spawned from `KgPack::warm()`, mirroring
//! `SessionPack::warm()`'s use of `khive-pack-session`'s own mirror service
//! (ADR-080 §6) — the poller shape, the nonzero-clamped poll interval, and
//! the never-advance-cursor-on-error failure posture are reused verbatim.
//! Only the write target differs (see `super::ingest`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use khive_runtime::{KhiveRuntime, Namespace};

use super::glob::{self, DEFAULT_EXCLUDE, DEFAULT_INCLUDE};
use super::ingest;

/// Default poll interval, in seconds, used when
/// `KHIVE_MIRROR_WORKSPACE_POLL_SECS` is unset, non-numeric, or explicitly
/// zero. Workspace files change far less often than live session
/// transcripts, so this default is coarser than the session mirror's.
const DEFAULT_WORKSPACE_POLL_SECS: u64 = 30;

/// Parse `KHIVE_MIRROR_WORKSPACE_POLL_SECS`, rejecting an explicit `0`
/// (PACKSESSION-AUD-002 precedent: a zero interval would create a hot
/// polling loop) and falling back to [`DEFAULT_WORKSPACE_POLL_SECS`] for
/// missing, non-numeric, or zero values. Explicit zero and non-numeric
/// input are logged as distinct warnings so an operator can tell which
/// mistake they made.
fn parse_workspace_poll_secs(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_WORKSPACE_POLL_SECS,
        Some(v) => match v.parse::<u64>() {
            Ok(0) => {
                tracing::warn!(
                    value = v,
                    default_secs = DEFAULT_WORKSPACE_POLL_SECS,
                    "KHIVE_MIRROR_WORKSPACE_POLL_SECS must be nonzero; using default"
                );
                DEFAULT_WORKSPACE_POLL_SECS
            }
            Ok(secs) => secs,
            Err(_) => {
                tracing::warn!(
                    value = v,
                    default_secs = DEFAULT_WORKSPACE_POLL_SECS,
                    "KHIVE_MIRROR_WORKSPACE_POLL_SECS is not numeric; using default"
                );
                DEFAULT_WORKSPACE_POLL_SECS
            }
        },
    }
}

/// Parse a comma-separated glob list, falling back to `default` when unset
/// or blank.
fn parse_glob_list(raw: Option<&str>, default: &[&str]) -> Vec<String> {
    match raw {
        Some(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// Configuration for the workspace mirror service, loaded from environment
/// variables at daemon boot via [`MirrorConfig::from_env`].
pub struct MirrorConfig {
    /// Whether the workspace mirror is enabled (default: `false` — opt-in,
    /// matching the session mirror's own opt-in-by-default convention).
    pub enabled: bool,
    /// Directory that CONTAINS `.khive/` (i.e. `workspace_root.join(".khive")`
    /// is what gets walked). Defaults to the daemon's current working
    /// directory.
    pub workspace_root: PathBuf,
    /// How long to sleep between polling ticks.
    pub poll_interval: Duration,
    /// Include globs, relative to `.khive/`.
    pub include: Vec<String>,
    /// Exclude globs, relative to `.khive/`. Exclude always wins over
    /// include.
    pub exclude: Vec<String>,
}

impl MirrorConfig {
    /// Build config from environment variables, falling back to safe
    /// defaults.
    ///
    /// | Variable                             | Default                    |
    /// |---------------------------------------|-----------------------------|
    /// | `KHIVE_MIRROR_WORKSPACE_ENABLED`       | `false`                     |
    /// | `KHIVE_MIRROR_WORKSPACE_ROOT`          | current working directory  |
    /// | `KHIVE_MIRROR_WORKSPACE_POLL_SECS`     | `30`                        |
    /// | `KHIVE_MIRROR_WORKSPACE_INCLUDE`       | ADR-087's recommended set  |
    /// | `KHIVE_MIRROR_WORKSPACE_EXCLUDE`       | ADR-087's recommended set  |
    pub fn from_env() -> Self {
        let enabled = std::env::var("KHIVE_MIRROR_WORKSPACE_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let workspace_root = std::env::var("KHIVE_MIRROR_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let poll_raw = std::env::var("KHIVE_MIRROR_WORKSPACE_POLL_SECS").ok();
        let poll_secs = parse_workspace_poll_secs(poll_raw.as_deref());

        let include = parse_glob_list(
            std::env::var("KHIVE_MIRROR_WORKSPACE_INCLUDE")
                .ok()
                .as_deref(),
            DEFAULT_INCLUDE,
        );
        let exclude = parse_glob_list(
            std::env::var("KHIVE_MIRROR_WORKSPACE_EXCLUDE")
                .ok()
                .as_deref(),
            DEFAULT_EXCLUDE,
        );

        Self {
            enabled,
            workspace_root,
            poll_interval: Duration::from_secs(poll_secs),
            include,
            exclude,
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::{parse_glob_list, parse_workspace_poll_secs, DEFAULT_INCLUDE};

    /// Regression for PACKSESSION-AUD-002 (reused precedent): an explicit
    /// zero must be rejected back to the documented default, never accepted
    /// as a hot loop.
    #[test]
    fn poll_secs_zero_is_rejected_and_default_remains_thirty_seconds() {
        assert_eq!(
            parse_workspace_poll_secs(None),
            30,
            "missing defaults to 30s"
        );
        assert_eq!(
            parse_workspace_poll_secs(Some("abc")),
            30,
            "non-numeric value defaults to 30s"
        );
        assert_eq!(
            parse_workspace_poll_secs(Some("0")),
            30,
            "explicit zero must be rejected back to the default, not accepted as a hot loop"
        );
        assert_eq!(parse_workspace_poll_secs(Some("1")), 1);
        assert_eq!(parse_workspace_poll_secs(Some("120")), 120);
    }

    #[test]
    fn glob_list_falls_back_to_default_when_unset_or_blank() {
        assert_eq!(
            parse_glob_list(None, DEFAULT_INCLUDE),
            DEFAULT_INCLUDE.to_vec()
        );
        assert_eq!(
            parse_glob_list(Some(""), DEFAULT_INCLUDE),
            DEFAULT_INCLUDE.to_vec()
        );
        assert_eq!(
            parse_glob_list(Some("  "), DEFAULT_INCLUDE),
            DEFAULT_INCLUDE.to_vec()
        );
    }

    #[test]
    fn glob_list_parses_and_trims_comma_separated_entries() {
        assert_eq!(
            parse_glob_list(Some("notes/**, reports/** ,x/**"), DEFAULT_INCLUDE),
            vec![
                "notes/**".to_string(),
                "reports/**".to_string(),
                "x/**".to_string(),
            ]
        );
    }
}

/// Infinite background polling loop. Returns only on a fatal setup error
/// (e.g. the mirror's own `authorize` call is denied).
///
/// Per-file errors are logged via `tracing::warn!` and do NOT stop the
/// loop — matching the session mirror's failure posture exactly.
pub async fn run_mirror_service(runtime: KhiveRuntime, config: MirrorConfig) {
    let token = match runtime.authorize(Namespace::local()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "workspace mirror: failed to authorize; service exiting");
            return;
        }
    };

    let khive_dir = config.workspace_root.join(".khive");

    tracing::info!(
        khive_dir = %khive_dir.display(),
        poll_interval_ms = config.poll_interval.as_millis(),
        include = ?config.include,
        exclude = ?config.exclude,
        "workspace mirror service starting"
    );

    loop {
        let files = discover_files(&khive_dir, &config.include, &config.exclude);
        let mut created: u64 = 0;
        let mut unchanged: u64 = 0;

        for path in &files {
            match ingest::mirror_file(&runtime, &token, &khive_dir, path).await {
                Ok(ingest::MirrorOutcome::Created { .. }) => created += 1,
                Ok(ingest::MirrorOutcome::Unchanged) => unchanged += 1,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "workspace mirror: per-file error (skipping, cursor unchanged)"
                    );
                }
            }
        }

        if created > 0 {
            tracing::info!(
                created,
                unchanged,
                total_tracked = files.len(),
                "workspace mirror tick"
            );
        } else {
            tracing::debug!(
                unchanged,
                total_tracked = files.len(),
                "workspace mirror: quiet tick"
            );
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

/// Walk `khive_dir` recursively, returning files whose path (relative to
/// `khive_dir`) matches the include/exclude glob configuration. Silently
/// skips unreadable subdirectories, like the session mirror's own scanners.
fn discover_files(khive_dir: &Path, include: &[String], exclude: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(khive_dir, khive_dir, include, exclude, &mut out);
    out
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    include: &[String],
    exclude: &[String],
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(root, &path, include, exclude, out);
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if glob::is_included(&rel_str, include, exclude) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod discovery_tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn discover_files_applies_default_include_exclude_over_a_real_tree() {
        let tmp = TempDir::new().expect("tempdir");
        let khive_dir = tmp.path().join(".khive");

        let write = |rel: &str| {
            let full = khive_dir.join(rel);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&full).unwrap();
            f.write_all(b"content").unwrap();
        };

        write("notes/handoffs/h1.md");
        write("reports/audit.md");
        write("kg/entities.ndjson");
        write("kg/schema.yaml");
        write("scripts/audit_crate.py");

        let include: Vec<String> = DEFAULT_INCLUDE.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<String> = DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect();
        let found = discover_files(&khive_dir, &include, &exclude);
        let found_rel: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&khive_dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert!(found_rel.contains(&"notes/handoffs/h1.md".to_string()));
        assert!(found_rel.contains(&"reports/audit.md".to_string()));
        assert!(!found_rel.contains(&"kg/entities.ndjson".to_string()));
        assert!(!found_rel.contains(&"kg/schema.yaml".to_string()));
        assert!(!found_rel.contains(&"scripts/audit_crate.py".to_string()));
    }
}
