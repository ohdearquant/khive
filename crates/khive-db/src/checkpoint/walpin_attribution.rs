#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::pool::ConnectionPool;

#[cfg(unix)]
use super::{now_epoch_secs, CachedWalpinAttribution, TruncateState, WalpinFullScanPlan};

/// Deterministic seam for the async-attribution regressions below. The hook
/// executes inside the actual `spawn_blocking` closure, so a current-thread
/// Tokio test can prove both thread displacement and awaited ordering without
/// relying on sleeps or scheduler timing.
#[cfg(all(test, unix))]
pub(super) mod walpin_attribution_test_sync {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};

    enum Behavior {
        Pause {
            reached_tx: tokio::sync::oneshot::Sender<std::thread::ThreadId>,
            proceed_rx: Receiver<()>,
        },
        Panic,
    }

    struct Hook {
        dir: PathBuf,
        behavior: Behavior,
    }

    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);
    static REPORT_COUNTER: Mutex<Option<Arc<AtomicUsize>>> = Mutex::new(None);

    pub(crate) fn install_pause(
        dir: PathBuf,
    ) -> (
        tokio::sync::oneshot::Receiver<std::thread::ThreadId>,
        SyncSender<()>,
        Arc<AtomicUsize>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = sync_channel(0);
        let report_counter = Arc::new(AtomicUsize::new(0));
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Hook {
                dir,
                behavior: Behavior::Pause {
                    reached_tx,
                    proceed_rx,
                },
            });
        assert!(
            replaced.is_none(),
            "walpin attribution hook already installed"
        );
        *REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&report_counter));
        (reached_rx, proceed_tx, report_counter)
    }

    pub(crate) fn install_panic(dir: PathBuf) {
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Hook {
                dir,
                behavior: Behavior::Panic,
            });
        assert!(
            replaced.is_none(),
            "walpin attribution hook already installed"
        );
    }

    pub(crate) fn uninstall() {
        *HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        *REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(crate) fn before_enumeration(dir: &Path) {
        let hook = {
            let mut guard = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(hook) if hook.dir == dir => guard.take(),
                _ => None,
            }
        };
        let Some(hook) = hook else {
            return;
        };
        match hook.behavior {
            Behavior::Pause {
                reached_tx,
                proceed_rx,
            } => {
                if reached_tx.send(std::thread::current().id()).is_ok() {
                    let _ = proceed_rx.recv();
                }
            }
            Behavior::Panic => panic!("injected walpin attribution worker panic"),
        }
    }

    pub(crate) fn report_used() {
        if let Some(counter) = REPORT_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Immutable work captured around an armed TRUNCATE and consumed by the
/// async checkpoint owner only when that attempt makes no progress.
///
/// The holder census belongs here because it must precede the bounded
/// TRUNCATE wait. The sidecar directory walk does not: it remains deferred
/// until after the outcome is known and is executed through an awaited
/// `spawn_blocking` by [`complete_walpin_attribution`].
#[cfg(unix)]
#[derive(Debug)]
pub(super) enum WalpinAttributionRequest {
    Fresh {
        dir: PathBuf,
        census: Result<crate::walpin::CensusResult, String>,
        legacy_fallback_interval: Duration,
        previous_last_attempt: Option<Instant>,
    },
    Cached(CachedWalpinAttribution),
    Suppressed,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WalpinReportFreshness {
    Fresh,
    Cached { age: Duration },
}

#[cfg(unix)]
impl WalpinReportFreshness {
    fn is_fresh(self) -> bool {
        self == Self::Fresh
    }
}

/// Non-Unix placeholder keeps the synchronous core's outcome shape stable;
/// daemon sidecar attribution itself is Unix-only.
#[cfg(not(unix))]
pub(super) type WalpinAttributionRequest = ();

/// Honest failure surface for an attempted no-progress attribution pass.
/// Both variants suppress same-tick housekeeping because a panicked blocking
/// worker may already have partially enumerated the directory; retrying a
/// second pass would violate the one-pass-per-tick bound.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WalpinAttributionFailure {
    Enumeration(String),
    Worker(String),
}

#[cfg(unix)]
impl WalpinAttributionFailure {
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Enumeration(_) => "enumeration",
            Self::Worker(_) => "blocking_worker",
        }
    }
}

#[cfg(unix)]
impl std::fmt::Display for WalpinAttributionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Enumeration(error) => write!(
                formatter,
                "sidecar directory failed the trust-boundary enumeration; cross-process \
                 WAL-pin attribution is unestablished for this tick: {error}"
            ),
            Self::Worker(error) => write!(
                formatter,
                "sidecar attribution blocking worker failed; cross-process WAL-pin \
                 attribution is unestablished for this tick: {error}"
            ),
        }
    }
}

