//! Checkpoint configuration, defaults, and shared transaction-age thresholds.

use std::time::Duration;

#[cfg(doc)]
use super::{CheckpointSeverityState, SessionSweepConfig};

/// Default number of consecutive above-`warn_pages` observed ticks required
/// to escalate from the INFO to the WARN rung of the ADR-091 severity ladder.
pub const DEFAULT_WARN_SUSTAINED_CYCLES: u8 = 3;

/// Configuration for the WAL checkpoint background task.
///
/// All fields default to conservative production values. Override via the
/// environment variables documented on each field.
#[derive(Clone, Debug)]
pub struct CheckpointConfig {
    /// How often to run a passive checkpoint when there is no active write.
    ///
    /// Overridable via `KHIVE_CHECKPOINT_INTERVAL_MS` (milliseconds).
    /// Default: 500 ms.
    pub interval: Duration,

    /// WAL page count above which a warning is logged.
    ///
    /// Overridable via `KHIVE_WAL_WARN_PAGES`.
    /// Default: 2000 pages (~8 MB at 4 KiB page size).
    pub warn_pages: u64,

    /// Number of consecutive observed ticks with `wal_pages >= warn_pages`
    /// required before the ADR-091 severity ladder escalates from INFO
    /// (first crossing) to WARN (sustained pressure). Edge-triggered once
    /// per elevation episode — see [`CheckpointSeverityState`].
    ///
    /// Overridable via `KHIVE_WAL_WARN_SUSTAINED_CYCLES`.
    /// Default: 3 cycles.
    pub warn_sustained_cycles: u8,

    /// WAL page count above which a high-pressure WARNING is logged.
    ///
    /// The periodic task always runs PASSIVE regardless; this threshold signals
    /// only that the WAL is not draining. Whether an old snapshot is pinning it
    /// is informed at the crossing by the in-process transaction registry,
    /// against `tx_warn_secs` — see `log_wal_high_water_warn`. This registry
    /// cannot exclude readers in another process. Either way an
    /// operator can schedule a blocking TRUNCATE at a safe moment outside
    /// normal write traffic; the two cases differ in what else is worth doing.
    ///
    /// Overridable via `KHIVE_WAL_HIGH_WATER_PAGES`.
    /// Default: 6000 pages (~24 MB at 4 KiB page size).
    pub high_water_pages: u64,

    /// WAL page count above which a TRUNCATE escalation attempt is armed
    /// (ADR-091 Plank 2).
    ///
    /// This is a separate, much higher threshold than `high_water_pages`:
    /// crossing it does not itself attempt TRUNCATE — it only arms the
    /// attempt, which additionally requires `truncate_min_interval` to have
    /// elapsed since the last attempt.
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES`.
    /// Default: 20000 pages.
    pub truncate_high_water_pages: u64,

    /// Minimum spacing between TRUNCATE *attempts* (not successes).
    ///
    /// A skipped tick (dedicated connection unavailable, below threshold, or
    /// interval not yet elapsed) never advances the "last attempt" clock, so
    /// the next tick where the connection is available and the threshold is
    /// still crossed is immediately eligible rather than waiting out the
    /// full interval again.
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS`.
    /// Default: 300 seconds (5 minutes).
    pub truncate_min_interval: Duration,

    /// Temporary `busy_timeout` used only for the duration of a TRUNCATE
    /// attempt, restored to the pool's configured busy timeout immediately
    /// after the attempt completes (win or lose).
    ///
    /// Overridable via `KHIVE_WAL_TRUNCATE_BUSY_MS`.
    /// Default: 2000 ms.
    pub truncate_busy_timeout: Duration,

    /// ADR-091 Plank 1 soft cap: age past which the oldest entry in the
    /// shared open-transaction registry is surfaced at `tracing::warn!` on
    /// every tick (Skipped or Observed), independent of WAL page pressure.
    /// See `crates/khive-db/docs/api/checkpoint.md` for the Plank 1 rationale.
    ///
    /// Overridable via `KHIVE_TX_WARN_SECS`.
    /// Default: 30 seconds.
    pub tx_warn_secs: Duration,

