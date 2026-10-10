//! Checkpoint escalation and transaction-age state machines.

use std::time::{Duration, Instant};

use super::CheckpointConfig;
#[cfg(unix)]
use super::{
    CachedWalpinAttribution, WalpinFullScanPlan, DEFAULT_SESSION_SWEEP_INTERVAL,
    DEFAULT_WALPIN_FULL_SCAN_INTERVAL,
};

/// Mutable escalation state carried across ticks by the caller (ADR-091 Plank 2).
///
/// Kept separate from [`CheckpointConfig`] because it is *state*, not
/// configuration: `last_attempt` and `consecutive_failures` mutate every tick,
/// while `CheckpointConfig` is parsed once and held immutable for the life of
/// the task.
#[derive(Debug)]
pub struct TruncateState {
    /// When the last TRUNCATE *attempt* ran (armed + writer held), regardless
    /// of whether it succeeded in reclaiming pages. `None` means no attempt
    /// has ever run, so the first armed tick is immediately eligible.
    pub(super) last_attempt: Option<Instant>,
    /// Count of measured TRUNCATE outcomes that failed to bring `wal_pages`
    /// below `warn_pages`, ignoring attempts whose post-TRUNCATE measurement
    /// was unavailable. A measured clearing result resets it; a one-shot
    /// escalated WARN fires at exactly 3 failures.
    pub(super) consecutive_failures: u32,
    /// Fallback freshness cadence for legacy sidecar records that do not
    /// declare their producer interval. Captured once when the daemon task
    /// starts; this is ADR-091's compiled 5000 ms session-sweep default, never
    /// the daemon's faster checkpoint cadence or a local environment override.
    #[cfg(unix)]
    pub(super) legacy_walpin_fallback_interval: Duration,
    /// Minimum spacing between full sidecar/OS-holder enumeration attempts.
    /// The attempt timestamp advances before blocking work starts, so an I/O
    /// failure or worker panic cannot turn sustained pressure into a hot retry
    /// loop. A successful report is retained only for diagnostic reuse.
    #[cfg(unix)]
    pub(super) walpin_full_scan_interval: Duration,
    #[cfg(unix)]
    pub(super) walpin_full_scan_last_attempt: Option<Instant>,
    #[cfg(unix)]
    pub(super) walpin_cached_attribution: Option<CachedWalpinAttribution>,
    /// Whether the no-progress attribution arm already attempted the one
    /// bounded sidecar enumeration allowed for this checkpoint tick.
    #[cfg(unix)]
    pub(super) sidecar_attribution_attempted_this_tick: bool,
}

impl Default for TruncateState {
    fn default() -> Self {
        Self {
            last_attempt: None,
            consecutive_failures: 0,
            #[cfg(unix)]
            legacy_walpin_fallback_interval: DEFAULT_SESSION_SWEEP_INTERVAL,
            #[cfg(unix)]
            walpin_full_scan_interval: DEFAULT_WALPIN_FULL_SCAN_INTERVAL,
            #[cfg(unix)]
            walpin_full_scan_last_attempt: None,
            #[cfg(unix)]
            walpin_cached_attribution: None,
            #[cfg(unix)]
            sidecar_attribution_attempted_this_tick: false,
        }
    }
}