/// Capture the pre-TRUNCATE OS holder census and stable sidecar inputs. A
/// no-op if the sidecar is disabled or this backend has no on-disk path.
#[cfg(unix)]
pub(super) fn capture_walpin_attribution_request(
    pool: &ConnectionPool,
    state: &mut TruncateState,
) -> Option<WalpinAttributionRequest> {
    let path = pool.canonical_path()?;
    if !crate::walpin::sidecar_enabled(true) {
        return None;
    }
    let legacy_fallback_interval = state.legacy_walpin_fallback_interval;
    Some(match state.plan_walpin_attribution_at(Instant::now()) {
        WalpinFullScanPlan::Refresh {
            previous_last_attempt,
        } => WalpinAttributionRequest::Fresh {
            dir: crate::walpin::sidecar_dir_for(path),
            census: crate::walpin::census_holders(path).map_err(|error| error.to_string()),
            legacy_fallback_interval,
            previous_last_attempt,
        },
        WalpinFullScanPlan::Cached(cached) => WalpinAttributionRequest::Cached(cached),
        WalpinFullScanPlan::Suppressed => WalpinAttributionRequest::Suppressed,
    })
}

/// Consume this tick's no-progress attribution request off the async runtime
/// worker and await it before any report or fallback housekeeping is used.
/// Returns `Ok(false)` when no pass was requested. Once a request exists the
/// state is marked attempted before spawning, so worker panic/cancellation
/// cannot accidentally authorize a second directory scan in the same tick.
#[cfg(unix)]
pub(super) async fn complete_walpin_attribution(
    request: Option<WalpinAttributionRequest>,
    state: &mut TruncateState,
) -> Result<bool, WalpinAttributionFailure> {
    let Some(request) = request else {
        return Ok(false);
    };
    match request {
        WalpinAttributionRequest::Suppressed => Ok(false),
        WalpinAttributionRequest::Cached(cached) => {
            log_walpin_sidecar_report(
                &cached.report,
                cached.census,
                WalpinReportFreshness::Cached {
                    age: Instant::now().saturating_duration_since(cached.captured_at),
                },
            );
            Ok(true)
        }
        WalpinAttributionRequest::Fresh {
            dir,
            census,
            legacy_fallback_interval,
            previous_last_attempt: _,
        } => {
            state.sidecar_attribution_attempted_this_tick = true;
            if state.walpin_full_scan_last_attempt.is_none() {
                state.walpin_full_scan_last_attempt = Some(Instant::now());
            }
            let fallback = state.walpin_cached_attribution.clone();
            let result = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                walpin_attribution_test_sync::before_enumeration(&dir);
                crate::walpin::enumerate_live(&dir, legacy_fallback_interval)
            })
            .await
            .map_err(|error| WalpinAttributionFailure::Worker(error.to_string()))
            .and_then(|result| {
                result.map_err(|error| WalpinAttributionFailure::Enumeration(error.to_string()))
            });

            match result {
                Ok(report) => {
                    let captured_at = Instant::now();
                    log_walpin_sidecar_report(
                        &report,
                        census.clone(),
                        WalpinReportFreshness::Fresh,
                    );
                    state.cache_walpin_attribution(report, census, captured_at);
                    Ok(true)
                }
                Err(error) => {
                    if let Some(cached) = fallback {
                        log_walpin_sidecar_report(
                            &cached.report,
                            cached.census,
                            WalpinReportFreshness::Cached {
                                age: Instant::now().saturating_duration_since(cached.captured_at),
                            },
                        );
                    }
                    Err(error)
                }
            }
        }
    }
}

