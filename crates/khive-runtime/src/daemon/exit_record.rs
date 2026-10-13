//! The daemon's exit record (ADR-185 Amendment 5): one line beside the PID
//! file saying how the last incarnation ended, and who signalled it.
//!
//! The record exists because a log line can be filtered away and a file read at
//! the next boot cannot. Only the process that owns the PID file writes it: the
//! `running` line right after the PID-file claim, the `exited` line when
//! shutdown begins, each only while the PID file still names this process.
//! Every write goes to a temporary file in the same directory and is renamed
//! over the record, so a reader sees either the previous line or the new one,
//! never a torn one.
//!
//! The lines are logged on the `khive.boot` target at WARN, which every
//! daemon log filter keeps.
#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

/// Pid of the process that sent the first SIGTERM or SIGINT; 0 until one
/// arrives, or when the kernel sent it.
static SIGNAL_SENDER_PID: AtomicI32 = AtomicI32::new(0);

/// How the incarnation ended, as written in the record's `reason=` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExitReason {
    /// A SIGTERM or SIGINT started graceful shutdown.
    Signal(&'static str),
    /// The demand-launched daemon reached its idle timeout.
    Idle,
    /// Startup failed after the PID file was claimed.
    Error,
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal(name) => write!(f, "signal {name}"),
            Self::Idle => f.write_str("idle"),
            Self::Error => f.write_str("error"),
        }
    }
}

/// `<pid file>.exit`, in the PID file's own directory.
pub(super) fn exit_record_path(pid_file: &Path) -> PathBuf {
    let mut name = pid_file.as_os_str().to_owned();
    name.push(".exit");
    PathBuf::from(name)
}

/// Record the sender of the first SIGTERM/SIGINT this process receives.
///
/// The action is registered through `signal-hook-registry`, the same registry
/// tokio's signal streams use, which runs every action registered for a
/// signal; the daemon's shutdown streams are unaffected. Registered at most
/// once per process.
pub(super) fn capture_signal_senders() -> std::io::Result<()> {
    static INSTALLED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    let mut installed = INSTALLED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *installed {
        return Ok(());
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the action performs one atomic compare-exchange on a static
        // and reads one field of the kernel-supplied siginfo; it allocates
        // nothing and takes no lock, so it is async-signal-safe.
        unsafe { signal_hook_registry::register_sigaction(signal, record_signal_sender) }?;
    }
    *installed = true;
    Ok(())
}

fn record_signal_sender(info: &libc::siginfo_t) {
    // SAFETY: SIGTERM and SIGINT are registered only for themselves; for
    // either, the kernel fills `si_pid` with the sender (0 when the kernel
    // itself raised it).
    let sender = unsafe { info.si_pid() };
    let _ = SIGNAL_SENDER_PID.compare_exchange(0, sender, Ordering::Relaxed, Ordering::Relaxed);
}

/// The sender of the first shutdown signal, when one arrived from a process.
pub(super) fn signal_sender_pid() -> Option<u32> {
    u32::try_from(SIGNAL_SENDER_PID.load(Ordering::Relaxed))
        .ok()
        .filter(|pid| *pid > 0)
}

/// What the previous incarnation left in the record.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PreviousExit {
    /// No record: the first boot at this rendezvous, or one from before records.
    Absent,
    /// The previous incarnation wrote its exit line.
    Exited(String),
    /// The previous incarnation never replaced its `running` line: it was
    /// killed (SIGKILL, OOM) or crashed.
    NoExit { pid: String },
    /// The record exists but could not be read or parsed.
    Unreadable(String),
}

pub(super) fn read_previous(record: &Path) -> PreviousExit {
    let contents = match std::fs::read_to_string(record) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PreviousExit::Absent,
        Err(error) => return PreviousExit::Unreadable(error.to_string()),
    };
    let line = contents.trim();
    if line.starts_with("exited ") {
        return PreviousExit::Exited(line.to_owned());
    }
    if line.starts_with("running ") {
        let pid = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("pid="))
            .unwrap_or("unknown");
        return PreviousExit::NoExit {
            pid: pid.to_owned(),
        };
    }
    PreviousExit::Unreadable(format!("unrecognized record {line:?}"))
}

