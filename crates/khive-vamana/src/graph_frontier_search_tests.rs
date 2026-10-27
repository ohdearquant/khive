//! The sorted-frontier searches against the scan-and-sort loop they replaced, plus the
//! tie-handling and repeatability contract of the SQ8 search.

use super::*;

const DIM: usize = 8;

struct Fixture {
    vectors: Vec<f32>,
    adjacency: Vec<Vec<u32>>,
}

/// `count` rows of `DIM` values. With `levels` the values lie on a coarse grid, which
/// makes equal distances (and equal SQ8 codes) the common case.
fn rows(count: usize, levels: Option<u32>, rng: &mut StdRng) -> Vec<f32> {
    (0..count * DIM)
        .map(|_| match levels {
            Some(l) => rng.random_range(0..l) as f32 / l as f32,
            None => rng.random_range(-1.0f32..1.0),
        })
        .collect()
}

fn fixture(n: usize, levels: Option<u32>, seed: u64) -> Fixture {
    let mut rng = StdRng::seed_from_u64(seed);
    let vectors = rows(n, levels, &mut rng);
    let adjacency = (0..n)
        .map(|node| {
            let mut neighbors: Vec<u32> = (0..8)
                .map(|_| rng.random_range(0..n as u32))
                .filter(|&v| v != node as u32)
                .collect();
            neighbors.sort_unstable();
            neighbors.dedup();
            neighbors
        })
        .collect();
    Fixture { vectors, adjacency }
}

/// Tombstone bitvec marking every node whose id ends in 3.
fn tombstones_ending_in_3(n: usize) -> Vec<u64> {
    let mut bits = vec![0u64; n.div_ceil(64)];
    for id in (3..n).step_by(10) {
        bits[id / 64] |= 1u64 << (id % 64);
    }
    bits
}

/// A start node that is not tombstoned by `tombstones_ending_in_3`.
fn live_start(qi: usize, n: usize) -> u32 {
    let start = (qi * 7) % n;
    if start % 10 == 3 {
        (start + 1) as u32
    } else {
        start as u32
    }
}

/// (node id, distance) pairs, best first.
type Scored = Vec<(u32, f32)>;

/// The search loop before the sorted frontier: a flat list re-sorted and truncated after
/// every expansion, the next candidate found by a linear scan. `dist` is the
/// acquisition-tier distance. Returns the final frontier, best first, and the expansion
/// order.
fn reference_search(
    adjacency: &[Vec<u32>],
    start: u32,
    effective_l: usize,
    tombstones: Option<&[u64]>,
    dist: &dyn Fn(u32) -> f32,
) -> (Scored, Scored) {
    let mut visited: HashSet<u32> = HashSet::new();
    visited.insert(start);
    // (id, distance, expanded)
    let mut frontier: Vec<(u32, f32, bool)> = vec![(start, dist(start), false)];
    let mut expanded: Vec<(u32, f32)> = Vec::new();

    while let Some(best) = frontier
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.2)
        .min_by(|(_, a), (_, b)| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .map(|(i, _)| i)
    {
        frontier[best].2 = true;
        let (id, d, _) = frontier[best];
        expanded.push((id, d));

        for &neighbor in &adjacency[id as usize] {
            if let Some(ts) = tombstones {
                if is_tombstoned_bit(ts, neighbor as usize) {
                    continue;
                }
            }
            if !visited.insert(neighbor) {
                continue;
            }
            frontier.push((neighbor, dist(neighbor), false));
        }

        frontier.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        frontier.truncate(effective_l);
    }

    (
        frontier.into_iter().map(|(id, d, _)| (id, d)).collect(),
        expanded,
    )
}

const SHAPES: [(usize, usize); 5] = [(1, 1), (4, 3), (16, 16), (64, 5), (200, 50)];

