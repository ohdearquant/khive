use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::pool::ConnectionPool;

#[cfg(unix)]
use super::TruncateState;
#[cfg(doc)]
use super::{log_tx_registry_oldest_warn, log_tx_registry_snapshot_warn};
use super::{TxAgeEmission, TxAgeRung};

#[cfg(unix)]
pub(super) const DEFAULT_WALPIN_FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(unix)]
#[derive(Debug, Clone)]
pub(super) struct CachedWalpinAttribution {
    pub(super) report: crate::walpin::WalpinReport,
    pub(super) census: Result<crate::walpin::CensusResult, String>,
    pub(super) captured_at: Instant,
}

#[cfg(unix)]
#[derive(Debug)]
pub(super) enum WalpinFullScanPlan {
    Refresh {
        previous_last_attempt: Option<Instant>,
    },
    Cached(CachedWalpinAttribution),
    Suppressed,
}

/// ADR-091 Plank 1: turn a [`TxAgeEmission`] into the appropriate `tracing`
/// call. Extracted from `run_checkpoint_task` so tests can drive the same
/// logging path `CaptureSubscriber`-style without spinning up the async task
/// (mirrors [`log_tx_registry_oldest_warn`]/[`log_tx_registry_snapshot_warn`]).
pub(super) fn log_tx_age_emission(emission: &TxAgeEmission) {
    let label = emission.label.as_deref().unwrap_or("<unlabeled>");
    match emission.rung {
        TxAgeRung::Warn => {
            tracing::warn!(
                tx_age_secs = emission.age.as_secs_f64(),
                tx_label = label,
                "ADR-091 Plank 1: open transaction registry entry exceeded soft-cap age"
            );
        }
        TxAgeRung::Stale => {
            tracing::error!(
                tx_age_secs = emission.age.as_secs_f64(),
                tx_label = label,
                "ADR-091 Plank 1: open transaction registry entry exceeded the cooperative \
                 stale-op cap; no in-process mechanism can force-close it — investigate the \
                 labeled caller directly"
            );
        }
    }
}

#[derive(Default)]
pub(super) struct ReaderCheckoutSweepState {
    was_above_warn: bool,
}

impl ReaderCheckoutSweepState {
    pub(super) fn observe(
        &mut self,
        oldest: Option<Duration>,
        threshold: Duration,
    ) -> Option<Duration> {
        let above_warn = oldest.is_some_and(|age| age > threshold);
        let warning = oldest.filter(|_| above_warn && !self.was_above_warn);
        // A different oldest checkout does not begin a new episode while
        // another checkout still exceeds the threshold.
        self.was_above_warn = above_warn;
        warning
    }

    pub(super) fn sweep_at(&mut self, pool: &ConnectionPool, now: Instant) {
        let oldest = pool.oldest_checkout_age_at(now);
        let threshold = pool.config().reader_checkout_warn_after;
        if let Some(age) = self.observe(oldest, threshold) {
            tracing::warn!(
                oldest_checkout_age_secs = age.as_secs_f64(),
                reader_checkout_warn_secs = threshold.as_secs_f64(),
                database = ?pool.canonical_path(),
                "ADR-091 Plank 1: pooled reader checkout exceeded warning age"
            );
        }
    }
}

/// ADR-091 Amendment 2 Plank B: per-process walpin sidecar state, carried
/// across ticks by whichever sweep owns it (the daemon's `run_checkpoint_task`
/// or a session's `run_session_sweep_task`). Once the registry's oldest span
/// exceeds `tx_warn_secs`, the first observation and each content change
/// rewrite the heartbeat body; unchanged ticks refresh only its mtime. The
/// heartbeat is removed once when the condition clears (and on shutdown), so
/// a process that never crosses the threshold writes no heartbeat body.
pub(super) struct WalpinSidecarState {
    pub(super) dir: PathBuf,
    pid: u32,
    role: &'static str,
    started_at: i64,
    /// This sweep's own tick cadence, recorded into every beacon and
    /// heartbeat so the enumerating daemon judges freshness against the
    /// PRODUCER's interval — a session on an independently slower configured
    /// cadence must not be misread as stale.
    sweep_interval_ms: u64,
    wrote: bool,
    /// Whether this process's registration beacon is believed present on
    /// disk. Cleared when a failed heartbeat write escalates to beacon
    /// removal (fail-closed — see `observe`) or a beacon touch fails; the
    /// next healthy tick then re-registers with a full write instead of a
    /// metadata touch.
    beacon_registered: bool,
    /// The content actually on disk in the last successful heartbeat body
    /// write, if any (ADR-091 Amendment 3 Plank F1). `None` whenever the
    /// next tick must go through a full write — no heartbeat written yet,
    /// the last write failed, or the threshold cleared. Compared against
    /// each new observation to decide touch (content unchanged) vs.
    /// rewrite (content changed).
    last_heartbeat: Option<LastHeartbeatState>,
}

