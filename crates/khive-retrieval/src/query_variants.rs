//! Pure query-variant normalization and candidate admission (ADR-160 D9).
//!
//! Callers validate provider grammar and measure generation tokens before normalization. They
//! bound retrieval work and supply one inner-fused ranked list per variant, with its channel
//! traces. This module does no retrieval or final scoring: outer RRF-60 scores select candidates
//! and are discarded. A merge returning `None` means reuse the caller's frozen baseline value.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;

use khive_fusion::reciprocal_rank_fusion;
use khive_score::DeterministicScore;
use serde::Serialize;
use uuid::Uuid;

use crate::{Result, RetrievalError};

/// Validated effective limits for the pure portion of query expansion.
///
/// All values must be explicit and positive. They may lower, but never raise, the v1 ceilings.
/// This output-pool bound does not replace the caller's retrieval-work budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryVariantLimits {
    max_generated_variants: usize,
    max_variant_bytes: usize,
    max_generated_bytes: usize,
    max_output_tokens: usize,
    max_fused_candidates: usize,
}

impl QueryVariantLimits {
    /// Validate the item, per-string byte, aggregate byte, token, and admission limits.
    ///
    /// # Errors
    /// Returns a permanent configuration error for a zero or above-ceiling value.
    pub fn try_new(
        max_generated_variants: usize,
        max_variant_bytes: usize,
        max_generated_bytes: usize,
        max_output_tokens: usize,
        max_fused_candidates: usize,
    ) -> Result<Self> {
        for (value, ceiling, message) in [
            (
                max_generated_variants,
                3,
                "max_generated_variants must be in 1..=3",
            ),
            (
                max_variant_bytes,
                256,
                "max_variant_bytes must be in 1..=256",
            ),
            (
                max_generated_bytes,
                1_024,
                "max_generated_bytes must be in 1..=1024",
            ),
            (
                max_output_tokens,
                192,
                "max_output_tokens must be in 1..=192",
            ),
            (
                max_fused_candidates,
                127,
                "max_fused_candidates must be in 1..=127",
            ),
        ] {
            if value == 0 || value > ceiling {
                return Err(RetrievalError::configuration(message));
            }
        }
        Ok(Self {
            max_generated_variants,
            max_variant_bytes,
            max_generated_bytes,
            max_output_tokens,
            max_fused_candidates,
        })
    }

    /// Maximum raw generated item count, before trimming or deduplication.
    pub fn max_generated_variants(&self) -> usize {
        self.max_generated_variants
    }

    /// Maximum UTF-8 bytes in each raw generated string.
    pub fn max_variant_bytes(&self) -> usize {
        self.max_variant_bytes
    }

    /// Maximum aggregate UTF-8 bytes in the raw generated strings.
    pub fn max_generated_bytes(&self) -> usize {
        self.max_generated_bytes
    }

    /// Maximum output-token count measured by the provider adapter.
    pub fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }

    /// Maximum number of candidates admitted by the outer merge.
    pub fn max_fused_candidates(&self) -> usize {
        self.max_fused_candidates
    }
}

/// Whether a variant is the unchanged original query or a generated rephrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryVariantOrigin {
    /// The caller's original query bytes, without additional normalization.
    Original,
    /// A validated, trimmed, distinct generated query.
    Generated,
}

/// One query variant in the transient expansion metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryVariant {
    /// Dense variant ID: original is zero, generated variants are one through three.
    pub id: u8,
    /// Original or generated origin.
    pub origin: QueryVariantOrigin,
    /// Exact original bytes or ASCII-trimmed generated UTF-8 text.
    pub text: String,
}

/// Text-free reason that the entire generated result was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryVariantRejection {
    /// The raw generated item count exceeds the effective limit.
    TooManyItems,
    /// The supplied measured generation-token count exceeds the effective limit.
    OutputTokenLimit,
    /// A raw generated string exceeds the effective byte limit.
    VariantByteLimit,
    /// The raw generated strings exceed the effective aggregate byte limit.
    AggregateByteLimit,
    /// A generated string is empty after the specified ASCII trim.
    EmptyVariant,
    /// A generated string contains a NUL byte.
    NulByte,
}

