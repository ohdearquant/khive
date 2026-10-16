//! Daemon command construction, spawn-time log rotation and process launch.

use std::process::Stdio;

use khive_runtime::process_retry::{spawn_retrying_executable_busy, EXECUTABLE_BUSY_BACKOFF_MS};

#[cfg(test)]
use super::SPAWN_COUNT;

/// Cap on `khived.log` size (bytes) before a spawn rotates it to `khived.log.1`.
///
/// Rotation only happens at spawn time (never mid-session): the daemon is
/// respawned often enough (rebuilds, reconnects, stale-daemon recovery) that
/// this alone bounds disk use, without pulling in `tracing-appender` or
/// touching `init_tracing`'s writer.
pub(super) const DAEMON_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Resolve `<home>/.khive/logs/khived.log` given an explicit `HOME` value.
///
/// Takes the HOME value as a parameter (rather than reading the environment
/// directly) so the resolution logic is unit-testable without mutating
/// process-global state. Mirrors the `Path::new(&home).join(...)` idiom used
/// for `~/.khive/.env` resolution in `kkernel`'s `load_khive_dotenv`.
pub(super) fn daemon_log_path_from_home(
    home: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    let home = home?;
    Some(
        std::path::Path::new(home)
            .join(".khive")
            .join("logs")
            .join("khived.log"),
    )
}

/// Resolve the daemon log path from the real process environment. Returns
/// `None` when `HOME` is unset — the caller falls back to discarding the
/// daemon's stderr rather than failing the spawn.
pub(super) fn daemon_log_path() -> Option<std::path::PathBuf> {
    daemon_log_path_from_home(std::env::var_os("HOME").as_deref())
}

/// Decide whether the log at `current_size` bytes must rotate before this
/// spawn, given a `cap` in bytes. Pulled out as a pure function so the
/// spawn-time rotation policy is unit-testable independent of the filesystem.
pub(super) fn daemon_log_should_rotate(current_size: u64, cap: u64) -> bool {
    current_size >= cap
}

