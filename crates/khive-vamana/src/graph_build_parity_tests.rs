//! Fixed-seed graph parity against fresh scratch for every proposal.

use super::*;

fn legacy_build(vectors: &[f32], config: &VamanaConfig) -> Result<VamanaGraph> {
    config.validate()?;
    let num_vectors = validate_vectors(vectors, config.dimensions)?;

    if num_vectors > u32::MAX as usize {
        return Err(VamanaError::TooManyVectors { count: num_vectors });
    }

    // Read batch size from env at runtime so vec_bench can tune it without recompiling.
    let batch_size: usize = std::env::var("KHIVE_BUILD_BATCH")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(BUILD_BATCH_SIZE);
    let batch_size = batch_size.max(1);

    let medoid = select_medoid(vectors, config.dimensions, num_vectors)?;
    let mut adjacency = initial_random_adjacency(num_vectors, config.max_degree)?;

    let mut rng = StdRng::seed_from_u64(BUILD_SEED ^ 0x0101_0101_0101_0101);
    let mut order: Vec<u32> = (0..num_vectors as u32).collect();
    order.shuffle(&mut rng);

    let alphas = refinement_alpha_schedule(config.alpha);
    for &pass_alpha in &alphas {
        for batch in order.chunks(batch_size) {
            // L1: capture only the current neighbors of the batch nodes (O(batch*R))
            // instead of cloning the full adjacency (O(N)). The greedy search reads
            // `adjacency` directly — this is safe because adjacency is not mutated
            // until after all proposals are collected (the par_iter below is read-only).
            let batch_prior: Vec<Vec<u32>> = batch
                .iter()
                .map(|&node| adjacency[node as usize].clone())
                .collect();

            let propose = |(&node, prior_neighbors): (&u32, &Vec<u32>)| {
                let mut visited = VisitedSet::new(num_vectors);
                let query = row(vectors, config.dimensions, node);
                let search = greedy_search_inner(
                    vectors,
                    config.dimensions,
                    &adjacency,
                    query,
                    medoid,
                    config.max_degree,
                    config.search_list_size,
                    &mut visited,
                    None, // no tombstones during build
                );

                let mut candidates: Vec<u32> = search
                    .expanded
                    .iter()
                    .map(|(id, _)| *id)
                    .chain(search.results.iter().map(|(id, _)| *id))
                    .chain(prior_neighbors.iter().copied())
                    .collect();
                sort_dedup_u32(&mut candidates);

                let neighbors = robust_prune_inner(
                    vectors,
                    config.dimensions,
                    node,
                    candidates,
                    pass_alpha,
                    config.max_degree,
                );

                (node, neighbors)
            };
            #[cfg(feature = "parallel")]
            let proposals: Vec<(u32, Vec<u32>)> = batch
                .par_iter()
                .zip(batch_prior.par_iter())
                .map(propose)
                .collect();
            #[cfg(not(feature = "parallel"))]
            let proposals: Vec<(u32, Vec<u32>)> =
                batch.iter().zip(batch_prior.iter()).map(propose).collect();

            for (node, neighbors) in &proposals {
                adjacency[*node as usize] = neighbors.clone();
            }

            // L3: build sparse backedge map (O(batch*R)) instead of an N-length
            // array + full par_iter_mut over all N entries.
            // BTreeMap preserves insertion-key order so backedge application is
            // deterministic across runs (HashMap order is non-deterministic).
            let mut backedges: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for (source, neighbors) in &proposals {
                for &target in neighbors {
                    if target != *source {
                        backedges.entry(target).or_default().push(*source);
                    }
                }
            }

            for (target, sources) in backedges {
                let neighbors = &mut adjacency[target as usize];
                for source in sources {
                    if !neighbors.contains(&source) {
                        neighbors.push(source);
                    }
                }
                if neighbors.len() > config.max_degree {
                    let candidates = std::mem::take(neighbors);
                    *neighbors = robust_prune_inner(
                        vectors,
                        config.dimensions,
                        target,
                        candidates,
                        pass_alpha,
                        config.max_degree,
                    );
                }
            }
        }
    }

    for list in &mut adjacency {
        sort_dedup_u32(list);
        list.truncate(config.max_degree);
    }

    let reverse_adj = build_reverse_adj(&adjacency);
    Ok(VamanaGraph {
        adjacency,
        reverse_adj,
        medoid,
    })
}