/// A normalized variant set that always contains the unchanged original as variant zero.
///
/// Construction is through [`normalize_query_variants`]; neither mutation nor deserialization
/// can bypass its whole-result validation. A valid empty or fully deduplicated result has only
/// variant zero and no rejection reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryVariants {
    variants: Vec<QueryVariant>,
    rejection: Option<QueryVariantRejection>,
}

impl QueryVariants {
    /// Variants in ID order, starting with the unchanged original query.
    pub fn variants(&self) -> &[QueryVariant] {
        &self.variants
    }

    /// Why the entire generated result was rejected, without any query text.
    pub fn rejection(&self) -> Option<QueryVariantRejection> {
        self.rejection
    }
}

/// Normalize generated strings without altering the original query bytes.
///
/// Count and byte limits apply before trimming/deduplication. Trimming removes only ASCII SP,
/// HT, CR and LF; deduplication compares exact UTF-8 bytes against the original and earlier
/// retained variants. Any invalid generated item discards the entire generated result.
///
/// The caller supplies its measured output-token count and validates provider grammar/encoded
/// envelopes upstream. Aggregate bytes here count the decoded generated strings only. The
/// original query has already passed the caller's query policy and is not revalidated here.
pub fn normalize_query_variants(
    original: &str,
    generated: &[String],
    generated_output_tokens: usize,
    limits: &QueryVariantLimits,
) -> QueryVariants {
    let mut result = QueryVariants {
        variants: vec![QueryVariant {
            id: 0,
            origin: QueryVariantOrigin::Original,
            text: original.to_owned(),
        }],
        rejection: None,
    };
    if let Err(reason) = validate_generated(generated, generated_output_tokens, limits) {
        result.rejection = Some(reason);
        return result;
    }

    for text in generated {
        let text = trim_generated(text);
        if !result.variants.iter().any(|variant| variant.text == text) {
            result.variants.push(QueryVariant {
                // Validation caps generated input at three items, before any deduplication.
                id: result.variants.len() as u8,
                origin: QueryVariantOrigin::Generated,
                text: text.to_owned(),
            });
        }
    }
    result
}

fn trim_generated(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\r', '\n'])
}

fn validate_generated(
    generated: &[String],
    output_tokens: usize,
    limits: &QueryVariantLimits,
) -> std::result::Result<(), QueryVariantRejection> {
    if generated.len() > limits.max_generated_variants {
        return Err(QueryVariantRejection::TooManyItems);
    }
    if output_tokens > limits.max_output_tokens {
        return Err(QueryVariantRejection::OutputTokenLimit);
    }
    let mut total_bytes = 0usize;
    for text in generated {
        if text.len() > limits.max_variant_bytes {
            return Err(QueryVariantRejection::VariantByteLimit);
        }
        if text.contains('\0') {
            return Err(QueryVariantRejection::NulByte);
        }
        total_bytes = total_bytes
            .checked_add(text.len())
            .ok_or(QueryVariantRejection::AggregateByteLimit)?;
        if total_bytes > limits.max_generated_bytes {
            return Err(QueryVariantRejection::AggregateByteLimit);
        }
        if trim_generated(text).is_empty() {
            return Err(QueryVariantRejection::EmptyVariant);
        }
    }
    Ok(())
}

/// Channel that observed a candidate, ordered lexical before ANN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VariantSource {
    /// Lexical ranked-list occurrence.
    Lexical,
    /// ANN ranked-list occurrence.
    Ann,
}

/// One transient candidate-origin fact, independent of relevance scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct VariantAttribution {
    /// Original or generated variant ID in zero through three.
    pub variant_id: u8,
    /// Channel that observed this occurrence; dual-channel hits have separate entries.
    pub source: VariantSource,
    /// One-based position in that channel's ranked list, including duplicate positions.
    pub source_rank: NonZeroUsize,
}

/// One variant's inner-fused ranking and its separate lexical/ANN provenance traces.
///
/// Every ranked ID must occur in at least one of its own channel traces. Extra trace-only IDs
/// provide provenance but do not enter outer RRF. Ordering inside each slice defines its ranks;
/// the caller has already canonicalized tied backend results and bounded retrieval work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VariantRankedList<'a> {
    /// ID from the normalized variant set.
    pub variant_id: u8,
    /// Authoritative inner-fused candidate order: one outer RRF source for this variant.
    pub ranked_ids: &'a [Uuid],
    /// Authoritative lexical channel order, used solely for attribution.
    pub lexical_ids: &'a [Uuid],
    /// Authoritative ANN channel order, used solely for attribution.
    pub ann_ids: &'a [Uuid],
}