/// ADR-091 Amendment 3 Plank F1: the content signature of the heartbeat
/// body currently on disk, plus the `oldest_tx_started_at` value that body
/// carries — kept separate from the signature proper because it is derived
/// (fixed for as long as the same span stays oldest), not an independent
/// change signal.
struct LastHeartbeatState {
    span_id: khive_storage::tx_registry::TxId,
    label: Option<String>,
    attribution_basis: &'static str,
    sweep_interval_ms: u64,
    oldest_tx_started_at: i64,
}

impl LastHeartbeatState {
    /// Whether a fresh observation carries exactly the content already on
    /// disk — the licensing condition for a metadata-only touch instead of
    /// a full body rewrite (the first over-threshold observation, a change
    /// of the oldest span's identity or label, a change of
    /// `attribution_basis`, or a change of the declared sweep cadence).
    fn content_matches(
        &self,
        span_id: khive_storage::tx_registry::TxId,
        label: &Option<String>,
        attribution_basis: &str,
        sweep_interval_ms: u64,
    ) -> bool {
        self.span_id == span_id
            && self.label == *label
            && self.attribution_basis == attribution_basis
            && self.sweep_interval_ms == sweep_interval_ms
    }
}

impl WalpinSidecarState {
    /// `None` when the sidecar is disabled for this backend/env, or the
    /// backend has no on-disk path (in-memory).
    pub(super) fn new(
        db_path: Option<&Path>,
        is_file_backed: bool,
        role: &'static str,
        interval: Duration,
    ) -> Option<Self> {
        let path = db_path?;
        if !crate::walpin::sidecar_enabled(is_file_backed) {
            return None;
        }
        let pid = std::process::id();
        Some(Self {
            dir: crate::walpin::sidecar_dir_for(path),
            pid,
            role,
            started_at: crate::walpin::process_start_time_secs(pid).unwrap_or(0),
            sweep_interval_ms: interval.as_millis().min(u64::MAX as u128) as u64,
            wrote: false,
            last_heartbeat: None,
            beacon_registered: false,
        })
    }

