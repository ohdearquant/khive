//! Ordering, bound and cursor pins for the sorted search frontier.

use super::*;

/// xorshift64*: deterministic and std-only.
struct Xorshift(u64);

impl Xorshift {
    fn step(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.step() % n
    }
}

/// The scan-and-sort algorithm the frontier replaced, kept as an oracle: push, re-sort the
/// whole list, truncate to the bound, find the next candidate by a linear scan.
struct ScanAndSort {
    entries: Vec<Candidate>,
    capacity: usize,
}

fn key_order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.distance
        .total_cmp(&b.distance)
        .then_with(|| a.id.cmp(&b.id))
}

impl ScanAndSort {
    fn insert(&mut self, id: u32, distance: f32) {
        self.entries.push(Candidate {
            id,
            distance,
            expanded: false,
        });
        self.entries.sort_unstable_by(key_order);
        self.entries.truncate(self.capacity);
    }

    fn take_next(&mut self) -> Option<(u32, f32)> {
        let best = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.expanded)
            .min_by(|(_, a), (_, b)| key_order(a, b))
            .map(|(i, _)| i)?;
        self.entries[best].expanded = true;
        Some((self.entries[best].id, self.entries[best].distance))
    }
}

fn ids(frontier: &Frontier) -> Vec<u32> {
    frontier.candidates().iter().map(|c| c.id).collect()
}

fn assert_matches_oracle(frontier: &Frontier, oracle: &ScanAndSort) {
    let got: Vec<(u32, u32, bool)> = frontier
        .candidates()
        .iter()
        .map(|c| (c.id, c.distance.to_bits(), c.expanded))
        .collect();
    let want: Vec<(u32, u32, bool)> = oracle
        .entries
        .iter()
        .map(|c| (c.id, c.distance.to_bits(), c.expanded))
        .collect();
    assert_eq!(got, want, "frontier diverged from the scan-and-sort oracle");

    for pair in frontier.candidates().windows(2) {
        assert!(
            key_order(&pair[0], &pair[1]).is_lt(),
            "entries must be strictly ascending by (distance, id): {} then {}",
            pair[0].id,
            pair[1].id
        );
    }
    assert!(frontier.candidates().len() <= frontier.capacity);

    let first_unexpanded = frontier
        .candidates()
        .iter()
        .position(|c| !c.expanded)
        .unwrap_or(frontier.candidates().len());
    assert_eq!(
        frontier.cursor, first_unexpanded,
        "cursor must be the first unexpanded position"
    );
}

#[test]
fn frontier_matches_scan_and_sort_oracle_under_random_inserts_and_takes() {
    const OPS: usize = 600;
    for capacity in [1usize, 2, 3, 5, 16, 64] {
        for seed in 1..=20u64 {
            let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

            // Unique ids in random order, so a later insert can carry a smaller id.
            let mut ids_pool: Vec<u32> = (0..OPS as u32).collect();
            for i in (1..ids_pool.len()).rev() {
                let j = rng.below(i as u64 + 1) as usize;
                ids_pool.swap(i, j);
            }
            let mut next_id = ids_pool.into_iter();

            let mut frontier = Frontier::new(capacity);
            let mut oracle = ScanAndSort {
                entries: Vec::new(),
                capacity,
            };

            for _ in 0..OPS {
                if rng.below(4) < 3 {
                    let Some(id) = next_id.next() else { break };
                    // Few distinct distances so ties are the common case; -0.0 sorts
                    // before 0.0 under total_cmp and must be handled the same way.
                    let distance = if rng.below(16) == 0 {
                        -0.0
                    } else {
                        rng.below(8) as f32 * 0.25
                    };
                    frontier.insert(id, distance);
                    oracle.insert(id, distance);
                } else {
                    assert_eq!(frontier.take_next(), oracle.take_next());
                }
                assert_matches_oracle(&frontier, &oracle);
            }

            // Drain: the expansion order must match the oracle to the end.
            loop {
                let got = frontier.take_next();
                assert_eq!(got, oracle.take_next());
                assert_matches_oracle(&frontier, &oracle);
                if got.is_none() {
                    break;
                }
            }
        }
    }
}

#[test]
fn frontier_orders_equal_distances_by_node_id() {
    let mut frontier = Frontier::new(8);
    frontier.insert(9, 1.0);
    frontier.insert(2, 1.0);
    frontier.insert(5, 1.0);
    frontier.insert(7, 0.5);
    assert_eq!(ids(&frontier), vec![7, 2, 5, 9]);
}

#[test]
fn frontier_keeps_only_the_best_capacity_candidates() {
    let mut frontier = Frontier::new(3);
    for (id, distance) in [(10, 5.0), (11, 4.0), (12, 3.0), (13, 2.0), (14, 1.0)] {
        frontier.insert(id, distance);
    }
    assert_eq!(ids(&frontier), vec![14, 13, 12]);

    // Farther than the current worst: not stored.
    frontier.insert(20, 3.5);
    assert_eq!(ids(&frontier), vec![14, 13, 12]);

    // Same distance as the worst but a larger id ranks after it: not stored.
    frontier.insert(21, 3.0);
    assert_eq!(ids(&frontier), vec![14, 13, 12]);

    // Same distance as the worst but a smaller id ranks before it: evicts it.
    frontier.insert(5, 3.0);
    assert_eq!(ids(&frontier), vec![14, 13, 5]);
}

#[test]
fn frontier_cursor_rewinds_to_a_better_candidate() {
    let mut frontier = Frontier::new(8);
    frontier.insert(1, 1.0);
    frontier.insert(2, 2.0);
    frontier.insert(3, 3.0);
    assert_eq!(frontier.take_next(), Some((1, 1.0)));
    assert_eq!(frontier.take_next(), Some((2, 2.0)));

    // Better than everything expanded so far: it is next, then the old frontier resumes.
    frontier.insert(4, 0.5);
    assert_eq!(frontier.take_next(), Some((4, 0.5)));
    assert_eq!(frontier.take_next(), Some((3, 3.0)));
    assert_eq!(frontier.take_next(), None);
}

#[test]
fn frontier_cursor_survives_eviction_when_every_entry_is_expanded() {
    let mut frontier = Frontier::new(2);
    frontier.insert(1, 1.0);
    frontier.insert(2, 2.0);
    assert_eq!(frontier.take_next(), Some((1, 1.0)));
    assert_eq!(frontier.take_next(), Some((2, 2.0)));
    assert_eq!(frontier.take_next(), None);

    // Full, all expanded: the new entry evicts the worst and becomes the next to expand.
    frontier.insert(3, 1.5);
    assert_eq!(ids(&frontier), vec![1, 3]);
    assert_eq!(frontier.take_next(), Some((3, 1.5)));
    assert_eq!(frontier.take_next(), None);
}

#[test]
fn frontier_does_not_re_expand_an_expanded_entry_it_shifted() {
    let mut frontier = Frontier::new(8);
    frontier.insert(1, 1.0);
    frontier.insert(2, 2.0);
    assert_eq!(frontier.take_next(), Some((1, 1.0)));

    // Inserted between the expanded entry and the unexpanded one.
    frontier.insert(3, 1.5);
    assert_eq!(frontier.take_next(), Some((3, 1.5)));
    assert_eq!(frontier.take_next(), Some((2, 2.0)));
    assert_eq!(frontier.take_next(), None);
}
