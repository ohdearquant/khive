//! Original tie-band and full training-byte parity with actual comparison bounds.

use super::*;

// Literal public d17642c7 oracles: no sorted-prefix counting in either oracle.
fn baseline_tie_band(tie_margins: &[f64], decisive_margins: &[f64]) -> (f64, f64) {
    let mut candidates = vec![0.0, 0.5];
    candidates.extend_from_slice(tie_margins);
    candidates.extend_from_slice(decisive_margins);
    candidates.sort_by(f64::total_cmp);
    candidates.dedup_by(|left, right| left.to_bits() == right.to_bits());

    let mut best = (f64::INFINITY, 0.0);
    for threshold in candidates {
        let tie_false_negative = tie_margins
            .iter()
            .filter(|margin| **margin > threshold)
            .count() as f64
            / tie_margins.len() as f64;
        let decisive_false_positive = decisive_margins
            .iter()
            .filter(|margin| **margin <= threshold)
            .count() as f64
            / decisive_margins.len() as f64;
        let balanced_error = 0.5 * (tie_false_negative + decisive_false_positive);
        if balanced_error < best.0 || (balanced_error == best.0 && threshold < best.1) {
            best = (balanced_error, threshold);
        }
    }
    (best.1, best.0)
}

fn baseline_train_model(
    data: &PreparedTrainingData,
    scope: PreferenceScope,
) -> Result<TrainedModel, RuntimeError> {
    let training_examples = data.decisive_examples(DataSplit::Train);
    let fit = fit_logistic(&training_examples)?;
    let (network, network_bytes) = materialize_fann(&fit.weights)?;

    let all_logits = fann_logits(
        &network,
        data.observations
            .iter()
            .map(|observation| (observation.judgment_id, observation.x)),
    )?;
    let calibration_examples = data.decisive_examples(DataSplit::Calibration);
    let calibration_logits: Vec<f64> = calibration_examples
        .iter()
        .map(|example| all_logits[&example.judgment_id])
        .collect();
    let temperature = calibrate_temperature(&calibration_examples, &calibration_logits);
    if !temperature.is_finite() || temperature <= 0.0 {
        return Err(RuntimeError::Internal(
            "moodboard temperature calibration returned an invalid temperature".to_string(),
        ));
    }

    let probabilities: BTreeMap<Uuid, f64> = all_logits
        .iter()
        .map(|(id, logit)| (*id, stable_sigmoid(*logit / temperature)))
        .collect();
    let tie_margins = data.class_group_margins(
        DataSplit::Calibration,
        &probabilities,
        CalibrationClass::Tie,
    );
    let decisive_margins = data.class_group_margins(
        DataSplit::Calibration,
        &probabilities,
        CalibrationClass::Decisive,
    );
    let (tie_band_half_width, tie_balanced_error) =
        baseline_tie_band(&tie_margins, &decisive_margins);
    if !tie_band_half_width.is_finite() || !(0.0..=0.5).contains(&tie_band_half_width) {
        return Err(RuntimeError::Internal(
            "moodboard tie-band calibration returned an invalid threshold".to_string(),
        ));
    }

    let split_counts = data
        .counts
        .iter()
        .map(|(split, count)| (split.name().to_string(), count.clone()))
        .collect();
    let bundle = ModelBundle {
        schema_version: MODEL_BUNDLE_SCHEMA_VERSION.to_string(),
        model_family: MODEL_FAMILY.to_string(),
        scope,
        feature_schema_version: FEATURE_SCHEMA_VERSION.to_string(),
        feature_schema_canonical_json_base64: BASE64.encode(FEATURE_SCHEMA_CANONICAL_JSON),
        training: TrainingProvenance {
            snapshot_sha256: data.snapshot_sha256.clone(),
            snapshot_event_count: data.snapshot_event_count,
            excluded_probability_shown: data.excluded_probability_shown,
            split_revision: PAIR_SPLIT_REVISION.to_string(),
            split_counts,
            optimizer: fit.provenance,
        },
        calibration: CalibrationProvenance {
            calibrated: true,
            temperature,
            log_temperature_bounds: [LOG_TEMPERATURE_MIN, LOG_TEMPERATURE_MAX],
            temperature_search_iterations: TEMPERATURE_SEARCH_ITERATIONS,
            tie_band_half_width,
            tie_band_rule: TIE_BAND_RULE_IDENTITY.to_string(),
            tie_balanced_error,
        },
        test_metrics: test_metrics(data, &probabilities, tie_band_half_width),
        fann: FannProvenance {
            crate_name: "lattice-fann".to_string(),
            crate_version: crate::LATTICE_VERSION.to_string(),
            format: FANN_FORMAT.to_string(),
            architecture: format!("{FEATURE_COUNT}->1 linear; zero intercept"),
            network_content_ref: String::new(),
            network_sha256: sha256_hex(&network_bytes),
        },
    };
    Ok(TrainedModel {
        bundle,
        network_bytes,
    })
}

#[derive(Default, Debug)]
struct Work {
    sorts: usize,
    dedups: usize,
    inputs: usize,
    margins: usize,
}

impl Work {
    fn observe(&mut self, event: TieBandWork) {
        match event {
            TieBandWork::SortComparison => self.sorts += 1,
            TieBandWork::DedupComparison => self.dedups += 1,
            TieBandWork::InputCheck => self.inputs += 1,
            TieBandWork::MarginComparison => self.margins += 1,
        }
    }
    fn comparisons(&self) -> usize {
        self.sorts + self.dedups + self.margins
    }
}

fn bits(result: (f64, f64)) -> (u64, u64) {
    (result.0.to_bits(), result.1.to_bits())
}