    /// Write this process's registration beacon (ADR-091 Amendment 2
    /// sidecar-health attribution). Called once right after construction,
    /// before the sweep loop starts, and again only when a fail-closed
    /// removal or failed touch cleared `beacon_registered` — steady state
    /// stays metadata-touch-only with no data writes. The blocking fs I/O
    /// runs on `spawn_blocking` (perf, ADR-091 Amendment 2): this is
    /// invoked from an async context and must not run synchronous I/O
    /// inline on the async runtime's worker thread.
    pub(super) async fn register_beacon(&mut self) {
        let dir = self.dir.clone();
        let beacon = crate::walpin::WalpinBeacon {
            pid: self.pid,
            process_role: self.role.to_string(),
            started_at: self.started_at,
            sweep_interval_ms: self.sweep_interval_ms,
        };
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::write_beacon(&dir, &beacon)).await;
        match result {
            Ok(Ok(())) => {
                self.beacon_registered = true;
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to write walpin registration beacon; \
                     this process's sidecar health will read as unknown, not registered-silent"
                );
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon write task panicked"
                );
            }
        }
    }

    /// Run one bounded housekeeping pass independently of WAL pressure. The
    /// collector removes only positively dead/reused-PID residue; uncertain
    /// evidence remains for a no-progress attribution pass. Directory work
    /// and report memory are capped, and all blocking filesystem operations
    /// stay off the async runtime worker.
    #[cfg(unix)]
    pub(super) async fn reap_dead_entries_bounded(
        &self,
        legacy_fallback_interval: Duration,
    ) -> Option<crate::walpin::WalpinReport> {
        let dir = self.dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::walpin::housekeep_live(&dir, legacy_fallback_interval)
        })
        .await;
        match result {
            Ok(Ok(report)) => Some(report),
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 6: bounded walpin sidecar cleanup failed"
                );
                None
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 6: walpin sidecar cleanup task panicked"
                );
                None
            }
        }
    }

    /// ADR-091 Amendment 2 beacon refresh rule: a metadata-only mtime touch
    /// of this process's already-registered beacon, performed on every
    /// sweep tick except one where an over-threshold heartbeat write failed
    /// (see `observe`) — `registered-silent` classification requires this
    /// refresh to stay within the freshness window, not just the beacon's
    /// original write. After a fail-closed beacon removal (or a failed
    /// touch), the beacon is re-registered with a full write on the next
    /// healthy tick. Best-effort: a failure here degrades this process to
    /// `unknown` at the next enumeration, not a sweep-task error.
    async fn refresh_beacon(&mut self) {
        if !self.beacon_registered {
            self.register_beacon().await;
            return;
        }
        let dir = self.dir.clone();
        let pid = self.pid;
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::touch_beacon(&dir, pid)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.beacon_registered = false;
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to refresh walpin registration beacon; \
                     this process's sidecar health will read as unknown, not registered-silent"
                );
            }
            Err(join_err) => {
                self.beacon_registered = false;
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon refresh task panicked"
                );
            }
        }
    }

    /// Fail-closed escalation for a failed heartbeat write: remove this
    /// process's beacon so enumeration cannot classify it
    /// `registered-silent` off the still-fresh prior refresh — skipping one
    /// touch alone leaves the previous mtime inside the freshness window
    /// for up to three producer ticks, an exoneration window. With the
    /// beacon gone the process either reports (once writes recover, the
    /// next tick re-registers and writes the heartbeat) or is caught by the
    /// OS-level holder census as an unattributed holder. If the removal
    /// itself fails, the beacon ages out over the freshness window — the
    /// narrowed fallback, not the contract.
    async fn drop_beacon_fail_closed(&mut self) {
        let dir = self.dir.clone();
        let pid = self.pid;
        self.beacon_registered = false;
        let result =
            tokio::task::spawn_blocking(move || crate::walpin::remove_beacon(&dir, pid)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "ADR-091 Amendment 2: failed to remove walpin beacon after a failed \
                     heartbeat write; beacon will age out of the freshness window instead"
                );
            }
            Err(join_err) => {
                tracing::warn!(
                    error = %join_err,
                    "ADR-091 Amendment 2: walpin beacon removal task panicked"
                );
            }
        }
    }

    /// Blocking heartbeat write/removal runs on `spawn_blocking` (perf,
    /// ADR-091 Amendment 2) — this async sweep task must not block its
    /// executor thread on synchronous filesystem I/O.
    pub(super) async fn observe(
        &mut self,
        oldest: Option<khive_storage::tx_registry::OldestSpan>,
        tx_warn_secs: Duration,
    ) {
        match oldest {
            Some(span) if span.age >= tx_warn_secs => {
                // ADR-091 Amendment 3 Plank F2: the caller's `TxOriginFilter`
                // guarantees a `Main` view's winner is either `Database` (this
                // backend's own identity) or `Unscoped` (the fallback), and a
                // `Secondary` view's winner is always `Database` — `Memory`
                // can never win a filtered query, so it degrades to
                // fallback-confidence rather than a reachability panic.
                let attribution_basis = match span.origin {
                    khive_storage::tx_registry::TxOrigin::Database(_) => "origin",
                    khive_storage::tx_registry::TxOrigin::Unscoped
                    | khive_storage::tx_registry::TxOrigin::Memory => "fallback",
                };

                // ADR-091 Amendment 3 Plank F1: a metadata-only mtime touch
                // advances freshness whenever nothing content-relevant has
                // changed since the last body write; a full rewrite happens
                // only on the first over-threshold observation or a genuine
                // content change.
                let content_unchanged = self.wrote
                    && self.last_heartbeat.as_ref().is_some_and(|last| {
                        last.content_matches(
                            span.id,
                            &span.label,
                            attribution_basis,
                            self.sweep_interval_ms,
                        )
                    });

                if content_unchanged {
                    let dir = self.dir.clone();
                    let pid = self.pid;
                    let touch_result = tokio::task::spawn_blocking(move || {
                        crate::walpin::touch_heartbeat(&dir, pid)
                    })
                    .await;
                    match touch_result {
                        Ok(Ok(())) => {
                            self.refresh_beacon().await;
                            return;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                error = %e,
                                "ADR-091 Amendment 3 Plank F1: walpin heartbeat touch failed; \
                                 recreating with a full body write"
                            );
                        }
                        Err(join_err) => {
                            tracing::warn!(
                                error = %join_err,
                                "ADR-091 Amendment 3 Plank F1: walpin heartbeat touch task \
                                 panicked; recreating with a full body write"
                            );
                        }
                    }
                    // Recovery rule: the touch path must never assume the
                    // target still exists — enumeration can delete a slow
                    // writer's heartbeat while its span is still live. Fall
                    // through to the full write below unconditionally.
                }

                // The oldest span's registration instant is fixed for as
                // long as it stays the SAME span: reuse the previously
                // recorded value rather than re-deriving it from `now -
                // age`, which would drift by measurement noise across ticks
                // for no reason. A genuinely new oldest span (or the first
                // observation) derives it fresh.
                let oldest_tx_started_at = self
                    .last_heartbeat
                    .as_ref()
                    .filter(|last| last.span_id == span.id)
                    .map(|last| last.oldest_tx_started_at)
                    .unwrap_or_else(|| now_epoch_secs().saturating_sub(span.age.as_secs() as i64));

                let heartbeat = crate::walpin::WalpinHeartbeat {
                    pid: self.pid,
                    process_role: self.role.to_string(),
                    started_at: self.started_at,
                    oldest_tx_age_secs: span.age.as_secs_f64(),
                    oldest_tx_label: span.label.clone(),
                    oldest_tx_started_at: Some(oldest_tx_started_at),
                    updated_at: now_epoch_secs(),
                    sweep_interval_ms: self.sweep_interval_ms,
                    attribution_basis: Some(attribution_basis.to_string()),
                };
                let dir = self.dir.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::walpin::write_heartbeat(&dir, &heartbeat)
                })
                .await;
                // The beacon refresh is gated on the heartbeat write
                // landing: a fresh beacon with no heartbeat file classifies
                // as `registered-silent` at enumeration, so a failed write
                // would exonerate a process that currently holds an
                // over-threshold transaction. Skipping the refresh alone is
                // not enough — the previous touch stays inside the freshness
                // window for up to three producer ticks — so the failure
                // path removes the beacon outright (`drop_beacon_fail_closed`);
                // the next successful tick re-registers it.
                match result {
                    Ok(Ok(())) => {
                        self.wrote = true;
                        self.last_heartbeat = Some(LastHeartbeatState {
                            span_id: span.id,
                            label: span.label,
                            attribution_basis,
                            sweep_interval_ms: self.sweep_interval_ms,
                            oldest_tx_started_at,
                        });
                        self.refresh_beacon().await;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "ADR-091 Amendment 2 Plank B: failed to write walpin heartbeat; \
                             removing beacon so this process cannot read as \
                             registered-silent while over threshold"
                        );
                        // Unknown what (if anything) is on disk now — the
                        // next tick must go through a full write, never a
                        // touch, until a write actually lands.
                        self.last_heartbeat = None;
                        self.drop_beacon_fail_closed().await;
                    }
                    Err(join_err) => {
                        tracing::warn!(
                            error = %join_err,
                            "ADR-091 Amendment 2 Plank B: walpin heartbeat write task panicked"
                        );
                        self.last_heartbeat = None;
                        self.drop_beacon_fail_closed().await;
                    }
                }
            }
            _ => {
                self.refresh_beacon().await;
                if self.wrote {
                    let dir = self.dir.clone();
                    let pid = self.pid;
                    let result = tokio::task::spawn_blocking(move || {
                        crate::walpin::remove_heartbeat(&dir, pid)
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(
                            error = %e,
                            "ADR-091 Amendment 2 Plank B: failed to remove walpin heartbeat"
                        ),
                        Err(join_err) => tracing::warn!(
                            error = %join_err,
                            "ADR-091 Amendment 2 Plank B: walpin heartbeat removal task panicked"
                        ),
                    }
                    self.wrote = false;
                    self.last_heartbeat = None;
                }
            }
        }
    }

    pub(super) async fn shutdown(&mut self) {
        if self.wrote {
            let dir = self.dir.clone();
            let pid = self.pid;
            let _ = tokio::task::spawn_blocking(move || crate::walpin::remove_heartbeat(&dir, pid))
                .await;
            self.wrote = false;
        }
    }
}

#[cfg(unix)]
pub(super) async fn run_walpin_housekeeping_if_due(
    sidecar: &WalpinSidecarState,
    state: &mut TruncateState,
    legacy_fallback_interval: Duration,
) -> bool {
    if !state.housekeeping_due() || !state.claim_walpin_full_scan_at(Instant::now()) {
        return false;
    }
    if let Some(report) = sidecar
        .reap_dead_entries_bounded(legacy_fallback_interval)
        .await
    {
        state.cache_walpin_attribution(
            report,
            Err("OS holder census is unavailable for a housekeeping-only scan".to_string()),
            Instant::now(),
        );
    }
    true
}

pub(super) fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
