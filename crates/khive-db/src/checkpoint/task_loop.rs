//! Periodic checkpoint work and pressure-episode transitions.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::pool::ConnectionPool;

#[cfg(unix)]
use super::{
    complete_walpin_attribution, run_walpin_housekeeping_if_due, WalpinSidecarState,
    DEFAULT_SESSION_SWEEP_INTERVAL,
};
use super::{
    crossing_warn, log_tx_age_emission, log_tx_registry_oldest_debug, log_tx_registry_oldest_warn,
    log_tx_registry_snapshot_warn, log_wal_high_water_warn, note_checkpoint_pressure_observation,
    note_checkpoint_skipped, off_worker, CheckpointConfig, CheckpointConnection,
    CheckpointLifecycleEmitter, CheckpointLifecycleOwner, CheckpointPressureEpisode,
    CheckpointRunTaskGuard, CheckpointSeverityRung, CheckpointSeverityState, CheckpointTick,
    ReaderCheckoutSweepState, TruncateState, TxAgeSweepState, CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS,
};

/// Run one due FTS5 maintenance step off this task's Tokio worker thread.
///
/// A due step can issue up to `config.merge_pages` pages of synchronous
/// SQLite incremental-merge I/O against a trigram index over a corpus of
/// hundreds of thousands of rows — the same class of blocking work the
/// WAL-pin beacon writes above already move off the worker via
/// `tokio::task::spawn_blocking`. An ordinary write owner is constructed
/// inside the blocking closure; the checkpoint file is never lent to FTS DML.
/// Scheduler state returns on non-panicking paths. A panic surfaces as the
/// `JoinError`, and the owner settles before releasing its lease.
pub(super) async fn run_fts_maintenance_off_worker(
    pool: Arc<ConnectionPool>,
    config: crate::fts_maintenance::FtsMaintenanceConfig,
    mut state: crate::fts_maintenance::FtsMaintenanceState,
    now: Instant,
) -> Result<
    (
        crate::fts_maintenance::FtsMaintenanceState,
        Result<Option<crate::fts_maintenance::FtsMaintenanceStep>, String>,
    ),
    tokio::task::JoinError,
> {
    tokio::task::spawn_blocking(move || {
        let result = pool.run_fts_maintenance(&config, &mut state, now);
        (state, result)
    })
    .await
}