/// Prepare `log_path` for the daemon's stderr: create its parent directory,
/// rotate the existing file to `<name>.1` (replacing any prior backup) if it
/// is at or over `cap` bytes, then open (or create) it for append.
///
/// Returns `None` on directory-creation or open failure so the caller can
/// fall back to `Stdio::null()`. A rotation (`rename`) failure is deliberately
/// swallowed and degrades to appending to the existing over-cap file — keeping
/// the daemon's stderr flowing to a slightly-too-large log beats losing it.
/// Logging is best-effort; daemon spawn correctness is not, and the daemon is
/// on the hot path for every MCP request.
pub(super) fn prepare_daemon_log_file_with_cap(
    log_path: &std::path::Path,
    cap: u64,
) -> Option<std::fs::File> {
    let dir = log_path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    if let Ok(meta) = std::fs::metadata(log_path) {
        if daemon_log_should_rotate(meta.len(), cap) {
            let backup = dir.join("khived.log.1");
            // `rename` replaces an existing destination atomically on Unix —
            // exactly the "replace any prior .1" behavior we want.
            let _ = std::fs::rename(log_path, &backup);
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok()
}

/// [`prepare_daemon_log_file_with_cap`] using the standing [`DAEMON_LOG_MAX_BYTES`] cap.
fn prepare_daemon_log_file(log_path: &std::path::Path) -> Option<std::fs::File> {
    prepare_daemon_log_file_with_cap(log_path, DAEMON_LOG_MAX_BYTES)
}

pub(super) fn spawn_daemon() -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    spawn_daemon_with_exe(&exe)
}

pub(super) fn spawn_daemon_with_exe(exe: &std::path::Path) -> std::io::Result<std::process::Child> {
    spawn_daemon_with_exe_and_config(exe, None, None, None)
}

pub(super) fn daemon_launch_command(
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> std::process::Command {
    // The binary is `kkernel`; the MCP server (and its daemon mode) live under
    // the `mcp` subcommand.
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("mcp")
        .arg("--daemon")
        .arg("--lifetime")
        .arg("demand");
    // A client-started daemon must not inherit a launcher incarnation claim
    // from an embedding process that happened to originate under supervision.
    #[cfg(unix)]
    cmd.env_remove(khive_runtime::daemon::SUPERVISOR_CLAIM_ENV);
    if let Some(path) = config {
        cmd.arg("--config").arg(path);
    }
    // Forward whatever override the caller hands over — the caller owns the
    // decision of WHICH override a spawned daemon must be constructed with
    // (`run_exec_inline_with_forward` in `crates/kkernel/src/exec.rs`):
    //
    // - `:memory:` always forwards: it is the one override a newly spawned
    //   daemon can honor byte-identically to the client (ephemeral by
    //   definition), and without it the fresh daemon would bind the config's
    //   declared persistent backend files instead — the opposite of what the
    //   operator requested.
    // - A CONCRETE path forwards in the single-backend case (no
    //   `[[backends]]` declared): the spawned daemon has no config-declared
    //   database path to default to, so without the override it would bind
    //   `$HOME/.khive/khive.db` and its `config_id` would never match the
    //   client's override-anchored frame.
    // - A redundant concrete override (multi-backend, proven to name the
    //   declared `main` backend) is deliberately NOT passed here by the
    //   caller: the spawned daemon's config-declared path IS that override's
    //   target, and the client's `config_id` has already been normalized to
    //   the no-override anchor — forwarding it would desync the child's
    //   fingerprint from the normalized frame.
    if let Some(db) = db {
        cmd.arg("--db").arg(db);
    }
    // Forward the spawning client's already-resolved pack set explicitly
    // rather than relying on ambient `KHIVE_PACKS` env inheritance: the
    // client may have resolved packs from a CLI `--pack` flag or a
    // discovered `[runtime].packs` config entry, neither of which travels
    // through `Command::new`'s default env inheritance. Without this, a
    // freshly spawned daemon falls back to the built-in default pack set,
    // its `config_id` fingerprint disagrees with every caller expecting the
    // wider set, and those callers permanently fall back to in-process
    // dispatch instead of the warm daemon (khive-oss#1941).
    if let Some(packs) = packs {
        for pack in packs {
            cmd.arg("--pack").arg(pack);
        }
    }
    cmd
}

pub(super) fn spawn_daemon_with_exe_and_config(
    exe: &std::path::Path,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> std::io::Result<std::process::Child> {
    #[cfg(test)]
    SPAWN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let mut cmd = daemon_launch_command(exe, config, db, packs);
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    // The daemon's tracing (including WAL/checkpoint telemetry) goes to
    // stderr honoring KHIVE_LOG (init_tracing in kkernel's main.rs) — wiring
    // it to /dev/null silently discards all of it. Route it to a log file
    // instead; fall back to null on any resolution/creation failure so a
    // logging problem never breaks the daemon spawn itself.
    match daemon_log_path().and_then(|path| prepare_daemon_log_file(&path)) {
        Some(file) => {
            cmd.stderr(file);
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // #898: return the live `Child` (rather than discarding it) so the caller
    // can positively confirm the respawned process is still alive before
    // treating recovery as healthy — see `RecoveryOutcome::Spawned` and its
    // use in `forward_or_spawn`. A binary that predates or otherwise rejects
    // `mcp --daemon` (version skew) exits immediately with a clap parse
    // error; without this handle that failure was invisible to everything
    // except `khived.log`.
    // A just-written executable can transiently fail `execve(2)` with
    // ETXTBSY on instrumented/contended filesystems. This is especially easy
    // to hit in the argv-forwarding tests, but it can also occur while a real
    // installation is atomically replacing `kkernel`. Retry only that precise
    // error, with a short finite budget; every other spawn failure remains
    // immediate and unchanged.
    spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || cmd.spawn())
}