#[test]
fn f32_search_matches_scan_and_sort_reference() {
    const N: usize = 300;
    let tombstones = tombstones_ending_in_3(N);
    for (levels, seed) in [(None, 11u64), (Some(4u32), 12), (Some(2), 13)] {
        let fx = fixture(N, levels, seed);
        let mut rng = StdRng::seed_from_u64(seed + 100);
        let queries = rows(24, levels, &mut rng);
        for (qi, query) in queries.chunks(DIM).enumerate() {
            for with_tombstones in [false, true] {
                let ts = with_tombstones.then_some(tombstones.as_slice());
                let start = if with_tombstones {
                    live_start(qi, N)
                } else {
                    ((qi * 7) % N) as u32
                };
                for (l, k) in SHAPES {
                    let dist = |id: u32| l2_squared(query, row(&fx.vectors, DIM, id));
                    let (frontier, want_expanded) =
                        reference_search(&fx.adjacency, start, l.max(k), ts, &dist);
                    let want: Vec<(u32, f32)> = frontier.into_iter().take(k).collect();

                    let mut visited = VisitedSet::new(N);
                    let got = greedy_search_inner(
                        &fx.vectors,
                        DIM,
                        &fx.adjacency,
                        query,
                        start,
                        k,
                        l,
                        &mut visited,
                        ts,
                    );
                    let label = format!("seed={seed} q={qi} l={l} k={k} tomb={with_tombstones}");
                    assert_eq!(got.results, want, "f32 results diverged: {label}");
                    assert_eq!(got.expanded, want_expanded, "f32 order diverged: {label}");
                }
            }
        }
    }
}

#[test]
fn sq8_search_matches_scan_and_sort_reference() {
    const N: usize = 300;
    let tombstones = tombstones_ending_in_3(N);
    let mut runs_with_equal_sq8_distances = 0usize;
    for (levels, seed) in [(None, 21u64), (Some(4u32), 22), (Some(2), 23)] {
        let fx = fixture(N, levels, seed);
        let codec = GsSq8Codec::train_flat(&fx.vectors, DIM);
        let encoded: Vec<GsEncodedVector> =
            fx.vectors.chunks(DIM).map(|v| codec.encode(v)).collect();
        let mut rng = StdRng::seed_from_u64(seed + 100);
        let queries = rows(24, levels, &mut rng);
        for (qi, query) in queries.chunks(DIM).enumerate() {
            let query_enc = codec.encode(query);
            for with_tombstones in [false, true] {
                let ts = with_tombstones.then_some(tombstones.as_slice());
                let start = if with_tombstones {
                    live_start(qi, N)
                } else {
                    ((qi * 7) % N) as u32
                };
                for (l, k) in SHAPES {
                    let dist =
                        |id: u32| codec.l2_sq_codes(&query_enc.codes, &encoded[id as usize].codes);
                    let (frontier, want_expanded) =
                        reference_search(&fx.adjacency, start, l.max(k), ts, &dist);
                    if frontier.windows(2).any(|w| w[0].1 == w[1].1) {
                        runs_with_equal_sq8_distances += 1;
                    }
                    // Acquisition order is (SQ8 distance, id); the exact f32 re-score of the
                    // final frontier decides the returned top-k.
                    let mut want: Vec<(u32, f32)> = frontier
                        .iter()
                        .map(|&(id, _)| (id, l2_squared(query, row(&fx.vectors, DIM, id))))
                        .collect();
                    want.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
                    want.truncate(k);

                    let mut visited = VisitedSet::new(N);
                    let got = greedy_search_inner_sq8(
                        &fx.vectors,
                        DIM,
                        CodesView::Owned(&encoded),
                        &codec,
                        &fx.adjacency,
                        query,
                        &query_enc.codes,
                        start,
                        k,
                        l,
                        &mut visited,
                        ts,
                    );
                    let label = format!("seed={seed} q={qi} l={l} k={k} tomb={with_tombstones}");
                    assert_eq!(got.results, want, "sq8 results diverged: {label}");
                    assert_eq!(got.expanded, want_expanded, "sq8 order diverged: {label}");
                }
            }
        }
    }
    assert!(
        runs_with_equal_sq8_distances > 0,
        "the grid fixtures must produce equal SQ8 distances or this test pins nothing about ties"
    );
}

/// Nodes 1..=4 share the query's u8 code (SQ8 distance 0, as does the start) but sit at
/// exact f32 distances that decrease with node id. Nodes 5 and 6 only stretch the codec range.
struct TieStar {
    vectors: Vec<f32>,
    adjacency: Vec<Vec<u32>>,
    codec: GsSq8Codec,
    encoded: Vec<GsEncodedVector>,
    query: Vec<f32>,
    query_enc: GsEncodedVector,
}

