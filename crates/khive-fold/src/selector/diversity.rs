use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use khive_score::DeterministicScore;

use super::{pragmatic_plus_epistemic, SelectorInput, SelectorWeights};

#[cfg(test)]
std::thread_local! {
    static WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn work() {
    #[cfg(test)]
    WORK.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(super) fn take_work() -> usize {
    WORK.with(|count| count.replace(0))
}

struct Key {
    base: f64,
    size: usize,
    id: usize,
    position: usize,
}

impl Key {
    fn tie(&self) -> (usize, usize, Reverse<usize>) {
        (self.size, self.id, Reverse(self.position))
    }

    fn score(&self, factor: f64) -> DeterministicScore {
        work();
        DeterministicScore::from_f64(self.base * factor)
    }
}

fn better(a: Option<usize>, b: Option<usize>, keys: &[Key]) -> Option<usize> {
    work();
    match (a, b) {
        (Some(a), Some(b)) => Some(if keys[a].tie() <= keys[b].tie() { a } else { b }),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

struct Category {
    order: Vec<usize>,
    tree: Vec<Option<usize>>,
    leaves: usize,
    count: usize,
    penalized: bool,
    epoch: usize,
}

impl Category {
    fn new(mut order: Vec<usize>, penalized: bool, keys: &[Key]) -> Self {
        order.sort_by(|&a, &b| {
            work();
            keys[b].base.total_cmp(&keys[a].base)
        });
        let leaves = order.len().next_power_of_two();
        let mut tree = vec![None; 2 * leaves];
        for (slot, &index) in order.iter().enumerate() {
            tree[leaves + slot] = Some(index);
        }
        for node in (1..leaves).rev() {
            tree[node] = better(tree[2 * node], tree[2 * node + 1], keys);
        }
        Self {
            order,
            tree,
            leaves,
            count: 0,
            penalized,
            epoch: 0,
        }
    }

    fn update(&mut self, slot: usize, value: Option<usize>, keys: &[Key]) {
        let mut node = self.leaves + slot;
        self.tree[node] = value;
        while node > 1 {
            node /= 2;
            self.tree[node] = better(self.tree[2 * node], self.tree[2 * node + 1], keys);
        }
    }

    fn first_fit(&self, budget: usize, keys: &[Key]) -> Option<usize> {
        if keys[self.tree[1]?].size > budget {
            return None;
        }
        let mut node = 1;
        while node < self.leaves {
            work();
            node *= 2;
            if self.tree[node].is_none_or(|index| keys[index].size > budget) {
                node += 1;
            }
        }
        Some(node - self.leaves)
    }

    fn range_best(&self, start: usize, end: usize, keys: &[Key]) -> usize {
        let mut left = self.leaves + start;
        let mut right = self.leaves + end;
        let mut best = None;
        while left < right {
            work();
            if left % 2 == 1 {
                best = better(best, self.tree[left], keys);
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                best = better(best, self.tree[right], keys);
            }
            left /= 2;
            right /= 2;
        }
        best.expect("range contains the fitting leader")
    }

    fn best(&self, budget: usize, bias: f32, keys: &[Key]) -> Option<(usize, DeterministicScore)> {
        let first = self.first_fit(budget, keys)?;
        let count = self.count as f64;
        let factor = 1.0 - bias as f64 * count / (count + 1.0);
        let score = keys[self.order[first]].score(factor);
        // Multiplication and quantization can merge distinct raw scores. The
        // whole plateau participates in the size/id/current-position tie-break.
        let end = self
            .order
            .partition_point(|&index| keys[index].score(factor) >= score);
        let index = self.range_best(first, end, keys);
        Some((index, score))
    }
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct Winner {
    score: DeterministicScore,
    size: Reverse<usize>,
    id: Reverse<usize>,
    position: usize,
    category: usize,
    epoch: usize,
    index: usize,
}

fn refresh(
    category: usize,
    categories: &mut [Category],
    heap: &mut BinaryHeap<Winner>,
    keys: &[Key],
    budget: usize,
    bias: f32,
) {
    let bucket = &mut categories[category];
    bucket.epoch += 1;
    if let Some((index, score)) = bucket.best(budget, bias, keys) {
        let key = &keys[index];
        heap.push(Winner {
            score,
            size: Reverse(key.size),
            id: Reverse(key.id),
            position: key.position,
            category,
            epoch: bucket.epoch,
            index,
        });
    }
}

pub(super) fn select<T>(
    inputs: &[SelectorInput<T>],
    budget: usize,
    weights: &SelectorWeights,
) -> Vec<usize> {
    let mut ids = BTreeMap::new();
    let mut groups: BTreeMap<Option<&str>, Vec<usize>> = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        ids.entry(input.id.as_str()).or_insert(0);
        groups
            .entry(input.category.as_deref())
            .or_default()
            .push(index);
    }
    for (rank, value) in ids.values_mut().enumerate() {
        *value = rank;
    }
    let mut keys: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(position, input)| Key {
            base: pragmatic_plus_epistemic(input, weights.epistemic_weight),
            size: input.size,
            id: ids[input.id.as_str()],
            position,
        })
        .collect();
    let mut categories: Vec<_> = groups
        .into_iter()
        .map(|(name, order)| Category::new(order, name.is_some(), &keys))
        .collect();
    let mut locations = vec![(0, 0); inputs.len()];
    for (category, bucket) in categories.iter().enumerate() {
        for (slot, &index) in bucket.order.iter().enumerate() {
            locations[index] = (category, slot);
        }
    }
    let mut heap = BinaryHeap::new();
    for category in 0..categories.len() {
        refresh(
            category,
            &mut categories,
            &mut heap,
            &keys,
            budget,
            weights.diversity_bias,
        );
    }
    // Keep even permanently oversized candidates here: their positions affect
    // the original max_by(last tie) + swap_remove behavior for duplicate IDs.
    let mut remaining: Vec<_> = (0..inputs.len()).collect();
    let mut selected = Vec::new();
    let mut available = budget;
    while let Some(winner) = heap.pop() {
        work();
        if winner.epoch != categories[winner.category].epoch {
            continue;
        }
        if keys[winner.index].size > available {
            refresh(
                winner.category,
                &mut categories,
                &mut heap,
                &keys,
                available,
                weights.diversity_bias,
            );
            continue;
        }
        let index = winner.index;
        let (category, slot) = locations[index];
        let position = keys[index].position;
        categories[category].update(slot, None, &keys);
        let removed = remaining.swap_remove(position);
        debug_assert_eq!(removed, index);
        available -= keys[index].size;
        selected.push(index);
        let moved_category = if let Some(&moved) = remaining.get(position) {
            keys[moved].position = position;
            let (moved_category, moved_slot) = locations[moved];
            categories[moved_category].update(moved_slot, Some(moved), &keys);
            Some(moved_category)
        } else {
            None
        };
        if categories[category].penalized {
            categories[category].count += 1;
        }
        refresh(
            category,
            &mut categories,
            &mut heap,
            &keys,
            available,
            weights.diversity_bias,
        );
        if let Some(moved_category) = moved_category.filter(|&moved| moved != category) {
            refresh(
                moved_category,
                &mut categories,
                &mut heap,
                &keys,
                available,
                weights.diversity_bias,
            );
        }
    }
    selected
}
