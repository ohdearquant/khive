//! Governed pairwise-preference feature, training, calibration, and FANN model contracts.
//!
//! `lattice-fann` 0.7.1's `BackpropTrainer` optimizes MSE.  This module therefore
//! fits the Bradley--Terry logistic objective directly in deterministic `f64`,
//! then materializes the learned zero-intercept 10 -> 1 linear head as a FANN
//! network.  FANN owns the persisted binary representation and every served
//! forward pass; the local optimizer is deliberately not described as FANN
//! backpropagation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use lattice_fann::{Activation, Layer, Network};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use khive_runtime::RuntimeError;

pub(crate) const FEATURE_COUNT: usize = 10;
pub(crate) const FEATURE_SCHEMA_VERSION: &str = "moodboard.preference-features.v1";
pub(crate) const SERVE_SCHEMA_VERSION: &str = "moodboard.preference-serve.v1";
pub(crate) const JUDGMENT_SCHEMA_VERSION: &str = "moodboard.preference-judgment.v1";
pub(crate) const MODEL_BUNDLE_SCHEMA_VERSION: &str = "moodboard.preference-model.v1";
pub(crate) const PREFERENCE_RESPONSE_SCHEMA_VERSION: &str = "moodboard.preference.v1";
pub(crate) const RANDOMIZATION_REVISION: &str = "moodboard-side-v1";
pub(crate) const PAIR_SPLIT_REVISION: &str = "moodboard-pair-split-v1";
pub(crate) const TRAINING_REVISION: &str = "moodboard-logistic-bce-l2-v1";
pub(crate) const MODEL_FAMILY: &str = "pairwise_zero_intercept_logistic";
pub(crate) const FANN_CRATE_VERSION: &str = "0.7.1";
pub(crate) const FANN_FORMAT: &str = "FANN binary v1";

pub(crate) const MIN_TRAIN_DECISIVE_GROUPS: usize = 64;
pub(crate) const MIN_CAL_DECISIVE_GROUPS: usize = 16;
pub(crate) const MIN_TEST_DECISIVE_GROUPS: usize = 16;
pub(crate) const MIN_CAL_TIE_GROUPS: usize = 16;
pub(crate) const MAX_TRAINING_EVENTS: usize = 50_000;

const L2: f64 = 1.0e-2;
const MAX_ITERATIONS: usize = 2_048;
const GRADIENT_TOLERANCE: f64 = 1.0e-8;
const OBJECTIVE_TOLERANCE: f64 = 1.0e-12;
const ARMIJO: f64 = 1.0e-4;
const MAX_BACKTRACKS: usize = 64;
const LOG_TEMPERATURE_MIN: f64 = -4.0;
const LOG_TEMPERATURE_MAX: f64 = 4.0;
const TEMPERATURE_SEARCH_ITERATIONS: usize = 128;
pub(crate) const OPTIMIZER_BACKTRACKING_IDENTITY: &str =
    "armijo=0.0001;initial_step=1;shrink=0.5;max_backtracks=64";
pub(crate) const TIE_BAND_RULE_IDENTITY: &str =
    "minimum equal-class balanced error on grouped calibration tie/decisive margins; lower threshold wins ties";

pub(crate) const FEATURE_NAMES: [&str; FEATURE_COUNT] = [
    "visual_local_max_similarity_01",
    "visual_local_top3_mean_similarity_01",
    "visual_local_mean_similarity_01",
    "style_conformal_p",
    "style_interval_width",
    "local_support_fraction",
    "local_effective_support_fraction",
    "palette_compatibility",
    "tone_compatibility",
    "composition_compatibility",
];

/// Canonical, closed feature-schema bytes.  Keys are lexicographically ordered;
/// the array order is the model input order and is therefore identity-bearing.
pub(crate) const FEATURE_SCHEMA_CANONICAL_JSON: &str = concat!(
    "{\"bounds\":[0.0,1.0],\"dtype\":\"float32\",\"features\":[",
    "\"visual_local_max_similarity_01\",",
    "\"visual_local_top3_mean_similarity_01\",",
    "\"visual_local_mean_similarity_01\",",
    "\"style_conformal_p\",",
    "\"style_interval_width\",",
    "\"local_support_fraction\",",
    "\"local_effective_support_fraction\",",
    "\"palette_compatibility\",",
    "\"tone_compatibility\",",
    "\"composition_compatibility\"],",
    "\"pair_transform\":\"left_minus_right\",",
    "\"schema_version\":\"moodboard.preference-features.v1\"}"
);

static FEATURE_SCHEMA_ID: OnceLock<String> = OnceLock::new();

pub(crate) fn feature_schema_id() -> &'static str {
    FEATURE_SCHEMA_ID
        .get_or_init(|| sha256_hex(FEATURE_SCHEMA_CANONICAL_JSON.as_bytes()))
        .as_str()
}

