//! Calendar-date → earliest-instant anchoring (ADR-169 D1), shared by every
//! surface that accepts a date-only value against the configured display
//! timezone. Moved verbatim from `khive-pack-gtd`'s handlers when
//! `brain.event_counts` gained the same date-only semantics — packs do not
//! depend on packs, so the shared rule lives here beside the
//! `display_timezone` config it interprets.

use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;

/// Half-width of the UTC window the skipped-midnight branch bisects. Real UTC
/// offsets stay well inside this, so the window always contains the boundary
/// the search is looking for. A date whose local midnight cannot carry the full
/// width is still answered: the window clamps to the representable range rather
/// than being abandoned.
const ANCHOR_SEARCH_RADIUS_HOURS: i64 = 48;

/// The `[midnight - radius, midnight + radius]` window the skipped-midnight
/// branch searches, clamped to the representable range.
///
/// Checked arithmetic, because chrono's `-` and `+` PANIC on overflow and
/// `date` reaches here from a caller. Clamped rather than failing: near the
/// ends of the range the window is truncated, not absent, and the instants
/// that remain in it are exactly the ones a search could return anyway.
fn anchor_search_window(date: chrono::NaiveDate) -> (chrono::NaiveDateTime, chrono::NaiveDateTime) {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is a valid time on every date");
    let radius = chrono::Duration::hours(ANCHOR_SEARCH_RADIUS_HOURS);
    (
        midnight
            .checked_sub_signed(radius)
            .unwrap_or(chrono::NaiveDateTime::MIN),
        midnight
            .checked_add_signed(radius)
            .unwrap_or(chrono::NaiveDateTime::MAX),
    )
}

/// Order the local date at UTC instant `utc`, in `tz`, against `date`.
///
/// Total where the obvious spelling is not. `tz.from_utc_datetime(&utc)
/// .date_naive()` PANICS when the zone's offset pushes the local wall clock
/// outside `NaiveDateTime`'s range, and both ends are reachable once the search
/// window is clamped: a positive-offset zone overflows at
/// `NaiveDateTime::MAX`, a negative-offset zone underflows at `MIN`.
///
/// The overflow direction carries the answer. A local time past the top of the
/// range is later than every representable date, and one below the bottom is
/// earlier, so the sign of the offset decides the ordering without needing the
/// date that cannot be built.
fn local_date_cmp(
    tz: Tz,
    utc: chrono::NaiveDateTime,
    date: chrono::NaiveDate,
) -> std::cmp::Ordering {
    use chrono::Offset;
    let offset_seconds = tz.offset_from_utc_datetime(&utc).fix().local_minus_utc();
    match utc.checked_add_signed(chrono::Duration::seconds(offset_seconds as i64)) {
        Some(local) => local.date().cmp(&date),
        None if offset_seconds > 0 => std::cmp::Ordering::Greater,
        None => std::cmp::Ordering::Less,
    }
}

