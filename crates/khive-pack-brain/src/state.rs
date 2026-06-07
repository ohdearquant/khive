//! Brain domain types — posteriors, profile records, bindings, and state snapshots.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── SectionType ───────────────────────────────────────────────────────────────

/// Knowledge-section types that the brain tracks per-profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionType {
    Overview,
    CoreModel,
    BoundaryConditions,
    Formalism,
    OperationalGuidance,
    Examples,
    FailureModes,
    ExpertLens,
    References,
    Other,
}

impl SectionType {
    /// Canonical string representation (matches serde snake_case).
    pub fn as_str(self) -> &'static str {
        match self {
            SectionType::Overview => "overview",
            SectionType::CoreModel => "core_model",
            SectionType::BoundaryConditions => "boundary_conditions",
            SectionType::Formalism => "formalism",
            SectionType::OperationalGuidance => "operational_guidance",
            SectionType::Examples => "examples",
            SectionType::FailureModes => "failure_modes",
            SectionType::ExpertLens => "expert_lens",
            SectionType::References => "references",
            SectionType::Other => "other",
        }
    }

    /// All section types in a stable canonical order.
    pub fn all() -> &'static [SectionType] {
        &Self::ALL
    }

    /// All section types as a const array.
    pub const ALL: [SectionType; 10] = [
        SectionType::Overview,
        SectionType::CoreModel,
        SectionType::BoundaryConditions,
        SectionType::Formalism,
        SectionType::OperationalGuidance,
        SectionType::Examples,
        SectionType::FailureModes,
        SectionType::ExpertLens,
        SectionType::References,
        SectionType::Other,
    ];
}

impl fmt::Display for SectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SectionType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "overview" => Ok(SectionType::Overview),
            "core_model" => Ok(SectionType::CoreModel),
            "boundary_conditions" => Ok(SectionType::BoundaryConditions),
            "formalism" => Ok(SectionType::Formalism),
            "operational_guidance" => Ok(SectionType::OperationalGuidance),
            "examples" => Ok(SectionType::Examples),
            "failure_modes" => Ok(SectionType::FailureModes),
            "expert_lens" => Ok(SectionType::ExpertLens),
            "references" => Ok(SectionType::References),
            "other" => Ok(SectionType::Other),
            _ => Err(format!("unknown SectionType: {s:?}")),
        }
    }
}

// ── BetaPosterior ─────────────────────────────────────────────────────────────

/// Beta-Binomial posterior for a single parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BetaPosterior {
    pub alpha: f64,
    pub beta: f64,
}

impl BetaPosterior {
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self { alpha, beta }
    }

    /// Validated constructor. Returns `Err` when either parameter is non-positive
    /// or non-finite so callers can reject corrupt or programmatically invalid
    /// posteriors without propagating NaN or zero-division through downstream math.
    pub fn try_new(alpha: f64, beta: f64) -> Result<Self, String> {
        if !alpha.is_finite() || alpha <= 0.0 {
            return Err(format!(
                "BetaPosterior: alpha must be finite and positive, got {alpha}"
            ));
        }
        if !beta.is_finite() || beta <= 0.0 {
            return Err(format!(
                "BetaPosterior: beta must be finite and positive, got {beta}"
            ));
        }
        Ok(Self { alpha, beta })
    }

    /// Validate the current field values, returning an error message if invalid.
    ///
    /// Used after snapshot deserialization to catch corrupt persisted data before
    /// it propagates into mean/variance/softmax computations.
    pub fn validate(&self) -> Result<(), String> {
        Self::try_new(self.alpha, self.beta).map(|_| ())
    }

    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    pub fn variance(&self) -> f64 {
        let n = self.alpha + self.beta;
        (self.alpha * self.beta) / (n * n * (n + 1.0))
    }

    pub fn effective_sample_size(&self) -> f64 {
        self.alpha + self.beta
    }

    pub fn update_success(&mut self) {
        self.alpha += 1.0;
    }

    pub fn update_failure(&mut self) {
        self.beta += 1.0;
    }

    /// Weighted success update (issue #268).
    ///
    /// Adds `weight` to α instead of a fixed 1.0. Used by semantic feedback
    /// event kinds that carry different evidence strength (explicit vs implicit).
    pub fn update_success_weighted(&mut self, weight: f64) {
        debug_assert!(
            weight > 0.0,
            "update_success_weighted: weight must be positive, got {weight}"
        );
        self.alpha += weight;
    }

    /// Weighted failure update (issue #268).
    ///
    /// Adds `weight` to β instead of a fixed 1.0. Used by semantic feedback
    /// event kinds that carry different evidence strength (explicit vs implicit).
    pub fn update_failure_weighted(&mut self, weight: f64) {
        debug_assert!(
            weight > 0.0,
            "update_failure_weighted: weight must be positive, got {weight}"
        );
        self.beta += weight;
    }

    /// Combine evidence from two independent observers sharing the same prior.
    ///
    /// merged = Beta(a₁ + a₂ − a_prior, b₁ + b₂ − b_prior)
    pub fn merge(&self, other: &BetaPosterior, prior: &BetaPosterior) -> BetaPosterior {
        BetaPosterior {
            alpha: self.alpha + other.alpha - prior.alpha,
            beta: self.beta + other.beta - prior.beta,
        }
    }

    /// Cap ESS at `cap` by scaling excess evidence back toward the prior.
    ///
    /// If current ESS exceeds cap, the excess evidence (above the prior) is
    /// scaled so the resulting ESS equals cap exactly.
    ///
    /// Formula: scale = (cap - prior_ess) / (ess - prior_ess)
    pub fn apply_ess_cap(&mut self, prior: &BetaPosterior, cap: f64) {
        let ess = self.effective_sample_size();
        if ess > cap {
            let prior_ess = prior.effective_sample_size();
            let scale = (cap - prior_ess) / (ess - prior_ess);
            self.alpha = prior.alpha + (self.alpha - prior.alpha) * scale;
            self.beta = prior.beta + (self.beta - prior.beta) * scale;
        }
    }

    /// Posterior mean floored at `floor`.
    pub fn floored_mean(&self, floor: f64) -> f64 {
        self.mean().max(floor)
    }
}

