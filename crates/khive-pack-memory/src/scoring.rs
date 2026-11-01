//! Tunable composite scoring, normalization, adjustments, and language routing.
//! See `crates/khive-pack-memory/docs/api/scoring.md` for the complete scoring model.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};

pub use khive_text::{contains_cjk_f32 as contains_cjk, is_cjk_char};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Adjustment conditions ─────────────────────────────────────────────────────

/// A condition that determines whether a score adjustment applies to a candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdjustmentCondition {
    /// Match by memory type ("episodic" or "semantic").
    MemoryType { kind: String },
    /// Match by age in days. Both bounds are optional (omit = no bound).
    AgeRange {
        #[serde(default)]
        min_days: Option<f32>,
        #[serde(default)]
        max_days: Option<f32>,
    },
    /// Match by salience score. Both bounds are optional.
    SalienceRange {
        #[serde(default)]
        min: Option<f32>,
        #[serde(default)]
        max: Option<f32>,
    },
    /// Match when query entity names appear in memory content.
    EntityMatch,
    /// Match when query entity names do NOT appear in memory content.
    EntityMiss,
    /// All sub-conditions must be true (conjunction).
    All {
        conditions: Vec<AdjustmentCondition>,
    },
}

/// The operation to apply when a condition matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdjustmentOp {
    /// Add a fixed value to the score.
    Add { value: f32 },
    /// Subtract a fixed value from the score.
    Subtract { value: f32 },
    /// Multiply the score by a factor.
    Multiply { factor: f32 },
}

/// A conditional score adjustment: if `condition` matches, apply `operation`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreAdjustment {
    pub condition: AdjustmentCondition,
    pub operation: AdjustmentOp,
}

/// Context passed to condition evaluation — properties of the candidate + query.
pub struct CandidateContext<'a> {
    pub memory_type: &'a str,
    pub age_days: f32,
    pub salience: f32,
    pub content: &'a str,
    pub entity_names: &'a [String],
}

