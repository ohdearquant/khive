use super::{
    epoch_abs_diff, io, is_process_alive, now_epoch_secs, process_start_time_secs,
    producer_temp_identity, stale_window_from, stale_window_secs, unix_impl, Duration, Path,
    ProducerTempKind, WalpinBeacon, WalpinHeartbeat, WalpinPidHealth, WalpinReport,
    START_TIME_EPSILON_SECS, UNIX_EPOCH,
};

/// Ceiling on sidecar entries listed and read per enumeration. After ADR-091
/// Amendment 5 there is no checkpoint writer guard on this path. For daemon
/// checkpoint callers, the cap instead bounds the per-tick filesystem work
/// admitted to the awaited blocking worker, the latency attributable to that
/// work, and memory retained by the returned report — the entry-count sibling
/// of the per-entry `MAX_SIDECAR_ENTRY_BYTES` bound. A real population is one
/// heartbeat/beacon pair per live process; a directory holding more than this
/// contributes one `CAP_SENTINEL_PID` `Unknown` marker (fail-closed:
/// unenumerated entries make the census inconclusive, never exonerated).
#[cfg(unix)]
pub(super) const MAX_SIDECAR_ENTRIES: usize = 512;

/// Sentinel PID carried by the `Unknown` marker for entries past the
/// enumeration cap: those entries were never listed, so no real PID is
/// available. PID 0 is the kernel scheduler on every supported Unix and can
/// never be a sidecar producer.
#[cfg(unix)]
const CAP_SENTINEL_PID: u32 = 0;

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EnumerationPurpose {
    /// Consume a fresh classification for a TRUNCATE-no-progress report.
    /// Unknown trusted residue is retained in this pass's report and removed
    /// from disk so it cannot accumulate indefinitely.
    Attribution,
    /// Ordinary healthy-tick collection. Only positively dead/reused-PID
    /// residue may be removed; uncertain evidence stays available for a later
    /// attribution pass.
    Housekeeping,
    /// Operator diagnostics: classify and reconcile without deleting any
    /// sidecar evidence.
    Diagnostics,
}

#[cfg(unix)]
impl EnumerationPurpose {
    fn removes_uncertain_evidence(self) -> bool {
        self == Self::Attribution
    }

    fn removes_dead_or_reused_evidence(self) -> bool {
        self != Self::Diagnostics
    }

    fn removes_orphan_temps(self) -> bool {
        self != Self::Diagnostics
    }
}

/// The outcome of examining one producer-temp candidate against its recorded
/// identity. Liveness alone never licenses a reap: a malformed or mismatched
/// dead-PID temp is exactly the evidence a later TRUNCATE-no-progress
/// attribution pass needs, so it must survive cleanup as `Untrusted`, never
/// fall through to `Reap`.
#[cfg(unix)]
enum OrphanTempVerdict {
    /// Not old enough yet, not owned by us, or a live producer still holding
    /// a matching identity — no report, ordinary in-flight state.
    Skip,
    /// Confirmed dead-PID or PID-reused evidence, identity verified against
    /// the filename.
    Reap(unix_impl::CheckedEntry),
    /// Old enough to act on, but the body does not parse for its recorded
    /// kind or its recorded identity does not match the filename — retained
    /// and reported regardless of whether the named PID is alive or dead.
    Untrusted(&'static str),
}

// A live producer's `proc_pidinfo`/`/proc` lookup can fail for reasons that
// have nothing to do with the temp's trustworthiness (a permission boundary
// on a shared host, a `/proc` mount restriction) — forcing that outcome from
// a portable test isn't practical, so this thread-local one-shot override is
// the seam.
#[cfg(all(unix, test))]
thread_local! {
    static STALE_ORPHAN_TEMP_START_TIME_OVERRIDE: std::cell::Cell<Option<Option<i64>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(unix, test))]
pub(super) fn set_stale_orphan_temp_start_time_override(value: Option<i64>) {
    STALE_ORPHAN_TEMP_START_TIME_OVERRIDE.with(|cell| cell.set(Some(value)));
}