fn assert_parity(tie: &[f64], decisive: &[f64]) {
    assert_eq!(
        bits(calibrate_tie_band(tie, decisive)),
        bits(baseline_tie_band(tie, decisive)),
        "tie={tie:?}; decisive={decisive:?}"
    );
}

#[test]
fn tie_band_finite_boundaries_and_empty_classes_bit_parity() {
    let half_below = f64::from_bits(0.5f64.to_bits() - 1);
    let half_above = f64::from_bits(0.5f64.to_bits() + 1);
    let zero_above = f64::from_bits(1);
    let cases = [
        (vec![], vec![]),
        (vec![], vec![0.2]),
        (vec![0.1], vec![]),
        (vec![0.0, -0.0, zero_above], vec![-0.0, 0.0, -zero_above]),
        (
            vec![0.5, half_below, half_above],
            vec![half_above, half_below, 0.5],
        ),
        (vec![-2.0, -0.5, 0.3, 0.3], vec![-1.0, 0.3, 0.4]),
        (vec![0.48, 0.1, 0.2, 0.48], vec![0.2, 0.3, 0.15, 0.4, 0.3]),
    ];
    for (tie, decisive) in cases {
        assert_parity(&tie, &decisive);
    }
}

#[test]
fn tie_band_equal_threshold_uses_original_inclusive_predicate() {
    let expected = baseline_tie_band(&[0.25], &[0.25]);
    assert_eq!(
        bits(expected),
        bits((0.0, 0.5)),
        "oracle establishes the strict-predicate witness"
    );
    assert_parity(&[0.25], &[0.25]);
}

#[test]
fn tie_band_zero_false_positive_companion_bit_parity() {
    assert_parity(&[0.0], &[0.5]);
}

#[test]
fn tie_band_nonfinite_complement_preserves_original_nan_error_bits() {
    let nan = f64::from_bits(0xfff8_0000_0000_0001);
    let expected = baseline_tie_band(&[nan], &[0.5]);
    assert_eq!(
        expected.0.to_bits(),
        nan.to_bits(),
        "oracle actually selects the signed NaN candidate"
    );
    assert_eq!(
        expected.1.to_bits(),
        0.0f64.to_bits(),
        "original comparisons count no errors at NaN"
    );
    assert_parity(&[nan], &[0.5]);
}

#[test]
fn tie_band_nan_payloads_infinities_and_signed_zero_bit_parity() {
    let nan_a = f64::from_bits(0x7ff8_0000_0000_0001);
    let nan_b = f64::from_bits(0x7ff8_0000_0000_0002);
    let negative_nan = f64::from_bits(0xfff8_0000_0000_0002);
    for (tie, decisive) in [
        (vec![nan_a, nan_b, -0.0, 0.0], vec![0.5, negative_nan]),
        (vec![f64::NEG_INFINITY], vec![f64::INFINITY]),
        (vec![f64::INFINITY, 0.1], vec![f64::NEG_INFINITY, 0.3]),
        (vec![nan_a], vec![nan_b]),
        (vec![-0.0, 0.0], vec![0.0, -0.0]),
    ] {
        assert_parity(&tie, &decisive);
    }
}

#[test]
fn tie_band_real_comparison_work_has_sorted_prefix_bound() {
    for groups in [1_024usize, 4_096] {
        let tie: Vec<_> = (0..groups / 2)
            .rev()
            .map(|index| (2 * index + 1) as f64 / (2 * groups) as f64)
            .collect();
        let decisive: Vec<_> = (0..groups / 2)
            .rev()
            .map(|index| (2 * index + 2) as f64 / (2 * groups) as f64)
            .collect();
        let mut work = Work::default();
        let actual = calibrate_tie_band_observed(&tie, &decisive, |event| work.observe(event));
        assert_eq!(bits(actual), bits(baseline_tie_band(&tie, &decisive)));
        let log = groups.ilog2() as usize + 2;
        let bound = 16 * (groups + 2) * log;
        assert!(work.inputs <= groups);
        assert!(
            work.comparisons() > groups,
            "must observe real sort/predicate comparisons: {work:?}"
        );
        assert!(work.comparisons() <= bound, "actual comparisons exceed the sorted-prefix bound: groups={groups}, bound={bound}, work={work:?}");
    }
}

#[test]
fn tie_band_duplicate_population_remains_small_candidate_companion() {
    let tie = vec![0.25; 512];
    let decisive = vec![0.25; 512];
    let mut work = Work::default();
    let actual = calibrate_tie_band_observed(&tie, &decisive, |event| work.observe(event));
    assert_eq!(bits(actual), bits(baseline_tie_band(&tie, &decisive)));
    assert!(
        work.comparisons() <= 32 * (tie.len() + decisive.len()),
        "duplicate population has only three candidates: {work:?}"
    );
}

#[test]
fn training_bundle_json_network_and_metrics_match_original_calibration() {
    let scope = tests::fixture_scope();
    let records = tests::sufficient_records(true);
    let data = prepare_training_data(&records, &scope).unwrap();
    let expected = baseline_train_model(&data, scope.clone()).unwrap();
    let actual = train_model(&data, scope).unwrap();
    assert!(!actual.network_bytes.is_empty());
    assert!(actual.bundle.training.optimizer.converged);
    assert_eq!(actual.network_bytes, expected.network_bytes);
    assert_eq!(actual.bundle, expected.bundle);
    assert_eq!(
        serde_json::to_vec(&actual.bundle).unwrap(),
        serde_json::to_vec(&expected.bundle).unwrap()
    );
}