fn tie_star() -> TieStar {
    let vectors: Vec<f32> = vec![0.2, 0.2004, 0.2003, 0.2002, 0.2001, 0.0, 1.0];
    let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); vectors.len()];
    adjacency[0] = vec![1, 2, 3, 4];
    let codec = GsSq8Codec::train_flat(&vectors, 1);
    let encoded: Vec<GsEncodedVector> = vectors.iter().map(|&v| codec.encode(&[v])).collect();
    let query = vec![0.2f32];
    let query_enc = codec.encode(&query);
    for (node, code) in encoded.iter().enumerate().take(5) {
        assert_eq!(
            code.codes, query_enc.codes,
            "node {node} must collide with the query in u8 code space"
        );
    }
    TieStar {
        vectors,
        adjacency,
        codec,
        encoded,
        query,
        query_enc,
    }
}

fn search_tie_star(star: &TieStar, k: usize, search_list_size: usize) -> GreedySearchResult {
    let mut visited = VisitedSet::new(star.vectors.len());
    greedy_search_inner_sq8(
        &star.vectors,
        1,
        CodesView::Owned(&star.encoded),
        &star.codec,
        &star.adjacency,
        &star.query,
        &star.query_enc.codes,
        0,
        k,
        search_list_size,
        &mut visited,
        None,
    )
}

#[test]
fn sq8_equal_codes_expand_by_node_id_and_exact_rescore_decides_top_k() {
    let star = tie_star();
    let result = search_tie_star(&star, 3, 8);

    let expanded: Vec<u32> = result.expanded.iter().map(|&(id, _)| id).collect();
    assert_eq!(
        expanded,
        vec![0, 1, 2, 3, 4],
        "equal SQ8 codes expand by id"
    );

    // The closest by exact f32 are 0 (exact), 4, 3 even though 4 and 3 expanded last.
    let top: Vec<u32> = result.results.iter().map(|&(id, _)| id).collect();
    assert_eq!(top, vec![0, 4, 3]);
}

#[test]
fn sq8_equal_codes_are_truncated_by_node_id() {
    let star = tie_star();
    // Room for three: the start plus the two lowest-id tied neighbors. Nodes 3 and 4 are
    // closer by exact f32 but rank after them at equal SQ8 distance, so they never enter.
    let result = search_tie_star(&star, 2, 3);

    let expanded: Vec<u32> = result.expanded.iter().map(|&(id, _)| id).collect();
    assert_eq!(expanded, vec![0, 1, 2]);

    let top: Vec<u32> = result.results.iter().map(|&(id, _)| id).collect();
    assert_eq!(top, vec![0, 2]);
}

#[test]
fn sq8_search_is_repeatable_with_reused_and_fresh_scratch() {
    const N: usize = 300;
    let fx = fixture(N, Some(4), 31);
    let codec = GsSq8Codec::train_flat(&fx.vectors, DIM);
    let encoded: Vec<GsEncodedVector> = fx.vectors.chunks(DIM).map(|v| codec.encode(v)).collect();
    let mut rng = StdRng::seed_from_u64(32);
    let queries = rows(16, Some(4), &mut rng);

    let mut reused = VisitedSet::new(N);
    for (qi, query) in queries.chunks(DIM).enumerate() {
        let query_enc = codec.encode(query);
        let start = ((qi * 5) % N) as u32;
        let run = |visited: &mut VisitedSet| {
            greedy_search_inner_sq8(
                &fx.vectors,
                DIM,
                CodesView::Owned(&encoded),
                &codec,
                &fx.adjacency,
                query,
                &query_enc.codes,
                start,
                20,
                32,
                visited,
                None,
            )
        };
        let first = run(&mut reused);
        let fresh = run(&mut VisitedSet::new(N));
        let again = run(&mut reused);
        assert_eq!(first, fresh, "fresh scratch changed the result for q={qi}");
        assert_eq!(first, again, "reused scratch changed the result for q={qi}");
    }
}