/// Candidate admitted for the caller's final original-query scoring pass.
///
/// No admission score is retained or serialized. These facts do not explain relevance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmittedVariantHit {
    /// Canonical UUID identity of the admitted candidate.
    pub id: Uuid,
    /// Ordered, deduplicated occurrence tuples from the supplied channel traces.
    pub variant_attribution: Vec<VariantAttribution>,
}

/// Merge one inner-fused ranked list per variant using the shared RRF primitive with k=60.
///
/// The outer list container may arrive in any order; within-list order defines rank. A duplicate
/// ID votes at its best rank once per variant without compressing later positions. Equal RRF
/// scores tie by canonical UUID order. All observed channel occurrences of each admitted ID are
/// returned in variant/source/rank order, including occurrences in other supplied traces.
///
/// `Ok(None)` means no generated ranked list contributes: reuse the frozen baseline response,
/// without reconstructing it through this fuser. Otherwise the output has at most the configured
/// candidate cap, and all admission scores are discarded. Final relevance scoring is upstream.
///
/// # Errors
/// Returns a permanent, text-free invalid-query error for missing/duplicate/unknown variant lists,
/// missing channel evidence for a ranked ID, or an unrepresentable rank. Validation also applies
/// before the original-only fallback. Empty or missing generated lists are allowed.
pub fn merge_query_variants(
    variants: &QueryVariants,
    lists: &[VariantRankedList<'_>],
    limits: &QueryVariantLimits,
) -> Result<Option<Vec<AdmittedVariantHit>>> {
    let mut ordered: [Option<&VariantRankedList<'_>>; 4] = [None; 4];
    for list in lists {
        let index = usize::from(list.variant_id);
        if index >= variants.variants.len() {
            return Err(RetrievalError::invalid_query("unknown query variant list"));
        }
        if ordered[index].replace(list).is_some() {
            return Err(RetrievalError::invalid_query(
                "duplicate query variant list",
            ));
        }
        if list.ranked_ids.len().checked_add(60).is_none() {
            return Err(RetrievalError::invalid_query("query variant rank overflow"));
        }
        let trace_ids: HashSet<_> = list.lexical_ids.iter().chain(list.ann_ids).collect();
        if list.ranked_ids.iter().any(|id| !trace_ids.contains(id)) {
            return Err(RetrievalError::invalid_query(
                "query variant ranked ID has no channel occurrence",
            ));
        }
    }
    if ordered[0].is_none() {
        return Err(RetrievalError::invalid_query(
            "missing original query variant list",
        ));
    }
    if !ordered[1..]
        .iter()
        .flatten()
        .any(|list| !list.ranked_ids.is_empty())
    {
        return Ok(None);
    }

    let sources = ordered
        .iter()
        .flatten()
        .map(|list| {
            list.ranked_ids
                .iter()
                .map(|id| (*id, DeterministicScore::ZERO))
                .collect()
        })
        .collect();
    let mut admitted: Vec<_> = reciprocal_rank_fusion(sources, 60)
        .into_iter()
        .take(limits.max_fused_candidates)
        .map(|(id, _)| AdmittedVariantHit {
            id,
            variant_attribution: Vec::new(),
        })
        .collect();
    let positions: HashMap<_, _> = admitted
        .iter()
        .enumerate()
        .map(|(index, hit)| (hit.id, index))
        .collect();
    for list in ordered.iter().flatten() {
        for (source, ids) in [
            (VariantSource::Lexical, list.lexical_ids),
            (VariantSource::Ann, list.ann_ids),
        ] {
            for (index, id) in ids.iter().enumerate() {
                if let Some(&position) = positions.get(id) {
                    let source_rank = NonZeroUsize::MIN.checked_add(index).ok_or_else(|| {
                        RetrievalError::invalid_query("query variant source rank overflow")
                    })?;
                    admitted[position]
                        .variant_attribution
                        .push(VariantAttribution {
                            variant_id: list.variant_id,
                            source,
                            source_rank,
                        });
                }
            }
        }
    }
    for hit in &mut admitted {
        hit.variant_attribution.sort_unstable();
        hit.variant_attribution.dedup();
    }
    Ok(Some(admitted))
}