/// Match a pre-lowercased name at non-alphanumeric outer boundaries.
/// All-CJK names use substring matching because equivalent word boundaries are absent.
fn contains_at_word_boundary_cached(
    haystack: &str,
    cached_chars: &mut Option<Vec<char>>,
    needle: &str,
) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.chars().all(is_cjk_char) {
        return haystack.contains(needle);
    }
    let haystack_chars = cached_chars.get_or_insert_with(|| {
        #[cfg(test)]
        loop_3711_tests::body_materialized();
        haystack.chars().collect()
    });
    let needle_chars: Vec<char> = needle.chars().collect();
    let n = needle_chars.len();
    if n == 0 || haystack_chars.len() < n {
        return false;
    }
    for start in 0..=(haystack_chars.len() - n) {
        if haystack_chars[start..start + n] != needle_chars[..] {
            continue;
        }
        let before_ok = start == 0 || !haystack_chars[start - 1].is_alphanumeric();
        let after_idx = start + n;
        let after_ok =
            after_idx >= haystack_chars.len() || !haystack_chars[after_idx].is_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn matches_any_entity(ctx: &CandidateContext<'_>) -> bool {
    let lower = ctx.content.to_lowercase();
    let mut chars = None;
    ctx.entity_names
        .iter()
        .any(|name| contains_at_word_boundary_cached(&lower, &mut chars, name))
}

#[cfg(test)]
fn contains_at_word_boundary(haystack: &str, needle: &str) -> bool {
    contains_at_word_boundary_cached(haystack, &mut None, needle)
}

impl AdjustmentCondition {
    /// Return `true` when this condition applies to the given candidate context.
    pub fn matches(&self, ctx: &CandidateContext<'_>) -> bool {
        match self {
            Self::MemoryType { kind } => ctx.memory_type == kind.as_str(),
            Self::AgeRange { min_days, max_days } => {
                if let Some(min) = min_days {
                    if ctx.age_days < *min {
                        return false;
                    }
                }
                if let Some(max) = max_days {
                    if ctx.age_days > *max {
                        return false;
                    }
                }
                true
            }
            Self::SalienceRange { min, max } => {
                if let Some(lo) = min {
                    if ctx.salience < *lo {
                        return false;
                    }
                }
                if let Some(hi) = max {
                    if ctx.salience > *hi {
                        return false;
                    }
                }
                true
            }
            Self::EntityMatch => {
                if ctx.entity_names.is_empty() {
                    return false;
                }
                matches_any_entity(ctx)
            }
            Self::EntityMiss => {
                if ctx.entity_names.is_empty() {
                    return false;
                }
                !matches_any_entity(ctx)
            }
            Self::All { conditions } => conditions.iter().all(|c| c.matches(ctx)),
        }
    }
}

impl AdjustmentOp {
    /// Apply this operation to `score` and return the adjusted value.
    pub fn apply(&self, score: f32) -> f32 {
        match self {
            Self::Add { value } => score + value,
            Self::Subtract { value } => score - value,
            Self::Multiply { factor } => score * factor,
        }
    }
}

impl ScoreAdjustment {
    /// Apply this adjustment to `score` if the condition matches, otherwise return `score` unchanged.
    pub fn apply(&self, score: f32, ctx: &CandidateContext<'_>) -> f32 {
        if self.condition.matches(ctx) {
            self.operation.apply(score)
        } else {
            score
        }
    }
}

/// Default score adjustments: episodic recency bonus, entity boost.
///
/// A third default — a semantic age penalty (semantic, >= 30 days old,
/// salience >= 0.85, subtract 0.05) — was removed after a 2026-07-21
/// single-delta ablation on the live corpus: removing only that penalty
/// improved golden-suite hit@5 from 0.4706 to 0.5882 (MRR 0.2766 -> 0.3105)
/// while a 24-query holdout suite was byte-identical. Old high-salience
/// semantic memories are typically durable reference material (directives,
/// corrections) that recall must keep surfacing; penalizing them by age
/// suppressed exactly that class. The penalty remains expressible per-call
/// via `ScoringConfig.adjustments`.
pub fn default_adjustments() -> Vec<ScoreAdjustment> {
    vec![
        // Episodic recency bonus: recent episodic memories get an additive boost.
        ScoreAdjustment {
            condition: AdjustmentCondition::All {
                conditions: vec![
                    AdjustmentCondition::MemoryType {
                        kind: "episodic".into(),
                    },
                    AdjustmentCondition::AgeRange {
                        min_days: None,
                        max_days: Some(7.0),
                    },
                ],
            },
            operation: AdjustmentOp::Add { value: 0.05 },
        },
        // Entity match boost: memories mentioning queried entities get boosted.
        ScoreAdjustment {
            condition: AdjustmentCondition::EntityMatch,
            operation: AdjustmentOp::Multiply { factor: 1.3 },
        },
    ]
}

// ── Auto entity-name extraction ─────────────────────────────────────────────────

/// Conservative English function words excluded from heuristic entity candidates.
const ENTITY_STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "in", "on", "at", "to", "for", "with", "by",
    "from", "is", "are", "was", "were", "be", "been", "being", "this", "that", "these", "those",
    "it", "its", "he", "she", "his", "her", "him", "they", "them", "their", "we", "us", "our",
    "you", "your", "i", "my", "me", "as", "if", "so", "not", "no", "yes", "do", "does", "did",
    "has", "have", "had", "will", "would", "can", "could", "should", "may", "might", "about",
    "into", "than", "then", "there", "here", "what", "which", "who", "whom", "when", "where",
    "why", "how",
];

/// Maximum heuristic entity names per query; it bounds scans, not boosted memories.
pub const MAX_AUTO_ENTITY_NAMES: usize = 8;