    /// ADR-091 Plank 1 hard cap: age past which the same sweep escalates the
    /// oldest registry entry to `tracing::error!`. The sweep itself is
    /// visibility only — nothing in `TxAgeSweepState` force-closes a stale
    /// span. `sql_bridge.rs`'s cached-reader read-transaction path shares
    /// this exact value (via `PoolConfig::read_tx_max_age`, #1846) to
    /// actually roll back and evict an explicit read transaction the next
    /// time its handle is reused past this age — reclamation for the
    /// "reused periodically" case, not the "held idle with no further calls"
    /// case the ADR named as its accepted gap; see
    /// `crates/khive-db/docs/api/checkpoint.md`'s Plank 1 section for the
    /// distinction and why the latter remains open design work.
    ///
    /// Overridable via `KHIVE_TX_MAX_AGE_SECS`.
    /// Default: 120 seconds.
    pub tx_max_age_secs: Duration,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            warn_pages: 2000,
            warn_sustained_cycles: DEFAULT_WARN_SUSTAINED_CYCLES,
            high_water_pages: 6000,
            truncate_high_water_pages: 20_000,
            truncate_min_interval: Duration::from_secs(300),
            truncate_busy_timeout: Duration::from_millis(2000),
            tx_warn_secs: Duration::from_secs(30),
            tx_max_age_secs: Duration::from_secs(120),
        }
    }
}

impl CheckpointConfig {
    /// Build a `CheckpointConfig` from the environment.
    ///
    /// Unset or unparseable variables fall back to the compiled-in defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(ms) = std::env::var("KHIVE_CHECKPOINT_INTERVAL_MS") {
            if let Ok(v) = ms.parse::<u64>() {
                if v > 0 {
                    cfg.interval = Duration::from_millis(v);
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_WARN_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.warn_pages = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_WARN_SUSTAINED_CYCLES") {
            if let Ok(n) = v.parse::<u8>() {
                if n > 0 {
                    cfg.warn_sustained_cycles = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_HIGH_WATER_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.high_water_pages = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_high_water_pages = n;
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_MIN_INTERVAL_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_min_interval = Duration::from_secs(n);
                }
            }
        }

        if let Ok(v) = std::env::var("KHIVE_WAL_TRUNCATE_BUSY_MS") {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    cfg.truncate_busy_timeout = Duration::from_millis(n);
                }
            }
        }

        (cfg.tx_warn_secs, cfg.tx_max_age_secs) =
            tx_age_thresholds_from_env(cfg.tx_warn_secs, cfg.tx_max_age_secs);

        cfg
    }
}

/// Parse `KHIVE_TX_WARN_SECS`/`KHIVE_TX_MAX_AGE_SECS` against the given
/// defaults, applying the same ordering guard both [`CheckpointConfig`] and
/// [`SessionSweepConfig`] need (minor, ADR-091 Amendment 2: this was
/// previously duplicated verbatim in both `from_env` methods).
///
/// The severity ladder assumes `tx_warn_secs < tx_max_age_secs` (Warn fires
/// before Stale as an entry ages). A reversed or equal pair — whether from
/// one misconfigured var or the interaction of both — would invert or
/// collapse that ordering (e.g. WARN_SECS=120, MAX_AGE_SECS=30 emits Stale at
/// 30s and never reaches the Warn crossing until 120s), so both are rejected
/// together rather than silently honored. Resetting both to the caller's
/// defaults (rather than just clamping one) avoids guessing which of the two
/// the operator actually meant to change.
pub(crate) fn tx_age_thresholds_from_env(
    default_warn: Duration,
    default_max: Duration,
) -> (Duration, Duration) {
    let mut warn_secs = default_warn;
    let mut max_age_secs = default_max;

    if let Ok(v) = std::env::var("KHIVE_TX_WARN_SECS") {
        if let Ok(n) = v.parse::<u64>() {
            if n > 0 {
                warn_secs = Duration::from_secs(n);
            }
        }
    }

    if let Ok(v) = std::env::var("KHIVE_TX_MAX_AGE_SECS") {
        if let Ok(n) = v.parse::<u64>() {
            if n > 0 {
                max_age_secs = Duration::from_secs(n);
            }
        }
    }

    if warn_secs >= max_age_secs {
        tracing::warn!(
            configured_tx_warn_secs = warn_secs.as_secs_f64(),
            configured_tx_max_age_secs = max_age_secs.as_secs_f64(),
            fallback_tx_warn_secs = default_warn.as_secs_f64(),
            fallback_tx_max_age_secs = default_max.as_secs_f64(),
            "KHIVE_TX_WARN_SECS must be strictly less than KHIVE_TX_MAX_AGE_SECS; \
             both transaction-age thresholds were rejected and reset to their defaults"
        );
        return (default_warn, default_max);
    }

    (warn_secs, max_age_secs)
}