/// When a TRUNCATE attempt makes no progress, enumerate the walpin sidecar and
/// combine it with the holder census captured immediately before that attempt.
/// This pass consumes the classifications for attribution and returns whether
/// enumeration was attempted; the caller uses that marker to suppress the
/// ordinary housekeeping pass later in the same tick. Holder identity cannot
/// be deferred because a transient blocker may have released by then.
///
/// Sidecar-health attribution (ADR-091 Amendment 2):
/// the sharper "unregistered/native mechanism" conclusion is licensed only
/// when every discovered PID is `reporting` or `registered-silent`
/// (`WalpinReport::fully_attributed`); any `unknown` PID — including the
/// directory itself failing the trust-boundary check — makes attribution
/// inconclusive, and the WARN below names exactly which PIDs are unresolved
/// instead of silently exonerating them.
#[cfg(unix)]
pub(super) fn log_walpin_sidecar_report(
    report: &crate::walpin::WalpinReport,
    census: Result<crate::walpin::CensusResult, String>,
    freshness: WalpinReportFreshness,
) {
    #[cfg(test)]
    walpin_attribution_test_sync::report_used();
    let now = now_epoch_secs();
    for hb in report.reporting() {
        // ADR-091 Amendment 3 Plank F2 fail-closed reading rule: the
        // logger must never let a fallback-confidence entry read as live
        // cross-process ground truth, so the confidence distinction is
        // always emitted alongside the raw field — never inferred by the
        // reader of this log line.
        tracing::warn!(
            walpin_pid = hb.pid,
            walpin_role = %hb.process_role,
            walpin_oldest_tx_age_secs = hb.current_oldest_tx_age_secs(now),
            walpin_oldest_tx_label = hb.oldest_tx_label.as_deref().unwrap_or("<unlabeled>"),
            walpin_attribution_basis = hb.attribution_basis.as_deref().unwrap_or("<unspecified>"),
            walpin_attribution_evidence_backed = hb.attribution_is_evidence_backed(),
            walpin_attribution_fresh = freshness.is_fresh(),
            walpin_health = "reporting",
            "ADR-091 Amendment 2 Plank B: live cross-process WAL-pin attribution report"
        );
    }
    for pid in report.registered_silent_pids() {
        tracing::debug!(
            walpin_pid = pid,
            walpin_health = "registered_silent",
            walpin_attribution_fresh = freshness.is_fresh(),
            "ADR-091 Amendment 2 Plank B: process affirmatively reports no over-threshold span"
        );
    }
    let mut unknown_pids: Vec<u32> = report.unknown_pids().collect();
    if let WalpinReportFreshness::Cached { age } = freshness {
        tracing::warn!(
            walpin_cache_age_ms = age.as_millis() as u64,
            "cached WAL-pin attribution is diagnostic-only; fully-attributed \
             conclusion is not licensed"
        );
        unknown_pids.push(0);
    }

    // The sidecar directory alone can only speak for PIDs that wrote
    // something there. Widen the universe to every PID the OS reports as
    // holding the database immediately before the TRUNCATE attempt; any holder
    // absent from `report` is unknown.
    match census {
        Ok(census) => {
            let sidecar_known: std::collections::HashSet<u32> = report
                .reporting()
                .map(|hb| hb.pid)
                .chain(report.registered_silent_pids())
                .chain(unknown_pids.iter().copied())
                .collect();
            let mut census_only: Vec<u32> =
                census.holders.difference(&sidecar_known).copied().collect();
            if !census_only.is_empty() {
                census_only.sort_unstable();
                tracing::warn!(
                    ?census_only,
                    "ADR-091 Amendment 2: these PIDs hold the database file open \
                     at the OS level but have no sidecar data at all (pre-feature binary, \
                     sidecar disabled, or wedged before its first write)"
                );
                unknown_pids.extend(census_only);
            }
            if !census.is_complete() {
                let mut uninspectable = census.uninspectable_pids.clone();
                uninspectable.sort_unstable();
                tracing::warn!(
                    ?uninspectable,
                    truncated = census.truncated,
                    "ADR-091 Amendment 2: the OS-derived holder census is \
                     INCOMPLETE — either specific PIDs' open file descriptors could not be \
                     inspected (permission denied, or a listing race), or the enumeration walk \
                     itself has positive evidence it did not see the full live-process universe \
                     (namespace/visibility check, directory-iterator error, self-canary, or a \
                     libproc buffer that stayed at capacity after bounded retries) — cannot \
                     rule out an unregistered holder"
                );
                if uninspectable.is_empty() {
                    // `truncated` fired with no specific PID list (a
                    // namespace/visibility or buffer-truncation signal, not
                    // a per-PID inspection failure) — still makes
                    // attribution inconclusive. Mirror the census-failure
                    // arm below with the same non-PID sentinel rather than
                    // silently trusting a walk we know was incomplete.
                    unknown_pids.push(0);
                } else {
                    unknown_pids.extend(uninspectable);
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ADR-091 Amendment 2: OS-derived holder census failed; \
                 attribution cannot rule out an unregistered database holder this tick"
            );
            // A failed census is itself a health failure for the sharper
            // conclusion below — treat it as if at least one PID were
            // unresolved, without fabricating a specific PID number.
            unknown_pids.push(0);
        }
    }

    unknown_pids.sort_unstable();
    unknown_pids.dedup();
    if !unknown_pids.is_empty() {
        tracing::warn!(
            ?unknown_pids,
            "ADR-091 Amendment 2 Plank B: sidecar health unestablished for these PIDs; \
             attribution is inconclusive and the native/unregistered-mechanism conclusion \
             is NOT licensed this tick"
        );
    } else if report.reporting().next().is_none() {
        tracing::info!(
            "ADR-091 Amendment 2 Plank B: every live PID is reporting or registered-silent \
             with none pinning; the WAL pin is not attributable to any in-process registry \
             span this sidecar covers"
        );
    }
}