/// Resolve a calendar date to the earliest instant that belongs to it in
/// `tz` (ADR-169 D1). Midnight is not a total function of date and zone: on a
/// zone's own transition date local midnight can fail to exist (the clock
/// jumps over it) or occur twice (the clock repeats it). Both are one rule —
/// take the least instant whose local date, in `tz`, is `date` — not two
/// exceptions to it; neither case may be resolved by unwrapping to UTC,
/// which would silently reintroduce the defect ADR-169 exists to remove.
///
/// Returns `None` only when no representable instant carries `date` as its
/// local date in `tz` — the zone's local calendar skips the date entirely, a
/// case ADR-169 does not name since its own examples are ordinary sub-day
/// transitions. The function is total over every date `%Y` can parse, so a
/// caller never needs a range check of its own.
pub fn anchor_date_to_earliest_instant(date: chrono::NaiveDate, tz: Tz) -> Option<DateTime<Tz>> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is a valid time on every date");
    match tz.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) => Some(dt),
        // Fall-back overlap: two instants map to the same wall-clock time.
        // The earlier one is the least instant whose local date is `date`.
        chrono::LocalResult::Ambiguous(earliest, _latest) => Some(earliest),
        // Spring-forward gap (or a larger jump): local midnight does not
        // exist. Solve for the boundary directly rather than projecting to
        // it. Within the window searched below, the local date is a
        // non-decreasing function of UTC time, so the least instant whose
        // local date is `date` is the bisection point on the UTC axis
        // between "still the previous date" and "already this date".
        //
        // That premise is SCOPED to this window on purpose: it is not true
        // globally. Zones that crossed the international date line run the
        // local calendar backwards at the crossing — `America/Adak` at
        // 1867-10-19T00:31:13Z moves from local 1867-10-19 to 1867-10-18 as
        // the offset goes from +12:13:22 to -11:46:38 — and the pinned
        // chrono-tz table holds 107 such date-decreasing transitions. What
        // makes the bisection sound is that none of them falls inside a
        // window this branch searches.
        //
        // That is checked rather than argued, and it is checked as the
        // CONCLUSION rather than the premise. `every_gap_date_resolves_to_the
        // _least_instant` sweeps every zone in the pinned database over
        // 1850-2100 and asserts directly that one second before the returned
        // instant is a different local date — the least-instant property
        // itself, which is what a monotonicity violation would cost. At the
        // current pin it reports 597 zones over 91311 dates each, 4299 gap
        // dates, 818 folds, 7 whole days a zone's calendar skips, and zero
        // violations, in about three seconds. Those counts are printed by the
        // test rather than asserted, because their magnitudes are the
        // database's business and only their reaching zero would mean the
        // sweep broke. Run it whenever the chrono-tz pin moves:
        //
        //   cargo test -p khive-runtime --lib --release -- --ignored --nocapture
        //
        // It is ignored by default because it is an all-zone sweep, not
        // because it is optional. A violation costs the LEAST-instant
        // property only; the same-date check at the end of this branch keeps
        // a wrong DATE from being returned regardless.
        //
        // The earlier implementation instead read the offset in effect at
        // noon on the previous calendar date and projected local midnight
        // forward under it. That is correct only when the gap BEGINS at
        // local midnight. It need not: for `America/Toronto` on 1919-03-31
        // the clock jumped from 23:30 on the previous date to 00:30, so the
        // first instant belonging to the date is 00:30 local (04:30Z), while
        // the projection under the prior -05:00 offset returned 01:00 local
        // (05:00Z) — thirty minutes late, and late in a way that still
        // passed a same-date check. Six IANA zones share that transition.
        //
        // One-second granularity is required, not one-minute: historical
        // LMT offsets are not whole minutes.
        chrono::LocalResult::None => {
            let (mut lo, mut hi) = anchor_search_window(date);
            // The window's lower bound can itself already carry `date`. At the
            // bottom of the representable range a positive-offset zone has no
            // representable local midnight, but later local times on that same
            // date do exist: in `Pacific/Apia` the least representable instant
            // is local 12:33:04 on chrono's minimum date. That instant IS the
            // least one carrying the date, so return it rather than reporting
            // the date as absent.
            if local_date_cmp(tz, lo, date) == std::cmp::Ordering::Equal {
                return Some(tz.from_utc_datetime(&lo));
            }
            // Guard the bisection invariant instead of assuming it. Real UTC
            // offsets stay well inside +/-48h, so a violation here means the
            // zone is outside anything this rule can answer for; fail closed.
            if local_date_cmp(tz, lo, date) == std::cmp::Ordering::Greater
                || local_date_cmp(tz, hi, date) == std::cmp::Ordering::Less
            {
                return None;
            }
            while hi - lo > chrono::Duration::seconds(1) {
                let mid = lo + (hi - lo) / 2;
                if local_date_cmp(tz, mid, date) == std::cmp::Ordering::Less {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            // `hi` is now the least instant whose local date is >= `date`.
            // It is > `date` exactly when the zone's local calendar skips
            // the date entirely, which is the documented `None` case.
            (local_date_cmp(tz, hi, date) == std::cmp::Ordering::Equal)
                .then(|| tz.from_utc_datetime(&hi))
        }
    }
}

#[cfg(test)]
mod tz_database_audit {
    use super::*;

    /// Search a window around `base` at `step` granularity for any instant whose local date is
    /// `target`, returning the first hit.
    ///
    /// Generic over the date lookup rather than over `TimeZone`, and that is the whole point. The
    /// only thing `step` decides is whether a local-date interval SHORTER THAN THE STEP can hide
    /// between two probes, and the pinned `chrono-tz` table contains no such interval — so the real
    /// database cannot exercise the one property this function's granularity governs. Taking the
    /// lookup as a closure lets a fabricated calendar do it instead, which is why
    /// `a_sub_step_interval_is_missed_by_a_coarse_probe` below can go red against a coarse step and
    /// green against a fine one without touching the pinned data or the production signature.
    /// The probe granularity, as a function rather than a literal at the call site, so that the
    /// sweep below and the synthetic arm read the SAME value. Inline the literal in both places and
    /// the arm pins only itself: someone coarsening the sweep would leave the arm green while
    /// reintroducing exactly the defect it exists to catch. Coarsen this and
    /// `a_sub_step_interval_is_missed_by_a_coarse_probe` goes red.
    fn probe_step() -> chrono::Duration {
        chrono::Duration::seconds(1)
    }