/// Strip leading/trailing non-alphanumeric characters from a token (quotes,
/// commas, trailing periods, etc.), keeping internal punctuation like
/// apostrophes in a name intact.
fn strip_token_punctuation(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Extract capitalized, non-stopword entity names for the default entity boost.
///
/// Results are punctuation-trimmed, lowercase, first-seen deduplicated, and capped at
/// [`MAX_AUTO_ENTITY_NAMES`]. Lowercase-only queries return no heuristic names.
/// See `crates/khive-pack-memory/docs/api/scoring.md` for the precision rationale.
pub fn extract_entity_candidates(query: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for raw in query.split_whitespace() {
        let stripped = strip_token_punctuation(raw);
        if stripped.is_empty() {
            continue;
        }
        if !stripped.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        let lower = stripped.to_lowercase();
        if ENTITY_STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(lower);
            if out.len() >= MAX_AUTO_ENTITY_NAMES {
                break;
            }
        }
    }
    out
}

/// Maximum candidate strings sent to one batched KG entity-name lookup.
pub const MAX_ENTITY_LOOKUP_CANDIDATES: usize = 64;

const MAX_BIGRAM_LOOKUP_CANDIDATES: usize = MAX_ENTITY_LOOKUP_CANDIDATES / 4;
const MIN_CJK_LOOKUP_CHARS: usize = 2;
const MAX_CJK_LOOKUP_CHARS: usize = 8;

fn entity_lookup_case_variants(candidate: String) -> [Option<String>; 2] {
    let lower = candidate.to_ascii_lowercase();
    if candidate.is_ascii() {
        [None, Some(lower)]
    } else {
        [Some(candidate), Some(lower)]
    }
}