#[cfg(unix)]
fn stale_orphan_temp_actual_start(pid: u32) -> Option<i64> {
    #[cfg(test)]
    if let Some(overridden) = STALE_ORPHAN_TEMP_START_TIME_OVERRIDE.with(|cell| cell.take()) {
        return overridden;
    }
    process_start_time_secs(pid)
}

#[cfg(unix)]
fn stale_orphan_temp(
    handle: &unix_impl::SidecarDirHandle,
    name: &str,
    pid: u32,
    kind: ProducerTempKind,
    now: i64,
    stale_after_secs: i64,
) -> io::Result<OrphanTempVerdict> {
    let entry = match handle.read_checked_entry(name) {
        Ok(Some(entry)) => entry,
        Ok(None) => return Ok(OrphanTempVerdict::Skip),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Ok(OrphanTempVerdict::Untrusted(
                "refused: producer temp not owned by current user",
            ));
        }
        Err(e) => return Err(e),
    };
    let modified_at = entry
        .mtime
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    if now.saturating_sub(modified_at) <= stale_after_secs {
        return Ok(OrphanTempVerdict::Skip);
    }

    let recorded_identity = match kind {
        ProducerTempKind::Heartbeat => serde_json::from_slice::<WalpinHeartbeat>(&entry.body)
            .ok()
            .map(|record| (record.pid, record.started_at)),
        ProducerTempKind::Beacon => serde_json::from_slice::<WalpinBeacon>(&entry.body)
            .ok()
            .map(|record| (record.pid, record.started_at)),
    };
    let Some((recorded_pid, recorded_start)) = recorded_identity else {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp body does not parse as its recorded kind",
        ));
    };
    if recorded_pid != pid {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp identity does not match its filename",
        ));
    }

    if !is_process_alive(pid) {
        return Ok(OrphanTempVerdict::Reap(entry));
    }
    let Some(actual_start) = stale_orphan_temp_actual_start(pid) else {
        return Ok(OrphanTempVerdict::Untrusted(
            "refused: producer temp process start time unavailable",
        ));
    };
    if epoch_abs_diff(actual_start, recorded_start) > START_TIME_EPSILON_SECS {
        return Ok(OrphanTempVerdict::Reap(entry));
    }
    Ok(OrphanTempVerdict::Skip)
}