pub(crate) fn feature_schema_response() -> serde_json::Value {
    serde_json::json!({
        "schema_version": FEATURE_SCHEMA_VERSION,
        "feature_schema_id": feature_schema_id(),
        "dtype": "float32",
        "bounds": [0.0, 1.0],
        "pair_transform": "left_minus_right",
        "features": FEATURE_NAMES,
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

pub(crate) fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn validate_features(
    features: &[f32; FEATURE_COUNT],
    context: &str,
) -> Result<(), RuntimeError> {
    for (index, value) in features.iter().copied().enumerate() {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(RuntimeError::InvalidInput(format!(
                "{context}: feature {} ({}) must be finite and in [0,1], got {value}",
                FEATURE_NAMES[index], index
            )));
        }
    }
    Ok(())
}

pub(crate) fn feature_difference(
    left: &[f32; FEATURE_COUNT],
    right: &[f32; FEATURE_COUNT],
) -> [f64; FEATURE_COUNT] {
    std::array::from_fn(|index| f64::from(left[index]) - f64::from(right[index]))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreferenceScope {
    pub namespace: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub board_entity_id: Uuid,
    pub board_id: String,
    pub model_key: String,
    pub descriptor_fingerprint: String,
    pub feature_schema_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JudgmentChoice {
    Left,
    Right,
    Tie,
    Abstain,
}

impl JudgmentChoice {
    pub(crate) fn decisive_label(self) -> Option<f64> {
        match self {
            Self::Left => Some(1.0),
            Self::Right => Some(0.0),
            Self::Tie | Self::Abstain => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasonCode {
    Style,
    Palette,
    Tone,
    Composition,
    EquallyGood,
    EquallyBad,
    InsufficientContext,
    BothUnacceptable,
    RenderFailure,
    Other,
}

pub(crate) fn validate_reason_code(
    choice: JudgmentChoice,
    reason: Option<ReasonCode>,
) -> Result<(), RuntimeError> {
    let allowed = match choice {
        JudgmentChoice::Left | JudgmentChoice::Right => matches!(
            reason,
            None | Some(ReasonCode::Style)
                | Some(ReasonCode::Palette)
                | Some(ReasonCode::Tone)
                | Some(ReasonCode::Composition)
                | Some(ReasonCode::Other)
        ),
        JudgmentChoice::Tie => matches!(
            reason,
            None | Some(ReasonCode::EquallyGood)
                | Some(ReasonCode::EquallyBad)
                | Some(ReasonCode::Other)
        ),
        JudgmentChoice::Abstain => matches!(
            reason,
            Some(ReasonCode::InsufficientContext)
                | Some(ReasonCode::BothUnacceptable)
                | Some(ReasonCode::RenderFailure)
                | Some(ReasonCode::Other)
        ),
    };
    if !allowed {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.judge reason_code is incompatible with choice {choice:?}; abstain requires an abstention reason"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResultOccurrence {
    pub result_occurrence_id: Uuid,
    pub source_candidate_index: u8,
    pub asset_id: Uuid,
    pub content_ref: String,
    pub source_rank: Option<u32>,
    pub features: [f32; FEATURE_COUNT],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectionProvenance {
    pub policy_revision: String,
    pub pair_propensity: Option<f64>,
    pub candidate_pool_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresentationProvenance {
    pub preference_probability_shown: bool,
    pub source_rank_shown: bool,
    pub served_preference_model_id: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RandomizationProvenance {
    pub revision: String,
    pub sha256: String,
    pub swap_applied: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServeRecord {
    pub schema_version: String,
    pub serve_id: Uuid,
    pub scope: PreferenceScope,
    pub source_report_sha256: String,
    pub left: ResultOccurrence,
    pub right: ResultOccurrence,
    pub selection: SelectionProvenance,
    pub presentation: PresentationProvenance,
    pub randomization: RandomizationProvenance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JudgmentRecord {
    pub schema_version: String,
    pub judgment_id: Uuid,
    pub serve_id: Uuid,
    pub scope: PreferenceScope,
    pub source_report_sha256: String,
    pub left: ResultOccurrence,
    pub right: ResultOccurrence,
    pub selection: SelectionProvenance,
    pub presentation: PresentationProvenance,
    pub randomization: RandomizationProvenance,
    pub choice: JudgmentChoice,
    pub reason_code: Option<ReasonCode>,
    pub response_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DataSplit {
    Train,
    Calibration,
    Test,
}

impl DataSplit {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Calibration => "calibration",
            Self::Test => "test",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PairKey {
    pub lower_content_ref: String,
    pub upper_content_ref: String,
}

impl PairKey {
    pub(crate) fn new(left: &str, right: &str) -> Self {
        if left <= right {
            Self {
                lower_content_ref: left.to_string(),
                upper_content_ref: right.to_string(),
            }
        } else {
            Self {
                lower_content_ref: right.to_string(),
                upper_content_ref: left.to_string(),
            }
        }
    }
}

pub(crate) fn pair_split(scope: &PreferenceScope, pair: &PairKey) -> DataSplit {
    let mut hasher = Sha256::new();
    hasher.update(PAIR_SPLIT_REVISION.as_bytes());
    hasher.update([0]);
    for field in [
        scope.board_id.as_str(),
        scope.descriptor_fingerprint.as_str(),
        scope.feature_schema_id.as_str(),
        pair.lower_content_ref.as_str(),
        pair.upper_content_ref.as_str(),
    ] {
        hasher.update(field.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let bucket =
        u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix is 8 bytes")) % 20;
    match bucket {
        0..=13 => DataSplit::Train,
        14..=16 => DataSplit::Calibration,
        _ => DataSplit::Test,
    }
}

#[derive(Clone, Debug)]
struct Observation {
    judgment_id: Uuid,
    pair: PairKey,
    split: DataSplit,
    x: [f64; FEATURE_COUNT],
    choice: JudgmentChoice,
}

#[derive(Clone, Debug)]
struct WeightedDecisive {
    judgment_id: Uuid,
    x: [f64; FEATURE_COUNT],
    y: f64,
    weight: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SplitCounts {
    pub decisive_groups: usize,
    pub decisive_judgments: usize,
    pub left_labels: usize,
    pub right_labels: usize,
    pub tie_groups: usize,
    pub tie_judgments: usize,
    pub abstain_groups: usize,
    pub abstain_judgments: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedTrainingData {
    observations: Vec<Observation>,
    pub snapshot_sha256: String,
    pub snapshot_event_count: usize,
    pub excluded_probability_shown: usize,
    pub counts: BTreeMap<DataSplit, SplitCounts>,
}

impl PreparedTrainingData {
    fn decisive_examples(&self, split: DataSplit) -> Vec<WeightedDecisive> {
        let mut grouped: BTreeMap<&PairKey, Vec<&Observation>> = BTreeMap::new();
        for observation in self.observations.iter().filter(|observation| {
            observation.split == split && observation.choice.decisive_label().is_some()
        }) {
            grouped
                .entry(&observation.pair)
                .or_default()
                .push(observation);
        }
        let mut examples = Vec::new();
        for observations in grouped.values() {
            let weight = 1.0 / observations.len() as f64;
            for observation in observations {
                examples.push(WeightedDecisive {
                    judgment_id: observation.judgment_id,
                    x: observation.x,
                    y: observation
                        .choice
                        .decisive_label()
                        .expect("group contains decisive judgments"),
                    weight,
                });
            }
        }
        examples
    }

    fn class_group_margins(
        &self,
        split: DataSplit,
        probabilities: &BTreeMap<Uuid, f64>,
        class: CalibrationClass,
    ) -> Vec<f64> {
        let mut grouped: BTreeMap<&PairKey, Vec<f64>> = BTreeMap::new();
        for observation in self.observations.iter().filter(|observation| {
            observation.split == split
                && match class {
                    CalibrationClass::Decisive => observation.choice.decisive_label().is_some(),
                    CalibrationClass::Tie => observation.choice == JudgmentChoice::Tie,
                }
        }) {
            let probability = probabilities
                .get(&observation.judgment_id)
                .expect("every calibrated observation has a probability");
            grouped
                .entry(&observation.pair)
                .or_default()
                .push((probability - 0.5).abs());
        }
        grouped
            .into_values()
            .map(|margins| margins.iter().sum::<f64>() / margins.len() as f64)
            .collect()
    }
}

#[derive(Clone, Copy)]
enum CalibrationClass {
    Decisive,
    Tie,
}

pub(crate) fn prepare_training_data(
    records: &[(i64, JudgmentRecord)],
    scope: &PreferenceScope,
) -> Result<PreparedTrainingData, RuntimeError> {
    if records.len() > MAX_TRAINING_EVENTS {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.train_preference matched {} judgments, exceeding the {MAX_TRAINING_EVENTS}-event snapshot ceiling",
            records.len()
        )));
    }

    let mut scoped: Vec<&(i64, JudgmentRecord)> = records
        .iter()
        .filter(|(_, record)| &record.scope == scope)
        .collect();
    scoped.sort_by_key(|(_, record)| record.judgment_id);

    let snapshot_bytes = serde_json::to_vec(
        &scoped
            .iter()
            .map(|(created_at, record)| {
                serde_json::json!({
                    "created_at": created_at,
                    "record": record,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| RuntimeError::Internal(format!("serialize training snapshot: {error}")))?;

    let mut observations = Vec::new();
    let mut excluded_probability_shown = 0usize;
    for (_, record) in scoped.iter().copied() {
        if record.presentation.preference_probability_shown {
            excluded_probability_shown += 1;
            continue;
        }
        validate_features(&record.left.features, "stored left occurrence")?;
        validate_features(&record.right.features, "stored right occurrence")?;
        let pair = PairKey::new(&record.left.content_ref, &record.right.content_ref);
        observations.push(Observation {
            judgment_id: record.judgment_id,
            split: pair_split(scope, &pair),
            pair,
            x: feature_difference(&record.left.features, &record.right.features),
            choice: record.choice,
        });
    }
    observations.sort_by_key(|observation| observation.judgment_id);

    let mut counts = BTreeMap::new();
    for split in [DataSplit::Train, DataSplit::Calibration, DataSplit::Test] {
        let split_observations: Vec<_> = observations
            .iter()
            .filter(|observation| observation.split == split)
            .collect();
        let decisive_groups: BTreeSet<_> = split_observations
            .iter()
            .filter(|observation| observation.choice.decisive_label().is_some())
            .map(|observation| &observation.pair)
            .collect();
        let tie_groups: BTreeSet<_> = split_observations
            .iter()
            .filter(|observation| observation.choice == JudgmentChoice::Tie)
            .map(|observation| &observation.pair)
            .collect();
        let abstain_groups: BTreeSet<_> = split_observations
            .iter()
            .filter(|observation| observation.choice == JudgmentChoice::Abstain)
            .map(|observation| &observation.pair)
            .collect();
        counts.insert(
            split,
            SplitCounts {
                decisive_groups: decisive_groups.len(),
                decisive_judgments: split_observations
                    .iter()
                    .filter(|observation| observation.choice.decisive_label().is_some())
                    .count(),
                left_labels: split_observations
                    .iter()
                    .filter(|observation| observation.choice == JudgmentChoice::Left)
                    .count(),
                right_labels: split_observations
                    .iter()
                    .filter(|observation| observation.choice == JudgmentChoice::Right)
                    .count(),
                tie_groups: tie_groups.len(),
                tie_judgments: split_observations
                    .iter()
                    .filter(|observation| observation.choice == JudgmentChoice::Tie)
                    .count(),
                abstain_groups: abstain_groups.len(),
                abstain_judgments: split_observations
                    .iter()
                    .filter(|observation| observation.choice == JudgmentChoice::Abstain)
                    .count(),
            },
        );
    }

    validate_support(&counts)?;

    Ok(PreparedTrainingData {
        observations,
        snapshot_sha256: sha256_hex(&snapshot_bytes),
        snapshot_event_count: scoped.len(),
        excluded_probability_shown,
        counts,
    })
}

fn validate_support(counts: &BTreeMap<DataSplit, SplitCounts>) -> Result<(), RuntimeError> {
    for (split, minimum) in [
        (DataSplit::Train, MIN_TRAIN_DECISIVE_GROUPS),
        (DataSplit::Calibration, MIN_CAL_DECISIVE_GROUPS),
        (DataSplit::Test, MIN_TEST_DECISIVE_GROUPS),
    ] {
        let count = counts
            .get(&split)
            .expect("all split counts are constructed");
        if count.decisive_groups < minimum {
            return Err(RuntimeError::InvalidInput(format!(
                "moodboard.train_preference requires at least {minimum} distinct decisive {} unordered-pair groups; observed {}",
                split.name(),
                count.decisive_groups
            )));
        }
        if count.left_labels == 0 || count.right_labels == 0 {
            return Err(RuntimeError::InvalidInput(format!(
                "moodboard.train_preference {} split must contain both randomized displayed-side labels; left={}, right={}",
                split.name(), count.left_labels, count.right_labels
            )));
        }
    }
    let calibration = counts
        .get(&DataSplit::Calibration)
        .expect("calibration count exists");
    if calibration.tie_groups < MIN_CAL_TIE_GROUPS {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard.train_preference requires at least {MIN_CAL_TIE_GROUPS} distinct calibration tie groups to calibrate an indifference band; observed {}",
            calibration.tie_groups
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptimizerProvenance {
    pub revision: String,
    pub loss: String,
    pub precision: String,
    pub intercept: String,
    pub l2: f64,
    pub max_iterations: usize,
    pub iterations: usize,
    pub converged: bool,
    pub final_objective: f64,
    pub gradient_infinity_norm: f64,
    pub seed: u64,
    pub backtracking: String,
}

#[derive(Clone, Debug)]
struct LogisticFit {
    weights: [f64; FEATURE_COUNT],
    provenance: OptimizerProvenance,
}

fn fit_logistic(examples: &[WeightedDecisive]) -> Result<LogisticFit, RuntimeError> {
    let mut weights = [0.0; FEATURE_COUNT];
    let mut converged = false;
    let mut iterations = 0usize;
    let mut final_objective = f64::INFINITY;
    let mut final_gradient_norm = f64::INFINITY;

    for iteration in 0..MAX_ITERATIONS {
        let (objective, gradient) = logistic_objective_gradient(&weights, examples);
        if !objective.is_finite() || gradient.iter().any(|value| !value.is_finite()) {
            return Err(RuntimeError::Internal(
                "moodboard logistic optimizer produced non-finite state".to_string(),
            ));
        }
        let gradient_norm = gradient.iter().map(|value| value.abs()).fold(0.0, f64::max);
        iterations = iteration;
        final_objective = objective;
        final_gradient_norm = gradient_norm;
        if gradient_norm <= GRADIENT_TOLERANCE {
            converged = true;
            break;
        }

        let squared_norm: f64 = gradient.iter().map(|value| value * value).sum();
        let mut step = 1.0;
        let mut accepted = None;
        for _ in 0..MAX_BACKTRACKS {
            let candidate = std::array::from_fn(|index| weights[index] - step * gradient[index]);
            let candidate_objective = logistic_objective(&candidate, examples);
            if candidate_objective.is_finite()
                && candidate_objective <= objective - ARMIJO * step * squared_norm
            {
                accepted = Some((candidate, candidate_objective));
                break;
            }
            step *= 0.5;
        }
        let Some((candidate, candidate_objective)) = accepted else {
            return Err(RuntimeError::Internal(
                "moodboard logistic optimizer backtracking failed to find a finite descent step"
                    .to_string(),
            ));
        };
        weights = candidate;
        iterations = iteration + 1;
        final_objective = candidate_objective;
        if (objective - candidate_objective).abs() <= OBJECTIVE_TOLERANCE * (1.0 + objective.abs())
        {
            let (_, gradient) = logistic_objective_gradient(&weights, examples);
            final_gradient_norm = gradient.iter().map(|value| value.abs()).fold(0.0, f64::max);
            converged = true;
            break;
        }
    }

    if !converged {
        return Err(RuntimeError::Internal(format!(
            "moodboard logistic optimizer did not converge after {MAX_ITERATIONS} deterministic full-batch iterations"
        )));
    }

    Ok(LogisticFit {
        weights,
        provenance: OptimizerProvenance {
            revision: TRAINING_REVISION.to_string(),
            loss: "weighted_binary_cross_entropy".to_string(),
            precision: "float64".to_string(),
            intercept: "fixed_zero".to_string(),
            l2: L2,
            max_iterations: MAX_ITERATIONS,
            iterations,
            converged,
            final_objective,
            gradient_infinity_norm: final_gradient_norm,
            seed: 0,
            backtracking: OPTIMIZER_BACKTRACKING_IDENTITY.to_string(),
        },
    })
}

fn logistic_objective_gradient(
    weights: &[f64; FEATURE_COUNT],
    examples: &[WeightedDecisive],
) -> (f64, [f64; FEATURE_COUNT]) {
    let total_weight: f64 = examples.iter().map(|example| example.weight).sum();
    let mut objective = 0.0;
    let mut gradient = [0.0; FEATURE_COUNT];
    for example in examples {
        let logit = dot_f64(weights, &example.x);
        objective += example.weight * (softplus(logit) - example.y * logit);
        let residual = stable_sigmoid(logit) - example.y;
        for (gradient_value, feature_value) in gradient.iter_mut().zip(example.x) {
            *gradient_value += example.weight * residual * feature_value;
        }
    }
    objective /= total_weight;
    for (gradient_value, weight) in gradient.iter_mut().zip(weights) {
        *gradient_value /= total_weight;
        objective += 0.5 * L2 * weight * weight;
        *gradient_value += L2 * weight;
    }
    (objective, gradient)
}

fn logistic_objective(weights: &[f64; FEATURE_COUNT], examples: &[WeightedDecisive]) -> f64 {
    logistic_objective_gradient(weights, examples).0
}

fn softplus(value: f64) -> f64 {
    if value > 0.0 {
        value + (-value).exp().ln_1p()
    } else {
        value.exp().ln_1p()
    }
}

pub(crate) fn stable_sigmoid(value: f64) -> f64 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exp = value.exp();
        exp / (1.0 + exp)
    }
}

fn dot_f64(left: &[f64; FEATURE_COUNT], right: &[f64; FEATURE_COUNT]) -> f64 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

pub(crate) fn materialize_fann(
    weights: &[f64; FEATURE_COUNT],
) -> Result<(Network, Vec<u8>), RuntimeError> {
    let weights_f32: Vec<f32> = weights.iter().map(|value| *value as f32).collect();
    if weights_f32.iter().any(|value| !value.is_finite()) {
        return Err(RuntimeError::Internal(
            "moodboard learned weight cannot be represented as finite float32".to_string(),
        ));
    }
    let layer =
        Layer::with_weights(FEATURE_COUNT, 1, weights_f32, vec![0.0], Activation::Linear)
            .map_err(|error| RuntimeError::Internal(format!("construct FANN layer: {error}")))?;
    let network = Network::new(vec![layer])
        .map_err(|error| RuntimeError::Internal(format!("construct FANN network: {error}")))?;
    let bytes = network.to_bytes();
    validate_fann_network(&network)?;
    Ok((network, bytes))
}

pub(crate) fn deserialize_fann(bytes: &[u8]) -> Result<Network, RuntimeError> {
    let network = Network::from_bytes(bytes).map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "moodboard preference model FANN blob is corrupt: {error}"
        ))
    })?;
    validate_fann_network(&network)?;
    Ok(network)
}

fn validate_fann_network(network: &Network) -> Result<(), RuntimeError> {
    if network.num_layers() != 1
        || network.num_inputs() != FEATURE_COUNT
        || network.num_outputs() != 1
        || network.total_params() != FEATURE_COUNT + 1
    {
        return Err(RuntimeError::InvalidInput(format!(
            "moodboard preference model must be an exact {FEATURE_COUNT}->1 single-layer FANN network"
        )));
    }
    let layer = network.layer(0).expect("one layer was just checked");
    if layer.activation() != Activation::Linear {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference FANN output activation must be linear".to_string(),
        ));
    }
    if layer.weights().iter().any(|value| !value.is_finite())
        || layer.biases().iter().any(|value| !value.is_finite())
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference FANN parameters must be finite".to_string(),
        ));
    }
    if layer.biases().len() != 1 || layer.biases()[0].to_bits() != 0.0_f32.to_bits() {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference FANN intercept must be exactly zero".to_string(),
        ));
    }
    Ok(())
}

fn fann_logits(
    network: &Network,
    observations: impl Iterator<Item = (Uuid, [f64; FEATURE_COUNT])>,
) -> Result<BTreeMap<Uuid, f64>, RuntimeError> {
    let mut network = network.clone();
    let mut logits = BTreeMap::new();
    for (id, x) in observations {
        let input: [f32; FEATURE_COUNT] = std::array::from_fn(|index| x[index] as f32);
        let output = network
            .forward(&input)
            .map_err(|error| RuntimeError::Internal(format!("FANN forward: {error}")))?;
        let logit = f64::from(output[0]);
        if !logit.is_finite() {
            return Err(RuntimeError::Internal(
                "FANN preference forward returned a non-finite logit".to_string(),
            ));
        }
        logits.insert(id, logit);
    }
    Ok(logits)
}

fn calibrate_temperature(examples: &[WeightedDecisive], logits: &[f64]) -> f64 {
    debug_assert_eq!(examples.len(), logits.len());
    let objective = |log_temperature: f64| {
        let temperature = log_temperature.exp();
        let total_weight: f64 = examples.iter().map(|example| example.weight).sum();
        examples
            .iter()
            .zip(logits)
            .map(|(example, logit)| {
                example.weight
                    * (softplus(*logit / temperature) - example.y * (*logit / temperature))
            })
            .sum::<f64>()
            / total_weight
    };

    let golden = (5.0_f64.sqrt() - 1.0) / 2.0;
    let mut lower = LOG_TEMPERATURE_MIN;
    let mut upper = LOG_TEMPERATURE_MAX;
    let mut c = upper - golden * (upper - lower);
    let mut d = lower + golden * (upper - lower);
    let mut fc = objective(c);
    let mut fd = objective(d);
    for _ in 0..TEMPERATURE_SEARCH_ITERATIONS {
        if fc <= fd {
            upper = d;
            d = c;
            fd = fc;
            c = upper - golden * (upper - lower);
            fc = objective(c);
        } else {
            lower = c;
            c = d;
            fc = fd;
            d = lower + golden * (upper - lower);
            fd = objective(d);
        }
    }
    ((lower + upper) * 0.5).exp()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CalibrationProvenance {
    pub calibrated: bool,
    pub temperature: f64,
    pub log_temperature_bounds: [f64; 2],
    pub temperature_search_iterations: usize,
    pub tie_band_half_width: f64,
    pub tie_band_rule: String,
    pub tie_balanced_error: f64,
}

fn calibrate_tie_band(tie_margins: &[f64], decisive_margins: &[f64]) -> (f64, f64) {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestMetrics {
    pub decisive_groups: usize,
    pub decisive_judgments: usize,
    pub log_loss: f64,
    pub brier: f64,
    pub accuracy: f64,
    pub tie_groups: usize,
    pub tie_detection_rate: Option<f64>,
}

fn test_metrics(
    data: &PreparedTrainingData,
    probabilities: &BTreeMap<Uuid, f64>,
    tie_band_half_width: f64,
) -> TestMetrics {
    let examples = data.decisive_examples(DataSplit::Test);
    let total_weight: f64 = examples.iter().map(|example| example.weight).sum();
    let mut log_loss = 0.0;
    let mut brier = 0.0;
    let mut accuracy = 0.0;
    for example in &examples {
        let probability = probabilities[&example.judgment_id].clamp(1e-15, 1.0 - 1e-15);
        log_loss += example.weight
            * (-example.y * probability.ln() - (1.0 - example.y) * (1.0 - probability).ln());
        brier += example.weight * (probability - example.y).powi(2);
        accuracy += example.weight * f64::from((probability >= 0.5) == (example.y >= 0.5));
    }
    let tie_margins =
        data.class_group_margins(DataSplit::Test, probabilities, CalibrationClass::Tie);
    TestMetrics {
        decisive_groups: data.counts[&DataSplit::Test].decisive_groups,
        decisive_judgments: data.counts[&DataSplit::Test].decisive_judgments,
        log_loss: log_loss / total_weight,
        brier: brier / total_weight,
        accuracy: accuracy / total_weight,
        tie_groups: tie_margins.len(),
        tie_detection_rate: (!tie_margins.is_empty()).then(|| {
            tie_margins
                .iter()
                .filter(|margin| **margin <= tie_band_half_width)
                .count() as f64
                / tie_margins.len() as f64
        }),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrainingProvenance {
    pub snapshot_sha256: String,
    pub snapshot_event_count: usize,
    pub excluded_probability_shown: usize,
    pub split_revision: String,
    pub split_counts: BTreeMap<String, SplitCounts>,
    pub optimizer: OptimizerProvenance,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FannProvenance {
    pub crate_name: String,
    pub crate_version: String,
    pub format: String,
    pub architecture: String,
    pub network_content_ref: String,
    pub network_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelBundle {
    pub schema_version: String,
    pub model_family: String,
    pub scope: PreferenceScope,
    pub feature_schema_version: String,
    pub feature_schema_canonical_json_base64: String,
    pub training: TrainingProvenance,
    pub calibration: CalibrationProvenance,
    pub test_metrics: TestMetrics,
    pub fann: FannProvenance,
}

pub(crate) struct TrainedModel {
    pub bundle: ModelBundle,
    pub network_bytes: Vec<u8>,
}

pub(crate) fn train_model(
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
        calibrate_tie_band(&tie_margins, &decisive_margins);
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
            crate_version: FANN_CRATE_VERSION.to_string(),
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

pub(crate) fn validate_loaded_bundle(bundle: &ModelBundle) -> Result<(), RuntimeError> {
    if bundle.schema_version != MODEL_BUNDLE_SCHEMA_VERSION
        || bundle.model_family != MODEL_FAMILY
        || bundle.feature_schema_version != FEATURE_SCHEMA_VERSION
        || bundle.scope.feature_schema_id != feature_schema_id()
        || bundle.feature_schema_canonical_json_base64
            != BASE64.encode(FEATURE_SCHEMA_CANONICAL_JSON)
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model has the wrong model or feature-schema identity".to_string(),
        ));
    }
    if bundle.fann.crate_name != "lattice-fann"
        || bundle.fann.crate_version != FANN_CRATE_VERSION
        || bundle.fann.format != FANN_FORMAT
        || bundle.fann.architecture != format!("{FEATURE_COUNT}->1 linear; zero intercept")
        || !is_lower_hex_64(&bundle.fann.network_content_ref)
        || !is_lower_hex_64(&bundle.fann.network_sha256)
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model has the wrong FANN identity".to_string(),
        ));
    }
    if !bundle.calibration.calibrated
        || !bundle.calibration.temperature.is_finite()
        || !(LOG_TEMPERATURE_MIN.exp()..=LOG_TEMPERATURE_MAX.exp())
            .contains(&bundle.calibration.temperature)
        || bundle.calibration.log_temperature_bounds != [LOG_TEMPERATURE_MIN, LOG_TEMPERATURE_MAX]
        || bundle.calibration.temperature_search_iterations != TEMPERATURE_SEARCH_ITERATIONS
        || bundle.calibration.tie_band_rule != TIE_BAND_RULE_IDENTITY
        || !bundle.calibration.tie_band_half_width.is_finite()
        || !(0.0..=0.5).contains(&bundle.calibration.tie_band_half_width)
        || !bundle.calibration.tie_balanced_error.is_finite()
        || !(0.0..=1.0).contains(&bundle.calibration.tie_balanced_error)
        || !bundle.training.optimizer.converged
        || bundle.training.optimizer.seed != 0
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model is not fully calibrated".to_string(),
        ));
    }
    let optimizer = &bundle.training.optimizer;
    if optimizer.revision != TRAINING_REVISION
        || optimizer.loss != "weighted_binary_cross_entropy"
        || optimizer.precision != "float64"
        || optimizer.intercept != "fixed_zero"
        || optimizer.l2.to_bits() != L2.to_bits()
        || optimizer.max_iterations != MAX_ITERATIONS
        || optimizer.iterations > MAX_ITERATIONS
        || optimizer.backtracking != OPTIMIZER_BACKTRACKING_IDENTITY
        || !optimizer.final_objective.is_finite()
        || optimizer.final_objective < 0.0
        || !optimizer.gradient_infinity_norm.is_finite()
        || optimizer.gradient_infinity_norm < 0.0
        || bundle.training.split_revision != PAIR_SPLIT_REVISION
        || !is_lower_hex_64(&bundle.training.snapshot_sha256)
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model has invalid optimizer or split provenance".to_string(),
        ));
    }
    let metrics = &bundle.test_metrics;
    if [metrics.log_loss, metrics.brier, metrics.accuracy]
        .iter()
        .any(|value| !value.is_finite())
        || metrics.log_loss < 0.0
        || !(0.0..=1.0).contains(&metrics.brier)
        || !(0.0..=1.0).contains(&metrics.accuracy)
        || metrics
            .tie_detection_rate
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model has invalid held-out metric provenance".to_string(),
        ));
    }
    let required = [
        ("train", MIN_TRAIN_DECISIVE_GROUPS),
        ("calibration", MIN_CAL_DECISIVE_GROUPS),
        ("test", MIN_TEST_DECISIVE_GROUPS),
    ];
    if bundle.training.split_counts.len() != required.len() {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model has an unknown or missing split provenance key".to_string(),
        ));
    }
    let mut included_judgments = 0usize;
    for (split, minimum) in required {
        let Some(count) = bundle.training.split_counts.get(split) else {
            return Err(RuntimeError::InvalidInput(format!(
                "moodboard preference model is missing {split} split provenance"
            )));
        };
        let category_counts = [
            (count.decisive_groups, count.decisive_judgments),
            (count.tie_groups, count.tie_judgments),
            (count.abstain_groups, count.abstain_judgments),
        ];
        if category_counts
            .iter()
            .any(|(groups, judgments)| (*groups == 0) != (*judgments == 0) || groups > judgments)
            || count.left_labels.checked_add(count.right_labels) != Some(count.decisive_judgments)
            || count.decisive_groups < minimum
            || count.left_labels == 0
            || count.right_labels == 0
        {
            return Err(RuntimeError::InvalidInput(format!(
                "moodboard preference model {split} support provenance is inconsistent or below the serving gate"
            )));
        }
        let split_judgments = count
            .decisive_judgments
            .checked_add(count.tie_judgments)
            .and_then(|total| total.checked_add(count.abstain_judgments));
        included_judgments = split_judgments
            .and_then(|total| included_judgments.checked_add(total))
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "moodboard preference model support provenance overflows".to_string(),
                )
            })?;
    }
    let accounted_snapshot_events =
        included_judgments.checked_add(bundle.training.excluded_probability_shown);
    if accounted_snapshot_events != Some(bundle.training.snapshot_event_count)
        || bundle.training.snapshot_event_count > MAX_TRAINING_EVENTS
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model support totals do not match its training snapshot"
                .to_string(),
        ));
    }
    if bundle.training.split_counts["calibration"].tie_groups < MIN_CAL_TIE_GROUPS {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model lacks calibrated tie support".to_string(),
        ));
    }
    let test_support = &bundle.training.split_counts["test"];
    if metrics.decisive_groups != test_support.decisive_groups
        || metrics.decisive_judgments != test_support.decisive_judgments
        || metrics.tie_groups != test_support.tie_groups
        || (metrics.tie_groups == 0) != metrics.tie_detection_rate.is_none()
    {
        return Err(RuntimeError::InvalidInput(
            "moodboard preference model held-out metrics do not match split provenance".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn predict(
    network: &Network,
    temperature: f64,
    left: &[f32; FEATURE_COUNT],
    right: &[f32; FEATURE_COUNT],
) -> Result<(f64, f64), RuntimeError> {
    validate_features(left, "moodboard.preference left")?;
    validate_features(right, "moodboard.preference right")?;
    if !temperature.is_finite() || temperature <= 0.0 {
        return Err(RuntimeError::InvalidInput(
            "moodboard.preference model temperature is uncalibrated".to_string(),
        ));
    }
    let input: [f32; FEATURE_COUNT] = std::array::from_fn(|index| left[index] - right[index]);
    let mut network = network.clone();
    let output = network
        .forward(&input)
        .map_err(|error| RuntimeError::InvalidInput(format!("FANN preference forward: {error}")))?;
    let logit = f64::from(output[0]);
    if !logit.is_finite() {
        return Err(RuntimeError::InvalidInput(
            "moodboard.preference FANN output is non-finite".to_string(),
        ));
    }
    Ok((logit, stable_sigmoid(logit / temperature)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_scope() -> PreferenceScope {
        PreferenceScope {
            namespace: "local".to_string(),
            actor_kind: "actor".to_string(),
            actor_id: "alice".to_string(),
            board_entity_id: Uuid::from_u128(7),
            board_id: "a".repeat(64),
            model_key: "moodboard_fixture_10".to_string(),
            descriptor_fingerprint: "b".repeat(64),
            feature_schema_id: feature_schema_id().to_string(),
        }
    }

    fn features(preferred: bool) -> [f32; FEATURE_COUNT] {
        let mut values = [0.5; FEATURE_COUNT];
        values[0] = if preferred { 0.9 } else { 0.1 };
        values
    }

    #[allow(clippy::too_many_arguments)]
    fn fixture_record(
        id_counter: u128,
        scope: &PreferenceScope,
        lower_ref: &str,
        upper_ref: &str,
        choice: JudgmentChoice,
        left_features: [f32; FEATURE_COUNT],
        right_features: [f32; FEATURE_COUNT],
    ) -> JudgmentRecord {
        let serve_id = Uuid::from_u128(id_counter * 10 + 1);
        JudgmentRecord {
            schema_version: JUDGMENT_SCHEMA_VERSION.to_string(),
            judgment_id: Uuid::from_u128(id_counter * 10 + 2),
            serve_id,
            scope: scope.clone(),
            source_report_sha256: "c".repeat(64),
            left: ResultOccurrence {
                result_occurrence_id: Uuid::from_u128(id_counter * 10 + 3),
                source_candidate_index: 0,
                asset_id: Uuid::from_u128(id_counter * 10 + 4),
                content_ref: lower_ref.to_string(),
                source_rank: Some(1),
                features: left_features,
            },
            right: ResultOccurrence {
                result_occurrence_id: Uuid::from_u128(id_counter * 10 + 5),
                source_candidate_index: 1,
                asset_id: Uuid::from_u128(id_counter * 10 + 6),
                content_ref: upper_ref.to_string(),
                source_rank: Some(2),
                features: right_features,
            },
            selection: SelectionProvenance {
                policy_revision: "fixture-random-v1".to_string(),
                pair_propensity: Some(0.5),
                candidate_pool_sha256: Some("d".repeat(64)),
            },
            presentation: PresentationProvenance {
                preference_probability_shown: false,
                source_rank_shown: false,
                served_preference_model_id: None,
            },
            randomization: RandomizationProvenance {
                revision: RANDOMIZATION_REVISION.to_string(),
                sha256: "e".repeat(64),
                swap_applied: false,
            },
            choice,
            reason_code: None,
            response_ms: Some(100),
        }
    }

    fn sufficient_records(include_abstain: bool) -> Vec<(i64, JudgmentRecord)> {
        let scope = fixture_scope();
        let targets = BTreeMap::from([
            (DataSplit::Train, MIN_TRAIN_DECISIVE_GROUPS),
            (DataSplit::Calibration, MIN_CAL_DECISIVE_GROUPS),
            (DataSplit::Test, MIN_TEST_DECISIVE_GROUPS),
        ]);
        let mut counts = BTreeMap::from([
            (DataSplit::Train, 0usize),
            (DataSplit::Calibration, 0usize),
            (DataSplit::Test, 0usize),
        ]);
        let mut records = Vec::new();
        let mut counter = 1u128;
        for candidate_index in 0u64..100_000 {
            if counts.iter().all(|(split, count)| *count >= targets[split]) {
                break;
            }
            let lower = sha256_hex(format!("fixture-lower-{candidate_index}").as_bytes());
            let upper = sha256_hex(format!("fixture-upper-{candidate_index}").as_bytes());
            let pair = PairKey::new(&lower, &upper);
            let split = pair_split(&scope, &pair);
            if counts[&split] >= targets[&split] {
                continue;
            }
            let sequence = counts[&split];
            let left_wins = sequence % 2 == 0;
            let choice = if left_wins {
                JudgmentChoice::Left
            } else {
                JudgmentChoice::Right
            };
            let record = fixture_record(
                counter,
                &scope,
                &lower,
                &upper,
                choice,
                features(left_wins),
                features(!left_wins),
            );
            records.push((counter as i64, record));
            counter += 1;
            if split == DataSplit::Calibration && sequence < MIN_CAL_TIE_GROUPS {
                let tie = fixture_record(
                    counter,
                    &scope,
                    &lower,
                    &upper,
                    JudgmentChoice::Tie,
                    [0.5; FEATURE_COUNT],
                    [0.5; FEATURE_COUNT],
                );
                records.push((counter as i64, tie));
                counter += 1;
            }
            counts.insert(split, sequence + 1);
        }
        assert_eq!(counts, targets, "fixture must satisfy exact support gates");
        if include_abstain {
            let train_record = records
                .iter()
                .find(|(_, record)| {
                    let pair = PairKey::new(&record.left.content_ref, &record.right.content_ref);
                    pair_split(&scope, &pair) == DataSplit::Train
                })
                .expect("fixture has train pair")
                .1
                .clone();
            records.push((
                counter as i64,
                JudgmentRecord {
                    judgment_id: Uuid::from_u128(counter * 10 + 2),
                    serve_id: Uuid::from_u128(counter * 10 + 1),
                    choice: JudgmentChoice::Abstain,
                    reason_code: Some(ReasonCode::InsufficientContext),
                    ..train_record
                },
            ));
        }
        records
    }

    #[test]
    fn feature_schema_identity_is_golden_and_closed() {
        assert_eq!(FEATURE_NAMES.len(), FEATURE_COUNT);
        assert_eq!(
            feature_schema_id(),
            "f691fc73bf9a50d72157e21601fa579caa707bf2c448df546c63e915b4e42175"
        );
        assert_eq!(
            BASE64
                .decode(BASE64.encode(FEATURE_SCHEMA_CANONICAL_JSON))
                .unwrap(),
            FEATURE_SCHEMA_CANONICAL_JSON.as_bytes()
        );
    }

    #[test]
    fn insufficient_grouped_support_fails_before_training() {
        let scope = fixture_scope();
        let record = fixture_record(
            1,
            &scope,
            &"1".repeat(64),
            &"2".repeat(64),
            JudgmentChoice::Left,
            features(true),
            features(false),
        );
        let error = prepare_training_data(&[(1, record)], &scope).unwrap_err();
        assert!(error.to_string().contains("distinct decisive"));
    }

    #[test]
    fn sufficient_group_counts_still_require_both_displayed_side_labels() {
        let scope = fixture_scope();
        let mut records = sufficient_records(false);
        for (_, record) in &mut records {
            let pair = PairKey::new(&record.left.content_ref, &record.right.content_ref);
            if pair_split(&scope, &pair) == DataSplit::Test
                && record.choice.decisive_label().is_some()
            {
                record.choice = JudgmentChoice::Left;
            }
        }
        let error = prepare_training_data(&records, &scope).unwrap_err();
        assert!(error.to_string().contains("both randomized displayed-side"));
    }

    #[test]
    fn training_and_fann_serialization_are_deterministic() {
        let scope = fixture_scope();
        let records = sufficient_records(true);
        let data_a = prepare_training_data(&records, &scope).unwrap();
        let mut reversed = records.clone();
        reversed.reverse();
        let data_b = prepare_training_data(&reversed, &scope).unwrap();
        assert_eq!(data_a.snapshot_sha256, data_b.snapshot_sha256);
        let model_a = train_model(&data_a, scope.clone()).unwrap();
        let model_b = train_model(&data_b, scope).unwrap();
        assert_eq!(model_a.network_bytes, model_b.network_bytes);
        assert_eq!(model_a.bundle, model_b.bundle);
        assert_eq!(
            serde_json::to_vec(&model_a.bundle).unwrap(),
            serde_json::to_vec(&model_b.bundle).unwrap()
        );
        assert_eq!(model_a.bundle.training.optimizer.seed, 0);
        assert!(model_a.bundle.training.optimizer.converged);
    }

    #[test]
    fn side_swap_is_antisymmetric_and_probability_symmetric() {
        let scope = fixture_scope();
        let data = prepare_training_data(&sufficient_records(false), &scope).unwrap();
        let trained = train_model(&data, scope).unwrap();
        let network = deserialize_fann(&trained.network_bytes).unwrap();
        let left = features(true);
        let right = features(false);
        let (forward_logit, forward) = predict(
            &network,
            trained.bundle.calibration.temperature,
            &left,
            &right,
        )
        .unwrap();
        let (reverse_logit, reverse) = predict(
            &network,
            trained.bundle.calibration.temperature,
            &right,
            &left,
        )
        .unwrap();
        assert!((forward_logit + reverse_logit).abs() <= f64::EPSILON);
        assert!((forward + reverse - 1.0).abs() <= 4.0 * f64::EPSILON);
        assert!(forward > 0.5);
    }

    #[test]
    fn concurrent_inference_uses_independent_fann_buffers() {
        let (_, bytes) = materialize_fann(&[0.125; FEATURE_COUNT]).unwrap();
        let expected_network = deserialize_fann(&bytes).unwrap();
        let (_, expected) = predict(
            &expected_network,
            1.0,
            &[0.8; FEATURE_COUNT],
            &[0.2; FEATURE_COUNT],
        )
        .unwrap();
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..32 {
                let bytes = &bytes;
                handles.push(scope.spawn(move || {
                    let network = deserialize_fann(bytes).unwrap();
                    predict(&network, 1.0, &[0.8; FEATURE_COUNT], &[0.2; FEATURE_COUNT])
                        .unwrap()
                        .1
                }));
            }
            for handle in handles {
                assert_eq!(handle.join().unwrap(), expected);
            }
        });
    }

    #[test]
    fn tie_and_abstain_never_enter_decisive_bce() {
        let scope = fixture_scope();
        let records = sufficient_records(false);
        let without = prepare_training_data(&records, &scope).unwrap();
        let with = prepare_training_data(&sufficient_records(true), &scope).unwrap();
        let mut changed_ties = records;
        for (_, record) in &mut changed_ties {
            if record.choice == JudgmentChoice::Tie {
                record.left.features = [1.0; FEATURE_COUNT];
                record.right.features = [0.0; FEATURE_COUNT];
            }
        }
        let changed_ties = prepare_training_data(&changed_ties, &scope).unwrap();
        let model_without = train_model(&without, scope.clone()).unwrap();
        let model_with = train_model(&with, scope.clone()).unwrap();
        let model_changed_ties = train_model(&changed_ties, scope).unwrap();
        assert_eq!(model_without.network_bytes, model_with.network_bytes);
        assert_eq!(
            model_without.network_bytes,
            model_changed_ties.network_bytes
        );
        assert_eq!(
            with.counts[&DataSplit::Train].abstain_judgments,
            without.counts[&DataSplit::Train].abstain_judgments + 1
        );
        assert!(model_with.bundle.calibration.calibrated);
        let network = deserialize_fann(&model_with.network_bytes).unwrap();
        let (_, neutral) = predict(
            &network,
            model_with.bundle.calibration.temperature,
            &[0.5; FEATURE_COUNT],
            &[0.5; FEATURE_COUNT],
        )
        .unwrap();
        assert_eq!(neutral, 0.5);
        assert!((neutral - 0.5).abs() <= model_with.bundle.calibration.tie_band_half_width);
    }

    #[test]
    fn probability_exposed_records_are_excluded_from_support_and_bce() {
        let scope = fixture_scope();
        let records = sufficient_records(false);
        let baseline_data = prepare_training_data(&records, &scope).unwrap();
        let baseline_model = train_model(&baseline_data, scope.clone()).unwrap();

        let needed_index = records
            .iter()
            .position(|(_, record)| {
                let pair = PairKey::new(&record.left.content_ref, &record.right.content_ref);
                pair_split(&scope, &pair) == DataSplit::Train
                    && record.choice.decisive_label().is_some()
            })
            .unwrap();
        let mut missing_support = records.clone();
        missing_support[needed_index]
            .1
            .presentation
            .preference_probability_shown = true;
        missing_support[needed_index]
            .1
            .presentation
            .served_preference_model_id = Some(Uuid::from_u128(999));
        let error = prepare_training_data(&missing_support, &scope).unwrap_err();
        assert!(error.to_string().contains("distinct decisive train"));

        let mut with_exposed_duplicate = records;
        let mut exposed = with_exposed_duplicate[needed_index].1.clone();
        exposed.judgment_id = Uuid::from_u128(10_001);
        exposed.serve_id = Uuid::from_u128(10_002);
        exposed.left.features = [0.0; FEATURE_COUNT];
        exposed.right.features = [1.0; FEATURE_COUNT];
        exposed.presentation.preference_probability_shown = true;
        exposed.presentation.served_preference_model_id = Some(Uuid::from_u128(999));
        with_exposed_duplicate.push((10_001, exposed));
        let exposed_data = prepare_training_data(&with_exposed_duplicate, &scope).unwrap();
        assert_eq!(exposed_data.excluded_probability_shown, 1);
        let exposed_model = train_model(&exposed_data, scope).unwrap();
        assert_eq!(baseline_model.network_bytes, exposed_model.network_bytes);
    }

    #[test]
    fn fann_loader_rejects_corrupt_wrong_architecture_and_nonfinite_weights() {
        let (_, mut valid) = materialize_fann(&[0.1; FEATURE_COUNT]).unwrap();
        valid.push(0);
        assert!(deserialize_fann(&valid).is_err());

        let wrong = Network::new(vec![Layer::with_weights(
            9,
            1,
            vec![0.0; 9],
            vec![0.0],
            Activation::Linear,
        )
        .unwrap()])
        .unwrap();
        assert!(deserialize_fann(&wrong.to_bytes()).is_err());

        let nonfinite = Network::new(vec![Layer::with_weights(
            FEATURE_COUNT,
            1,
            vec![f32::NAN; FEATURE_COUNT],
            vec![0.0],
            Activation::Linear,
        )
        .unwrap()])
        .unwrap();
        assert!(deserialize_fann(&nonfinite.to_bytes()).is_err());
    }

    #[test]
    fn wrong_or_uncalibrated_bundle_identity_fails_closed() {
        let scope = fixture_scope();
        let data = prepare_training_data(&sufficient_records(false), &scope).unwrap();
        let mut trained = train_model(&data, scope).unwrap();
        trained.bundle.fann.network_content_ref = "f".repeat(64);
        validate_loaded_bundle(&trained.bundle).unwrap();

        let mut wrong_schema = trained.bundle.clone();
        wrong_schema.scope.feature_schema_id = "0".repeat(64);
        assert!(validate_loaded_bundle(&wrong_schema).is_err());

        let mut wrong_optimizer = trained.bundle.clone();
        wrong_optimizer.training.optimizer.backtracking = "different".to_string();
        assert!(validate_loaded_bundle(&wrong_optimizer).is_err());

        let mut wrong_calibration = trained.bundle.clone();
        wrong_calibration.calibration.temperature_search_iterations -= 1;
        assert!(validate_loaded_bundle(&wrong_calibration).is_err());

        let mut impossible_temperature = trained.bundle.clone();
        impossible_temperature.calibration.temperature = f64::MAX;
        assert!(validate_loaded_bundle(&impossible_temperature).is_err());

        let mut wrong_metrics = trained.bundle.clone();
        wrong_metrics.test_metrics.decisive_groups += 1;
        assert!(validate_loaded_bundle(&wrong_metrics).is_err());

        let mut impossible_support = trained.bundle.clone();
        impossible_support
            .training
            .split_counts
            .get_mut("train")
            .unwrap()
            .decisive_judgments = 0;
        assert!(validate_loaded_bundle(&impossible_support).is_err());

        let mut unknown_split = trained.bundle.clone();
        unknown_split
            .training
            .split_counts
            .insert("future".to_string(), SplitCounts::default());
        assert!(validate_loaded_bundle(&unknown_split).is_err());

        let mut wrong_snapshot_total = trained.bundle.clone();
        wrong_snapshot_total.training.snapshot_event_count += 1;
        assert!(validate_loaded_bundle(&wrong_snapshot_total).is_err());

        let mut negative_objective = trained.bundle.clone();
        negative_objective.training.optimizer.final_objective = -0.1;
        assert!(validate_loaded_bundle(&negative_objective).is_err());

        let mut uncalibrated = trained.bundle;
        uncalibrated.calibration.calibrated = false;
        assert!(validate_loaded_bundle(&uncalibrated).is_err());
    }

    #[test]
    fn zero_difference_support_produces_a_valid_zero_iteration_head() {
        let scope = fixture_scope();
        let mut records = sufficient_records(false);
        for (_, record) in &mut records {
            record.left.features = [0.5; FEATURE_COUNT];
            record.right.features = [0.5; FEATURE_COUNT];
        }
        let data = prepare_training_data(&records, &scope).unwrap();
        let mut trained = train_model(&data, scope).unwrap();
        assert_eq!(trained.bundle.training.optimizer.iterations, 0);
        trained.bundle.fann.network_content_ref = "f".repeat(64);
        validate_loaded_bundle(&trained.bundle).unwrap();
    }
}