/// Build bounded strings for recall's batched, case-insensitive KG entity lookup.
///
/// Includes lowercase unigrams/bigrams and length-fair CJK substrings, deduplicated and
/// capped at [`MAX_ENTITY_LOOKUP_CANDIDATES`]. The caller restores precision by retaining
/// only real entity names. See `crates/khive-pack-memory/docs/api/scoring.md`.
pub fn entity_lookup_candidates(query: &str) -> Vec<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(strip_token_punctuation)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    let chars: Vec<char> = query.chars().collect();
    let mut cjk_runs = Vec::new();
    let mut run_start = 0;
    while run_start < chars.len() {
        if !is_cjk_char(chars[run_start]) {
            run_start += 1;
            continue;
        }
        let mut run_end = run_start + 1;
        while run_end < chars.len() && is_cjk_char(chars[run_end]) {
            run_end += 1;
        }
        cjk_runs.push((run_start, run_end));
        run_start = run_end;
    }

    let length_count = MAX_CJK_LOOKUP_CHARS - MIN_CJK_LOOKUP_CHARS + 1;
    let base_quota = MAX_ENTITY_LOOKUP_CANDIDATES / length_count;
    let quota_remainder = MAX_ENTITY_LOOKUP_CANDIDATES % length_count;

    let cjk_candidates: Vec<Vec<String>> = (MIN_CJK_LOOKUP_CHARS..=MAX_CJK_LOOKUP_CHARS)
        .map(|len| {
            let mut last_start_by_candidate = std::collections::HashMap::new();
            for &(run_start, run_end) in &cjk_runs {
                if run_end - run_start < len {
                    continue;
                }
                for start in run_start..=run_end - len {
                    let candidate: String = chars[start..start + len].iter().collect();
                    last_start_by_candidate.insert(candidate, start);
                }
            }

            let mut positioned: Vec<(usize, String)> = last_start_by_candidate
                .into_iter()
                .map(|(candidate, start)| (start, candidate))
                .collect();
            positioned.sort_unstable_by_key(|(start, _)| *start);
            positioned
                .into_iter()
                .map(|(_, candidate)| candidate)
                .collect()
        })
        .collect();

    let mut quotas: Vec<usize> = (0..length_count)
        .map(|index| base_quota + usize::from(index < quota_remainder))
        .collect();
    let mut unused = 0;
    for (quota, candidates) in quotas.iter_mut().zip(&cjk_candidates) {
        if candidates.len() < *quota {
            unused += *quota - candidates.len();
            *quota = candidates.len();
        }
    }
    while unused > 0 {
        let mut redistributed = false;
        for (quota, candidates) in quotas.iter_mut().zip(&cjk_candidates) {
            if *quota < candidates.len() {
                *quota += 1;
                unused -= 1;
                redistributed = true;
                if unused == 0 {
                    break;
                }
            }
        }
        if !redistributed {
            break;
        }
    }

    for (candidates, quota) in cjk_candidates.iter().zip(quotas) {
        if quota == 0 {
            continue;
        }
        let num_positions = candidates.len();
        let mut sampled_indices = if quota == 1 {
            vec![0]
        } else {
            (0..quota)
                .map(|index| index * (num_positions - 1) / (quota - 1))
                .collect::<Vec<_>>()
        };
        sampled_indices.dedup();
        for index in sampled_indices {
            let candidate = candidates[index].clone();
            if seen.insert(candidate.clone()) {
                out.push(candidate);
            }
        }
    }
    if out.len() >= MAX_ENTITY_LOOKUP_CANDIDATES {
        return out;
    }

    let mut bigrams = tokens
        .windows(2)
        .map(|pair| format!("{} {}", pair[0], pair[1]));
    for bigram in bigrams.by_ref().take(MAX_BIGRAM_LOOKUP_CANDIDATES) {
        for candidate in entity_lookup_case_variants(bigram).into_iter().flatten() {
            if seen.insert(candidate.clone()) {
                out.push(candidate);
                if out.len() >= MAX_ENTITY_LOOKUP_CANDIDATES {
                    return out;
                }
            }
        }
    }

    for token in tokens.iter().filter(|token| {
        !ENTITY_STOPWORDS.contains(&token.to_ascii_lowercase().as_str())
            && !token.chars().all(is_cjk_char)
    }) {
        for candidate in entity_lookup_case_variants(token.clone())
            .into_iter()
            .flatten()
        {
            if seen.insert(candidate.clone()) {
                out.push(candidate);
                if out.len() >= MAX_ENTITY_LOOKUP_CANDIDATES {
                    return out;
                }
            }
        }
    }

    // If the unigram share did not fill the cap, admit more adjacent bigrams.
    for bigram in bigrams {
        for candidate in entity_lookup_case_variants(bigram).into_iter().flatten() {
            if seen.insert(candidate.clone()) {
                out.push(candidate);
                if out.len() >= MAX_ENTITY_LOOKUP_CANDIDATES {
                    return out;
                }
            }
        }
    }
    out
}

// ── Scoring weights ───────────────────────────────────────────────────────────

/// Weights for the combined memory score: `score = w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringWeights {
    /// Multiplicative boost from salience in `(1 + w_imp × salience)`. Default: 0.2.
    pub salience: f32,
    /// Multiplicative boost from recency in `(1 + w_temp × recency)`. Default: 0.1.
    pub temporal: f32,
    /// Base multiplier applied to relevance. Default: 0.7.
    pub relevance: f32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            salience: 0.2,
            temporal: 0.1,
            relevance: 0.7,
        }
    }
}

// ── ScoringConfig ─────────────────────────────────────────────────────────────