/// The exit record of the daemon that owns `pid_file`.
pub(super) struct ExitRecord {
    pid_file: PathBuf,
    record: PathBuf,
}

impl ExitRecord {
    /// Called right after this process claimed `pid_file`: log the previous
    /// incarnation's record and the effective descriptor limit, then replace
    /// the record with this incarnation's `running` line.
    pub(super) fn claim(pid_file: &Path) -> Self {
        let this = Self {
            pid_file: pid_file.to_path_buf(),
            record: exit_record_path(pid_file),
        };
        match read_previous(&this.record) {
            PreviousExit::Absent => {}
            PreviousExit::Exited(line) => tracing::warn!(
                target: "khive.boot",
                record = ?this.record,
                "previous exit: {line}"
            ),
            PreviousExit::NoExit { pid } => tracing::warn!(
                target: "khive.boot",
                record = ?this.record,
                "previous incarnation pid={pid} recorded no exit (killed or crashed)"
            ),
            PreviousExit::Unreadable(error) => tracing::warn!(
                target: "khive.boot",
                record = ?this.record,
                error = %error,
                "previous exit record unreadable"
            ),
        }
        log_descriptor_limit();
        let line = format!("running pid={} started={}", std::process::id(), now());
        if let Err(error) = this.write_if_owned(&line) {
            tracing::warn!(target: "khive.boot", record = ?this.record, error = %error, "could not write the running record");
        }
        this
    }

    /// Log the shutdown line and record how this incarnation is ending. Called
    /// when the shutdown reason is decided, before the drain, so a second
    /// signal that ends a blocked drain still leaves the first one on record.
    pub(super) fn exiting(&self, reason: ExitReason) {
        let pid = std::process::id();
        let sender_pid = match (reason, signal_sender_pid()) {
            (ExitReason::Signal(_), Some(sender)) => sender.to_string(),
            _ => "unknown".to_owned(),
        };
        tracing::warn!(
            target: "khive.boot",
            pid,
            reason = %reason,
            sender_pid = %sender_pid,
            "khived exiting"
        );
        let line = format!(
            "exited pid={pid} reason={reason} sender_pid={sender_pid} at={}",
            now()
        );
        if let Err(error) = self.write_if_owned(&line) {
            tracing::warn!(target: "khive.boot", record = ?self.record, error = %error, "could not write the exit record");
        }
    }

    /// Replace the record with `line` while the PID file still names this
    /// process. A replacement daemon claims the PID file only once this one's
    /// is gone, so the check keeps a late write from overwriting its record.
    fn write_if_owned(&self, line: &str) -> std::io::Result<()> {
        let owner = std::fs::read_to_string(&self.pid_file)?;
        if owner.trim().parse::<u32>().ok() != Some(std::process::id()) {
            return Err(std::io::Error::other(format!(
                "the PID file {} no longer names this process",
                self.pid_file.display()
            )));
        }
        write_atomically(&self.record, line)
    }
}

/// Write `line` to a same-directory temporary file, flush it to disk, and
/// rename it over `record`.
fn write_atomically(record: &Path, line: &str) -> std::io::Result<()> {
    let file_name = record
        .file_name()
        .ok_or_else(|| std::io::Error::other("exit record path has no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = record.with_file_name(tmp_name);
    // A leftover from an earlier process that had this pid; never followed.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, record)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Log the effective RLIMIT_NOFILE, soft and hard.
fn log_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only into the owned struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(target: "khive.boot", error = %error, "descriptor limit unreadable");
        return;
    }
    let show = |value: libc::rlim_t| {
        if value == libc::RLIM_INFINITY {
            "unlimited".to_owned()
        } else {
            value.to_string()
        }
    };
    tracing::warn!(
        target: "khive.boot",
        nofile_soft = %show(limit.rlim_cur),
        nofile_hard = %show(limit.rlim_max),
        "descriptor limit"
    );
}