fn legacy_build_sq8(
    vectors: &[f32],
    encoded: CodesView<'_>,
    codec: &GsSq8Codec,
    config: &VamanaConfig,
) -> Result<VamanaGraph> {
    config.validate()?;
    let num_vectors = validate_vectors(vectors, config.dimensions)?;
    if encoded.len() != num_vectors {
        return Err(VamanaError::DimensionMismatch {
            expected: num_vectors,
            actual: encoded.len(),
        });
    }

    if num_vectors > u32::MAX as usize {
        return Err(VamanaError::TooManyVectors { count: num_vectors });
    }

    let batch_size: usize = std::env::var("KHIVE_BUILD_BATCH")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(BUILD_BATCH_SIZE);
    let batch_size = batch_size.max(1);

    let medoid = select_medoid(vectors, config.dimensions, num_vectors)?;
    let mut adjacency = initial_random_adjacency(num_vectors, config.max_degree)?;

    let mut rng = StdRng::seed_from_u64(BUILD_SEED ^ 0x0101_0101_0101_0101);
    let mut order: Vec<u32> = (0..num_vectors as u32).collect();
    order.shuffle(&mut rng);

    let alphas = refinement_alpha_schedule(config.alpha);
    for &pass_alpha in &alphas {
        for batch in order.chunks(batch_size) {
            let batch_prior: Vec<Vec<u32>> = batch
                .iter()
                .map(|&node| adjacency[node as usize].clone())
                .collect();

            let propose = |(&node, prior_neighbors): (&u32, &Vec<u32>)| {
                let mut visited = VisitedSet::new(num_vectors);
                let query = row(vectors, config.dimensions, node);
                let query_enc = encoded.code(node as usize);
                let search = greedy_search_inner_sq8(
                    vectors,
                    config.dimensions,
                    encoded,
                    codec,
                    &adjacency,
                    query,
                    query_enc,
                    medoid,
                    config.max_degree,
                    config.search_list_size,
                    &mut visited,
                    None,
                );

                let mut candidates: Vec<u32> = search
                    .expanded
                    .iter()
                    .map(|(id, _)| *id)
                    .chain(search.results.iter().map(|(id, _)| *id))
                    .chain(prior_neighbors.iter().copied())
                    .collect();
                sort_dedup_u32(&mut candidates);

                let neighbors = robust_prune_inner_sq8(
                    vectors,
                    config.dimensions,
                    encoded,
                    codec,
                    node,
                    candidates,
                    pass_alpha,
                    config.max_degree,
                );

                (node, neighbors)
            };
            #[cfg(feature = "parallel")]
            let proposals: Vec<(u32, Vec<u32>)> = batch
                .par_iter()
                .zip(batch_prior.par_iter())
                .map(propose)
                .collect();
            #[cfg(not(feature = "parallel"))]
            let proposals: Vec<(u32, Vec<u32>)> =
                batch.iter().zip(batch_prior.iter()).map(propose).collect();

            for (node, neighbors) in &proposals {
                adjacency[*node as usize] = neighbors.clone();
            }

            let mut backedges: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for (source, neighbors) in &proposals {
                for &target in neighbors {
                    if target != *source {
                        backedges.entry(target).or_default().push(*source);
                    }
                }
            }

            for (target, sources) in backedges {
                let neighbors = &mut adjacency[target as usize];
                for source in sources {
                    if !neighbors.contains(&source) {
                        neighbors.push(source);
                    }
                }
                if neighbors.len() > config.max_degree {
                    let candidates = std::mem::take(neighbors);
                    *neighbors = robust_prune_inner_sq8(
                        vectors,
                        config.dimensions,
                        encoded,
                        codec,
                        target,
                        candidates,
                        pass_alpha,
                        config.max_degree,
                    );
                }
            }
        }
    }

    for list in &mut adjacency {
        sort_dedup_u32(list);
        list.truncate(config.max_degree);
    }

    let reverse_adj = build_reverse_adj(&adjacency);
    Ok(VamanaGraph {
        adjacency,
        reverse_adj,
        medoid,
    })
}

