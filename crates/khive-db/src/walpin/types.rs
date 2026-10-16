use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::remove_beacon;

/// One process's walpin heartbeat record (ADR-091 Amendment 2 Plank B).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinHeartbeat {
    pub pid: u32,
    pub process_role: String,
    /// OS-reported process start time (epoch seconds), used as the identity
    /// check at enumeration time — a reused PID is rejected deterministically
    /// rather than probabilistically.
    pub started_at: i64,
    /// Age of the oldest span as of the last body write. Not current once a
    /// tick advances freshness via a metadata-only mtime touch (ADR-091
    /// Amendment 3 Plank F1) — readers that want the current age prefer
    /// [`WalpinHeartbeat::current_oldest_tx_age_secs`], which uses
    /// `oldest_tx_started_at` when present.
    pub oldest_tx_age_secs: f64,
    pub oldest_tx_label: Option<String>,
    /// Epoch timestamp of the oldest span's registration instant, fixed for
    /// as long as that span stays the oldest one (ADR-091 Amendment 3 Plank
    /// F1). `None` for records written before this field existed. Present
    /// specifically to let readers compute a current age from a body that a
    /// touch-only tick left otherwise unchanged.
    #[serde(default)]
    pub oldest_tx_started_at: Option<i64>,
    /// The instant of the last body write. No longer part of liveness
    /// classification for a record carrying `oldest_tx_started_at` (Plank
    /// F1 moves that basis to the entry's mtime); records without it are
    /// still classified on this field exactly as before Amendment 3.
    pub updated_at: i64,
    /// The producer's own sweep cadence in milliseconds. Freshness at
    /// enumeration is judged against THIS cadence, not the enumerating
    /// daemon's — two processes with independently configured sweep
    /// intervals must not misread each other as stale. `0` (absent in a
    /// record written before this field existed) falls back to the
    /// enumerator's own interval. `interval_ms` is accepted as an alias for
    /// records written before the ADR-091 Amendment 2 review-follow-up
    /// rename — a live writer still on the old field name must not have its
    /// real cadence silently dropped to the enumerator's fallback.
    #[serde(default, alias = "interval_ms")]
    pub sweep_interval_ms: u64,
    /// ADR-091 Amendment 3 Plank F2: `"origin"` when the oldest span above
    /// carried this backend's own origin identity, `"fallback"` when it was
    /// an `Unscoped` span observed only through the main view's
    /// never-silently-drop fallback. `None` for records written before this
    /// field existed. Exactly these two values when present — every
    /// consumer MUST fail closed (treat as fallback-confidence) on any
    /// other value, per the amendment's reading rule; see
    /// [`WalpinHeartbeat::attribution_is_evidence_backed`].
    #[serde(default)]
    pub attribution_basis: Option<String>,
}

impl WalpinHeartbeat {
    /// ADR-091 Amendment 3 Plank F2 fail-closed reading rule, binding on
    /// every consumer: only the exact string `"origin"` licenses an
    /// evidence-backed reading. A missing field, or any value this
    /// amendment does not define, classifies as fallback-confidence —
    /// never evidence-backed.
    pub fn attribution_is_evidence_backed(&self) -> bool {
        self.attribution_basis.as_deref() == Some("origin")
    }

    /// ADR-091 Amendment 3 Plank F1: age computed at read time. Prefers
    /// `oldest_tx_started_at` — fixed for as long as the span stays the
    /// oldest one, so it stays correct across metadata-only touches — over
    /// the possibly-stale `oldest_tx_age_secs` body field. Records written
    /// before this amendment lack the field and fall back to the body
    /// value exactly as before.
    pub fn current_oldest_tx_age_secs(&self, now_epoch_secs: i64) -> f64 {
        match self.oldest_tx_started_at {
            Some(started_at) => (now_epoch_secs - started_at).max(0) as f64,
            None => self.oldest_tx_age_secs,
        }
    }
}

/// A heartbeat that survived the three-test liveness gate at enumeration time.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWalpinEntry {
    pub heartbeat: WalpinHeartbeat,
}

/// Per-PID registration marker (ADR-091 Amendment 2, sidecar-health
/// attribution). Written at sidecar initialization (and re-written only
/// after a fail-closed removal — see [`remove_beacon`]); its body is never
/// refreshed per tick, only its mtime. A live process that has no
/// over-threshold span still has a footprint in the sidecar directory: the
/// absence of a *heartbeat* then affirmatively means "no old span," rather
/// than "sidecar never worked."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalpinBeacon {
    pub pid: u32,
    pub process_role: String,
    pub started_at: i64,
    /// The producer's own sweep cadence in milliseconds — the beacon's
    /// refresh mtime is judged against this cadence at enumeration, not the
    /// enumerating daemon's. `0` falls back to the enumerator's interval.
    /// `interval_ms` is accepted as an alias — see
    /// [`WalpinHeartbeat::sweep_interval_ms`] for why the old field name
    /// must still deserialize correctly.
    #[serde(default, alias = "interval_ms")]
    pub sweep_interval_ms: u64,
}

