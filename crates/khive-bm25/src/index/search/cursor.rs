//! Term cursor for Block-Max WAND traversal.

use super::super::{BlockMaxBlock, Bm25Index, Bm25TermScorer, PostingList};
use super::{ShallowBlockInfo, TERMINATED_DOC};

pub(super) struct TermCursor<'a> {
    postings: &'a PostingList,
    blocks: &'a [BlockMaxBlock],
    pos: usize,
    block_size: usize,
    pub(super) scorer: Bm25TermScorer,
}

impl<'a> TermCursor<'a> {
    #[inline]
    pub(super) fn new(
        postings: &'a PostingList,
        blocks: &'a [BlockMaxBlock],
        block_size: usize,
        scorer: Bm25TermScorer,
    ) -> Self {
        Self {
            postings,
            blocks,
            pos: 0,
            block_size,
            scorer,
        }
    }

    #[inline]
    pub(super) fn is_terminated(&self) -> bool {
        self.pos >= self.postings.len()
    }

    #[inline]
    pub(super) fn doc(&self) -> u32 {
        if self.pos < self.postings.doc_ids.len() {
            self.postings.doc_ids[self.pos]
        } else {
            TERMINATED_DOC
        }
    }

    #[inline]
    fn current_doc_id(&self) -> u32 {
        self.postings.doc_ids[self.pos]
    }

    #[inline]
    fn current_term_freq(&self) -> u8 {
        self.postings.term_freqs[self.pos]
    }

    #[inline]
    fn current_block_idx(&self) -> Option<usize> {
        if self.is_terminated() {
            None
        } else {
            Some(self.pos / self.block_size)
        }
    }

    #[inline]
    pub(super) fn remaining_max_score(&self) -> f64 {
        self.current_block_idx()
            .and_then(|idx| self.blocks.get(idx))
            .map(|block| block.suffix_max_score)
            .unwrap_or(0.0)
    }

    #[inline]
    pub(super) fn advance(&mut self) -> u32 {
        if !self.is_terminated() {
            self.pos += 1;
        }
        self.doc()
    }

    #[inline]
    pub(super) fn seek(&mut self, target_doc: u32) -> u32 {
        if self.is_terminated() {
            return TERMINATED_DOC;
        }
        if self.doc() >= target_doc {
            return self.doc();
        }

        let rel = self.postings.doc_ids[self.pos..].partition_point(|&id| id < target_doc);
        self.pos += rel;
        self.doc()
    }

    #[inline]
    pub(super) fn shallow_block_info(&self, target_doc: u32) -> Option<ShallowBlockInfo> {
        let current_block_idx = self.current_block_idx()?;
        let rel =
            self.blocks[current_block_idx..].partition_point(|block| block.max_doc_id < target_doc);
        let block = self.blocks.get(current_block_idx + rel)?;
        Some(ShallowBlockInfo {
            max_score: block.max_score_contribution,
            last_doc: block.max_doc_id,
        })
    }

    #[inline]
    pub(super) fn score_current(&self, index: &Bm25Index) -> f64 {
        if self.is_terminated() {
            return 0.0;
        }
        let doc_id = self.current_doc_id();
        let term_freq = self.current_term_freq();
        let doc_length = index.doc_length_fast(doc_id);
        self.scorer.score(term_freq, doc_length)
    }
}
