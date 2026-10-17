use khive_retrieval::{
    merge_query_variants, normalize_query_variants, reciprocal_rank_fusion, AdmittedVariantHit,
    ErrorKind, QueryVariantLimits, QueryVariantOrigin, QueryVariantRejection, QueryVariants,
    RetrievalError, VariantAttribution, VariantRankedList, VariantSource,
};
use khive_score::DeterministicScore;
use serde_json::json;
use std::num::NonZeroUsize;
use uuid::Uuid;

fn limits() -> QueryVariantLimits {
    QueryVariantLimits::try_new(3, 256, 1_024, 192, 127).unwrap()
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

fn variants() -> QueryVariants {
    normalize_query_variants(
        "original",
        &strings(&["one", "two", "three"]),
        10,
        &limits(),
    )
}

fn ids(numbers: &[u128]) -> Vec<Uuid> {
    numbers.iter().copied().map(Uuid::from_u128).collect()
}

fn list<'a>(
    variant_id: u8,
    ranked_ids: &'a [Uuid],
    lexical_ids: &'a [Uuid],
    ann_ids: &'a [Uuid],
) -> VariantRankedList<'a> {
    VariantRankedList {
        variant_id,
        ranked_ids,
        lexical_ids,
        ann_ids,
    }
}

fn order(hits: &[AdmittedVariantHit]) -> Vec<Uuid> {
    hits.iter().map(|hit| hit.id).collect()
}

fn occurrence(variant_id: u8, source: VariantSource, source_rank: usize) -> VariantAttribution {
    VariantAttribution {
        variant_id,
        source,
        source_rank: NonZeroUsize::new(source_rank).unwrap(),
    }
}

fn assert_rejected(
    original: &str,
    generated: &[String],
    tokens: usize,
    limits: &QueryVariantLimits,
    reason: QueryVariantRejection,
) {
    let normalized = normalize_query_variants(original, generated, tokens, limits);
    assert_eq!(normalized.rejection(), Some(reason));
    assert_eq!(
        serde_json::to_value(normalized.variants()).unwrap(),
        json!([{"id": 0, "origin": "original", "text": original}])
    );
    assert_eq!(
        merge_query_variants(&normalized, &[list(0, &[], &[], &[])], limits).unwrap(),
        None
    );
}

include!("query_variants/normalization.rs");

include!("query_variants/limits.rs");

include!("query_variants/ranking_and_order.rs");

include!("query_variants/admission_and_wire.rs");