/// Complete, tunable scoring configuration for the memory recall pipeline.
/// DoS caps: max_recall_candidates≤500, default_token_budget≤16000, default_recall_limit≤200.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoringConfig {
    // ── Composite weights ──────────────────────────────────────────────────
    pub weights: ScoringWeights,

    // ── Relevance thresholds ───────────────────────────────────────────────
    /// Minimum raw cosine similarity to include a vector hit. Hits below this
    /// are excluded before RRF fusion. Default: 0.10.
    pub min_raw_relevance: f32,
    /// Minimum RRF score after fusion before normalization. Default: 0.0.
    pub min_rrf_relevance: f32,
    /// Relevance floor for the min-max normalization band. Default: 0.15.
    pub baseline_relevance: f32,

    // ── Temporal decay ─────────────────────────────────────────────────────
    /// Upper cap on per-entry decay_factor before temporal recency calculation.
    /// Default: 0.05.
    pub decay_cap: f32,

    // ── DoS caps (enforced server-side) ───────────────────────────────────
    /// Maximum search candidates to retrieve. Server-side cap: 500. Default: 200.
    pub max_recall_candidates: usize,
    /// Default result limit when caller doesn't specify. Server-side cap: 200.
    /// Default: 10.
    pub default_recall_limit: usize,
    /// Default token budget (tokens). Server-side cap: 16000. Default: 4000.
    pub default_token_budget: usize,
    /// Approximate characters per token (for token budget). Default: 4.
    pub chars_per_token: usize,

    // ── MMR diversity penalty ──────────────────────────────────────────────
    /// Score penalty applied to results whose first `mmr_prefix_len` characters
    /// match a higher-ranked result in pre-penalty composite-score order.
    /// Equal scores use the result ID as the keeper tie-break. Default: 0.1.
    pub mmr_penalty: f32,
    /// Character prefix length used for MMR duplicate detection. Default: 100.
    pub mmr_prefix_len: usize,

    // ── Feature toggles ────────────────────────────────────────────────────
    /// When true, suppress memories whose `properties.supersedes` value matches
    /// the ID of another memory in the result set. Default: true.
    pub enable_supersedes_suppression: bool,
    /// When true, a query with more than 15% CJK characters (`contains_cjk`) skips
    /// document-frequency term selection in the FTS gather path and sends its first
    /// `term_k` terms in query order to the ranked FTS search. The flag is read only
    /// when FTS gather is enabled (`fts_gather.enabled`, off by default) and
    /// `cjk_bypass_ranked` is true. Otherwise the plain FTS search runs unchanged. It
    /// never removes the FTS leg and does not affect vector retrieval or fusion.
    /// Default: true.
    #[serde(alias = "enable_multilingual_routing")]
    pub enable_cjk_fts_bypass: bool,

    // ── Conditional adjustments ────────────────────────────────────────────
    /// Score adjustments applied after the base formula. Default: episodic bonus,
    /// entity boost (semantic age penalty removed 2026-07-21, see `default_adjustments`).
    pub adjustments: Vec<ScoreAdjustment>,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            weights: ScoringWeights::default(),

            min_raw_relevance: 0.10,
            min_rrf_relevance: 0.0,
            baseline_relevance: 0.15,

            decay_cap: 0.05,

            max_recall_candidates: 200,
            default_recall_limit: 10,
            default_token_budget: 4000,
            chars_per_token: 4,

            mmr_penalty: 0.1,
            mmr_prefix_len: 100,

            enable_supersedes_suppression: true,
            enable_cjk_fts_bypass: true,

            adjustments: default_adjustments(),
        }
    }
}

// ── Server-side DoS caps ──────────────────────────────────────────────────────

/// Maximum candidates a caller may request (server-side cap).
pub const MAX_RECALL_CANDIDATES: usize = 500;
/// Maximum token budget a caller may request (server-side cap).
pub const MAX_TOKEN_BUDGET: usize = 16_000;
/// Maximum result limit a caller may request (server-side cap).
pub const MAX_RECALL_LIMIT: usize = 200;

