#[test]
fn worked_rrf60_example_votes_once_per_variant_and_matches_shared_primitive() {
    // B: 1/61 + 1/62; A: 1/61 + 1/63; D: 1/62; C: 1/64.
    // A's duplicate retains C's rank four, and A's dual-channel provenance adds no outer vote.
    let original = ids(&[10, 20, 10, 30]);
    let generated = ids(&[20, 40, 10]);
    let ann = ids(&[10]);
    let lists = [
        list(0, &original, &original, &ann),
        list(1, &generated, &generated, &[]),
    ];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(order(&admitted), ids(&[20, 10, 40, 30]));
    let sources = [&original, &generated]
        .into_iter()
        .map(|source| {
            source
                .iter()
                .map(|id| (*id, DeterministicScore::ZERO))
                .collect()
        })
        .collect();
    let expected: Vec<_> = reciprocal_rank_fusion(sources, 60)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(order(&admitted), expected);

    let without_ann = [list(0, &original, &original, &[]), lists[1]];
    let without_ann = merge_query_variants(&variants(), &without_ann, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(order(&admitted), order(&without_ann));
    assert_ne!(
        admitted[1].variant_attribution,
        without_ann[1].variant_attribution
    );
}

#[test]
fn duplicate_ranks_do_not_compress_later_candidates() {
    let repeated = ids(&[30, 30, 10]);
    let second = ids(&[30, 20]);
    let lists = [
        list(0, &repeated, &repeated, &[]),
        list(1, &second, &second, &[]),
    ];
    let admitted = merge_query_variants(&variants(), &lists, &limits())
        .unwrap()
        .unwrap();
    // 20's rank two beats 10's rank three; compression would tie them and put UUID 10 first.
    assert_eq!(order(&admitted), ids(&[30, 20, 10]));
}

#[test]
fn admission_uses_rrf60_rather_than_a_different_rrf_constant() {
    let a = Uuid::from_u128(900);
    let b = Uuid::from_u128(800);
    let mut original: Vec<_> = (10_000..10_100).map(Uuid::from_u128).collect();
    let mut generated: Vec<_> = (20_000..20_100).map(Uuid::from_u128).collect();
    original[0] = a;
    original[19] = b;
    generated[19] = b;
    generated[99] = a;
    let lists = [
        list(0, &original, &original, &[]),
        list(1, &generated, &generated, &[]),
    ];
    let cap = QueryVariantLimits::try_new(3, 256, 1_024, 192, 2).unwrap();
    let admitted = merge_query_variants(&variants(), &lists, &cap)
        .unwrap()
        .unwrap();
    // k=60: B's 2/80 exceeds A's 1/61 + 1/160; k=10 or k=1 would reverse them.
    // Every other ID occurs only once, so its contribution is below both A and B.
    assert_eq!(order(&admitted), vec![b, a]);
}

fn permutations<T: Copy>(items: &mut [T], start: usize, visit: &mut impl FnMut(&[T])) {
    if start == items.len() {
        visit(items);
    } else {
        for index in start..items.len() {
            items.swap(start, index);
            permutations(items, start + 1, visit);
            items.swap(start, index);
        }
    }
}

#[test]
fn every_outer_permutation_preserves_ids_occurrences_and_serialized_bytes() {
    let a = ids(&[4, 1, 9]);
    let b = ids(&[1, 4, 9]);
    let c = ids(&[9, 2]);
    let d = ids(&[2, 9]);
    let traces = [
        list(0, &a, &a, &b),
        list(1, &b, &b, &a),
        list(2, &c, &[], &c),
        list(3, &d, &d, &[]),
    ];
    for count in 1..=4 {
        let mut lists = traces[..count].to_vec();
        let expected = merge_query_variants(&variants(), &lists, &limits()).unwrap();
        let bytes = serde_json::to_vec(&expected).unwrap();
        let mut visited = 0;
        permutations(&mut lists, 0, &mut |permuted| {
            let actual = merge_query_variants(&variants(), permuted, &limits()).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(serde_json::to_vec(&actual).unwrap(), bytes);
            visited += 1;
        });
        assert_eq!(visited, (1..=count).product::<usize>());
    }
}

#[test]
fn equal_rrf_scores_use_canonical_uuid_order_including_across_uuid_fields() {
    let low = Uuid::parse_str("00000000-ffff-ffff-ffff-ffffffffffff").unwrap();
    let high = Uuid::parse_str("00000001-0000-0000-0000-000000000000").unwrap();
    let a = [high, low];
    let b = [low, high];
    let admitted = merge_query_variants(
        &variants(),
        &[list(0, &a, &a, &[]), list(1, &b, &b, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    assert!(low.to_string() < high.to_string());
    assert_eq!(order(&admitted), vec![low, high]);
}

#[test]
fn changing_inner_rank_order_is_a_different_input() {
    let forward = ids(&[20, 10]);
    let reverse = ids(&[10, 20]);
    let forward_hits = merge_query_variants(
        &variants(),
        &[list(0, &[], &[], &[]), list(1, &forward, &forward, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    let reverse_hits = merge_query_variants(
        &variants(),
        &[list(0, &[], &[], &[]), list(1, &reverse, &reverse, &[])],
        &limits(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(order(&forward_hits), forward);
    assert_eq!(order(&reverse_hits), reverse);
    assert_ne!(forward_hits, reverse_hits);
}
