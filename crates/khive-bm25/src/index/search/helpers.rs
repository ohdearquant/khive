//! Standalone helper functions for BM25 search operations.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use khive_score::DeterministicScore;

use super::super::Bm25Index;
use super::cursor::TermCursor;
use super::{HeapEntry, SearchContext, TERMINATED_DOC};

pub(super) fn heap_to_results(
    index: &Bm25Index,
    ctx: &mut SearchContext,
) -> Vec<(Arc<str>, DeterministicScore)> {
    ctx.results_buf.clear();

    while let Some(Reverse(entry)) = ctx.heap.pop() {
        ctx.results_buf.push((entry.doc_id, entry.score));
    }

    ctx.results_buf
        .sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    ctx.results_buf
        .iter()
        .filter_map(|(internal_id, score)| {
            // resolve_internal_id returns Arc<str>; clone = atomic refcount bump.
            let doc_id = index.resolve_internal_id(*internal_id)?;
            Some((doc_id, DeterministicScore::from_f64(*score)))
        })
        .collect()
}

pub(super) fn current_threshold_score(heap: &BinaryHeap<Reverse<HeapEntry>>, k: usize) -> f64 {
    if heap.len() < k {
        0.0
    } else {
        heap.peek().map(|entry| entry.0.score).unwrap_or(0.0)
    }
}

pub(super) fn maybe_push_top_k(
    heap: &mut BinaryHeap<Reverse<HeapEntry>>,
    k: usize,
    candidate: HeapEntry,
) {
    if k == 0 {
        return;
    }

    if heap.len() < k {
        heap.push(Reverse(candidate));
        return;
    }

    let should_replace = heap.peek().map(|worst| candidate > worst.0).unwrap_or(true);
    if should_replace {
        let _ = heap.pop();
        heap.push(Reverse(candidate));
    }
}

pub(super) fn find_pivot_doc(
    cursors: &[TermCursor<'_>],
    threshold: f64,
) -> Option<(usize, usize, u32)> {
    let mut upper_bound_sum = 0.0;
    let mut before_pivot_len = 0usize;
    let mut pivot_doc = TERMINATED_DOC;

    while before_pivot_len < cursors.len() {
        upper_bound_sum += cursors[before_pivot_len].remaining_max_score();
        if upper_bound_sum >= threshold {
            pivot_doc = cursors[before_pivot_len].doc();
            break;
        }
        before_pivot_len += 1;
    }

    if pivot_doc == TERMINATED_DOC {
        return None;
    }

    let mut pivot_len = before_pivot_len + 1;
    while pivot_len < cursors.len() && cursors[pivot_len].doc() == pivot_doc {
        pivot_len += 1;
    }

    Some((before_pivot_len, pivot_len, pivot_doc))
}

pub(super) fn align_cursors(
    cursors: &mut Vec<TermCursor<'_>>,
    pivot_doc: u32,
    before_pivot_len: usize,
) -> bool {
    debug_assert_ne!(pivot_doc, TERMINATED_DOC);

    for idx in (0..before_pivot_len).rev() {
        let new_doc = cursors[idx].seek(pivot_doc);
        if new_doc != pivot_doc {
            sort_and_prune_terminated(cursors);
            return false;
        }
    }

    true
}

pub(super) fn advance_all_cursors_on_pivot(cursors: &mut Vec<TermCursor<'_>>, pivot_len: usize) {
    for cursor in &mut cursors[..pivot_len] {
        cursor.advance();
    }
    sort_and_prune_terminated(cursors);
}

/// Advance the pivot cursor with the earliest block end past its current block.
pub(super) fn advance_one_cursor_past_block(
    cursors: &mut Vec<TermCursor<'_>>,
    pivot_len: usize,
    pivot_doc: u32,
) {
    let mut cursor_to_seek = None;
    let mut earliest_block_end = TERMINATED_DOC;
    let mut doc_to_seek_after = TERMINATED_DOC;

    for (idx, cursor) in cursors[..pivot_len].iter().enumerate() {
        if let Some(info) = cursor.shallow_block_info(pivot_doc) {
            if info.last_doc < doc_to_seek_after {
                doc_to_seek_after = info.last_doc;
            }
            // Select the cursor with the earliest block end (minimum last_doc).
            // This minimizes skip distance for optimal BMW pruning.
            if info.last_doc < earliest_block_end {
                earliest_block_end = info.last_doc;
                cursor_to_seek = Some(idx);
            }
        }
    }

    if doc_to_seek_after != TERMINATED_DOC {
        doc_to_seek_after = doc_to_seek_after.saturating_add(1);
    }

    for cursor in &cursors[pivot_len..] {
        let doc = cursor.doc();
        if doc < doc_to_seek_after {
            doc_to_seek_after = doc;
        }
    }

    if let Some(idx) = cursor_to_seek {
        // Ensure forward progress: if the non-pivot cap reduced doc_to_seek_after
        // to at or below the cursor's current position, the seek would be a no-op.
        // This can happen when a non-pivot cursor points to a doc_id smaller than
        // the block-end target (e.g., a short posting list cursor at doc 3 while
        // the chosen cursor is already at doc 150). Force at least +1 advance.
        let current_doc = cursors[idx].doc();
        if doc_to_seek_after <= current_doc {
            doc_to_seek_after = current_doc.saturating_add(1);
        }
        cursors[idx].seek(doc_to_seek_after);
    }

    sort_and_prune_terminated(cursors);
}

pub(super) fn sort_and_prune_terminated(cursors: &mut Vec<TermCursor<'_>>) {
    cursors.retain(|cursor| !cursor.is_terminated());
    cursors.sort_by_key(|cursor| cursor.doc());
}