impl ScoringConfig {
    /// Clamp candidate, result, and token budgets to their server-side maximums.
    pub fn apply_dos_caps(&mut self) {
        self.max_recall_candidates = self.max_recall_candidates.min(MAX_RECALL_CANDIDATES);
        self.default_token_budget = self.default_token_budget.min(MAX_TOKEN_BUDGET);
        self.default_recall_limit = self.default_recall_limit.min(MAX_RECALL_LIMIT);
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// Normalize `min_score`: 0–1 passes through, 1–100 divides by 100, others return Err.
pub fn normalize_min_score(score: f64) -> Result<f32, crate::config::MinScoreError> {
    khive_score::UnitScore::from_fraction_or_percent(score)
        .map(|score| score.value() as f32)
        .map_err(|error| match error {
            khive_score::ArithmeticError::NonFinite { .. } => {
                crate::config::MinScoreError::NotFinite
            }
            _ => crate::config::MinScoreError::OutOfRange(score),
        })
}

/// Returns `true` if the query has enough semantic content for meaningful recall.
pub fn is_meaningful_recall_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    let is_alpha_or_cjk = |c: char| c.is_alphabetic() || is_cjk_char(c);

    let meaningful_chars: usize = trimmed.chars().filter(|c| is_alpha_or_cjk(*c)).count();
    if meaningful_chars == 0 {
        return false;
    }

    let cjk_chars: usize = trimmed.chars().filter(|c| is_cjk_char(*c)).count();
    if meaningful_chars < 2 && cjk_chars == 0 {
        return false;
    }

    // Detect repeated-character patterns: "aaaa", "aaa bbb ccc" (gibberish).
    let words: Vec<&str> = trimmed
        .split_whitespace()
        .filter(|w| w.chars().any(is_alpha_or_cjk))
        .collect();
    if !words.is_empty() {
        let all_repeated = words.iter().all(|w| {
            let chars: Vec<char> = w.chars().filter(|c| is_alpha_or_cjk(*c)).collect();
            if chars.len() <= 2 {
                return false;
            }
            let unique: std::collections::HashSet<char> = chars
                .iter()
                .map(|c| c.to_lowercase().next().unwrap_or(*c))
                .collect();
            (unique.len() as f32) / (chars.len() as f32) < 0.4
        });
        if all_repeated {
            return false;
        }
    }

    true
}

/// Deprecated name for [`is_meaningful_recall_query`].
#[deprecated(note = "use is_meaningful_recall_query")]
pub fn is_meaningful_query(query: &str) -> bool {
    is_meaningful_recall_query(query)
}

/// Returns `true` when CJK characters (`is_cjk_char`) outnumber every other letter in `text`.
///
/// Other letters are the alphabetic characters that are not CJK. Digits, whitespace, and
/// punctuation count for neither side, so a tie, a text with no letters, and a text with no CJK
/// characters are all `false`. Recall uses this to decide when an English-only embedding model's
/// vector list is left out of fusion; it is stricter than the 15% share `contains_cjk` tests.
pub(crate) fn is_cjk_dominant_query(text: &str) -> bool {
    let (cjk, other) = text.chars().fold((0usize, 0usize), |(cjk, other), c| {
        if is_cjk_char(c) {
            (cjk + 1, other)
        } else if c.is_alphabetic() {
            (cjk, other + 1)
        } else {
            (cjk, other)
        }
    });
    cjk > other
}

// ── Score normalization ────────────────────────────────────────────────────────

/// Calibrated relevance ceiling for normalized scores.
///
/// Prevents the best candidate from entering the 1.0 saturation zone before
/// temporal, salience, and entity adjustments are applied.
const NORMALIZED_RELEVANCE_CEILING: f32 = 0.82;

/// RRF score threshold above which the best result is considered a genuine
/// relevance signal (~rank 20 in dual-source RRF(k=60)).
const RRF_SIGNAL_THRESHOLD: f32 = 0.025;

/// Normalize raw-cosine or BM25 scores (single-source) into a calibrated relevance band.
pub fn normalize_rank_fusion_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
) -> HashMap<Uuid, f32> {
    normalize_relevance_scores(scores, config, RelevanceSource::Single)
}

/// Normalize RRF-fused scores (dual-source) into a calibrated relevance band.
pub fn normalize_rrf_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
) -> HashMap<Uuid, f32> {
    normalize_relevance_scores(scores, config, RelevanceSource::Fused)
}