impl Default for BetaPosterior {
    fn default() -> Self {
        Self::new(1.0, 1.0)
    }
}

// ── EntityPosteriors ──────────────────────────────────────────────────────────

/// Bounded LRU map for per-entity posteriors.
/// Uses a VecDeque to track insertion order; evicts oldest on insert when full.
pub struct EntityPosteriors {
    map: HashMap<Uuid, BetaPosterior>,
    order: VecDeque<Uuid>,
    capacity: usize,
}

impl EntityPosteriors {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn get_or_insert(
        &mut self,
        id: Uuid,
        default: impl FnOnce() -> BetaPosterior,
    ) -> &mut BetaPosterior {
        if !self.map.contains_key(&id) {
            if self.map.len() >= self.capacity {
                if let Some(evicted) = self.order.pop_front() {
                    self.map.remove(&evicted);
                }
            }
            self.map.insert(id, default());
            self.order.push_back(id);
        }
        self.map.get_mut(&id).unwrap()
    }

    pub fn get(&self, id: &Uuid) -> Option<&BetaPosterior> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    pub fn to_snapshot(&self) -> HashMap<Uuid, BetaPosterior> {
        self.map.clone()
    }

    pub fn from_snapshot(snapshot: HashMap<Uuid, BetaPosterior>, capacity: usize) -> Self {
        let mut ep = Self::new(capacity);
        for (id, posterior) in snapshot {
            ep.map.insert(id, posterior);
            ep.order.push_back(id);
        }
        ep
    }
}

// ── BalancedRecallState ───────────────────────────────────────────────────────

/// Live Beta-posterior state for the `balanced-recall-v1` profile.
pub struct BalancedRecallState {
    /// relevance_weight — prior Beta(7,3): warm-starts expecting 70% success
    pub relevance: BetaPosterior,
    /// salience_weight — prior Beta(2,8)
    pub salience: BetaPosterior,
    /// temporal_weight — prior Beta(1,9)
    pub temporal: BetaPosterior,
    /// Per-entity posteriors, bounded LRU (10K default)
    pub entity_posteriors: EntityPosteriors,
    /// Total events processed by this profile
    pub total_events: u64,
    /// Incremented each time posteriors are reset to priors
    pub exploration_epoch: u64,
}

impl BalancedRecallState {
    pub fn new(entity_capacity: usize) -> Self {
        Self {
            relevance: BetaPosterior::new(7.0, 3.0),
            salience: BetaPosterior::new(2.0, 8.0),
            temporal: BetaPosterior::new(1.0, 9.0),
            entity_posteriors: EntityPosteriors::new(entity_capacity),
            total_events: 0,
            exploration_epoch: 0,
        }
    }

    pub fn reset_posteriors(&mut self) {
        self.relevance = BetaPosterior::new(7.0, 3.0);
        self.salience = BetaPosterior::new(2.0, 8.0);
        self.temporal = BetaPosterior::new(1.0, 9.0);
        self.entity_posteriors.clear();
        self.exploration_epoch += 1;
    }