    /// Search a CLOSED WINDOW, not a centre and a radius.
    ///
    /// The caller supplies the bounds so the searched window has a single definition: the sweep
    /// passes `anchor_search_window`, the same function the resolver uses, and a change to the
    /// radius cannot leave this probe checking the old window while still reporting no violations.
    /// Taking a centre and adding a radius here would recreate that second definition, and would
    /// also reintroduce bare `-`/`+` on `NaiveDateTime`, which panics at the ends of the
    /// representable range rather than returning `None`.
    fn find_instant_carrying_date(
        lo: chrono::NaiveDateTime,
        hi: chrono::NaiveDateTime,
        step: chrono::Duration,
        target: chrono::NaiveDate,
        local_date_at: impl Fn(chrono::NaiveDateTime) -> chrono::NaiveDate,
    ) -> Option<chrono::NaiveDateTime> {
        let mut probe = lo;
        while probe <= hi {
            if local_date_at(probe) == target {
                return Some(probe);
            }
            // Checked, for the same reason the bounds are: stepping past the representable
            // range must end the search, not abort the process. `?` is the whole behaviour
            // here — the function already returns `Option`, so an overflow IS "no instant
            // carries this date within the window".
            probe = probe.checked_add_signed(step)?;
        }
        None
    }

    /// The discriminating arm for the probe granularity.
    ///
    /// A fabricated calendar in which one local date exists for THIRTY SECONDS. A one-minute probe
    /// steps straight over it and reports that no instant carries that date, which is the failure
    /// this granularity exists to prevent and is invisible against the real table. A one-second
    /// probe finds it. Synthetic data is the correct fixture here precisely because the pinned
    /// database has no transition of this shape: an assertion that cannot be made to fail is not an
    /// assertion, and the alternative was shipping the finer step on argument alone.
    #[test]
    fn a_sub_step_interval_is_missed_by_a_coarse_probe() {
        let base = chrono::NaiveDate::from_ymd_opt(2000, 1, 2)
            .expect("base date")
            .and_hms_opt(0, 0, 0)
            .expect("midnight");
        let target = chrono::NaiveDate::from_ymd_opt(2000, 1, 2).expect("target date");
        let before = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("previous date");
        let after = chrono::NaiveDate::from_ymd_opt(2000, 1, 3).expect("next date");

        // `target` occupies [base + 10s, base + 40s) and nothing else does. The window is offset
        // from every whole minute in the search so a coarse probe lands either side of it.
        let window_start = base + chrono::Duration::seconds(10);
        let window_end = base + chrono::Duration::seconds(40);
        let calendar = move |probe: chrono::NaiveDateTime| {
            if probe >= window_start && probe < window_end {
                target
            } else if probe < window_start {
                before
            } else {
                after
            }
        };

        // Guard the fixture itself: the interval must actually be shorter than the coarse step,
        // otherwise this test would pass for a reason that has nothing to do with granularity.
        assert!(
            window_end - window_start < chrono::Duration::minutes(1),
            "fixture is not sub-step: the interval must be shorter than the coarse probe"
        );

        // The radius is a whole number of minutes, so a one-minute probe lands on base exactly and
        // steps to base + 60s, straddling the window. That is why the coarse arm misses
        // deterministically rather than by luck, and the assertion below fails loudly if it ever
        // stops being true.
        // `base` is a fixed ordinary date, so this arithmetic is provably in range; the helper
        // takes bounds rather than a radius, so the window is stated once here.
        let radius = chrono::Duration::hours(48);
        let lo = base - radius;
        let hi = base + radius;
        let coarse =
            find_instant_carrying_date(lo, hi, chrono::Duration::minutes(1), target, calendar);
        // The FINE arm reads the same `probe_step()` the sweep passes, so coarsening the sweep's
        // granularity reddens this test.
        let fine = find_instant_carrying_date(lo, hi, probe_step(), target, calendar);

        assert!(
            coarse.is_none(),
            "the one-minute probe found {coarse:?}, so this fixture does not discriminate and \
             the arm proves nothing"
        );
        let hit = fine.expect("the one-second probe must find the 30-second window");
        assert_eq!(
            calendar(hit),
            target,
            "the one-second probe returned {hit}, whose local date is not the target"
        );
    }