#[derive(Clone, Copy)]
enum RelevanceSource {
    Single,
    Fused,
}

fn normalize_relevance_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
    source: RelevanceSource,
) -> HashMap<Uuid, f32> {
    if scores.is_empty() {
        return HashMap::new();
    }
    let min_rrf = config.min_rrf_relevance;
    let filtered: Vec<(Uuid, f32)> = scores
        .into_iter()
        .filter(|(_, score)| score.is_finite() && *score >= min_rrf)
        .collect();
    if filtered.is_empty() {
        return HashMap::new();
    }
    let max_score = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_score.is_finite() || max_score <= 0.0 {
        return HashMap::new();
    }
    let min_score_seen = filtered
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::INFINITY, f32::min);
    let span = max_score - min_score_seen;
    let floor = config
        .baseline_relevance
        .clamp(0.0, NORMALIZED_RELEVANCE_CEILING);
    let range = NORMALIZED_RELEVANCE_CEILING - floor;

    let signal_strength = match source {
        RelevanceSource::Single => Some((max_score / RRF_SIGNAL_THRESHOLD).min(1.0)),
        RelevanceSource::Fused => None,
    };

    filtered
        .into_iter()
        .map(|(id, score)| {
            let calibrated = if span <= f32::EPSILON {
                match source {
                    RelevanceSource::Single => max_score.clamp(floor, NORMALIZED_RELEVANCE_CEILING),
                    RelevanceSource::Fused => floor + range,
                }
            } else {
                let percentile = ((score - min_score_seen) / span).clamp(0.0, 1.0);
                floor + percentile * range
            };
            let normalized = match signal_strength {
                Some(signal) => calibrated * signal,
                None => calibrated,
            };
            (id, normalized)
        })
        .collect()
}

// ── Composite scoring ─────────────────────────────────────────────────────────

/// Input data for `calculate_score` — groups per-candidate fields to stay
/// within clippy's 7-argument limit.
pub struct ScoreInput<'a> {
    pub salience: f32,
    pub memory_type_str: &'a str,
    pub content: &'a str,
    pub created_at_millis: i64,
    pub decay_factor: f32,
    pub now_millis: i64,
    pub relevance_score: f32,
    pub entity_names: &'a [String],
}

#[derive(Clone, Copy)]
enum ScoreFallbackReason {
    TemporalBoost,
    SalienceBoost,
    NonfiniteComposite,
}

impl ScoreFallbackReason {
    fn bit(self) -> u8 {
        match self {
            Self::TemporalBoost => 1,
            Self::SalienceBoost => 2,
            Self::NonfiniteComposite => 4,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::TemporalBoost => "temporal_boost",
            Self::SalienceBoost => "salience_boost",
            Self::NonfiniteComposite => "nonfinite_composite",
        }
    }
}

struct ScoreFallbackWarnings {
    emitted: AtomicU8,
}

impl ScoreFallbackWarnings {
    const fn new() -> Self {
        Self {
            emitted: AtomicU8::new(0),
        }
    }

    fn record(&self, reason: ScoreFallbackReason) {
        if !tracing::enabled!(target: "khive.memory.scoring", tracing::Level::WARN) {
            return;
        }
        let bit = reason.bit();
        if self.emitted.load(Ordering::Relaxed) & bit != 0 {
            return;
        }
        if self.emitted.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            tracing::warn!(
                target: "khive.memory.scoring",
                reason = reason.as_str(),
                "Memory score arithmetic fallback returned zero"
            );
        }
    }
}

static SCORE_FALLBACK_WARNINGS: ScoreFallbackWarnings = ScoreFallbackWarnings::new();

