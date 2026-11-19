//! Bounded, sorted candidate list for the greedy beam searches.

/// A search candidate: node id, its acquisition-tier distance to the query, and whether
/// the beam has already expanded it.
#[derive(Clone, Copy)]
pub(super) struct Candidate {
    pub(super) id: u32,
    pub(super) distance: f32,
    pub(super) expanded: bool,
}

/// Search frontier kept sorted ascending by `(distance, id)` and bounded to `capacity`.
///
/// The beam search only needs the `capacity` best candidates seen so far, so a candidate
/// that ranks beyond the bound is never stored and one that ranks inside it is placed by
/// binary search rather than by re-sorting the whole list. `cursor` is the position of the
/// first unexpanded entry (`entries.len()` when none is left), so the next candidate to
/// expand is read without scanning.
///
/// The order is total because node ids are unique: callers mark a node in their
/// `VisitedSet` before offering it. Equal distances therefore order by id, which keeps the
/// traversal deterministic.
pub(super) struct Frontier {
    entries: Vec<Candidate>,
    capacity: usize,
    cursor: usize,
}

impl Frontier {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
            cursor: 0,
        }
    }

    /// Offer a candidate. Stored only if it ranks within the best `capacity`; a full
    /// frontier evicts its worst entry to make room.
    pub(super) fn insert(&mut self, id: u32, distance: f32) {
        let pos = self.entries.partition_point(|c| {
            c.distance
                .total_cmp(&distance)
                .then_with(|| c.id.cmp(&id))
                .is_lt()
        });
        if pos >= self.capacity {
            return;
        }
        if self.entries.len() == self.capacity {
            // Drop the worst entry. `cursor` can now point beyond the end, which the
            // `pos <= self.cursor` rule below repairs because `pos <= self.entries.len()`.
            self.entries.pop();
        }
        self.entries.insert(
            pos,
            Candidate {
                id,
                distance,
                expanded: false,
            },
        );
        // Every entry before `cursor` is expanded, so a new entry at or before it is the
        // first unexpanded one.
        if pos <= self.cursor {
            self.cursor = pos;
        }
    }

    /// Mark the best unexpanded candidate expanded and return its id and distance.
    pub(super) fn take_next(&mut self) -> Option<(u32, f32)> {
        let best = self.entries.get_mut(self.cursor)?;
        best.expanded = true;
        let taken = (best.id, best.distance);
        self.cursor += 1;
        while self.entries.get(self.cursor).is_some_and(|c| c.expanded) {
            self.cursor += 1;
        }
        Some(taken)
    }

    /// Every stored candidate, best first.
    pub(super) fn candidates(&self) -> &[Candidate] {
        &self.entries
    }
}

#[cfg(test)]
#[path = "frontier_tests.rs"]
mod tests;
