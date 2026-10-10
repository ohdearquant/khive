use chrono::{DateTime, TimeDelta, Utc};
use khive_brain_core::{EvidenceHalfLifeDays, EvidencePolarity, EvidencePosterior, EvidenceUpdate};
use serde::de::value::{Error as ValueError, F64Deserializer};
use serde::Deserialize;

fn anchor() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap()
}

fn half_life(days: f64) -> EvidenceHalfLifeDays {
    EvidenceHalfLifeDays::try_new(days).unwrap()
}

fn observed(state: &EvidencePosterior) -> (u64, u64, DateTime<Utc>) {
    (
        state.alpha_ev().to_bits(),
        state.beta_ev().to_bits(),
        state.last_event_at(),
    )
}

fn close(actual: f64, expected: f64) {
    assert!(actual.is_finite(), "nonfinite result: {actual}");
    assert!(
        (actual - expected).abs() <= 2e-15,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn empty_evidence_has_one_fixed_prior_and_reads_preserve_the_anchor() {
    let state = EvidencePosterior::new(anchor());
    let before = observed(&state);
    assert_eq!(before, (0.0_f64.to_bits(), 0.0_f64.to_bits(), anchor()));
    for now in [
        DateTime::<Utc>::MIN_UTC,
        anchor(),
        anchor() + TimeDelta::days(30),
        DateTime::<Utc>::MAX_UTC,
    ] {
        assert_eq!(state.mean_at(now, EvidenceHalfLifeDays::default()), 0.5);
        assert_eq!(observed(&state), before);
    }
}

#[test]
fn projection_decays_only_evidence_and_never_stores_the_read_prior() {
    let state = EvidencePosterior::try_new(3.0, 1.0, anchor()).unwrap();
    let before = observed(&state);
    for (days, expected) in [(0, 2.0 / 3.0), (30, 5.0 / 8.0), (60, 7.0 / 12.0)] {
        close(
            state.mean_at(anchor() + TimeDelta::days(days), half_life(30.0)),
            expected,
        );
        assert_eq!(observed(&state), before);
    }
    // A later read cannot change an earlier projection.
    close(state.mean_at(anchor(), half_life(30.0)), 2.0 / 3.0);
}

#[test]
fn later_observations_decay_both_sides_before_adding_and_stamping() {
    let h = half_life(10.0);
    let mut state = EvidencePosterior::try_new(8.0, 4.0, anchor()).unwrap();
    let first = anchor() + TimeDelta::days(10);
    assert_eq!(
        state.observe_at(EvidencePolarity::Positive, 3.0, first, h),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(
        observed(&state),
        (7.0_f64.to_bits(), 2.0_f64.to_bits(), first)
    );
    close(state.mean_at(first, h), 8.0 / 11.0);

    let second = anchor() + TimeDelta::days(20);
    assert_eq!(
        state.observe_at(EvidencePolarity::Negative, 0.5, second, h),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(
        observed(&state),
        (3.5_f64.to_bits(), 1.5_f64.to_bits(), second)
    );
    close(state.mean_at(second, h), 9.0 / 14.0);
}

#[test]
fn equal_timestamp_adds_fractional_mass_without_changing_the_stamp() {
    let mut state = EvidencePosterior::try_new(4.0, 6.0, anchor()).unwrap();
    for (polarity, weight) in [
        (EvidencePolarity::Positive, 0.5),
        (EvidencePolarity::Negative, 0.25),
    ] {
        assert_eq!(
            state.observe_at(polarity, weight, anchor(), half_life(f64::from_bits(1))),
            Ok(EvidenceUpdate::Applied)
        );
        assert_eq!(state.last_event_at(), anchor());
    }
    assert_eq!(state.alpha_ev(), 4.5);
    assert_eq!(state.beta_ev(), 6.25);
}

#[test]
fn earlier_observations_ignore_even_unusable_mass_and_preserve_exact_field_bits() {
    for (alpha, beta) in [(-0.0, 4.0), (3.0, -0.0), (-0.0, -0.0)] {
        for polarity in [EvidencePolarity::Positive, EvidencePolarity::Negative] {
            let mut state = EvidencePosterior::try_new(alpha, beta, anchor()).unwrap();
            let before = observed(&state);
            for weight in [
                2.0,
                0.0,
                -0.0,
                -1.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ] {
                assert_eq!(
                    state.observe_at(
                        polarity,
                        weight,
                        anchor() - TimeDelta::nanoseconds(1),
                        half_life(30.0)
                    ),
                    Ok(EvidenceUpdate::IgnoredClockRegression)
                );
                assert_eq!(observed(&state), before);
            }
        }
    }
}

#[test]
fn regressed_read_clock_does_not_amplify_evidence() {
    let state = EvidencePosterior::try_new(1.0, 5.0, anchor()).unwrap();
    let before = observed(&state);
    for now in [
        DateTime::<Utc>::MIN_UTC,
        anchor() - TimeDelta::nanoseconds(1),
        anchor(),
    ] {
        assert_eq!(state.mean_at(now, half_life(0.25)), 0.25);
        assert_eq!(observed(&state), before);
    }
}

#[test]
fn mixed_event_prefixes_match_literal_states_and_replay_identically() {
    let t0 = anchor();
    let t1 = t0 + TimeDelta::days(2);
    let t2 = t0 + TimeDelta::days(4);
    let events = [
        (
            EvidencePolarity::Positive,
            2.0,
            t0,
            2.0_f64,
            0.0_f64,
            t0,
            EvidenceUpdate::Applied,
        ),
        (
            EvidencePolarity::Negative,
            1.0,
            t0,
            2.0,
            1.0,
            t0,
            EvidenceUpdate::Applied,
        ),
        (
            EvidencePolarity::Positive,
            3.0,
            t1,
            4.0,
            0.5,
            t1,
            EvidenceUpdate::Applied,
        ),
        (
            EvidencePolarity::Negative,
            9.0,
            t0,
            4.0,
            0.5,
            t1,
            EvidenceUpdate::IgnoredClockRegression,
        ),
        (
            EvidencePolarity::Negative,
            1.0,
            t2,
            2.0,
            1.25,
            t2,
            EvidenceUpdate::Applied,
        ),
        (
            EvidencePolarity::Positive,
            0.5,
            t2,
            2.5,
            1.25,
            t2,
            EvidenceUpdate::Applied,
        ),
    ];
    let expected_means = [
        3.0 / 4.0,
        3.0 / 5.0,
        10.0 / 13.0,
        10.0 / 13.0,
        4.0 / 7.0,
        14.0 / 23.0,
    ];
    let mut reference_run = Vec::new();
    for run in 0..2 {
        let mut state = EvidencePosterior::new(t0);
        let mut prefixes = Vec::new();
        for ((polarity, weight, time, alpha, beta, stamp, outcome), mean) in
            events.into_iter().zip(expected_means)
        {
            assert_eq!(
                state.observe_at(polarity, weight, time, half_life(2.0)),
                Ok(outcome)
            );
            assert_eq!(observed(&state), (alpha.to_bits(), beta.to_bits(), stamp));
            close(state.mean_at(stamp, half_life(2.0)), mean);
            prefixes.push(observed(&state));
        }
        if run == 0 {
            reference_run = prefixes;
        } else {
            assert_eq!(prefixes, reference_run);
        }
    }
}

#[test]
fn recovery_band_depends_on_evidence_mass_not_a_flat_two_half_lives() {
    let h = half_life(30.0);
    for (mass, expected_mean, within_two) in [
        (1.0_f64, 4.0 / 9.0, true),
        (2.0, 2.0 / 5.0, false),
        (8.0, 1.0 / 4.0, false),
    ] {
        let state = EvidencePosterior::try_new(0.0, mass, anchor()).unwrap();
        let before = observed(&state);
        let mean = state.mean_at(anchor() + TimeDelta::days(60), h);
        close(mean, expected_mean);
        let multiplier = (1.0 + 0.3 * (mean - 0.5)).clamp(0.85, 1.15);
        assert_eq!((multiplier - 1.0).abs() < 0.02, within_two);
        // Round up to the next whole second of the ADR's magnitude-dependent bound.
        let seconds = (30.0 * (mass.log2() + 2.2) * 86_400.0).ceil() as i64;
        let recovered = state.mean_at(anchor() + TimeDelta::seconds(seconds), h);
        let multiplier = (1.0 + 0.3 * (recovered - 0.5)).clamp(0.85, 1.15);
        assert!((multiplier - 1.0).abs() < 0.02);
        assert_eq!(state.mean_at(anchor() + TimeDelta::days(36_000), h), 0.5);
        assert_eq!(observed(&state), before);
    }
}

#[test]
fn half_life_configuration_validates_all_construction_and_deserialization_routes() {
    assert_eq!(EvidenceHalfLifeDays::default().days(), 30.0);
    for invalid in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(EvidenceHalfLifeDays::try_new(invalid).is_err());
        assert!(EvidenceHalfLifeDays::try_from(invalid).is_err());
        assert!(
            EvidenceHalfLifeDays::deserialize(F64Deserializer::<ValueError>::new(invalid)).is_err()
        );
    }
    for invalid in [
        "0", "-0.0", "-1", "1e400", "null", "true", "\"30\"", "[]", "{}",
    ] {
        assert!(serde_json::from_str::<EvidenceHalfLifeDays>(invalid).is_err());
    }
    for valid in [0.25, 30.0, f64::MAX] {
        let h = EvidenceHalfLifeDays::try_from(valid).unwrap();
        assert_eq!(h.days(), valid);
        let wire = serde_json::to_value(h).unwrap();
        assert!(wire.is_number());
        assert_eq!(
            serde_json::from_value::<EvidenceHalfLifeDays>(wire).unwrap(),
            h
        );
    }
}

#[test]
fn smallest_positive_half_life_is_valid_and_later_decay_can_underflow_to_prior() {
    let smallest = f64::from_bits(1);
    let h = EvidenceHalfLifeDays::try_new(smallest).unwrap();
    assert_eq!(h.days().to_bits(), 1);
    assert_eq!(
        EvidenceHalfLifeDays::deserialize(F64Deserializer::<ValueError>::new(smallest)).unwrap(),
        h
    );
    let mut state = EvidencePosterior::try_new(8.0, 0.0, anchor()).unwrap();
    close(state.mean_at(anchor(), h), 9.0 / 10.0);
    let next = anchor() + TimeDelta::nanoseconds(1);
    assert_eq!(state.mean_at(next, h), 0.5);
    assert_eq!(
        state.observe_at(EvidencePolarity::Negative, 0.5, next, h),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(
        observed(&state),
        (0.0_f64.to_bits(), 0.5_f64.to_bits(), next)
    );
}

#[test]
fn checked_restoration_rejects_invalid_counts_and_preserves_signed_zero() {
    for invalid in [
        -f64::from_bits(1),
        -1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert!(EvidencePosterior::try_new(invalid, 0.0, anchor()).is_err());
        assert!(EvidencePosterior::try_new(0.0, invalid, anchor()).is_err());
    }
    let signed = EvidencePosterior::try_new(-0.0, -0.0, anchor()).unwrap();
    assert_eq!(
        observed(&signed),
        ((-0.0_f64).to_bits(), (-0.0_f64).to_bits(), anchor())
    );
    let large = EvidencePosterior::try_new(f64::MAX, f64::MAX, anchor()).unwrap();
    assert_eq!(large.alpha_ev(), f64::MAX);
    assert_eq!(large.beta_ev(), f64::MAX);
}

#[test]
fn invalid_applicable_weights_preserve_both_counts_and_timestamp() {
    for time in [anchor(), anchor() + TimeDelta::days(30)] {
        for polarity in [EvidencePolarity::Positive, EvidencePolarity::Negative] {
            let mut state = EvidencePosterior::try_new(-0.0, 8.0, anchor()).unwrap();
            let before = observed(&state);
            for weight in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert!(state
                    .observe_at(polarity, weight, time, half_life(30.0))
                    .is_err());
                assert_eq!(observed(&state), before);
            }
        }
    }
}

#[test]
fn overflowing_additions_refuse_atomically_even_after_tentative_decay() {
    let later = anchor() + TimeDelta::days(30);
    for (alpha, beta, polarity) in [
        (f64::MAX, 8.0, EvidencePolarity::Positive),
        (8.0, f64::MAX, EvidencePolarity::Negative),
    ] {
        let mut state = EvidencePosterior::try_new(alpha, beta, anchor()).unwrap();
        let before = observed(&state);
        for time in [anchor(), later] {
            assert!(state
                .observe_at(polarity, f64::MAX, time, half_life(30.0))
                .is_err());
            assert_eq!(observed(&state), before);
        }
        assert_eq!(
            state.observe_at(polarity, f64::MAX / 4.0, later, half_life(30.0)),
            Ok(EvidenceUpdate::Applied)
        );
        let big = f64::MAX / 2.0 + f64::MAX / 4.0;
        let expected = match polarity {
            EvidencePolarity::Positive => (big.to_bits(), 4.0_f64.to_bits(), later),
            EvidencePolarity::Negative => (4.0_f64.to_bits(), big.to_bits(), later),
        };
        assert_eq!(observed(&state), expected);
    }
}

#[test]
fn extreme_finite_counts_have_stable_means_without_overflow_or_new_clamps() {
    for (alpha, beta, expected) in [
        (f64::MAX, f64::MAX, 0.5),
        (f64::MAX, f64::MAX / 2.0, 2.0 / 3.0),
        (f64::MAX / 2.0, f64::MAX, 1.0 / 3.0),
        (f64::MAX, 0.0, 1.0),
    ] {
        let state = EvidencePosterior::try_new(alpha, beta, anchor()).unwrap();
        close(state.mean_at(anchor(), half_life(30.0)), expected);
    }
    let negative = EvidencePosterior::try_new(0.0, f64::MAX, anchor()).unwrap();
    let mean = negative.mean_at(anchor(), half_life(30.0));
    assert!(mean.is_finite() && mean > 0.0 && mean < f64::MIN_POSITIVE);
    let mut large = EvidencePosterior::try_new(f64::MAX, 0.0, anchor()).unwrap();
    // A positive mass below the addition's precision is valid, not a fabricated minimum.
    let next = anchor() + TimeDelta::seconds(1);
    assert_eq!(
        large.observe_at(
            EvidencePolarity::Positive,
            f64::from_bits(1),
            next,
            half_life(f64::MAX)
        ),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(large.alpha_ev(), f64::MAX);
    assert_eq!(large.last_event_at(), next);
}

#[test]
fn extreme_chrono_dates_do_not_require_a_total_nanosecond_interval() {
    let earliest = DateTime::<Utc>::MIN_UTC;
    let latest = DateTime::<Utc>::MAX_UTC;
    let mut state = EvidencePosterior::try_new(2.0, 8.0, earliest).unwrap();
    assert_eq!(state.mean_at(latest, half_life(30.0)), 0.5);
    // Huge valid H must not overflow an intermediate seconds-per-half-life value.
    close(state.mean_at(latest, half_life(f64::MAX)), 0.25);
    assert_eq!(state.last_event_at(), earliest);
    assert_eq!(
        state.observe_at(EvidencePolarity::Positive, 4.0, latest, half_life(30.0)),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(
        observed(&state),
        (4.0_f64.to_bits(), 0.0_f64.to_bits(), latest)
    );
    close(state.mean_at(earliest, half_life(30.0)), 5.0 / 6.0);
}

#[test]
fn subsecond_intervals_preserve_fractional_decay_across_second_boundaries() {
    let time = DateTime::from_timestamp(1_700_000_000, 999_999_999).unwrap();
    let mut state = EvidencePosterior::try_new(8.0, 4.0, time).unwrap();
    let next = time + TimeDelta::nanoseconds(1);
    let h = half_life(1.0 / 86_400_000_000_000.0);
    close(state.mean_at(next, h), 5.0 / 8.0);
    assert_eq!(
        state.observe_at(EvidencePolarity::Negative, 2.0, next, h),
        Ok(EvidenceUpdate::Applied)
    );
    assert_eq!(
        observed(&state),
        (4.0_f64.to_bits(), 4.0_f64.to_bits(), next)
    );

    let state = EvidencePosterior::try_new(8.0, 4.0, time).unwrap();
    let next = time + TimeDelta::milliseconds(1_500);
    close(state.mean_at(next, half_life(1.5 / 86_400.0)), 5.0 / 8.0);
    close(
        state.mean_at(time - TimeDelta::nanoseconds(1), h),
        9.0 / 14.0,
    );
}