    pub fn to_snapshot(&self) -> BalancedRecallSnapshot {
        BalancedRecallSnapshot {
            relevance: self.relevance.clone(),
            salience: self.salience.clone(),
            temporal: self.temporal.clone(),
            entity_posteriors: self.entity_posteriors.to_snapshot(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: BalancedRecallSnapshot, entity_capacity: usize) -> Self {
        Self {
            relevance: snapshot.relevance,
            salience: snapshot.salience,
            temporal: snapshot.temporal,
            entity_posteriors: EntityPosteriors::from_snapshot(
                snapshot.entity_posteriors,
                entity_capacity,
            ),
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }
}

/// Validate all `BetaPosterior` values in a snapshot; returns the first error or `Ok(())`.
pub fn validate_brain_state_snapshot(snapshot: &BrainStateSnapshot) -> Result<(), String> {
    // Built-in profile scalars.
    let br = &snapshot.balanced_recall;
    br.relevance
        .validate()
        .map_err(|e| format!("balanced_recall.relevance: {e}"))?;
    br.salience
        .validate()
        .map_err(|e| format!("balanced_recall.salience: {e}"))?;
    br.temporal
        .validate()
        .map_err(|e| format!("balanced_recall.temporal: {e}"))?;
    for (id, p) in &br.entity_posteriors {
        p.validate()
            .map_err(|e| format!("balanced_recall.entity_posteriors[{id}]: {e}"))?;
    }

    // User-created profile snapshots.
    for (pid, ps) in &snapshot.profile_states {
        ps.relevance
            .validate()
            .map_err(|e| format!("profile_states[{pid}].relevance: {e}"))?;
        ps.salience
            .validate()
            .map_err(|e| format!("profile_states[{pid}].salience: {e}"))?;
        ps.temporal
            .validate()
            .map_err(|e| format!("profile_states[{pid}].temporal: {e}"))?;
        for (id, p) in &ps.entity_posteriors {
            p.validate()
                .map_err(|e| format!("profile_states[{pid}].entity_posteriors[{id}]: {e}"))?;
        }
    }

    // Section posteriors.
    for (pid, ss) in &snapshot.section_states {
        for (st, p) in &ss.posteriors {
            p.validate()
                .map_err(|e| format!("section_states[{pid}].posteriors[{st:?}]: {e}"))?;
        }
        for (st, p) in &ss.priors {
            p.validate()
                .map_err(|e| format!("section_states[{pid}].priors[{st:?}]: {e}"))?;
        }
    }

    Ok(())
}

/// Serializable snapshot of `BalancedRecallState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancedRecallSnapshot {
    pub relevance: BetaPosterior,
    pub salience: BetaPosterior,
    pub temporal: BetaPosterior,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

// ── SectionPosteriorState ─────────────────────────────────────────────────────

/// Default ESS cap for section posteriors (cap=100).
pub const DEFAULT_ESS_CAP: f64 = 100.0;

/// Default exploration epoch countdown.
pub const DEFAULT_EXPLORATION_EPOCH: u64 = 50;

/// Initial temperature for Thompson sampling softmax.
pub const DEFAULT_TAU_0: f64 = 1.0;

/// Exploit-mode temperature when exploration_epoch == 0.
pub const DEFAULT_TAU_EXPLOIT: f64 = 0.1;

/// Default weight floor for section weights (5%).
pub const DEFAULT_SECTION_WEIGHT_FLOOR: f64 = 0.05;

/// Per-profile section posterior state.
pub struct SectionPosteriorState {
    pub posteriors: HashMap<SectionType, BetaPosterior>,
    pub priors: HashMap<SectionType, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl SectionPosteriorState {
    /// Create with default informative priors for all 10 section types.
    pub fn new() -> Self {
        let priors = Self::default_priors();
        let posteriors = priors.clone();
        Self {
            posteriors,
            priors,
            total_events: 0,
            exploration_epoch: DEFAULT_EXPLORATION_EPOCH,
        }
    }

    /// Create from explicit prior map. Missing sections get neutral Beta(2,2) fallback.
    pub fn from_priors(mut priors: HashMap<SectionType, BetaPosterior>) -> Self {
        let neutral = BetaPosterior::new(2.0, 2.0);
        for &st in SectionType::all() {
            priors.entry(st).or_insert_with(|| neutral.clone());
        }
        let posteriors = priors.clone();
        Self {
            posteriors,
            priors,
            total_events: 0,
            exploration_epoch: DEFAULT_EXPLORATION_EPOCH,
        }
    }

    /// Default informative priors.
    pub fn default_priors() -> HashMap<SectionType, BetaPosterior> {
        let mut m = HashMap::new();
        m.insert(SectionType::Overview, BetaPosterior::new(2.0, 2.0));
        m.insert(SectionType::CoreModel, BetaPosterior::new(4.0, 2.0));
        m.insert(
            SectionType::BoundaryConditions,
            BetaPosterior::new(2.0, 3.0),
        );
        m.insert(SectionType::Formalism, BetaPosterior::new(1.5, 4.0));
        m.insert(
            SectionType::OperationalGuidance,
            BetaPosterior::new(6.0, 1.5),
        );
        m.insert(SectionType::Examples, BetaPosterior::new(5.0, 2.0));
        m.insert(SectionType::FailureModes, BetaPosterior::new(3.0, 2.0));
        m.insert(SectionType::ExpertLens, BetaPosterior::new(3.0, 2.0));
        m.insert(SectionType::References, BetaPosterior::new(2.0, 2.0));
        m.insert(SectionType::Other, BetaPosterior::new(2.0, 2.0));
        m
    }

    pub fn to_snapshot(&self) -> SectionPosteriorSnapshot {
        SectionPosteriorSnapshot {
            posteriors: self.posteriors.clone(),
            priors: self.priors.clone(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: SectionPosteriorSnapshot) -> Self {
        Self {
            posteriors: snapshot.posteriors,
            priors: snapshot.priors,
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }

    /// Thompson sampling weights (stochastic when exploring, deterministic otherwise).
    pub fn weights(&self, rng: &mut impl rand::Rng) -> HashMap<SectionType, f64> {
        if self.exploration_epoch > 0 {
            self.sample_weights(rng)
        } else {
            self.deterministic_weights()
        }
    }

    /// Stochastic weights via Thompson sampling + softmax.
    ///
    /// Explore mode (exploration_epoch > 0):
    ///   `tau = tau_0 * (exploration_epoch / DEFAULT_EXPLORATION_EPOCH)`
    ///   `theta_i ~ Beta(alpha_i, beta_i)` via Gamma-ratio method
    ///   `w_i = softmax(theta_i / tau)`, then floor at `DEFAULT_SECTION_WEIGHT_FLOOR` + renorm
    ///
    /// Exploit mode (exploration_epoch == 0): delegates to `deterministic_weights()` with
    ///   `tau_exploit = DEFAULT_TAU_EXPLOIT` applied over posterior means.
    pub fn sample_weights(&self, rng: &mut impl rand::Rng) -> HashMap<SectionType, f64> {
        // Compute tau proportional to remaining exploration budget.
        let tau =
            DEFAULT_TAU_0 * (self.exploration_epoch as f64 / DEFAULT_EXPLORATION_EPOCH as f64);
        let tau = tau.max(1e-6); // guard against divide-by-zero at epoch boundary

        // Draw one Thompson sample per section.
        let mut samples: HashMap<SectionType, f64> = HashMap::new();
        for (&st, posterior) in &self.posteriors {
            let theta = sample_beta_gamma(posterior.alpha.max(1e-6), posterior.beta.max(1e-6), rng);
            samples.insert(st, theta / tau);
        }

        // Numerically stable softmax: subtract max before exp.
        let max_val = samples.values().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut raw: HashMap<SectionType, f64> = HashMap::new();
        for (&st, &logit) in &samples {
            raw.insert(st, (logit - max_val).exp());
        }

        apply_floor_and_renorm(&mut raw, DEFAULT_SECTION_WEIGHT_FLOOR);
        raw
    }

    /// Deterministic weights from posterior means with exploit-mode softmax.
    ///
    /// Uses `tau_exploit = DEFAULT_TAU_EXPLOIT` (0.1) over posterior means, then applies
    /// weight floor at `DEFAULT_SECTION_WEIGHT_FLOOR` and renormalizes.
    pub fn deterministic_weights(&self) -> HashMap<SectionType, f64> {
        let tau = DEFAULT_TAU_EXPLOIT;

        // Numerically stable softmax over posterior means / tau.
        let logits: HashMap<SectionType, f64> = self
            .posteriors
            .iter()
            .map(|(&st, p)| (st, p.mean() / tau))
            .collect();
        let max_val = logits.values().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mut raw: HashMap<SectionType, f64> = HashMap::new();
        for (&st, &logit) in &logits {
            raw.insert(st, (logit - max_val).exp());
        }

        apply_floor_and_renorm(&mut raw, DEFAULT_SECTION_WEIGHT_FLOOR);
        raw
    }

    /// Reset posteriors to their stored priors.
    pub fn reset_posteriors(&mut self) {
        self.posteriors = self.priors.clone();
    }
}

impl Default for SectionPosteriorState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Weight floor helpers ──────────────────────────────────────────────────────

/// Apply a weight floor and renormalize iteratively until all weights meet the floor.
///
/// A single floor+renorm pass can push other weights below the floor after renorm.
/// Iterate until stable (at most N iterations; in practice 2-3 suffice).
fn apply_floor_and_renorm(weights: &mut HashMap<SectionType, f64>, floor: f64) {
    // Normalize first so we work with probabilities.
    let sum: f64 = weights.values().sum();
    if sum > 0.0 {
        for v in weights.values_mut() {
            *v /= sum;
        }
    }
    // Iterative floor: push up pinned values, renormalize the free mass.
    for _ in 0..20 {
        let (pinned_sum, n_free) = weights.values().fold((0.0f64, 0usize), |(ps, nf), &w| {
            if w <= floor {
                (ps + floor, nf)
            } else {
                (ps, nf + 1)
            }
        });
        if n_free == 0 {
            // All sections are at or above floor; uniform renorm suffices.
            let total: f64 = weights.values().sum();
            if total > 0.0 {
                for v in weights.values_mut() {
                    *v /= total;
                }
            }
            break;
        }
        let free_mass = (1.0 - pinned_sum).max(0.0);
        let free_sum: f64 = weights.values().filter(|&&w| w > floor).sum();
        // Rescale free entries so they fill free_mass proportionally.
        for v in weights.values_mut() {
            if *v <= floor {
                *v = floor;
            } else if free_sum > 0.0 {
                *v = (*v / free_sum) * free_mass;
            }
        }
        // Check convergence: all free entries above floor?
        if weights.values().all(|&w| w >= floor - 1e-12) {
            break;
        }
    }
}

// ── Beta/Gamma sampling helpers ───────────────────────────────────────────────

/// Sample from Beta(alpha, beta) using the Gamma-ratio method.
///
/// X ~ Gamma(alpha, 1), Y ~ Gamma(beta, 1) → Beta = X / (X + Y).
fn sample_beta_gamma(alpha: f64, beta: f64, rng: &mut impl rand::Rng) -> f64 {
    let x = sample_gamma_mt(alpha, rng);
    let y = sample_gamma_mt(beta, rng);
    let s = x + y;
    if s <= 0.0 {
        0.5
    } else {
        x / s
    }
}

/// Sample from Gamma(shape, 1) using Marsaglia-Tsang's method (shape >= 1),
/// or the transformation Gamma(shape) = Gamma(shape+1) * U^(1/shape) for shape < 1.
fn sample_gamma_mt(shape: f64, rng: &mut impl rand::Rng) -> f64 {
    if shape < 1.0 {
        let g = sample_gamma_mt(shape + 1.0, rng);
        let u: f64 = rng.gen();
        return g * u.powf(1.0 / shape);
    }
    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();
    loop {
        let x: f64 = sample_standard_normal_bm(rng);
        let v_raw = 1.0 + c * x;
        if v_raw <= 0.0 {
            continue;
        }
        let v = v_raw * v_raw * v_raw;
        let u: f64 = rng.gen();
        if u < 1.0 - 0.0331 * (x * x) * (x * x) {
            return d * v;
        }
        if u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

/// Sample from N(0,1) using the Box-Muller transform.
fn sample_standard_normal_bm(rng: &mut impl rand::Rng) -> f64 {
    let u1: f64 = rng.gen::<f64>().max(f64::EPSILON);
    let u2: f64 = rng.gen();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Serializable snapshot of SectionPosteriorState.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionPosteriorSnapshot {
    pub posteriors: HashMap<SectionType, BetaPosterior>,
    pub priors: HashMap<SectionType, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

// ── ProfileLifecycle ──────────────────────────────────────────────────────────

/// Lifecycle states for a registered profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLifecycle {
    /// Profile code and metadata exist; not yet registered with brain.
    Defined,
    /// Brain knows about it; backtest-eligible. Not yet in live update loop.
    Registered,
    /// Live update loop running; snapshots persist.
    Active,
    /// Registered but no live updates. State retained; read-only.
    Inactive,
    /// Live updates stopped; snapshots and event log retained for audit.
    Archived,
}

// ── ProfileRecord ─────────────────────────────────────────────────────────────

/// Profile metadata stored in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub id: String,
    pub description: String,
    pub consumer_kind: String,
    pub state_class: String,
    pub lifecycle: ProfileLifecycle,
    pub created_at: DateTime<Utc>,
    /// Serialized state snapshot (opaque bytes to brain core)
    pub state_snapshot: Option<serde_json::Value>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl ProfileRecord {
    pub fn new_balanced_recall(entity_capacity: usize) -> Self {
        let state = BalancedRecallState::new(entity_capacity);
        let snapshot = state.to_snapshot();
        Self {
            id: "balanced-recall-v1".into(),
            description: "Default recall profile: three-scalar Beta posteriors".into(),
            consumer_kind: "recall".into(),
            state_class: "Bayesian".into(),
            lifecycle: ProfileLifecycle::Active,
            created_at: Utc::now(),
            state_snapshot: serde_json::to_value(snapshot).ok(),
            total_events: 0,
            exploration_epoch: 0,
        }
    }
}

// ── ProfileBinding ────────────────────────────────────────────────────────────

/// One row in the profile binding table.
///
/// Resolution uses longest-match wins; `*` is the wildcard sentinel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileBinding {
    pub actor: String,
    pub namespace: String,
    pub consumer_kind: String,
    pub profile_id: String,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

// ── BrainState (profile registry) ────────────────────────────────────────────

/// Runtime brain state — profile registry + active state per profile.
///
/// `BrainState` holds profile registry and lifecycle metadata. Posteriors live
/// inside each profile's own state, opaque to brain core.
///
/// Per-profile state: `balanced_recall` holds the live state for the built-in
/// `balanced-recall-v1` profile. `profile_states` holds live `BalancedRecallState`
/// for every user-created Bayesian profile. Both maps are initialised at profile
/// creation and cleared on hard-delete; they are never absent for a living profile
/// whose `state_class == "Bayesian"`.
///
/// `section_states`: per-profile section-level Beta posteriors.
/// Keys are profile_id; values are `SectionPosteriorState`.
pub struct BrainState {
    /// Registered profiles indexed by profile_id.
    pub profiles: HashMap<String, ProfileRecord>,
    /// In-memory BalancedRecallState for the built-in `balanced-recall-v1` profile.
    pub balanced_recall: BalancedRecallState,
    /// Per-profile live state for user-created Bayesian profiles.
    pub profile_states: HashMap<String, BalancedRecallState>,
    /// Profile binding table — maps (actor, namespace, consumer_kind) → profile_id.
    pub bindings: Vec<ProfileBinding>,
    /// Per-profile section posteriors.
    pub section_states: HashMap<String, SectionPosteriorState>,
}

impl BrainState {
    pub fn new(entity_capacity: usize) -> Self {
        let mut profiles = HashMap::new();
        let record = ProfileRecord::new_balanced_recall(entity_capacity);
        let profile_id = record.id.clone();
        profiles.insert(profile_id.clone(), record);

        Self {
            profiles,
            balanced_recall: BalancedRecallState::new(entity_capacity),
            profile_states: HashMap::new(),
            bindings: Vec::new(),
            section_states: HashMap::new(),
        }
    }

    pub fn to_snapshot(&self) -> BrainStateSnapshot {
        let extra: HashMap<String, BalancedRecallSnapshot> = self
            .profile_states
            .iter()
            .map(|(id, s)| (id.clone(), s.to_snapshot()))
            .collect();
        let section_states: HashMap<String, SectionPosteriorSnapshot> = self
            .section_states
            .iter()
            .map(|(id, s)| (id.clone(), s.to_snapshot()))
            .collect();
        BrainStateSnapshot {
            profiles: self.profiles.clone(),
            balanced_recall: self.balanced_recall.to_snapshot(),
            profile_states: extra,
            bindings: self.bindings.clone(),
            section_states,
        }
    }

    pub fn from_snapshot(snapshot: BrainStateSnapshot, entity_capacity: usize) -> Self {
        let extra: HashMap<String, BalancedRecallState> = snapshot
            .profile_states
            .into_iter()
            .map(|(id, s)| (id, BalancedRecallState::from_snapshot(s, entity_capacity)))
            .collect();
        let section_states: HashMap<String, SectionPosteriorState> = snapshot
            .section_states
            .into_iter()
            .map(|(id, s)| (id, SectionPosteriorState::from_snapshot(s)))
            .collect();
        Self {
            profiles: snapshot.profiles,
            balanced_recall: BalancedRecallState::from_snapshot(
                snapshot.balanced_recall,
                entity_capacity,
            ),
            profile_states: extra,
            bindings: snapshot.bindings,
            section_states,
        }
    }

    /// Reset the balanced-recall profile posteriors to priors.
    pub fn reset_posteriors(&mut self) {
        self.balanced_recall.reset_posteriors();
        if let Some(record) = self.profiles.get_mut("balanced-recall-v1") {
            record.exploration_epoch = self.balanced_recall.exploration_epoch;
            record.state_snapshot = serde_json::to_value(self.balanced_recall.to_snapshot()).ok();
        }
        if let Some(ss) = self.section_states.get_mut("balanced-recall-v1") {
            ss.reset_posteriors();
        }
    }

    /// Reset posteriors for a user-created Bayesian profile.
    pub fn reset_profile_posteriors(&mut self, profile_id: &str) {
        if let Some(ps) = self.profile_states.get_mut(profile_id) {
            ps.reset_posteriors();
            let snap = serde_json::to_value(ps.to_snapshot()).ok();
            let epoch = ps.exploration_epoch;
            if let Some(record) = self.profiles.get_mut(profile_id) {
                record.exploration_epoch = epoch;
                record.state_snapshot = snap;
            }
        }
        if let Some(ss) = self.section_states.get_mut(profile_id) {
            ss.reset_posteriors();
        }
    }

    /// Resolve a profile_id for the given caller context.
    ///
    /// Longest-match wins: actor + namespace + consumer_kind beats actor + consumer_kind
    /// beats namespace + consumer_kind beats consumer_kind alone. Returns the
    /// `balanced-recall-v1` default when no explicit binding matches.
    ///
    /// Archived profiles are never returned, whether reached via binding or fallback.
    pub fn resolve(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<&ProfileRecord> {
        self.resolve_with_match(actor, namespace, consumer_kind)
            .map(|(record, _)| record)
    }

    /// Like `resolve`, but also returns the `consumer_kind` field from the matched
    /// binding row (H3: lets the caller distinguish a wildcard match from an exact match).
    ///
    /// Returns `(profile_record, matched_binding_consumer_kind)`.
    /// For the implicit default fallback the matched kind equals the profile's own
    /// `consumer_kind`.
    pub fn resolve_with_match(
        &self,
        actor: Option<&str>,
        namespace: Option<&str>,
        consumer_kind: &str,
    ) -> Option<(&ProfileRecord, String)> {
        let actor_val = actor.unwrap_or("*");
        let namespace_val = namespace.unwrap_or("*");

        // Pre-filter: exclude bindings whose target profile is archived or missing.
        // This ensures archived profiles are excluded from candidate selection entirely,
        // so a lower-priority live binding can win over a higher-priority archived one.
        let best = self
            .bindings
            .iter()
            .filter(|b| {
                (b.actor == "*" || b.actor == actor_val)
                    && (b.namespace == "*" || b.namespace == namespace_val)
                    && (b.consumer_kind == "*" || b.consumer_kind == consumer_kind)
                    && self
                        .profiles
                        .get(&b.profile_id)
                        .is_some_and(|p| p.lifecycle != ProfileLifecycle::Archived)
            })
            .max_by_key(|b| {
                let actor_score = if b.actor != "*" { 4 } else { 0 };
                let ns_score = if b.namespace != "*" { 2 } else { 0 };
                let kind_score = if b.consumer_kind != "*" { 1 } else { 0 };
                (
                    actor_score + ns_score + kind_score,
                    b.priority,
                    -(b.created_at.timestamp()),
                )
            });

        if let Some(binding) = best {
            if let Some(record) = self.profiles.get(&binding.profile_id) {
                return Some((record, binding.consumer_kind.clone()));
            }
            // Profile disappeared between filter and get (very unlikely) — fall through.
        }

        // No explicit binding (or all matched bindings point at archived profiles) —
        // return the named default profile if it exists and is usable.
        // "balanced-recall-v1" is the v1 system-default for recall.
        if let Some(default) = self.profiles.get("balanced-recall-v1") {
            if default.lifecycle == ProfileLifecycle::Active
                && (default.consumer_kind == consumer_kind
                    || consumer_kind == "*"
                    || default.consumer_kind == "*")
            {
                return Some((default, default.consumer_kind.clone()));
            }
        }

        // Generic fallback: first active profile matching consumer_kind.
        self.profiles.values().find_map(|p| {
            if p.consumer_kind == consumer_kind && p.lifecycle == ProfileLifecycle::Active {
                Some((p, p.consumer_kind.clone()))
            } else {
                None
            }
        })
    }
}

/// Serializable snapshot of the full brain state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainStateSnapshot {
    pub profiles: HashMap<String, ProfileRecord>,
    pub balanced_recall: BalancedRecallSnapshot,
    /// Snapshots for user-created Bayesian profiles.
    #[serde(default)]
    pub profile_states: HashMap<String, BalancedRecallSnapshot>,
    pub bindings: Vec<ProfileBinding>,
    /// Per-profile section posteriors.
    #[serde(default)]
    pub section_states: HashMap<String, SectionPosteriorSnapshot>,
}

#[cfg(test)]
// INLINE TEST JUSTIFICATION: tests exercise private arithmetic invariants
// (merge formula, ESS cap, floored mean) that operate directly on BetaPosterior
// fields not re-exported to integration tests.
mod tests {
    use super::*;

    #[test]
    fn beta_posterior_mean() {
        let p = BetaPosterior::new(7.0, 3.0);
        assert!((p.mean() - 0.7).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_variance() {
        let p = BetaPosterior::new(7.0, 3.0);
        let expected = 21.0 / 1100.0;
        assert!((p.variance() - expected).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_ess() {
        let p = BetaPosterior::new(7.0, 3.0);
        assert!((p.effective_sample_size() - 10.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_update() {
        let mut p = BetaPosterior::new(1.0, 1.0);
        p.update_success();
        p.update_success();
        p.update_failure();
        assert!((p.alpha - 3.0).abs() < 1e-12);
        assert!((p.beta - 2.0).abs() < 1e-12);
        assert!((p.mean() - 0.6).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_merge() {
        let prior = BetaPosterior::new(2.0, 8.0);
        let a = BetaPosterior::new(5.0, 9.0); // prior + 3 success, 1 failure
        let b = BetaPosterior::new(4.0, 10.0); // prior + 2 success, 2 failure
        let merged = a.merge(&b, &prior);
        // merged = (5+4-2, 9+10-8) = (7, 11)
        assert!((merged.alpha - 7.0).abs() < 1e-12);
        assert!((merged.beta - 11.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_apply_ess_cap_noop() {
        // ESS = 10 ≤ cap = 50 → no change
        let prior = BetaPosterior::new(2.0, 2.0);
        let mut p = BetaPosterior::new(7.0, 3.0);
        p.apply_ess_cap(&prior, 50.0);
        assert!((p.alpha - 7.0).abs() < 1e-12);
        assert!((p.beta - 3.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_apply_ess_cap_rescale() {
        // alpha=30, beta=30 → ESS=60 > cap=50, prior_ess=4
        // scale = (cap - prior_ess) / (ess - prior_ess) = (50-4)/(60-4) = 46/56
        // new_alpha = 2 + 28*(46/56), new_beta = 2 + 28*(46/56)
        // new_ess = 4 + 56*(46/56) = 50 exactly
        let prior = BetaPosterior::new(2.0, 2.0);
        let mut p = BetaPosterior::new(30.0, 30.0);
        p.apply_ess_cap(&prior, 50.0);
        let scale = (50.0 - 4.0) / (60.0 - 4.0); // (cap - prior_ess) / (ess - prior_ess)
        let expected_excess = 28.0 * scale;
        assert!((p.alpha - (2.0 + expected_excess)).abs() < 1e-10);
        assert!((p.beta - (2.0 + expected_excess)).abs() < 1e-10);
        assert!((p.effective_sample_size() - 50.0).abs() < 1e-10);
    }

    #[test]
    fn beta_posterior_floored_mean() {
        let p = BetaPosterior::new(1.0, 99.0); // mean = 0.01
        assert!((p.floored_mean(0.05) - 0.05).abs() < 1e-12);

        let p2 = BetaPosterior::new(7.0, 3.0); // mean = 0.7
        assert!((p2.floored_mean(0.05) - 0.7).abs() < 1e-12);
    }

    #[test]
    fn entity_posteriors_eviction() {
        let mut ep = EntityPosteriors::new(3);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            ep.get_or_insert(*id, BetaPosterior::default);
        }
        assert_eq!(ep.len(), 3);
        assert!(ep.get(&ids[0]).is_none());
        assert!(ep.get(&ids[1]).is_none());
        assert!(ep.get(&ids[2]).is_some());
        assert!(ep.get(&ids[3]).is_some());
        assert!(ep.get(&ids[4]).is_some());
    }

    #[test]
    fn entity_posteriors_get_or_insert_existing() {
        let mut ep = EntityPosteriors::new(10);
        let id = Uuid::new_v4();
        ep.get_or_insert(id, BetaPosterior::default)
            .update_success();
        let p = ep.get_or_insert(id, BetaPosterior::default);
        assert!((p.alpha - 2.0).abs() < 1e-12);
    }

    #[test]
    fn balanced_recall_state_snapshot_roundtrip() {
        let mut state = BalancedRecallState::new(100);
        state.relevance.update_success();
        state.total_events = 42;
        let id = Uuid::new_v4();
        state
            .entity_posteriors
            .get_or_insert(id, BetaPosterior::default)
            .update_success();

        let snapshot = state.to_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: BalancedRecallSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_events, 42);
        assert!((back.relevance.alpha - 8.0).abs() < 1e-12);
        assert!(back.entity_posteriors.contains_key(&id));
    }

    #[test]
    fn balanced_recall_state_reset_preserves_epoch_increment() {
        let mut state = BalancedRecallState::new(10);
        state.total_events = 100;
        state.reset_posteriors();
        assert_eq!(state.total_events, 100);
        assert_eq!(state.exploration_epoch, 1);
        assert!((state.relevance.alpha - 7.0).abs() < 1e-12);
        assert!((state.relevance.beta - 3.0).abs() < 1e-12);
    }

    #[test]
    fn brain_state_has_balanced_recall_profile_by_default() {
        let state = BrainState::new(100);
        assert!(state.profiles.contains_key("balanced-recall-v1"));
        let record = &state.profiles["balanced-recall-v1"];
        assert_eq!(record.lifecycle, ProfileLifecycle::Active);
        assert_eq!(record.consumer_kind, "recall");
        assert_eq!(record.state_class, "Bayesian");
    }

    #[test]
    fn brain_state_reset_posteriors_updates_record() {
        let mut state = BrainState::new(10);
        state.balanced_recall.relevance.update_success();
        state.balanced_recall.total_events = 50;
        state.reset_posteriors();
        assert_eq!(state.balanced_recall.exploration_epoch, 1);
        let record = &state.profiles["balanced-recall-v1"];
        assert_eq!(record.exploration_epoch, 1);
    }

    #[test]
    fn brain_state_resolve_falls_back_to_default() {
        let state = BrainState::new(100);
        let resolved = state.resolve(None, None, "recall");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "balanced-recall-v1");
    }

    #[test]
    fn brain_state_resolve_uses_explicit_binding() {
        let mut state = BrainState::new(100);
        // Add a second profile
        let mut alt = ProfileRecord::new_balanced_recall(100);
        alt.id = "alt-profile".into();
        state.profiles.insert("alt-profile".into(), alt);

        // Bind alt-profile for actor "agent-1"
        state.bindings.push(ProfileBinding {
            actor: "agent-1".into(),
            namespace: "*".into(),
            consumer_kind: "recall".into(),
            profile_id: "alt-profile".into(),
            priority: 0,
            created_at: Utc::now(),
        });

        let resolved = state.resolve(Some("agent-1"), None, "recall");
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().id, "alt-profile");

        // Different actor falls back to default
        let resolved_other = state.resolve(Some("agent-2"), None, "recall");
        assert_eq!(resolved_other.unwrap().id, "balanced-recall-v1");
    }

    // Regression test for MAJ-005: an archived default profile must NOT be returned
    // by resolve (archived profiles are never resolvable for live recall).
    #[test]
    fn brain_state_resolve_skips_archived_default() {
        let mut state = BrainState::new(100);

        // Archive the built-in default
        state
            .profiles
            .get_mut("balanced-recall-v1")
            .expect("default profile always exists")
            .lifecycle = ProfileLifecycle::Archived;

        // No explicit binding → must not return the archived default
        let resolved = state.resolve(None, None, "recall");
        assert!(
            resolved.is_none(),
            "archived default profile must not be returned by resolve"
        );
    }

    #[test]
    fn entity_posteriors_from_snapshot_rebuilds_map() {
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let mut snapshot = HashMap::new();
        snapshot.insert(id1, BetaPosterior::new(3.0, 2.0));
        snapshot.insert(id2, BetaPosterior::new(5.0, 1.0));

        let ep = EntityPosteriors::from_snapshot(snapshot, 100);
        assert_eq!(ep.len(), 2);
        let p1 = ep.get(&id1).unwrap();
        assert!((p1.alpha - 3.0).abs() < 1e-12);
        let p2 = ep.get(&id2).unwrap();
        assert!((p2.alpha - 5.0).abs() < 1e-12);
    }

    #[test]
    fn brain_state_snapshot_roundtrip() {
        let mut state = BrainState::new(100);
        state.balanced_recall.relevance.update_success();
        state.balanced_recall.total_events = 55;
        state.balanced_recall.exploration_epoch = 2;
        let id = Uuid::new_v4();
        state
            .balanced_recall
            .entity_posteriors
            .get_or_insert(id, || BetaPosterior::new(4.0, 6.0))
            .update_success();

        let snap1 = state.to_snapshot();
        let restored = BrainState::from_snapshot(snap1, 100);
        let snap2 = restored.to_snapshot();

        assert_eq!(snap2.balanced_recall.total_events, 55);
        assert_eq!(snap2.balanced_recall.exploration_epoch, 2);
        assert!((snap2.balanced_recall.relevance.alpha - 8.0).abs() < 1e-12);
        let ep = snap2.balanced_recall.entity_posteriors.get(&id).unwrap();
        assert!((ep.alpha - 5.0).abs() < 1e-12);
        assert!((ep.beta - 6.0).abs() < 1e-12);
    }

    #[test]
    fn profile_lifecycle_serde_roundtrip() {
        let lc = ProfileLifecycle::Active;
        let json = serde_json::to_string(&lc).unwrap();
        let back: ProfileLifecycle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProfileLifecycle::Active);
    }

    #[test]
    fn beta_posterior_default_has_uniform_prior() {
        let p = BetaPosterior::default();
        assert!((p.alpha - 1.0).abs() < 1e-12);
        assert!((p.beta - 1.0).abs() < 1e-12);
        assert!((p.mean() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn section_type_serde_roundtrip() {
        for &st in SectionType::all() {
            let json = serde_json::to_string(&st).unwrap();
            let back: SectionType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, st, "roundtrip failed for {st}");
        }
    }

    #[test]
    fn section_type_display_and_from_str() {
        for &st in SectionType::all() {
            let s = st.to_string();
            let parsed: SectionType = s.parse().expect("parse should succeed");
            assert_eq!(parsed, st);
        }
    }

    #[test]
    fn section_type_from_str_unknown_rejected() {
        let result = "unknown_section".parse::<SectionType>();
        assert!(result.is_err());
    }

    #[test]
    fn section_posterior_state_new_has_all_sections() {
        let state = SectionPosteriorState::new();
        assert_eq!(state.posteriors.len(), 10);
        assert_eq!(state.priors.len(), 10);
        for &st in SectionType::all() {
            assert!(
                state.posteriors.contains_key(&st),
                "missing section {st} in posteriors"
            );
            assert!(
                state.priors.contains_key(&st),
                "missing section {st} in priors"
            );
        }
    }

    #[test]
    fn section_posterior_state_snapshot_roundtrip() {
        let state = SectionPosteriorState::new();
        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: SectionPosteriorSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.posteriors.len(), 10);
        assert_eq!(back.total_events, 0);
        assert_eq!(back.exploration_epoch, DEFAULT_EXPLORATION_EPOCH);

        // Reconstruct from snapshot and verify posteriors match
        let restored = SectionPosteriorState::from_snapshot(back);
        assert_eq!(restored.posteriors.len(), 10);
        let op = &state.posteriors[&SectionType::OperationalGuidance];
        let rp = &restored.posteriors[&SectionType::OperationalGuidance];
        assert!((op.alpha - rp.alpha).abs() < 1e-12);
        assert!((op.beta - rp.beta).abs() < 1e-12);
    }

    #[test]
    fn section_posterior_state_deterministic_weights_normalized() {
        let state = SectionPosteriorState::new();
        let weights = state.deterministic_weights();
        assert_eq!(weights.len(), 10);
        let sum: f64 = weights.values().sum();
        assert!((sum - 1.0).abs() < 1e-10, "weights sum = {sum}");
        for (&st, &w) in &weights {
            assert!(
                w >= DEFAULT_SECTION_WEIGHT_FLOOR - 1e-12,
                "weight for {st} below floor: {w}"
            );
        }
    }

    #[test]
    fn section_posterior_state_from_priors_fills_missing() {
        // Provide only 3 sections; the other 7 should be filled with neutral Beta(2,2)
        let mut partial: HashMap<SectionType, BetaPosterior> = HashMap::new();
        partial.insert(SectionType::Overview, BetaPosterior::new(5.0, 1.0));
        partial.insert(SectionType::CoreModel, BetaPosterior::new(4.0, 2.0));
        partial.insert(SectionType::Formalism, BetaPosterior::new(3.0, 3.0));

        let state = SectionPosteriorState::from_priors(partial);
        assert_eq!(state.priors.len(), 10);
        assert_eq!(state.posteriors.len(), 10);

        // Explicitly provided priors are preserved
        assert!((state.priors[&SectionType::Overview].alpha - 5.0).abs() < 1e-12);

        // Missing sections get neutral Beta(2,2)
        let neutral = &state.priors[&SectionType::Examples];
        assert!((neutral.alpha - 2.0).abs() < 1e-12);
        assert!((neutral.beta - 2.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_weighted_success_adds_weight_to_alpha() {
        let mut p = BetaPosterior::new(2.0, 8.0);
        p.update_success_weighted(1.5);
        assert!((p.alpha - 3.5).abs() < 1e-12);
        assert!((p.beta - 8.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_weighted_failure_adds_weight_to_beta() {
        let mut p = BetaPosterior::new(2.0, 8.0);
        p.update_failure_weighted(2.0);
        assert!((p.alpha - 2.0).abs() < 1e-12);
        assert!((p.beta - 10.0).abs() < 1e-12);
    }

    #[test]
    fn beta_posterior_weighted_fractional_update() {
        let mut p = BetaPosterior::new(1.0, 1.0);
        p.update_success_weighted(0.5);
        assert!((p.alpha - 1.5).abs() < 1e-12);
        assert!((p.beta - 1.0).abs() < 1e-12);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "update_success_weighted: weight must be positive")]
    fn beta_posterior_weighted_success_rejects_zero_weight() {
        let mut p = BetaPosterior::new(1.0, 1.0);
        p.update_success_weighted(0.0);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "update_failure_weighted: weight must be positive")]
    fn beta_posterior_weighted_failure_rejects_negative_weight() {
        let mut p = BetaPosterior::new(1.0, 1.0);
        p.update_failure_weighted(-1.0);
    }

    // BRAIN-AUD-002: validate try_new and snapshot validation boundaries.
    #[test]
    fn try_new_rejects_zero_alpha() {
        assert!(BetaPosterior::try_new(0.0, 1.0).is_err());
    }

    #[test]
    fn try_new_rejects_negative_beta() {
        assert!(BetaPosterior::try_new(1.0, -1.0).is_err());
    }

    #[test]
    fn try_new_rejects_nan_alpha() {
        assert!(BetaPosterior::try_new(f64::NAN, 1.0).is_err());
    }

    #[test]
    fn try_new_rejects_inf_beta() {
        assert!(BetaPosterior::try_new(1.0, f64::INFINITY).is_err());
    }

    #[test]
    fn try_new_accepts_valid_values() {
        assert!(BetaPosterior::try_new(7.0, 3.0).is_ok());
    }

    #[test]
    fn validate_brain_state_snapshot_rejects_invalid_alpha() {
        let mut snapshot = BrainState::new(10).to_snapshot();
        snapshot.balanced_recall.relevance.alpha = 0.0;
        assert!(validate_brain_state_snapshot(&snapshot).is_err());
    }

    #[test]
    fn validate_brain_state_snapshot_rejects_nan_posterior() {
        let mut snapshot = BrainState::new(10).to_snapshot();
        snapshot.balanced_recall.salience.beta = f64::NAN;
        assert!(validate_brain_state_snapshot(&snapshot).is_err());
    }

    #[test]
    fn validate_brain_state_snapshot_accepts_valid_default() {
        let snapshot = BrainState::new(10).to_snapshot();
        assert!(validate_brain_state_snapshot(&snapshot).is_ok());
    }
}