    /// Sweep every zone in the pinned `chrono-tz` over 1850-2100 and assert that
    /// `anchor_date_to_earliest_instant` returns the LEAST instant of the requested local date.
    ///
    /// This is the re-derivation duty the gap branch's comment names, and it exists because the
    /// comment used to assert a premise instead: that a local date is monotonically non-decreasing
    /// in UTC time. That premise is false globally — zones that crossed the international date line
    /// run the local calendar backwards, `America/Adak` at 1867-10-19T00:31:13Z being one — and a
    /// true premise supporting a wrong inference reads as verified, so this checks the inference.
    ///
    /// The property asserted is direct and needs no transition table: for the returned instant `r`,
    /// `r` has the requested local date and `r - 1s` does not. Anything less than the least instant
    /// fails the first half; anything more fails the second.
    ///
    /// Ignored by default because it is an all-zone sweep, not because it is optional:
    ///
    ///   cargo test -p khive-runtime --lib --release -- --ignored --nocapture
    #[test]
    #[ignore = "all-zone chrono-tz sweep; run on a pin bump (see the command in the doc comment)"]
    fn every_gap_date_resolves_to_the_least_instant() {
        let start = chrono::NaiveDate::from_ymd_opt(1850, 1, 1).expect("start date");
        let end = chrono::NaiveDate::from_ymd_opt(2100, 1, 1).expect("end date");

        let mut zones = 0usize;
        let mut gaps = 0usize;
        let mut folds = 0usize;
        let mut skipped_days = 0usize;
        let mut dates_examined = 0usize;
        let mut failures: Vec<String> = Vec::new();

        for tz in chrono_tz::TZ_VARIANTS.iter().copied() {
            zones += 1;
            let mut date = start;
            while date < end {
                dates_examined += 1;
                let midnight = date.and_hms_opt(0, 0, 0).expect("midnight");
                match tz.from_local_datetime(&midnight) {
                    chrono::LocalResult::Single(_) => {}
                    chrono::LocalResult::Ambiguous(_, _) => folds += 1,
                    chrono::LocalResult::None => gaps += 1,
                }

                match anchor_date_to_earliest_instant(date, tz) {
                    Some(resolved) => {
                        let previous = resolved - chrono::Duration::seconds(1);
                        if resolved.date_naive() != date {
                            failures.push(format!(
                                "{tz}: {date} resolved to {resolved}, whose local date is \
                                 {}",
                                resolved.date_naive()
                            ));
                        } else if tz.from_utc_datetime(&previous.naive_utc()).date_naive() == date {
                            failures.push(format!(
                                "{tz}: {date} resolved to {resolved}, but one second earlier is \
                                 still {date} — not the least instant"
                            ));
                        }
                    }
                    // `None` is correct only for a date the zone's local calendar skips whole.
                    // Verify that rather than accepting it: no instant in the searched window
                    // may carry the requested local date.
                    None => {
                        skipped_days += 1;
                        // ONE-SECOND steps, matching the granularity the implementation itself
                        // searches at. A coarser step was here first and was wrong in the
                        // reassuring direction: a local-date interval shorter than the step can
                        // sit between two probes, so the search reports "no instant carries this
                        // date" without having looked at the instants that would. The pinned table
                        // happens to contain no such interval, but that is a property of today's
                        // data and this test exists to be run against data that has changed, which
                        // is exactly why the step size is pinned by
                        // `a_sub_step_interval_is_missed_by_a_coarse_probe` against a fabricated
                        // calendar rather than by this sweep. Skipped days are rare (single digits
                        // across the whole sweep), so the finer step costs nothing measurable.
                        //
                        // The bounds come from `anchor_search_window`, not from a literal, so the
                        // searched window has one definition. A second literal here would let a
                        // change to `ANCHOR_SEARCH_RADIUS_HOURS` leave this sweep checking the old
                        // window while still reporting zero violations.
                        let (lo, hi) = anchor_search_window(date);
                        let found =
                            find_instant_carrying_date(lo, hi, probe_step(), date, |probe| {
                                tz.from_utc_datetime(&probe).date_naive()
                            });
                        if let Some(hit) = found {
                            failures.push(format!(
                                "{tz}: {date} was reported as carried by no instant, but {hit}Z \
                                 has that local date"
                            ));
                        }
                    }
                }

                date = date.succ_opt().expect("date in range has a successor");
            }
        }

        println!(
            "swept {zones} zones over {} dates each: {gaps} gaps, {folds} folds, \
             {skipped_days} whole days skipped by a zone's calendar, {} violations",
            (end - start).num_days(),
            failures.len()
        );

        // The substantive result is asserted FIRST, deliberately. An earlier version put the
        // population checks above this one and guessed their thresholds; one guess was wrong
        // (it demanded thousands of fold dates against an actual 818), the test failed on the
        // guess, and the property it exists to check never ran. A bound nobody measured is not
        // a safety net, it is a second thing that can be wrong.
        assert!(
            failures.is_empty(),
            "{} least-instant violations, first 10:\n{}",
            failures.len(),
            failures
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Then assert the sweep actually examined something, so an audit that silently swept
        // nothing cannot pass by finding nothing.
        //
        // TRAVERSAL counts are exact, because they are ours: the number of zones and the number
        // of dates per zone are fixed by this test's own range and by the pinned table's length,
        // so anything else means the loop did not run. A floor here would have accepted a
        // 501-zone subset of 597 as "the full database".
        //
        // TRANSITION counts stay floors at zero per SHAPE. Those belong to the database, not to
        // us, and this test exists to be run when the database changes; pinning 4299 gaps would
        // make a legitimate pin bump fail for a reason that has nothing to do with the property
        // under test. Zero for a shape means the traversal never reached that kind of date.
        let expected_dates = (end - start).num_days() as usize;
        assert_eq!(
            zones,
            chrono_tz::TZ_VARIANTS.len(),
            "swept {zones} zones but the pinned table has {}",
            chrono_tz::TZ_VARIANTS.len()
        );
        assert_eq!(
            dates_examined,
            zones * expected_dates,
            "expected {zones} zones x {expected_dates} dates = {}, examined {dates_examined}",
            zones * expected_dates
        );
        assert!(
            gaps > 0,
            "no gap dates found -- the sweep did not reach a transition"
        );
        assert!(
            folds > 0,
            "no fold dates found -- the sweep did not reach a transition"
        );
        assert!(
            skipped_days > 0,
            "no whole-day skips found -- the None-return path was never exercised"
        );
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// The boundary is a property of the window, not of a magic year, so pin
    /// it against `anchor_search_window` itself rather than a literal. Every
    /// place that needs the searched window — the resolver and the all-zone
    /// sweep — calls this one function, so this is the single definition.
    #[test]
    fn the_search_window_clamps_at_the_bounds_and_is_exact_elsewhere() {
        let (lo, hi) = super::anchor_search_window(chrono::NaiveDate::MIN);
        assert_eq!(
            lo,
            chrono::NaiveDateTime::MIN,
            "the minimum date cannot carry a full -48h window, so the bound clamps \
             rather than vanishing"
        );
        assert!(
            hi > chrono::NaiveDateTime::MIN,
            "the upper bound is exact here"
        );

        let (_, hi_max) = super::anchor_search_window(chrono::NaiveDate::MAX);
        assert_eq!(
            hi_max,
            chrono::NaiveDateTime::MAX,
            "and symmetrically at the top of the range"
        );

        // The must-not-fire half: away from the bounds the window is the real
        // +/-48h, so the clamp cannot be silently swallowing ordinary dates.
        let ordinary = chrono::NaiveDate::from_ymd_opt(2026, 8, 23).expect("valid date");
        let midnight = ordinary.and_hms_opt(0, 0, 0).expect("midnight");
        let radius = chrono::Duration::hours(super::ANCHOR_SEARCH_RADIUS_HOURS);
        assert_eq!(
            super::anchor_search_window(ordinary),
            (midnight - radius, midnight + radius),
            "an ordinary date gets the exact window"
        );
    }

    /// `local_date_cmp` exists because `date_naive()` PANICS at the bounds.
    /// Exercise exactly those two spellings' disagreement, or the helper looks
    /// like ceremony to the next reader.
    #[test]
    fn local_date_cmp_is_total_where_date_naive_panics() {
        let east: Tz = "Pacific/Apia".parse().expect("known IANA zone");
        assert_eq!(
            super::local_date_cmp(east, chrono::NaiveDateTime::MAX, chrono::NaiveDate::MAX),
            std::cmp::Ordering::Greater,
            "east of UTC the top of the range maps past the last representable date"
        );
        // NOT `America/Adak`, whose earliest LMT is +12:13:22 because it
        // crossed the date line; a zone is chosen here by its MEASURED offset
        // at the instant under test, never by what its name suggests.
        let west: Tz = "America/New_York".parse().expect("known IANA zone");
        assert_eq!(
            super::local_date_cmp(west, chrono::NaiveDateTime::MIN, chrono::NaiveDate::MIN),
            std::cmp::Ordering::Less,
            "west of UTC the bottom of the range maps before the first representable date"
        );
    }
}