/// Run the WAL checkpoint background task.
///
/// Long-running async task — spawn with `tokio::spawn`. Loops until
/// `shutdown_rx` observes a change (or its sender is dropped). Callers MUST
/// hold the paired `tokio::sync::watch::Sender` for the daemon's run scope
/// and send on it to shut down — do NOT rely on `pool`'s `Arc` refcount
/// reaching zero; a sibling owner (e.g. `event_store`) holding its own clone
/// makes that check unreachable (issue #774).
///
/// Issues `PRAGMA wal_checkpoint(PASSIVE)` every tick on the task's dedicated
/// `CheckpointConnection` — never the pool's writer mutex, so a concurrent
/// `pool.writer()` checkout can never queue behind a checkpoint tick. That
/// guarantee is admission-only: an armed TRUNCATE still takes SQLite's writer
/// lock and can block new write transactions, on any connection, for up to
/// `truncate_busy_timeout` (see `CheckpointConnection`'s contract). The
/// checkpoint call itself runs on `spawn_blocking`, so that wait never holds
/// one of the runtime's worker threads. A tick is
/// `Skipped` when that connection is unavailable or SQLite returns a busy
/// PASSIVE row without a usable pressure observation. A
/// WARNING fires once per below→above threshold crossing, not every tick.
///
/// `lifecycle_owner` (ADR-094): exactly one task in a multi-backend fan-out
/// should receive `Some`. That task appends a best-effort
/// `CheckpointOutcomeRecorded` event on the elevation transition and one
/// recovery summary when pressure falls back below `warn_pages`. Sustained
/// elevated ticks aggregate in memory and in `db_diagnostics`; they never
/// write one primary-store row per checkpoint attempt. `None` explicitly
/// marks a non-owner. See `crates/khive-db/docs/api/checkpoint.md` for the
/// full shutdown-mechanism and event-emission design history.
///
/// `is_main` (ADR-091 Amendment 3): whether `pool` is the deployment's main
/// backend. A daemon owning several file-backed backends spawns one task per
/// backend, each with its own pool and shutdown-channel clone (the sender
/// broadcasts to every receiver clone alike). Lifecycle ownership is selected
/// independently through `lifecycle_owner`; `is_main` only controls registry
/// filtering. See the `tx_filter` construction below.
pub async fn run_checkpoint_task(
    pool: Arc<ConnectionPool>,
    config: CheckpointConfig,
    lifecycle_owner: Option<CheckpointLifecycleOwner>,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
    is_main: bool,
) {
    let _checkpoint_run_guard = CheckpointRunTaskGuard::start(&pool, config.interval);
    // This task IS the dedicated checkpoint owner: claim the pool so writer
    // connections drop the bounded autocheckpoint fallback and routine
    // checkpoint I/O stays off application commit paths. Pools without a
    // running checkpoint task never claim and keep SQLite's bounded WAL
    // reclamation. A failed claim leaves connections on the bounded fallback
    // — safe, just not the low-latency posture — so it warns and continues.
    match pool.claim_checkpoint_ownership() {
        Ok(()) => {
            if let Err(e) = pool.propagate_checkpoint_claim_to_writer_task().await {
                tracing::warn!(target: "khive_db::checkpoint",
                    error = %e,
                    "checkpoint task could not reach the writer task's connection; it keeps the \
                     bounded autocheckpoint fallback"
                );
            }
        }
        Err(e) => {
            tracing::warn!(target: "khive_db::checkpoint",
                error = %e,
                "checkpoint task could not re-apply the ownership pragma on the pooled writer; \
                 writer connections keep the bounded autocheckpoint fallback unless ownership is \
                 claimed later"
            );
        }
    }
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut severity_state = CheckpointSeverityState::default();
    let mut tx_age_state = TxAgeSweepState::default();
    let mut reader_checkout_state = ReaderCheckoutSweepState::default();
    let mut was_above_high_water = false;
    #[cfg(unix)]
    let legacy_walpin_fallback_interval = DEFAULT_SESSION_SWEEP_INTERVAL;
    #[cfg(unix)]
    let mut truncate_state =
        TruncateState::with_legacy_walpin_fallback(legacy_walpin_fallback_interval);
    #[cfg(not(unix))]
    let mut truncate_state = TruncateState::default();
    let mut lifecycle_emitter = CheckpointLifecycleEmitter::new(lifecycle_owner);
    // Independent of `severity_state` (which owns the WARN ladder): this
    // tracks the lifecycle sink's accepted elevation state. A full queue
    // leaves it unchanged, so an opening or recovery transition is retried
    // without admitting more than one primary-store write for that edge.
    let mut event_elevation_open = false;
    let mut pressure_episode: Option<CheckpointPressureEpisode> = None;
    // A recovery row whose `try_emit` lost the race against a full queue.
    // Retried on later ticks (before that tick's own transition handling)
    // instead of leaving `pressure_episode` open for a stale episode to
    // absorb the next, genuinely separate, pressure incident (#1857).
    let mut pending_recovery: Option<khive_storage::CheckpointOutcomeRecordedPayload> = None;
    let mut was_observed_above_warn = false;
    // ADR-091 Amendment 3: this task's own backend-scoped view of the
    // registry. `is_main` selects which `TxOriginFilter` variant applies —
    // the caller passes `true` for exactly the one checkpoint task covering
    // the deployment's main backend, so only that task also observes legacy
    // `Unscoped` spans from any call site not yet threaded to an origin, the
    // designed never-silently-drop fallback. A secondary backend's task
    // never falls back to `Unscoped`: those spans belong to the main view or
    // to no view, never to a database they were never registered against.
    // `None` only when this pool's own origin isn't `Database` (an in-memory
    // checkpoint pool) — degrades to "no open span observed" for the tick
    // rather than panicking a long-running daemon loop on an
    // assumed-impossible state.
    let tx_filter = match pool.origin() {
        khive_storage::tx_registry::TxOrigin::Database(id) => Some(if is_main {
            khive_storage::tx_registry::TxOriginFilter::Main(id)
        } else {
            khive_storage::tx_registry::TxOriginFilter::Secondary(id)
        }),
        khive_storage::tx_registry::TxOrigin::Memory
        | khive_storage::tx_registry::TxOrigin::Unscoped => None,
    };
    // ADR-091 Amendment 2 Plank B: the checkpoint pool is only ever wired for
    // file-backed backends (`checkpoint_pool_for`), so `is_file_backed: true`
    // is always correct here. `canonical_path()` (not `pool.config().path`)
    // so the sidecar directory is keyed off the same minted identity every
    // alias of this backend's configured path converges to.
    #[cfg(unix)]
    let mut walpin_state =
        WalpinSidecarState::new(pool.canonical_path(), true, "daemon", config.interval);
    #[cfg(unix)]
    if let Some(sidecar) = walpin_state.as_mut() {
        sidecar.register_beacon().await;
    }

    // Opened once here, at task startup; `ensure_open` is a no-op in steady
    // state and only reopens after a connection-level failure or (for an
    // in-memory/read-only pool) retries the open on every subsequent tick.
    let mut checkpoint_conn = CheckpointConnection::new();
    checkpoint_conn.ensure_open(&pool);
    // FTS5 segment maintenance uses an admitted write owner at its own
    // cadence. Each due call performs at most one bounded
    // merge step on one table and refuses immediately when another writer
    // owns SQLite's write lock.
    let mut fts_maintenance_config = crate::fts_maintenance::FtsMaintenanceConfig::from_env();
    // Secondary backends have independent schemas (for example the code-map
    // database) and are not required to contain the substrate FTS tables.
    // Exactly the main backend owns this derived-index maintenance.
    fts_maintenance_config.enabled &= is_main;
    let mut fts_maintenance_state =
        crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());

    loop {
        // A closed sender (the daemon returning without an explicit send)
        // makes `changed()` resolve with `Err` immediately, which `select!`
        // treats as ready — so shutdown is observed either way, not just on
        // an explicit send.
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown_rx.changed() => break,
        }

        #[cfg(unix)]
        truncate_state.begin_tick();

        #[cfg(unix)]
        let mut pending_sidecar_attribution = None;

        let tick = if checkpoint_conn.ensure_open(&pool).is_none() {
            note_checkpoint_skipped();
            CheckpointTick::Skipped
        } else {
            // `ensure_open` above just confirmed a connection is open. Take
            // ownership of it so the checkpoint cycle and the FTS maintenance
            // step below can each move it onto a blocking thread; every path
            // either restores it to `checkpoint_conn` or lets it drop, which is
            // the moved-ownership equivalent of the former `drop_connection()`
            // call.
            let conn = checkpoint_conn
                .conn
                .take()
                .expect("ensure_open just confirmed a connection is open");
            match off_worker::run_checkpoint_core_off_worker(
                Arc::clone(&pool),
                conn,
                config.clone(),
                truncate_state,
            )
            .await
            {
                Ok((conn, state, Ok(outcome))) => {
                    truncate_state = state;
                    #[cfg(unix)]
                    {
                        pending_sidecar_attribution = outcome.sidecar_attribution;
                    }
                    #[cfg(not(unix))]
                    let _ = outcome.sidecar_attribution;

                    // Restore the recovery file before starting independent
                    // FTS work. Only a due tick constructs an admitted writer
                    // on a blocking thread; other ticks record a no-op.
                    checkpoint_conn.conn = Some(conn);
                    let fts_result = if !fts_maintenance_state
                        .is_due(&fts_maintenance_config, Instant::now())
                    {
                        Ok(None)
                    } else {
                        match run_fts_maintenance_off_worker(
                            Arc::clone(&pool),
                            fts_maintenance_config.clone(),
                            fts_maintenance_state,
                            Instant::now(),
                        )
                        .await
                        {
                            Ok((state, fts_result)) => {
                                fts_maintenance_state = state;
                                fts_result
                            }
                            Err(join_err) => {
                                // The step panicked on the blocking thread. The
                                // scheduler state moved into it is gone; its
                                // admitted writer has settled. The independent
                                // checkpoint connection remains usable.
                                // The checkpoint pragma itself already succeeded.
                                fts_maintenance_state =
                                    crate::fts_maintenance::FtsMaintenanceState::new(Instant::now());
                                Err(format!(
                                    "bounded FTS5 segment maintenance task panicked: {join_err}"
                                ))
                            }
                        }
                    };

                    match fts_result {
                        Ok(Some(step)) => match step.outcome {
                            crate::fts_maintenance::FtsMaintenanceOutcome::Worked => {
                                tracing::info!(target: "khive_db::checkpoint",
                                    table = step.table,
                                    requested_pages = step.requested_pages,
                                    segments_before = step.segments_before,
                                    segments_after = step.segments_after,
                                    "bounded FTS5 segment maintenance made progress"
                                );
                            }
                            crate::fts_maintenance::FtsMaintenanceOutcome::Busy => {
                                tracing::debug!(target: "khive_db::checkpoint",
                                    table = step.table,
                                    requested_pages = step.requested_pages,
                                    segments = step.segments_before,
                                    "bounded FTS5 segment maintenance skipped a busy writer"
                                );
                            }
                            crate::fts_maintenance::FtsMaintenanceOutcome::Noop
                            | crate::fts_maintenance::FtsMaintenanceOutcome::BelowThreshold => {
                                tracing::debug!(target: "khive_db::checkpoint",
                                    table = step.table,
                                    outcome = ?step.outcome,
                                    segments = step.segments_before,
                                    "bounded FTS5 segment maintenance had no work"
                                );
                            }
                        },
                        Ok(None) => {}
                        Err(error) => {
                            // The checkpoint pragma already succeeded. An FTS
                            // structure/read/merge error is an independent,
                            // best-effort maintenance failure and must not make
                            // the task discard an otherwise healthy connection.
                            tracing::warn!(target: "khive_db::checkpoint",
                                error = %error,
                                "bounded FTS5 segment maintenance failed"
                            );
                        }
                    }
                    match outcome.wal_pages {
                        Some(wal_pages) => CheckpointTick::Observed(wal_pages),
                        None => {
                            note_checkpoint_skipped();
                            CheckpointTick::Skipped
                        }
                    }
                }
                Ok((_conn, state, Err(e))) => {
                    truncate_state = state;
                    tracing::warn!(target: "khive_db::checkpoint",
                        error = %e,
                        "dedicated checkpoint connection failed a pragma; \
                         dropping it for a fresh reopen next tick"
                    );
                    note_checkpoint_skipped();
                    CheckpointTick::Skipped
                }
                Err(panicked) => {
                    // The cycle panicked: its connection is gone (reopened next tick) and the
                    // escalation state is the one it was handed, counted as a TRUNCATE attempt.
                    truncate_state = panicked.truncate_state;
                    tracing::warn!(target: "khive_db::checkpoint",
                        error = %panicked.join_error,
                        "WAL checkpoint cycle panicked on its blocking thread; \
                         dropping the connection for a fresh reopen next tick"
                    );
                    note_checkpoint_skipped();
                    CheckpointTick::Skipped
                }
            }
        };

        // A no-progress or unmeasured TRUNCATE returns a bounded attribution
        // request alongside the core outcome. Consume it before any
        // report-derived decision or ordinary housekeeping for this tick.
        // The await is intentional: enumeration may perform up to 512
        // filesystem reads/classifications, so none of that work is allowed
        // to run on this Tokio worker, while one-pass-per-tick ordering still
        // requires the result (or an honest worker/enumeration failure) before
        // the fallback housekeeping arm is considered.
        #[cfg(unix)]
        if let Err(error) =
            complete_walpin_attribution(pending_sidecar_attribution, &mut truncate_state).await
        {
            tracing::warn!(target: "khive_db::checkpoint",
                error = %error,
                failure_kind = error.kind(),
                "ADR-091 Amendment 2 Plank B: no-progress sidecar attribution failed"
            );
        }

        // ADR-091 Plank 1: age-based sweep over the registry's oldest entry
        // MUST run on every tick, including a Skipped one — deliberately
        // BEFORE the Skipped early-continue below. Since the dedicated
        // checkpoint connection amendment, a `Skipped` tick means that
        // connection was unavailable or its PASSIVE result was busy, not that
        // some registered span held the pool's writer mutex (a checkpoint
        // tick no longer touches it at all) — but the sweep must not go blind for the
        // duration of that outage, and the two failure surfaces are
        // independent: a registry span can go stale
        // (KHIVE_TX_WARN_SECS / KHIVE_TX_MAX_AGE_SECS) while wal_pages sits
        // well under warn_pages, or while the checkpoint connection itself is
        // down. Edge-triggered per rung, same debounce idiom as the severity
        // ladder below, so a sustained stale span logs once per rung rather
        // than once per tick.
        reader_checkout_state.sweep_at(&pool, Instant::now());
        let oldest_tx = tx_filter
            .as_ref()
            .and_then(khive_storage::tx_registry::oldest_for);
        for emission in tx_age_state.observe(
            oldest_tx.as_ref().map(|s| (s.id, s.age, s.label.clone())),
            config.tx_warn_secs,
            config.tx_max_age_secs,
        ) {
            log_tx_age_emission(&emission);
        }
        // ADR-091 Amendment 2 Plank B: refresh (or clear) this daemon
        // process's own walpin heartbeat on the same cadence, so its own
        // pin — if any — is attributable the same way a session's is.
        #[cfg(unix)]
        if let Some(sidecar) = walpin_state.as_mut() {
            sidecar
                .observe(oldest_tx.clone(), config.tx_warn_secs)
                .await;
            let _ = run_walpin_housekeeping_if_due(
                sidecar,
                &mut truncate_state,
                legacy_walpin_fallback_interval,
            )
            .await;
        }

        // Skipped ticks leave crossing state unchanged — a busy tick must not
        // re-arm the rate limit while WAL pressure is still elevated.
        let wal_pages = match tick {
            CheckpointTick::Skipped => continue,
            CheckpointTick::Observed(n) => n,
        };

        let above_warn = wal_pages >= config.warn_pages;
        let above_high_water = wal_pages >= config.high_water_pages;
        let above_truncate_high_water = wal_pages >= config.truncate_high_water_pages;
        note_checkpoint_pressure_observation(above_warn, was_observed_above_warn);
        was_observed_above_warn = above_warn;

        // Per-tick debug for the oldest open entry always fires (cheap —
        // reuses this tick's already-computed `oldest_tx`); the two
        // `warn!`-level registry logs below are gated on the SAME crossing
        // state as the WAL-threshold WARNs above, so sustained pressure
        // logs once per crossing, not once per tick.
        log_tx_registry_oldest_debug(wal_pages, oldest_tx.as_ref());

        // ADR-091 severity ladder: INFO on the first below→above crossing,
        // WARN once `warn_sustained_cycles` consecutive ticks stay elevated.
        // The oldest-entry registry WARN rides the same INFO edge the old
        // binary crossing_warn used to gate on.
        for emission in severity_state.observe_wal_pages(wal_pages, &config) {
            match emission.rung {
                CheckpointSeverityRung::Info => {
                    log_tx_registry_oldest_warn(wal_pages, oldest_tx.as_ref());
                    tracing::info!(target: "khive_db::checkpoint",
                        wal_pages = emission.wal_pages,
                        warn_threshold = emission.threshold_pages,
                        "WAL page count crossed warn threshold"
                    );
                }
                CheckpointSeverityRung::Warn => {
                    tracing::warn!(target: "khive_db::checkpoint",
                        wal_pages = emission.wal_pages,
                        warn_threshold = emission.threshold_pages,
                        consecutive_cycles = emission.consecutive_cycles,
                        "WAL page count failed to drain below warn threshold"
                    );
                }
                CheckpointSeverityRung::Alarm => {
                    // Never produced by `observe_wal_pages`; see its doc.
                }
            }
        }

        let high_water_crossed = crossing_warn(above_high_water, &mut was_above_high_water);
        if high_water_crossed {
            log_tx_registry_snapshot_warn(wal_pages);
            log_wal_high_water_warn(
                wal_pages,
                config.high_water_pages,
                oldest_tx.as_ref(),
                config.tx_warn_secs,
            );
        }

        // ADR-094/#1838, #1857: one elevation row and one recovery summary
        // per genuinely continuous episode. Sustained elevated ticks update
        // only the bounded in-memory aggregate and process diagnostics
        // above; a dropped recovery handoff must not fold the next,
        // separate, pressure incident into this episode's aggregate.
        observe_checkpoint_pressure_tick(
            above_warn,
            wal_pages,
            above_high_water,
            above_truncate_high_water,
            &config,
            &mut event_elevation_open,
            &mut pressure_episode,
            &mut pending_recovery,
            |payload| {
                lifecycle_emitter
                    .try_emit(khive_types::EventKind::CheckpointOutcomeRecorded, payload)
            },
        );
    }

    lifecycle_emitter.shutdown().await;

    #[cfg(unix)]
    if let Some(sidecar) = walpin_state.as_mut() {
        sidecar.shutdown().await;
    }
}