/// Composite score for a single memory candidate.
///
/// Formula (semantic-gate model, multiplicative):
///   `score = w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`
///
/// Then each `ScoreAdjustment` in `config.adjustments` is evaluated and applied in order.
/// Non-finite results fall to zero; finite results are clamped to `[0, 1]`.
/// Arithmetic fallbacks emit at most one WARN per reason per process while WARN
/// is enabled; genuine finite zero scores do not emit fallback warnings.
pub fn calculate_score(input: &ScoreInput<'_>, config: &ScoringConfig) -> f32 {
    calculate_score_with_reporter(input, config, &SCORE_FALLBACK_WARNINGS)
}

fn calculate_score_with_reporter(
    input: &ScoreInput<'_>,
    config: &ScoringConfig,
    reporter: &ScoreFallbackWarnings,
) -> f32 {
    let w = &config.weights;
    let semantic_base = w.relevance * input.relevance_score;

    let time_diff_days = ((input.now_millis - input.created_at_millis) as f32
        / (24.0 * 60.0 * 60.0 * 1000.0))
        .max(0.0);

    let capped_decay = input.decay_factor.min(config.decay_cap);
    let temporal_recency = (-capped_decay * time_diff_days).exp();

    let Ok(temporal_boost) = khive_score::boost_factor_f32(w.temporal, temporal_recency) else {
        reporter.record(ScoreFallbackReason::TemporalBoost);
        return 0.0;
    };
    let Ok(salience_boost) = khive_score::boost_factor_f32(w.salience, input.salience) else {
        reporter.record(ScoreFallbackReason::SalienceBoost);
        return 0.0;
    };

    let mut score = semantic_base * temporal_boost * salience_boost;

    let ctx = CandidateContext {
        memory_type: input.memory_type_str,
        age_days: time_diff_days,
        salience: input.salience,
        content: input.content,
        entity_names: input.entity_names,
    };

    for adj in &config.adjustments {
        score = adj.apply(score, &ctx);
    }

    // Individually finite adjustments can overflow before a later operation
    // (for example, +Inf * 0) produces NaN. Keep the rank score numeric.
    if !score.is_finite() {
        reporter.record(ScoreFallbackReason::NonfiniteComposite);
        return 0.0;
    }
    score.clamp(0.0, 1.0)
}

// ── ADR-104 §2 (Stage B): bounded per-entity posterior term ───────────────────

/// Default weight for the per-entity posterior term. Chosen so the clamp
/// bounds below are exactly reachable at posterior means of 0.0 and 1.0
/// (`1 + 0.3 * (0.0 - 0.5) = 0.85`, `1 + 0.3 * (1.0 - 0.5) = 1.15`).
pub const ENTITY_POSTERIOR_WEIGHT: f32 = 0.3;

/// Lower bound of the per-entity posterior multiplier — the term can never
/// move a score down by more than 15%.
pub const ENTITY_POSTERIOR_CLAMP_MIN: f32 = 0.85;

/// Upper bound of the per-entity posterior multiplier — the term can never
/// move a score up by more than 15%.
pub const ENTITY_POSTERIOR_CLAMP_MAX: f32 = 1.15;

/// `clamp(1 + w_ent * (entity_posterior_mean - 0.5), 0.85, 1.15)`.
///
/// `None` (no posterior for this candidate beyond the uninformative prior)
/// is neutral: exactly `1.0`, not the midpoint of the clamp band. Untouched
/// memories must be unaffected — the neutral value has to be an identity
/// multiplier, not a value that happens to fall inside the bounds.
pub fn entity_posterior_term(entity_posterior_mean: Option<f64>, w_ent: f32) -> f32 {
    match entity_posterior_mean {
        Some(mean) => (1.0 + w_ent * (mean as f32 - 0.5))
            .clamp(ENTITY_POSTERIOR_CLAMP_MIN, ENTITY_POSTERIOR_CLAMP_MAX),
        None => 1.0,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "entity_loop_3711_tests.rs"]
mod loop_3711_tests;

#[cfg(test)]
#[path = "scoring_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "scoring_normalization_tests.rs"]
mod normalization_tests;

#[cfg(test)]
#[path = "scoring/fallback_warning_tests.rs"]
mod fallback_warning_tests;