impl TruncateState {
    #[cfg(unix)]
    pub(super) fn with_legacy_walpin_fallback(interval: Duration) -> Self {
        Self {
            legacy_walpin_fallback_interval: interval,
            ..Self::default()
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn with_walpin_full_scan_cadence(interval: Duration) -> Self {
        Self {
            walpin_full_scan_interval: interval,
            ..Self::default()
        }
    }

    #[cfg(unix)]
    pub(super) fn begin_tick(&mut self) {
        self.sidecar_attribution_attempted_this_tick = false;
    }

    #[cfg(unix)]
    pub(super) fn housekeeping_due(&self) -> bool {
        !self.sidecar_attribution_attempted_this_tick
            && self.walpin_full_scan_due_at(Instant::now())
    }

    #[cfg(unix)]
    fn walpin_full_scan_due_at(&self, now: Instant) -> bool {
        self.walpin_full_scan_last_attempt.is_none_or(|last| {
            now.saturating_duration_since(last) >= self.walpin_full_scan_interval
        })
    }

    #[cfg(unix)]
    pub(super) fn claim_walpin_full_scan_at(&mut self, now: Instant) -> bool {
        if !self.walpin_full_scan_due_at(now) {
            return false;
        }
        self.walpin_full_scan_last_attempt = Some(now);
        true
    }

    #[cfg(unix)]
    pub(super) fn plan_walpin_attribution_at(&mut self, now: Instant) -> WalpinFullScanPlan {
        if self.walpin_full_scan_due_at(now) {
            let previous_last_attempt = self.walpin_full_scan_last_attempt.replace(now);
            WalpinFullScanPlan::Refresh {
                previous_last_attempt,
            }
        } else if let Some(cached) = self.walpin_cached_attribution.clone() {
            WalpinFullScanPlan::Cached(cached)
        } else {
            WalpinFullScanPlan::Suppressed
        }
    }

    #[cfg(unix)]
    pub(super) fn restore_walpin_full_scan_reservation(
        &mut self,
        previous_last_attempt: Option<Instant>,
    ) {
        self.walpin_full_scan_last_attempt = previous_last_attempt;
    }

    #[cfg(unix)]
    pub(super) fn cache_walpin_attribution(
        &mut self,
        report: crate::walpin::WalpinReport,
        census: Result<crate::walpin::CensusResult, String>,
        captured_at: Instant,
    ) {
        self.walpin_cached_attribution = Some(CachedWalpinAttribution {
            report,
            census,
            captured_at,
        });
    }
}

/// ADR-091 graduated severity rung for sustained WAL pressure.
///
/// `Alarm` is never produced by [`CheckpointSeverityState::observe_wal_pages`]
/// — it labels the existing TRUNCATE-escalation tier (`maybe_truncate`),
/// which is gated on its own threshold/interval state, not on this ladder.
/// It exists here so callers and tests can name all three rungs uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointSeverityRung {
    /// First observed tick crossing `warn_pages` after a below-warn tick.
    Info,
    /// `warn_sustained_cycles` consecutive observed ticks at/above
    /// `warn_pages`; edge-triggered once per elevation episode.
    Warn,
    /// The TRUNCATE-escalation tier (`checkpoint_high_water_pages` and
    /// above); never emitted by `observe_wal_pages`.
    Alarm,
}

/// ADR-091 severity ladder state, carried across ticks by the caller
/// alongside [`TruncateState`]. Pure state machine: no I/O, no logging —
/// callers turn the returned emissions into `tracing` calls.
#[derive(Debug, Default, Clone)]
pub struct CheckpointSeverityState {
    /// Whether the previous observed tick was at/above `warn_pages`. Drives
    /// the below→above edge that fires INFO.
    was_above_warn: bool,
    /// Run-length of consecutive observed ticks at/above `warn_pages` in the
    /// current elevation episode. Resets to 0 on any below-warn tick.
    consecutive_above_warn: u8,
    /// Whether WARN has already fired for the current elevation episode, so
    /// sustained pressure logs WARN once per episode, not once per tick past
    /// the threshold.
    pub(super) warn_emitted_for_episode: bool,
}

/// One severity-ladder emission produced by a single
/// [`CheckpointSeverityState::observe_wal_pages`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSeverityEmission {
    /// Which rung this emission represents (`Info` or `Warn`; see
    /// [`CheckpointSeverityRung::Alarm`] doc for why `Alarm` never appears
    /// here).
    pub rung: CheckpointSeverityRung,
    /// The WAL page count observed on the tick that produced this emission.
    pub wal_pages: u64,
    /// The `warn_pages` threshold in effect for this tick.
    pub threshold_pages: u64,
    /// Consecutive above-warn cycle count as of this tick (1 on the INFO
    /// edge, `warn_sustained_cycles` on the WARN edge).
    pub consecutive_cycles: u8,
}

impl CheckpointSeverityState {
    /// Advance the severity ladder by one observed tick and return every
    /// rung crossed on this tick (zero, one, or two emissions: a fresh
    /// elevation episode can produce INFO and, if `warn_sustained_cycles`
    /// is 1, WARN on the very same tick).
    ///
    /// A below-warn tick resets both the consecutive-cycle counter and the
    /// per-episode WARN latch, re-arming INFO/WARN for a later episode.
    /// Skipped ticks must not be passed here at all — the caller only calls
    /// this on `CheckpointTick::Observed`, matching the existing
    /// threshold-crossing WARN's skip-leaves-state-unchanged rule.
    pub fn observe_wal_pages(
        &mut self,
        wal_pages: u64,
        config: &CheckpointConfig,
    ) -> Vec<CheckpointSeverityEmission> {
        let mut emissions = Vec::new();
        let above_warn = wal_pages >= config.warn_pages;

        if above_warn {
            self.consecutive_above_warn = self.consecutive_above_warn.saturating_add(1);

            if !self.was_above_warn {
                emissions.push(CheckpointSeverityEmission {
                    rung: CheckpointSeverityRung::Info,
                    wal_pages,
                    threshold_pages: config.warn_pages,
                    consecutive_cycles: self.consecutive_above_warn,
                });
            }

            if !self.warn_emitted_for_episode
                && self.consecutive_above_warn >= config.warn_sustained_cycles
            {
                emissions.push(CheckpointSeverityEmission {
                    rung: CheckpointSeverityRung::Warn,
                    wal_pages,
                    threshold_pages: config.warn_pages,
                    consecutive_cycles: self.consecutive_above_warn,
                });
                self.warn_emitted_for_episode = true;
            }
        } else {
            self.consecutive_above_warn = 0;
            self.warn_emitted_for_episode = false;
        }

        self.was_above_warn = above_warn;
        emissions
    }
}