fn fixed_vectors(count: usize, dimensions: usize, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut vectors: Vec<f32> = (0..count * dimensions)
        .map(|_| rng.gen_range(-1.0..1.0))
        .collect();
    for vector in vectors.chunks_exact_mut(dimensions) {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in vector {
            *value /= norm;
        }
    }
    vectors
}

fn adjacency_bytes(graph: &VamanaGraph) -> Vec<u8> {
    let mut bytes = graph.medoid.to_le_bytes().to_vec();
    for adjacency in [&graph.adjacency, &graph.reverse_adj] {
        bytes.extend_from_slice(&(adjacency.len() as u64).to_le_bytes());
        for neighbors in adjacency {
            bytes.extend_from_slice(&(neighbors.len() as u64).to_le_bytes());
            for neighbor in neighbors {
                bytes.extend_from_slice(&neighbor.to_le_bytes());
            }
        }
    }
    bytes
}

type GraphBytes = Vec<(String, Vec<u8>)>;

fn for_each_worker_count<F: Fn() -> (GraphBytes, GraphBytes) + Send + Sync>(
    check: F,
) -> (GraphBytes, GraphBytes) {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    #[cfg(feature = "parallel")]
    for workers in [1, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap();
        let (actual_rows, expected_rows) = pool.install(&check);
        actual.extend(actual_rows);
        expected.extend(expected_rows);
    }
    #[cfg(not(feature = "parallel"))]
    {
        let (actual_rows, expected_rows) = check();
        actual.extend(actual_rows);
        expected.extend(expected_rows);
    }
    (actual, expected)
}

#[test]
fn both_builders_scratch_reuse_match_legacy_adjacency_bytes() {
    let vectors = fixed_vectors(173, 12, 0x3847);
    let codec = GsSq8Codec::train_flat(&vectors, 12);
    let encoded = codec.encode_flat_par(&vectors, 12);
    let flat: Vec<u8> = encoded
        .iter()
        .flat_map(|vector| vector.codes.iter().copied())
        .collect();
    let (actual, expected) = for_each_worker_count(|| {
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        for alpha in [1.0, 1.2] {
            let config = VamanaConfig::with_dimensions(12)
                .with_max_degree(8)
                .with_search_list_size(20)
                .with_alpha(alpha);
            let name = format!("f32 alpha={alpha}");
            expected.push((
                name.clone(),
                adjacency_bytes(&legacy_build(&vectors, &config).unwrap()),
            ));
            actual.push((
                name,
                adjacency_bytes(&VamanaGraph::build(&vectors, &config).unwrap()),
            ));
            for (layout, codes) in [
                ("owned", CodesView::Owned(&encoded)),
                (
                    "flat",
                    CodesView::Flat {
                        bytes: &flat,
                        dims: 12,
                    },
                ),
            ] {
                let name = format!("sq8 {layout} alpha={alpha}");
                expected.push((
                    name.clone(),
                    adjacency_bytes(&legacy_build_sq8(&vectors, codes, &codec, &config).unwrap()),
                ));
                actual.push((
                    name,
                    adjacency_bytes(
                        &VamanaGraph::build_sq8(&vectors, codes, &codec, &config).unwrap(),
                    ),
                ));
            }
        }
        (actual, expected)
    });
    // Both builders finish before this assertion, including both SQ8 layouts.
    assert_eq!(
        actual, expected,
        "scratch reuse changed ordered graph adjacency bytes"
    );
}
