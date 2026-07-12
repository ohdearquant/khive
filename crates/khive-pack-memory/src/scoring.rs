//! Composite memory scoring — ported from the archived internal service (v1 archive).
//!
//! Provides a fully-tunable scoring pipeline:
//!   1. `ScoringConfig`  — all knobs, all `pub`, all serde-friendly for agent sweeps.
//!   2. `calculate_score` — multiplicative formula: `w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`.
//!   3. `ScoreAdjustment` — declarative conditional rules applied after the base formula.
//!   4. `normalize_rrf_scores` / `normalize_rank_fusion_scores` — RRF and raw-cosine normalization.
//!   5. `normalize_min_score` — dual-scale input (0.0–1.0 fraction or 0–100 integer).
//!   6. `is_meaningful_query` — noise gate before embedding compute.
//!   7. `contains_cjk` — CJK routing decision.
//!   8. `needs_multilingual` — broad multilingual routing gate (non-ASCII alphabetic script).
// FILE SIZE JUSTIFICATION: scoring.rs bundles ScoringConfig, all normalization helpers,
// CJK routing, and the full test suite for the scoring pipeline. The tests require access
// to module-private helpers; splitting would require pub(crate) promotion of private fns.

use std::collections::{HashMap, HashSet};

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

/// Returns `true` when `needle` occurs in `haystack` at a **word (character)
/// boundary**. The character immediately before the match (if any) and the
/// character immediately after the match (if any) are both non-alphanumeric
/// (or the match sits at the start/end of `haystack`).
///
/// Plain substring `contains` matches inside unrelated words: a candidate
/// `"beta"` matches `"alphabet"` and `"betamax"`; `"car"` matches
/// `"scarcity"`. Anchoring to boundaries closes that class of false positive
/// while still matching multi-word phrases on their own boundaries. A
/// caller-supplied explicit name like `"knowledge graph"` still matches
/// `"...the knowledge graph shows..."` because the space characters on
/// either side of the phrase are themselves non-alphanumeric, satisfying the
/// boundary check without any special-casing for internal spaces.
///
/// Both `haystack` and `needle` are expected pre-lowercased by the caller
/// (matching `EntityMatch`'s existing lowercase-both-sides contract).
fn contains_at_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.chars().all(is_cjk_char) {
        return haystack.contains(needle);
    }
    let haystack_chars: Vec<char> = haystack.chars().collect();
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
                let lower = ctx.content.to_lowercase();
                ctx.entity_names
                    .iter()
                    .any(|e| contains_at_word_boundary(&lower, e))
            }
            Self::EntityMiss => {
                if ctx.entity_names.is_empty() {
                    return false;
                }
                let lower = ctx.content.to_lowercase();
                !ctx.entity_names
                    .iter()
                    .any(|e| contains_at_word_boundary(&lower, e))
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

/// Default score adjustments: episodic recency bonus, semantic age penalty, entity boost.
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
        // Semantic age penalty: old high-salience semantic memories get penalized
        // to prevent reference docs from crowding out episodic content.
        ScoreAdjustment {
            condition: AdjustmentCondition::All {
                conditions: vec![
                    AdjustmentCondition::MemoryType {
                        kind: "semantic".into(),
                    },
                    AdjustmentCondition::AgeRange {
                        min_days: Some(30.0),
                        max_days: None,
                    },
                    AdjustmentCondition::SalienceRange {
                        min: Some(0.85),
                        max: None,
                    },
                ],
            },
            operation: AdjustmentOp::Subtract { value: 0.05 },
        },
        // Entity match boost: memories mentioning queried entities get boosted.
        ScoreAdjustment {
            condition: AdjustmentCondition::EntityMatch,
            operation: AdjustmentOp::Multiply { factor: 1.3 },
        },
    ]
}

// ── Auto entity-name extraction ─────────────────────────────────────────────────

/// A small closed set of common English function words, used to keep
/// auto-extracted entity candidates low-noise. Deliberately conservative
/// (articles, prepositions, pronouns, conjunctions, common auxiliary verbs
/// only) — content words are intentionally left out of this list, since
/// distinguishing a proper noun from a common noun without an NER model
/// is exactly what the capitalization / length rules below are for.
const ENTITY_STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "in", "on", "at", "to", "for", "with", "by",
    "from", "is", "are", "was", "were", "be", "been", "being", "this", "that", "these", "those",
    "it", "its", "he", "she", "his", "her", "him", "they", "them", "their", "we", "us", "our",
    "you", "your", "i", "my", "me", "as", "if", "so", "not", "no", "yes", "do", "does", "did",
    "has", "have", "had", "will", "would", "can", "could", "should", "may", "might", "about",
    "into", "than", "then", "there", "here", "what", "which", "who", "whom", "when", "where",
    "why", "how",
];