/// ADR-091 Plank 1 rung for the open-transaction registry's background age
/// sweep: independent of the WAL-pressure ladder above, keyed purely off how
/// long the registry's oldest entry has been open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxAgeRung {
    /// The oldest registry entry's age crossed `tx_warn_secs`.
    Warn,
    /// The oldest registry entry's age crossed `tx_max_age_secs` — the ADR's
    /// "cooperative stale-op guard" cap. No in-process mechanism force-closes
    /// it (see [`CheckpointConfig::tx_max_age_secs`]); this rung is the
    /// sweep's strongest available signal.
    Stale,
}

/// One emission produced by a single [`TxAgeSweepState::observe`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxAgeEmission {
    pub rung: TxAgeRung,
    pub age: Duration,
    pub label: Option<String>,
}

/// ADR-091 Plank 1 background-sweep state, carried across ticks by the
/// caller alongside [`CheckpointSeverityState`] and [`TruncateState`]. Pure
/// state machine: no I/O, no logging — callers turn the returned emissions
/// into `tracing` calls, mirroring [`CheckpointSeverityState`]'s shape.
///
/// Keyed off `khive_storage::tx_registry::oldest()` — the single oldest
/// entry across every registered span, regardless of which call site created
/// it. Deliberately a different signal from the WAL-pressure ladder: a span
/// can go stale under low WAL pressure, or vice versa. See
/// `crates/khive-db/docs/api/checkpoint.md` for the full rationale.
#[derive(Debug, Default, Clone)]
pub struct TxAgeSweepState {
    /// Whether the previous observed tick's oldest entry was at/above
    /// `tx_warn_secs`. Drives the below→above edge that fires `Warn`.
    was_above_warn: bool,
    /// Whether the previous observed tick's oldest entry was at/above
    /// `tx_max_age_secs`. Drives the below→above edge that fires `Stale`.
    was_above_max_age: bool,
    /// Identity of the entry the previous observed tick reported as oldest,
    /// or `None` if the registry was empty. Tracked separately from the two
    /// latches above so a change in *which span* is oldest can be detected
    /// even when both latches are already `true` (see [`Self::observe`]).
    tracked_id: Option<khive_storage::tx_registry::TxId>,
}

impl TxAgeSweepState {
    /// Advance by one observed tick given the registry's current oldest
    /// entry (identity, age, label), or `None` if empty. Returns zero, one,
    /// or two emissions — an entry already stale the first time it's seen
    /// under a given identity crosses both rungs on the same tick.
    ///
    /// A below-threshold (or absent) oldest entry resets both latches. A
    /// change in the oldest entry's [`TxId`](khive_storage::tx_registry::TxId)
    /// also force-resets both latches before re-evaluating age, so a
    /// departed span's latched state cannot suppress the crossing for an
    /// already-stale successor. See `crates/khive-db/docs/api/checkpoint.md`
    /// for why identity tracking is required here, not just the age check.
    pub fn observe(
        &mut self,
        oldest: Option<(khive_storage::tx_registry::TxId, Duration, Option<String>)>,
        tx_warn_secs: Duration,
        tx_max_age_secs: Duration,
    ) -> Vec<TxAgeEmission> {
        let mut emissions = Vec::new();

        let Some((id, age, label)) = oldest else {
            self.was_above_warn = false;
            self.was_above_max_age = false;
            self.tracked_id = None;
            return emissions;
        };

        if self.tracked_id != Some(id) {
            self.was_above_warn = false;
            self.was_above_max_age = false;
        }
        self.tracked_id = Some(id);

        let above_warn = age >= tx_warn_secs;
        let above_max_age = age >= tx_max_age_secs;

        if above_warn && !self.was_above_warn {
            emissions.push(TxAgeEmission {
                rung: TxAgeRung::Warn,
                age,
                label: label.clone(),
            });
        }
        if above_max_age && !self.was_above_max_age {
            emissions.push(TxAgeEmission {
                rung: TxAgeRung::Stale,
                age,
                label,
            });
        }

        self.was_above_warn = above_warn;
        self.was_above_max_age = above_max_age;
        emissions
    }
}