#[cfg(unix)]
pub(super) fn enumerate_live_bounded(
    dir: &Path,
    sweep_interval: Duration,
    max_entries: usize,
    purpose: EnumerationPurpose,
) -> io::Result<WalpinReport> {
    let handle = match unix_impl::SidecarDirHandle::open_if_exists(dir) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(WalpinReport::default()),
        Err(e) => return Err(e),
    };

    let now = now_epoch_secs();
    // Fallback window for records that predate the `sweep_interval_ms`
    // field — records carrying their producer's own cadence are judged
    // against it instead (see `stale_window_secs`), so a session sweeping
    // on an independently slower configured interval is not misread as
    // stale by a faster-ticking daemon.
    let fallback_window_secs = stale_window_from(sweep_interval);

    let mut heartbeats: std::collections::HashMap<u32, WalpinHeartbeat> = Default::default();
    let mut beacon_pids: std::collections::HashSet<u32> = Default::default();
    let mut unknown: Vec<(u32, &'static str)> = Vec::new();
    // PIDs whose heartbeat or beacon passed the identity gate but failed
    // freshness — these are wedged, not absent, and must never resolve to
    // `RegisteredSilent` off a co-existing entry (item b).
    let mut wedged: std::collections::HashSet<u32> = Default::default();

    // Entry-count bound: listing itself stops at the cap (see
    // `list_names`), so neither the readdir loop, the names allocation,
    // nor this processing loop scales with directory content. A truncated
    // listing contributes one sentinel `Unknown` marker below — the
    // unlisted entries were never read, and the census stays inconclusive
    // rather than exonerating.
    let (names, producer_temps, truncated) = handle.list_names(max_entries)?;
    if truncated {
        unknown.push((
            CAP_SENTINEL_PID,
            "refused: sidecar entry count exceeds enumeration cap",
        ));
    }
    let mut cleanup_would_reap = 0usize;
    let mut orphan_temps_reaped = 0usize;
    for name in producer_temps {
        let Some((pid, kind)) = producer_temp_identity(&name) else {
            continue;
        };
        match stale_orphan_temp(&handle, &name, pid, kind, now, fallback_window_secs) {
            Ok(OrphanTempVerdict::Reap(entry)) => {
                cleanup_would_reap = cleanup_would_reap.saturating_add(1);
                if purpose.removes_orphan_temps() {
                    match handle.remove_if_same(&name, &entry) {
                        Ok(true) => orphan_temps_reaped = orphan_temps_reaped.saturating_add(1),
                        Ok(false) => unknown.push((
                            pid,
                            "producer temp changed while orphan cleanup was in progress",
                        )),
                        Err(_) => unknown
                            .push((pid, "refused: producer temp changed to an untrusted entry")),
                    }
                }
            }
            Ok(OrphanTempVerdict::Skip) => {}
            Ok(OrphanTempVerdict::Untrusted(reason)) => unknown.push((pid, reason)),
            Err(_) => unknown.push((
                pid,
                "refused: untrusted producer temp (symlink, non-regular, or oversized)",
            )),
        }
    }
    for name in names {
        let is_heartbeat = name.ends_with(".json");
        let is_beacon = name.ends_with(".beacon");
        if !is_heartbeat && !is_beacon {
            continue;
        }
        let Some(pid) = name
            .rsplit_once('.')
            .and_then(|(stem, _)| stem.parse::<u32>().ok())
        else {
            continue;
        };

        // Trust boundary: symlink/ownership refusal happens BEFORE any
        // content read, and contributes `Unknown` rather than being
        // silently dropped — the entry's health is unestablished, not
        // exonerating.
        let (body, mtime) = match handle.read_checked(&name) {
            Ok(Some(v)) => v,
            Ok(None) => continue, // raced away between listing and reading
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                unknown.push((pid, "refused: sidecar entry not owned by current user"));
                continue;
            }
            Err(_) => {
                unknown.push((
                    pid,
                    "refused: untrusted sidecar entry (symlink, non-regular, or oversized)",
                ));
                continue;
            }
        };
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if is_heartbeat {
            let heartbeat: WalpinHeartbeat = match serde_json::from_slice(&body) {
                Ok(hb) => hb,
                Err(_) => {
                    if purpose.removes_uncertain_evidence()
                        || (purpose.removes_dead_or_reused_evidence() && !is_process_alive(pid))
                    {
                        let _ = handle.unlink_tolerant(&name);
                    }
                    wedged.insert(pid);
                    unknown.push((pid, "malformed walpin heartbeat entry"));
                    continue;
                }
            };
            if heartbeat.pid != pid {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "walpin heartbeat PID does not match its entry name"));
                continue;
            }
            let alive = is_process_alive(heartbeat.pid);
            let actual_start = if alive {
                process_start_time_secs(heartbeat.pid)
            } else {
                None
            };
            let identity_ok = actual_start
                .map(|actual| {
                    epoch_abs_diff(actual, heartbeat.started_at) <= START_TIME_EPSILON_SECS
                })
                .unwrap_or(false);
            if !identity_ok {
                let positively_dead_or_reused = !alive
                    || actual_start.is_some_and(|actual| {
                        epoch_abs_diff(actual, heartbeat.started_at) > START_TIME_EPSILON_SECS
                    });
                if purpose.removes_uncertain_evidence()
                    || (purpose.removes_dead_or_reused_evidence() && positively_dead_or_reused)
                {
                    let _ = handle.unlink_tolerant(&name);
                } else {
                    wedged.insert(pid);
                    unknown.push((pid, "walpin heartbeat identity could not be verified"));
                }
                continue;
            }
            // ADR-091 Amendment 3 Plank F1: a record carrying
            // `oldest_tx_started_at` is new-style — its body is only
            // rewritten on content change, so freshness is judged against
            // the entry's mtime (advanced by a metadata-only touch every
            // tick), never the possibly-stale `updated_at` body field. A
            // record without it predates this amendment and is read
            // exactly as before: `updated_at` is its own freshness field.
            // Either way the window is the PRODUCER's recorded cadence,
            // not the enumerator's — the mixed-version rule (readers accept
            // both generations; see the amendment) depends on this branch.
            let window = stale_window_secs(heartbeat.sweep_interval_ms, fallback_window_secs);
            let hb_fresh = if heartbeat.oldest_tx_started_at.is_some() {
                epoch_abs_diff(now, mtime_secs) <= window as u64
            } else {
                epoch_abs_diff(now, heartbeat.updated_at) <= window as u64
            };
            if !hb_fresh {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "stale walpin heartbeat"));
                continue;
            }
            heartbeats.insert(heartbeat.pid, heartbeat);
        } else {
            let beacon: WalpinBeacon = match serde_json::from_slice(&body) {
                Ok(b) => b,
                Err(_) => {
                    if purpose.removes_uncertain_evidence()
                        || (purpose.removes_dead_or_reused_evidence() && !is_process_alive(pid))
                    {
                        let _ = handle.unlink_tolerant(&name);
                    }
                    wedged.insert(pid);
                    unknown.push((pid, "malformed walpin beacon entry"));
                    continue;
                }
            };
            if beacon.pid != pid {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "walpin beacon PID does not match its entry name"));
                continue;
            }
            let alive = is_process_alive(beacon.pid);
            let actual_start = if alive {
                process_start_time_secs(beacon.pid)
            } else {
                None
            };
            let identity_ok = actual_start
                .map(|actual| epoch_abs_diff(actual, beacon.started_at) <= START_TIME_EPSILON_SECS)
                .unwrap_or(false);
            if !identity_ok {
                let positively_dead_or_reused = !alive
                    || actual_start.is_some_and(|actual| {
                        epoch_abs_diff(actual, beacon.started_at) > START_TIME_EPSILON_SECS
                    });
                if purpose.removes_uncertain_evidence()
                    || (purpose.removes_dead_or_reused_evidence() && positively_dead_or_reused)
                {
                    let _ = handle.unlink_tolerant(&name);
                } else {
                    wedged.insert(pid);
                    unknown.push((pid, "walpin beacon identity could not be verified"));
                }
                continue;
            }
            // Beacon refresh rule: freshness is the entry's mtime (the
            // metadata-only touch), not any JSON field — the beacon's body
            // is written once and never refreshed. The window is the
            // producer's recorded cadence, not the enumerator's.
            let window = stale_window_secs(beacon.sweep_interval_ms, fallback_window_secs);
            let fresh = epoch_abs_diff(now, mtime_secs) <= window as u64;
            if !fresh {
                if purpose.removes_uncertain_evidence() {
                    let _ = handle.unlink_tolerant(&name);
                }
                wedged.insert(pid);
                unknown.push((pid, "stale walpin beacon"));
                continue;
            }
            beacon_pids.insert(beacon.pid);
        }
    }

    for (pid, _) in &unknown {
        wedged.insert(*pid);
    }

    let mut entries: Vec<WalpinPidHealth> = Vec::new();
    for (pid, hb) in heartbeats {
        if !wedged.contains(&pid) {
            entries.push(WalpinPidHealth::Reporting(hb));
        }
        beacon_pids.remove(&pid);
    }
    for pid in beacon_pids {
        if wedged.contains(&pid) {
            continue; // already carried as `Unknown` via `unknown` above
        }
        entries.push(WalpinPidHealth::RegisteredSilent { pid });
    }
    for (pid, reason) in unknown {
        entries.push(WalpinPidHealth::Unknown { pid, reason });
    }

    Ok(WalpinReport {
        entries,
        sidecar_listing_truncated: truncated,
        cleanup_would_reap,
        orphan_temps_reaped,
    })
}