/// Whether a `CheckpointOutcomeRecorded` transition should be enqueued for
/// this tick. Repeated observations in either state aggregate in memory;
/// only elevation and recovery edges reach the primary store.
pub(super) fn checkpoint_outcome_should_emit(above_warn: bool, was_elevated: bool) -> bool {
    above_warn != was_elevated
}

/// Advance the pressure-episode/lifecycle-emission state machine for one
/// observed tick. `try_emit` mirrors [`CheckpointLifecycleEmitter::try_emit`]
/// — `true` means the row was handed off, `false` means the queue was full
/// or closed.
///
/// #1857: on a dropped recovery handoff (`try_emit` returns `false` while
/// `above_warn` is `false`), the closed episode's summary is stashed in
/// `pending_recovery` for retry on later ticks — flushed here before this
/// tick's own transition is evaluated — instead of leaving
/// `event_elevation_open` and `pressure_episode` open for the next elevated
/// tick to silently extend, which would report two separate pressure
/// incidents as one merged episode.
#[allow(clippy::too_many_arguments)]
pub(super) fn observe_checkpoint_pressure_tick(
    above_warn: bool,
    wal_pages: u64,
    above_high_water: bool,
    above_truncate_high_water: bool,
    config: &CheckpointConfig,
    event_elevation_open: &mut bool,
    pressure_episode: &mut Option<CheckpointPressureEpisode>,
    pending_recovery: &mut Option<khive_storage::CheckpointOutcomeRecordedPayload>,
    mut try_emit: impl FnMut(khive_storage::CheckpointOutcomeRecordedPayload) -> bool,
) {
    // An undelivered recovery summary is a BARRIER, not merely a retry:
    // lifecycle consumers assert on the ordered event history (ADR-094), so
    // a later episode's opening must never be appended ahead of an earlier
    // episode's recovery. If the retry fails, the in-memory aggregate still
    // advances below, but no other emission is attempted this tick — a
    // deferred opening or recovery re-derives from state on a later tick,
    // after the pending summary has been delivered in order.
    let pending_blocks_emission = if let Some(payload) = pending_recovery.clone() {
        if try_emit(payload) {
            *pending_recovery = None;
            false
        } else {
            true
        }
    } else {
        false
    };

    if above_warn {
        match pressure_episode.as_mut() {
            Some(episode) => episode.observe(wal_pages),
            None => *pressure_episode = Some(CheckpointPressureEpisode::start(wal_pages)),
        }
    } else if !*event_elevation_open {
        // No elevation row reached the bounded handoff, so from any
        // consumer's view this episode never opened; discarding it keeps
        // the delivered history self-consistent. When the discard happens
        // because the barrier suppressed the opening attempt entirely, the
        // loss would otherwise be invisible even to the drop counters that
        // record failed attempts, so it is counted and logged here.
        if pending_blocks_emission && pressure_episode.is_some() {
            CHECKPOINT_LIFECYCLE_ENQUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "khive_db::checkpoint",
                wal_pages,
                "checkpoint pressure episode elapsed unreported behind an undelivered recovery summary"
            );
        }
        *pressure_episode = None;
    }

    if pending_blocks_emission || !checkpoint_outcome_should_emit(above_warn, *event_elevation_open)
    {
        return;
    }
    let Some(episode) = *pressure_episode else {
        tracing::warn!(target: "khive_db::checkpoint",
            above_warn,
            event_elevation_open = *event_elevation_open,
            "checkpoint pressure transition has no episode aggregate"
        );
        return;
    };
    let payload = khive_storage::CheckpointOutcomeRecordedPayload {
        wal_pages,
        warn_pages: config.warn_pages,
        high_water_pages: config.high_water_pages,
        truncate_high_water_pages: config.truncate_high_water_pages,
        above_warn,
        above_high_water,
        above_truncate_high_water,
        episode_elevated_ticks: Some(episode.elevated_ticks),
        episode_peak_wal_pages: Some(episode.peak_wal_pages),
    };
    if try_emit(payload.clone()) {
        *event_elevation_open = above_warn;
        if !above_warn {
            *pressure_episode = None;
        }
    } else if !above_warn {
        // The recovery handoff was dropped. Close this episode locally
        // anyway — `event_elevation_open` MUST NOT stay true, or the next
        // elevated tick would extend this (already finished) episode's
        // aggregate instead of starting a fresh one for what is genuinely a
        // new pressure incident. The dropped summary itself isn't thrown
        // away: it is stashed in `pending_recovery` and delivered on a
        // later tick, ahead of (and as a barrier to) every subsequent
        // emission, so lifecycle ordering survives the retry. The slot is
        // structurally empty here: a tick that entered with an undelivered
        // summary returned at the barrier above and never reached this arm.
        debug_assert!(
            pending_recovery.is_none(),
            "recovery emission attempted while an earlier summary was still pending"
        );
        *event_elevation_open = false;
        *pressure_episode = None;
        *pending_recovery = Some(payload);
    }
}