/// Three-state sidecar-health classification for one PID observed in the
/// sidecar directory (ADR-091 Amendment 2 "Sidecar-health attribution"
/// paragraph).
#[derive(Debug, Clone, PartialEq)]
pub enum WalpinPidHealth {
    /// A live, identity-matched, fresh heartbeat exists: this PID currently
    /// holds an over-threshold span.
    Reporting(WalpinHeartbeat),
    /// A live, identity-matched beacon exists with no live heartbeat: the
    /// process's sidecar is functioning and affirmatively reports no
    /// over-threshold span right now.
    RegisteredSilent { pid: u32 },
    /// This PID's sidecar-health could not be established — its beacon (or
    /// heartbeat) entry exists on disk but was refused by the trust-boundary
    /// check (symlink, non-owned) or failed to parse. Any `Unknown` PID makes
    /// the overall attribution inconclusive.
    Unknown { pid: u32, reason: &'static str },
}

/// The result of one sidecar-directory enumeration pass: every PID found,
/// classified three ways, plus whether the directory itself could be trusted
/// at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalpinReport {
    pub entries: Vec<WalpinPidHealth>,
    /// The bounded directory walk stopped before every entry was inspected.
    pub sidecar_listing_truncated: bool,
    /// Trusted stale producer temps that a housekeeping pass would remove.
    pub cleanup_would_reap: usize,
    /// Producer temps actually removed by this pass. Diagnostics always
    /// report zero because their enumeration purpose is non-destructive.
    pub orphan_temps_reaped: usize,
}

impl WalpinReport {
    pub fn reporting(&self) -> impl Iterator<Item = &WalpinHeartbeat> {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::Reporting(hb) => Some(hb),
            _ => None,
        })
    }

    pub fn registered_silent_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::RegisteredSilent { pid } => Some(*pid),
            _ => None,
        })
    }

    pub fn unknown_pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.iter().filter_map(|e| match e {
            WalpinPidHealth::Unknown { pid, .. } => Some(*pid),
            _ => None,
        })
    }

    /// Whether every discovered PID is either reporting or registered-silent
    /// — the licensing condition for the sharper "native/unregistered
    /// mechanism" conclusion (ADR-091 Amendment 2).
    pub fn fully_attributed(&self) -> bool {
        self.unknown_pids().next().is_none()
    }
}

pub(super) fn io_other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// `<db-file>.walpin` sibling of a database file, appended at the `OsString`
/// byte level (mirrors `khive-db`'s `ann_root_for`) so two databases sharing
/// a parent directory can never adopt each other's heartbeat entries.
pub fn sidecar_dir_for(db_path: &Path) -> PathBuf {
    let mut file = db_path.file_name().unwrap_or_default().to_os_string();
    file.push(".walpin");
    match db_path.parent() {
        Some(parent) => parent.join(file),
        None => PathBuf::from(file),
    }
}

/// Whether the sidecar is active for this backend. Defaults to `is_file_backed`
/// (on for file-backed, off for in-memory); `KHIVE_WALPIN_SIDECAR` overrides
/// either way when it parses as a recognized boolean.
pub fn sidecar_enabled(is_file_backed: bool) -> bool {
    crate::env::env_flag("KHIVE_WALPIN_SIDECAR", is_file_backed)
}

#[cfg(any(windows, test))]
pub(super) fn windows_attribute_tag_is_acceptable(
    file_attributes: u32,
    reparse_tag: u32,
    require_directory: bool,
) -> bool {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let is_directory = file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let is_reparse = file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    is_directory == require_directory && !is_reparse && reparse_tag == 0
}

#[cfg(any(windows, test))]
pub(super) fn windows_final_path_matches(expected: &[u16], opened: &[u16]) -> bool {
    expected == opened
}

#[cfg(any(windows, test))]
pub(super) fn windows_relative_child_name_is_safe(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

#[cfg(any(windows, test))]
pub(super) fn windows_owner_dacl_is_restricted(
    ace_count: u32,
    ace_type: u8,
    ace_flags: u8,
    access_mask: u32,
    owner_matches: bool,
    owner_is_token_user: bool,
    dacl_protected: bool,
) -> bool {
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const OBJECT_AND_CONTAINER_INHERIT: u8 = 0x03;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;

    ace_count == 1
        && ace_type == ACCESS_ALLOWED_ACE_TYPE
        && ace_flags == OBJECT_AND_CONTAINER_INHERIT
        && access_mask == FILE_ALL_ACCESS
        && owner_matches
        && owner_is_token_user
        && dacl_protected
}

#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) enum ProducerTempKind {
    Heartbeat,
    Beacon,
}

/// Parse only temp names emitted by this module. Unrecognized hidden files
/// remain outside both cleanup and the retained ordinary-entry budget.
#[cfg(unix)]
pub(super) fn producer_temp_identity(name: &str) -> Option<(u32, ProducerTempKind)> {
    let inner = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let (pid, record_kind) = inner.split_once('.')?;
    let pid = pid.parse::<u32>().ok().filter(|pid| *pid > 0)?;
    let kind = match record_kind {
        "json" => ProducerTempKind::Heartbeat,
        "beacon" => ProducerTempKind::Beacon,
        _ => return None,
    };
    Some((pid, kind))
}