/// Maximum number of auto-extracted entity-name candidates per query.
/// `EntityMatch` (see `default_adjustments` above) applies its multiplier to
/// *any* candidate memory that contains *any* one of the extracted names —
/// this constant does not limit how many memories can receive the boost
/// (one generic name can still match many memories). It only bounds the
/// extracted-name list length and, transitively, the number of
/// `contains_at_word_boundary` scans `EntityMatch` performs per note.
pub const MAX_AUTO_ENTITY_NAMES: usize = 8;

/// Strip leading/trailing non-alphanumeric characters from a token (quotes,
/// commas, trailing periods, etc.), keeping internal punctuation like
/// apostrophes in a name intact.
fn strip_token_punctuation(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Auto-extract entity-name candidates from a recall query when the caller
/// does not supply `entity_names` explicitly.
///
/// Context (khive #dead-parameter defect): `entity_names` was a
/// caller-supplied request field that fed `EntityMatch` — but no caller ever
/// populated it, so the ×1.3 boost in `default_adjustments` never fired in
/// practice. This function derives candidates server-side from the query
/// text so the boost has something to match against.
///
/// **Capitalized tokens only.** A query token qualifies iff, after stripping
/// surrounding punctuation, it is not a stopword and starts with an
/// uppercase letter. Capitalization is the only signal used here because it
/// is the sole low-noise proper-noun indicator available without an NER
/// model or a lookup against known entity records — `EntityMatch::matches`
/// (above) does a free-text boundary-anchored match against raw memory
/// content, not something anchored to actual KG entity references, so
/// admitting ordinary lowercase content words as candidates degenerates the
/// boost into a second, redundant lexical-overlap signal on top of
/// retrieval-stage relevance (confirmed by review: realistic score inputs
/// clamp at the `[0, 1]` ceiling and flatten top-rank ordering when generic
/// query words are treated as entity candidates).
///
/// **Queries with no capitalization extract nothing.** Many recall callers —
/// agents in particular — pass fully lowercase queries, and this function
/// deliberately returns an empty list for them rather than guessing at
/// content words. Covering that case precisely requires anchoring candidates
/// against known entity records (e.g. resolving query tokens against the KG)
/// rather than lexical heuristics over the query string; that is out of
/// scope here.
///
/// Returned names are lowercased (matching `EntityMatch`'s own lowercasing
/// of both sides before the boundary-anchored match), deduplicated
/// preserving first-seen order, and capped at `MAX_AUTO_ENTITY_NAMES`.
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

/// Maximum number of candidate strings a single recall sends to the batched
/// entity-name lookup
/// (ADR-104 §5 / Stage C, rider R1). Bounds the `LOWER(name) IN (...)`
/// placeholder list built in `khive-db`'s `build_entity_where`, independent
/// of `MAX_AUTO_ENTITY_NAMES`, which bounds the unrelated capitalized-token
/// fallback list above.
pub const MAX_ENTITY_LOOKUP_CANDIDATES: usize = 64;

const MAX_BIGRAM_LOOKUP_CANDIDATES: usize = MAX_ENTITY_LOOKUP_CANDIDATES / 4;
const MIN_CJK_LOOKUP_CHARS: usize = 2;
const MAX_CJK_LOOKUP_CHARS: usize = 8;

/// Build the candidate strings a recall query offers to the entity-anchored
/// lookup (ADR-104 §5 / Stage C). Alphabetic-script queries contribute raw and
/// ASCII-lowercased non-stopword unigrams and reserve one quarter of the cap
/// for adjacent-token bigrams. CJK substrings reserve a fair quota for every
/// supported length from 2 through 8, redistributing unused quota from short
/// runs. Within each length, available start positions are sampled evenly.
/// Quotas greater than one guarantee both the first and final valid starts;
/// a quota of one selects the first endpoint. The result is both length-fair
/// and position-fair under the 64-candidate cap. All candidates are deduplicated.
///
/// Unlike `extract_entity_candidates` above, this does **not** filter on
/// capitalization, lowercase queries are the whole point of this extension.
/// The precision-safety property instead comes from the caller (the
/// `memory.recall` handler) only keeping a candidate that matches the *name*
/// of a real KG entity, via one batched `EntityFilter::names_ci` lookup.
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
        for candidate in [bigram.clone(), bigram.to_ascii_lowercase()] {
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
        for candidate in [token.clone(), token.to_ascii_lowercase()] {
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
        for candidate in [bigram.clone(), bigram.to_ascii_lowercase()] {
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
    /// are excluded before RRF fusion (#2272). Default: 0.10.
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
    /// match an earlier result. Default: 0.1.
    pub mmr_penalty: f32,
    /// Character prefix length used for MMR duplicate detection. Default: 100.
    pub mmr_prefix_len: usize,

    // ── Feature toggles ────────────────────────────────────────────────────
    /// When true, suppress memories whose `properties.supersedes` value matches
    /// the ID of another memory in the result set. Default: true.
    pub enable_supersedes_suppression: bool,
    /// When true and a multilingual embedding model is registered, route
    /// non-ASCII-script queries (CJK, Cyrillic, Arabic, accented-Latin, …) to
    /// it as the primary dense model. Default: true.
    pub enable_multilingual_routing: bool,
    /// Name of the multilingual embedding model to prefer for dense routing.
    /// When None, the handler checks registered model names for substrings
    /// "multilingual" or "paraphrase". Default: None.
    pub multilingual_model: Option<String>,

    // ── Conditional adjustments ────────────────────────────────────────────
    /// Score adjustments applied after the base formula. Default: episodic bonus,
    /// semantic age penalty, entity boost.
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
            enable_multilingual_routing: true,
            multilingual_model: None,

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
    /// Clamp all DoS-cap fields to their server-side maximums.
    ///
    /// Called at the start of `handle_recall` so callers cannot trigger
    /// unbounded candidate retrieval or token budget consumption.
    pub fn apply_dos_caps(&mut self) {
        self.max_recall_candidates = self.max_recall_candidates.min(MAX_RECALL_CANDIDATES);
        self.default_token_budget = self.default_token_budget.min(MAX_TOKEN_BUDGET);
        self.default_recall_limit = self.default_recall_limit.min(MAX_RECALL_LIMIT);
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// Returns `true` if `c` is a CJK character (Unified, Extension A/B, Hiragana,
/// Katakana, Hangul).
#[inline]
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'       // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'     // CJK Extension A
        | '\u{F900}'..='\u{FAFF}'     // CJK Compatibility Ideographs
        | '\u{3040}'..='\u{309F}'     // Hiragana
        | '\u{30A0}'..='\u{30FF}'     // Katakana
        | '\u{20000}'..='\u{2A6DF}'   // CJK Extension B
        | '\u{AC00}'..='\u{D7AF}'     // Hangul Syllables
    )
}

/// Returns `true` when >15% of the query's characters are CJK.
pub fn contains_cjk(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    let cjk = chars.iter().filter(|&&c| is_cjk_char(c)).count();
    (cjk as f32) / (chars.len() as f32) > 0.15
}

/// Returns `true` when `text` should be routed to the multilingual embedding model
/// for dense retrieval.
///
/// Triggers when >15% of **alphabetic** characters are non-ASCII-alphabetic.
/// Denominator is alphabetic-character count (not total characters), so punctuation,
/// digits, and whitespace do not dilute the signal. This means `Müller` and
/// `Müller?` and `Müller!!!` all route identically.
///
/// Covers: CJK, Cyrillic, Arabic, Devanagari, Hebrew, Thai, accented-Latin
/// (é, ü, ñ, …) and any other non-ASCII script Unicode recognises as alphabetic,
/// without introducing new crate dependencies.
///
/// **Known limitation**: ASCII-only non-English Latin (`bonjour le monde`,
/// `como estas`, `ich suche einen buchhalter`) is NOT detected and routes to the
/// primary model. Real language detection for that case requires a dedicated crate
/// and is tracked as a follow-up to issue #101.
pub fn needs_multilingual(text: &str) -> bool {
    let alpha_chars: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if alpha_chars.is_empty() {
        return false;
    }
    let non_ascii_alpha = alpha_chars
        .iter()
        .filter(|&&c| !c.is_ascii_alphabetic())
        .count();
    (non_ascii_alpha as f32) / (alpha_chars.len() as f32) > 0.15
}

/// Normalize `min_score`: 0–1 passes through, 1–100 divides by 100, others return Err.
pub fn normalize_min_score(score: f64) -> Result<f32, crate::config::MinScoreError> {
    if !score.is_finite() {
        return Err(crate::config::MinScoreError::NotFinite);
    }
    if (0.0..=1.0).contains(&score) {
        return Ok(score as f32);
    }
    if (1.0..=100.0).contains(&score) {
        return Ok((score / 100.0) as f32);
    }
    Err(crate::config::MinScoreError::OutOfRange(score))
}

/// Returns `true` if the query has enough semantic content for meaningful recall.
pub fn is_meaningful_query(query: &str) -> bool {
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

    let signal_strength = (max_score / RRF_SIGNAL_THRESHOLD).min(1.0);

    filtered
        .into_iter()
        .map(|(id, score)| {
            let calibrated = if span <= f32::EPSILON {
                max_score.clamp(floor, NORMALIZED_RELEVANCE_CEILING)
            } else {
                let percentile = ((score - min_score_seen) / span).clamp(0.0, 1.0);
                floor + percentile * range
            };
            (id, calibrated * signal_strength)
        })
        .collect()
}

/// Normalize RRF-fused scores (dual-source) into a calibrated relevance band.
pub fn normalize_rrf_scores(
    scores: Vec<(Uuid, f32)>,
    config: &ScoringConfig,
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

    filtered
        .into_iter()
        .map(|(id, score)| {
            let calibrated = if span <= f32::EPSILON {
                floor + range
            } else {
                let percentile = ((score - min_score_seen) / span).clamp(0.0, 1.0);
                floor + percentile * range
            };
            (id, calibrated)
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

/// Composite score for a single memory candidate.
///
/// Formula (semantic-gate model, multiplicative):
///   `score = w_rel × relevance × (1 + w_temp × recency) × (1 + w_imp × salience)`
///
/// Then each `ScoreAdjustment` in `config.adjustments` is evaluated and applied in order.
/// Result is clamped to `[0, 1]`.
pub fn calculate_score(input: &ScoreInput<'_>, config: &ScoringConfig) -> f32 {
    let w = &config.weights;
    let semantic_base = w.relevance * input.relevance_score;

    let time_diff_days = ((input.now_millis - input.created_at_millis) as f32
        / (24.0 * 60.0 * 60.0 * 1000.0))
        .max(0.0);

    let capped_decay = input.decay_factor.min(config.decay_cap);
    let temporal_recency = (-capped_decay * time_diff_days).exp();

    let temporal_boost = 1.0 + w.temporal * temporal_recency;
    let salience_boost = 1.0 + w.salience * input.salience;

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
mod tests {
    use super::*;

    #[test]
    fn is_meaningful_query_rejects_empty() {
        assert!(!is_meaningful_query(""));
        assert!(!is_meaningful_query("   "));
    }

    #[test]
    fn is_meaningful_query_rejects_symbols_only() {
        assert!(!is_meaningful_query("!@#$%"));
        assert!(!is_meaningful_query("..."));
    }

    #[test]
    fn is_meaningful_query_rejects_single_latin_char() {
        assert!(!is_meaningful_query("a"));
        assert!(!is_meaningful_query("Z"));
    }

    #[test]
    fn is_meaningful_query_rejects_repeated_gibberish() {
        assert!(!is_meaningful_query("aaaa bbbb cccc"));
    }

    #[test]
    fn is_meaningful_query_accepts_normal_queries() {
        assert!(is_meaningful_query("what is the capital of France"));
        assert!(is_meaningful_query("rust async runtime"));
        assert!(is_meaningful_query("hello"));
    }

    #[test]
    fn contains_cjk_detects_chinese() {
        // Purely CJK — well above 15% threshold.
        assert!(contains_cjk("你好世界"));
        // Mixed string: "世界" = 2 CJK out of 6 chars = 33%, above threshold.
        assert!(contains_cjk("世界 hi"));
        // Mostly-Latin with 2 CJK chars out of 15 total = 13% → below threshold.
        assert!(!contains_cjk("hello 世界 world"));
    }

    #[test]
    fn contains_cjk_ignores_latin() {
        assert!(!contains_cjk("hello world"));
        assert!(!contains_cjk(""));
    }

    #[test]
    fn normalize_min_score_fraction_passthrough() {
        let v = normalize_min_score(0.5).unwrap();
        assert!((v - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn normalize_min_score_percent_form() {
        let v = normalize_min_score(50.0).unwrap();
        assert!((v - 0.5f32).abs() < 1e-6);
    }

    #[test]
    fn normalize_min_score_rejects_out_of_range() {
        assert!(normalize_min_score(200.0).is_err());
        assert!(normalize_min_score(-1.0).is_err());
        assert!(normalize_min_score(f64::NAN).is_err());
    }

    #[test]
    fn calculate_score_returns_unit_interval() {
        let config = ScoringConfig::default();
        let score = calculate_score(
            &ScoreInput {
                salience: 0.9,
                memory_type_str: "episodic",
                content: "test content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: 1000,
                relevance_score: 0.8,
                entity_names: &[],
            },
            &config,
        );
        assert!((0.0..=1.0).contains(&score), "score {score} out of [0,1]");
    }

    #[test]
    fn calculate_score_high_salience_ranks_higher() {
        let config = ScoringConfig {
            adjustments: vec![],
            ..ScoringConfig::default()
        };
        let now_ms = 1_000_000i64;
        let score_high = calculate_score(
            &ScoreInput {
                salience: 0.9,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.7,
                entity_names: &[],
            },
            &config,
        );
        let score_low = calculate_score(
            &ScoreInput {
                salience: 0.1,
                memory_type_str: "episodic",
                content: "content",
                created_at_millis: 0,
                decay_factor: 0.01,
                now_millis: now_ms,
                relevance_score: 0.7,
                entity_names: &[],
            },
            &config,
        );
        assert!(score_high > score_low, "high salience should rank higher");
    }

    #[test]
    fn calculate_score_entity_match_from_auto_extraction_lifts_equal_relevance_candidate() {
        // Wires `extract_entity_candidates` straight into `calculate_score` with
        // the default adjustments (EntityMatch ×1.3 included) — everything else
        // held identical between the two candidates — to isolate exactly what
        // the entity boost contributes, decoupled from retrieval-stage relevance
        // differences (which the handler-level tests in recall.rs cover instead).
        let config = ScoringConfig::default();
        let now_ms = 1_000_000i64;

        // Capitalized-signal query so extraction yields a single isolated
        // candidate ("gamma") — that keeps this test's two candidates
        // differing by exactly one factor (does the content mention the
        // named entity or not).
        let query = "sibling transfer to Gamma university";
        let entity_names = extract_entity_candidates(query);
        assert_eq!(
            entity_names,
            vec!["gamma"],
            "capitalized-signal query must extract exactly the one proper noun: {entity_names:?}"
        );

        let matching_score = calculate_score(
            &ScoreInput {
                salience: 0.5,
                memory_type_str: "semantic",
                content: "sibling transferred to gamma university this fall",
                created_at_millis: 0,
                decay_factor: 0.005,
                now_millis: now_ms,
                relevance_score: 0.5,
                entity_names: &entity_names,
            },
            &config,
        );
        let non_matching_score = calculate_score(
            &ScoreInput {
                salience: 0.5,
                memory_type_str: "semantic",
                content: "sibling transferred to a different university this fall",
                created_at_millis: 0,
                decay_factor: 0.005,
                now_millis: now_ms,
                relevance_score: 0.5,
                entity_names: &entity_names,
            },
            &config,
        );

        assert!(
            matching_score > non_matching_score,
            "memory matching an auto-extracted entity name must outrank an \
             equal-relevance memory that doesn't: matching={matching_score} \
             non_matching={non_matching_score}"
        );
        // The EntityMatch adjustment is a flat ×1.3 multiply applied after the
        // base formula and the other (inactive, for this age/salience combo)
        // adjustments — assert the lift is approximately that factor, not just
        // "higher", so a future change to the adjustment order/weights is caught.
        let ratio = matching_score / non_matching_score;
        assert!(
            (ratio - 1.3).abs() < 0.01,
            "expected ~1.3x lift from the EntityMatch adjustment, got ratio {ratio}"
        );
    }

    #[test]
    fn dos_caps_enforce_limits() {
        let mut config = ScoringConfig {
            max_recall_candidates: 9999,
            default_token_budget: 99999,
            default_recall_limit: 9999,
            ..ScoringConfig::default()
        };
        config.apply_dos_caps();
        assert_eq!(config.max_recall_candidates, MAX_RECALL_CANDIDATES);
        assert_eq!(config.default_token_budget, MAX_TOKEN_BUDGET);
        assert_eq!(config.default_recall_limit, MAX_RECALL_LIMIT);
    }

    #[test]
    fn normalize_rrf_scores_preserves_ordering() {
        let config = ScoringConfig::default();
        let input = vec![
            (Uuid::new_v4(), 0.030f32),
            (Uuid::new_v4(), 0.020f32),
            (Uuid::new_v4(), 0.015f32),
        ];
        let ids: Vec<Uuid> = input.iter().map(|(id, _)| *id).collect();
        let output = normalize_rrf_scores(input, &config);
        let s0 = output[&ids[0]];
        let s1 = output[&ids[1]];
        let s2 = output[&ids[2]];
        assert!(s0 > s1 && s1 > s2, "ordering must be preserved");
        assert!(s0 <= 1.0 && s2 >= 0.0, "scores must be in [0,1]");
    }

    // ── needs_multilingual ────────────────────────────────────────────────────

    #[test]
    fn needs_multilingual_ascii_english_routes_primary() {
        assert!(!needs_multilingual("hello world"));
        assert!(!needs_multilingual("rust programming language"));
        assert!(!needs_multilingual(""));
        assert!(!needs_multilingual("42 items in the list"));
    }

    #[test]
    fn needs_multilingual_cjk_routes_multilingual() {
        // Pure CJK: 4/4 = 100% non-ASCII alpha → well above 15%.
        assert!(needs_multilingual("你好世界"));
        // Mixed: 2 CJK out of 5 chars = 40%.
        assert!(needs_multilingual("abc你好"));
    }

    #[test]
    fn needs_multilingual_cyrillic_routes_multilingual() {
        // "Привет мир" (Hello world in Russian): all Cyrillic alphabetic.
        assert!(needs_multilingual("Привет мир"));
        // Single Cyrillic word mixed with Latin — Г alone is 1/7 ≈ 14% (below threshold).
        // Verify with a word that has enough Cyrillic chars to cross 15%.
        assert!(needs_multilingual("Привет hello")); // Привет = 6 non-ASCII alpha, total chars including space = 12 → 6/12 = 50%
    }

    #[test]
    fn needs_multilingual_arabic_routes_multilingual() {
        // "مرحبا" (Hello in Arabic): all Arabic alphabetic.
        assert!(needs_multilingual("مرحبا بالعالم"));
    }

    #[test]
    fn needs_multilingual_devanagari_routes_multilingual() {
        // "नमस्ते" (Namaste in Hindi/Devanagari).
        assert!(needs_multilingual("नमस्ते दुनिया"));
    }

    #[test]
    fn needs_multilingual_accented_latin_routes_multilingual() {
        // French: "café résumé" — é is alphabetic and non-ASCII.
        // Alphabetic chars: c,a,f,é,r,é,s,u,m,é = 10; non-ASCII alpha: 3; 3/10 = 30% > 15%.
        assert!(needs_multilingual("café résumé"));
        // German: "Müller" — ü is 1/6 alpha = 16.7% > 15%.
        assert!(needs_multilingual("Müller"));
        // Spanish: "niño" — ñ is 1/4 alpha = 25% > 15%.
        assert!(needs_multilingual("niño"));
    }

    #[test]
    fn needs_multilingual_alphabetic_denominator_is_stable_under_punctuation() {
        // INVARIANT 2: punctuation/digits must not change routing decision.
        // "Müller" alpha: M,ü,l,l,e,r = 6; non-ASCII alpha: ü = 1; 1/6 = 16.7% > 15%.
        assert!(needs_multilingual("Müller"));
        // "Müller?" — same 6 alpha chars; ? is not alphabetic; 1/6 = 16.7% → still routes.
        assert!(needs_multilingual("Müller?"));
        // "Müller!!!" — same 6 alpha chars; 1/6 = 16.7% → still routes.
        assert!(needs_multilingual("Müller!!!"));
        // Digit-heavy short accented query: "42 Ü" — alpha: Ü = 1; non-ASCII: 1; 1/1 = 100%.
        assert!(needs_multilingual("42 Ü"));
        // Empty / no-alpha: must return false without divide-by-zero.
        assert!(!needs_multilingual(""));
        assert!(!needs_multilingual("42 100 ???"));
    }

    #[test]
    fn needs_multilingual_pure_ascii_accented_below_threshold_routes_primary() {
        // "hello naïve" — ï is 1 non-ASCII alpha out of 10 alpha = 10% < 15% → primary.
        // (Deliberate design decision: a single diacritic in an otherwise English
        // sentence does not warrant multilingual routing.)
        assert!(!needs_multilingual("hello naive")); // no diacritics, definitely primary
                                                     // "über" — ü is 1/4 alpha = 25% → routes multilingual (German word).
        assert!(needs_multilingual("über"));
    }

    #[test]
    fn needs_multilingual_ascii_only_non_english_latin_routes_primary_known_limitation() {
        // INVARIANT 4: ASCII-only non-English Latin is NOT detected here.
        // Real language detection is tracked as a follow-up to issue #101.
        assert!(!needs_multilingual("bonjour le monde"));
        assert!(!needs_multilingual("como estas"));
        assert!(!needs_multilingual("ich suche einen buchhalter"));
    }

    // ── extract_entity_candidates ─────────────────────────────────────────────

    #[test]
    fn extract_entity_candidates_capitalized_tokens_only() {
        // Only capitalized tokens qualify; lowercase content words
        // ("sibling", "college") are excluded even though they're not
        // stopwords.
        let out = extract_entity_candidates("Alex sibling college Acme Beta Gamma");
        assert_eq!(out, vec!["alex", "acme", "beta", "gamma"]);
    }

    #[test]
    fn extract_entity_candidates_all_lowercase_query_extracts_nothing() {
        // DESIGN RULING: the lowercase fallback was removed after review —
        // it degenerated EntityMatch into a second lexical-overlap reward on
        // top of retrieval-stage relevance. An all-lowercase query (however
        // topical) must extract zero candidates.
        let out = extract_entity_candidates("alex sibling college acme beta gamma");
        assert!(out.is_empty());
    }

    #[test]
    fn extract_entity_candidates_strips_stopwords() {
        let out = extract_entity_candidates("what is the capital of France");
        // "what", "is", "the", "of" are stopwords; "capital" is lowercase (excluded);
        // "France" is capitalized and not a stopword, so it remains.
        assert_eq!(out, vec!["france"]);
    }

    #[test]
    fn extract_entity_candidates_strips_punctuation() {
        let out = extract_entity_candidates("Alex's sibling, Acme!");
        assert_eq!(out, vec!["alex's", "acme"]);
    }

    #[test]
    fn extract_entity_candidates_caps_at_max_auto_entity_names() {
        let query = "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliet";
        let out = extract_entity_candidates(query);
        assert_eq!(out.len(), MAX_AUTO_ENTITY_NAMES);
        assert_eq!(
            out,
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
        );
    }

    #[test]
    fn extract_entity_candidates_dedupes_case_insensitively() {
        let out = extract_entity_candidates("Acme ACME college");
        // capitalization signal present (Acme, ACME) → only capitalized tokens
        // qualify; "college" is dropped; Acme and ACME collapse to one entry.
        assert_eq!(out, vec!["acme"]);
    }

    #[test]
    fn extract_entity_candidates_empty_query_returns_empty() {
        assert!(extract_entity_candidates("").is_empty());
        assert!(extract_entity_candidates("   ").is_empty());
    }

    #[test]
    fn extract_entity_candidates_all_stopwords_returns_empty() {
        assert!(extract_entity_candidates("is the a of").is_empty());
    }

    #[test]
    fn entity_lookup_candidates_is_length_and_position_fair_for_long_cjk_run() {
        let run: String = (0..65)
            .map(|offset| char::from_u32(0x4e00 + offset).expect("valid CJK character"))
            .collect();
        let candidates = entity_lookup_candidates(&run);

        assert_eq!(candidates.len(), MAX_ENTITY_LOOKUP_CANDIDATES);
        for len in MIN_CJK_LOOKUP_CHARS..=MAX_CJK_LOOKUP_CHARS {
            let expected_quota = MAX_ENTITY_LOOKUP_CANDIDATES
                / (MAX_CJK_LOOKUP_CHARS - MIN_CJK_LOOKUP_CHARS + 1)
                + usize::from(len == MIN_CJK_LOOKUP_CHARS);
            assert_eq!(
                candidates
                    .iter()
                    .filter(|candidate| candidate.chars().count() == len)
                    .count(),
                expected_quota,
                "candidate set must reserve the exact quota for length {len}"
            );

            let chars: Vec<char> = run.chars().collect();
            let has_late_candidate = (chars.len() - 10..=chars.len() - len).any(|start| {
                let expected: String = chars[start..start + len].iter().collect();
                candidates.contains(&expected)
            });
            assert!(
                has_late_candidate,
                "length {len} must include a candidate starting in the final 10 positions"
            );
        }
    }

    // ── EntityMatch word-boundary matching ──────────────────────────────────────

    fn entity_match_ctx<'a>(content: &'a str, entity_names: &'a [String]) -> CandidateContext<'a> {
        CandidateContext {
            memory_type: "episodic",
            age_days: 0.0,
            salience: 0.5,
            content,
            entity_names,
        }
    }

    #[test]
    fn contains_at_word_boundary_rejects_substring_inside_another_word() {
        assert!(!contains_at_word_boundary("alphabet soup", "beta"));
        assert!(!contains_at_word_boundary("buy a betamax player", "beta"));
        assert!(!contains_at_word_boundary(
            "water scarcity is rising",
            "car"
        ));
    }

    #[test]
    fn contains_at_word_boundary_accepts_the_word_itself() {
        assert!(contains_at_word_boundary("beta release notes", "beta"));
        assert!(contains_at_word_boundary("drove the car home", "car"));
        // start/end of string edges (no adjacent char to fail the boundary check).
        assert!(contains_at_word_boundary("beta", "beta"));
        assert!(contains_at_word_boundary("notes beta", "beta"));
    }

    #[test]
    fn contains_at_word_boundary_accepts_multi_word_phrase() {
        // Internal spaces in a multi-word candidate are themselves
        // non-alphanumeric, so the phrase's own boundaries (not each word's)
        // are what the check anchors on.
        assert!(contains_at_word_boundary(
            "the knowledge graph shows this",
            "knowledge graph"
        ));
        assert!(!contains_at_word_boundary(
            "prior knowledge, graphical view",
            "knowledge graph"
        ));
    }

    #[test]
    fn entity_match_condition_rejects_substring_false_positives() {
        let entity_names = vec!["beta".to_string()];
        let ctx = entity_match_ctx("alphabet soup for dinner", &entity_names);
        assert!(
            !AdjustmentCondition::EntityMatch.matches(&ctx),
            "\"beta\" must not match inside \"alphabet\""
        );

        let entity_names = vec!["car".to_string()];
        let ctx = entity_match_ctx("water scarcity is rising", &entity_names);
        assert!(
            !AdjustmentCondition::EntityMatch.matches(&ctx),
            "\"car\" must not match inside \"scarcity\""
        );
    }

    #[test]
    fn entity_match_condition_matches_multi_word_explicit_name() {
        let entity_names = vec!["knowledge graph".to_string()];
        let ctx = entity_match_ctx("notes on the knowledge graph design", &entity_names);
        assert!(
            AdjustmentCondition::EntityMatch.matches(&ctx),
            "explicit multi-word entity name must still match on its own boundaries"
        );
    }

    #[test]
    fn entity_match_condition_still_matches_real_word_occurrence() {
        let entity_names = vec!["beta".to_string()];
        let ctx = entity_match_ctx("the beta release ships tomorrow", &entity_names);
        assert!(
            AdjustmentCondition::EntityMatch.matches(&ctx),
            "boundary anchoring must not break matching the actual word"
        );
    }

    #[test]
    fn entity_match_condition_matches_contiguous_cjk_with_cjk_on_both_sides() {
        let entity_names = vec!["北京大学".to_string()];
        let ctx = entity_match_ctx("我在北京大学学习", &entity_names);
        assert!(AdjustmentCondition::EntityMatch.matches(&ctx));
    }

    #[test]
    fn entity_match_condition_keeps_alphabetic_word_boundaries() {
        let entity_names = vec!["rust".to_string()];
        let ctx = entity_match_ctx("trust requires evidence", &entity_names);
        assert!(!AdjustmentCondition::EntityMatch.matches(&ctx));
    }

    // ── ADR-104 §2 (Stage B): entity_posterior_term ────────────────────────────

    #[test]
    fn entity_posterior_term_neutral_when_no_posterior() {
        assert_eq!(entity_posterior_term(None, ENTITY_POSTERIOR_WEIGHT), 1.0);
    }

    #[test]
    fn entity_posterior_term_neutral_at_uninformative_prior_mean() {
        // Beta(1,1) mean = 0.5 — the uninformative prior itself, distinct
        // from `None`, must also be an identity multiplier.
        let term = entity_posterior_term(Some(0.5), ENTITY_POSTERIOR_WEIGHT);
        assert!((term - 1.0).abs() < 1e-6, "got {term}");
    }

    #[test]
    fn entity_posterior_term_reaches_low_clamp_bound_at_mean_zero() {
        let term = entity_posterior_term(Some(0.0), ENTITY_POSTERIOR_WEIGHT);
        assert!(
            (term - ENTITY_POSTERIOR_CLAMP_MIN).abs() < 1e-6,
            "w_ent=0.3 at mean=0.0 must land exactly on the 0.85 clamp bound, got {term}"
        );
    }

    #[test]
    fn entity_posterior_term_reaches_high_clamp_bound_at_mean_one() {
        let term = entity_posterior_term(Some(1.0), ENTITY_POSTERIOR_WEIGHT);
        assert!(
            (term - ENTITY_POSTERIOR_CLAMP_MAX).abs() < 1e-6,
            "w_ent=0.3 at mean=1.0 must land exactly on the 1.15 clamp bound, got {term}"
        );
    }

    #[test]
    fn entity_posterior_term_never_exceeds_clamp_bounds_for_out_of_range_input() {
        // A Beta posterior mean is mathematically bounded to [0, 1], but the
        // clamp must hold defensively regardless — the term is never allowed
        // to move a score by more than +-15% no matter what value reaches it.
        let low = entity_posterior_term(Some(-5.0), ENTITY_POSTERIOR_WEIGHT);
        let high = entity_posterior_term(Some(5.0), ENTITY_POSTERIOR_WEIGHT);
        assert_eq!(low, ENTITY_POSTERIOR_CLAMP_MIN);
        assert_eq!(high, ENTITY_POSTERIOR_CLAMP_MAX);
    }

    #[test]
    fn entity_posterior_term_moves_monotonically_with_mean() {
        let low = entity_posterior_term(Some(0.2), ENTITY_POSTERIOR_WEIGHT);
        let mid = entity_posterior_term(Some(0.5), ENTITY_POSTERIOR_WEIGHT);
        let high = entity_posterior_term(Some(0.8), ENTITY_POSTERIOR_WEIGHT);
        assert!(low < mid, "low={low} mid={mid}");
        assert!(mid < high, "mid={mid} high={high}");
    }
}
