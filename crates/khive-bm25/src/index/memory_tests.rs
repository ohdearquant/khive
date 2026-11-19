use super::*;

#[test]
fn test_memory_usage_accounts_for_doc_lengths_vec_mirror() {
    let mut index = Bm25Index::default();
    let before = index.memory_usage();

    // Grow both O(1) doc-length vector mirrors by one slot, bypassing indexing so no
    // other accounted structure (postings, terms, id maps) changes.
    index.set_doc_length_fast(0, 42);

    let after = index.memory_usage();
    let delta = after - before;

    // Growing each mirror by exactly one slot changes memory_usage by exactly one usize plus
    // one f32; asserting equality keeps the test diagnostic if either term is dropped.
    assert_eq!(
        delta,
        size_of::<usize>() + size_of::<f32>(),
        "memory_usage delta must equal one usize + one f32 doc-length mirror slot"
    );
}

#[test]
fn test_estimate_document_cost_scales_mirror_with_id_gap() {
    // Two indexes identical except for the gap between `next_internal_id` and the rebuilt
    // mirror length, so estimate_document_cost differs ONLY in its doc-length mirror term. This
    // isolates the gap accounting from the estimate's other heuristics.
    let text = "quick brown fox";

    // Contiguous: the next insert resizes the mirrors by a single slot.
    let mut contiguous = Bm25Index::default();
    contiguous.doc_lengths.insert(0, 5);
    contiguous.next_internal_id = 1;
    contiguous.ensure_doc_lengths_vec();
    assert_eq!(contiguous.doc_lengths_vec.len(), 1);
    let contiguous_cost = contiguous.estimate_document_cost(text);

    // Gapped: ids 1..=3 were removed before deserialization, so the next id (4) sits three
    // slots past the rebuilt mirror length and one insert resizes across the whole gap.
    let mut gapped = Bm25Index::default();
    gapped.doc_lengths.insert(0, 5);
    gapped.next_internal_id = 4;
    gapped.ensure_doc_lengths_vec();
    assert_eq!(gapped.doc_lengths_vec.len(), 1);
    let gapped_cost = gapped.estimate_document_cost(text);

    // The gap adds exactly three extra mirror slots (one usize + one f32 each); a per-document
    // "one slot" estimate would report no difference and silently under-admit.
    assert_eq!(
        gapped_cost - contiguous_cost,
        3 * (size_of::<usize>() + size_of::<f32>()),
        "estimate must charge the extra doc-length mirror slots resized across an id gap"
    );
}
