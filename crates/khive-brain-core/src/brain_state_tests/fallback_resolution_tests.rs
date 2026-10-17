/// `sort_fallback_candidates` produces `(created_at ASC, id ASC)` order.
/// See crates/khive-brain-core/docs/testing-strategy.md#brain_staters-sort_fallback_candidates_produces_created_at_then_id_order
#[test]
fn sort_fallback_candidates_produces_created_at_then_id_order() {
    // Three profiles inserted in DESCENDING created_at order (worst case for
    // any unsorted implementation).
    let p1 = make_profile("aardvark", "recall", 1_000); // earliest, lowest id
    let p2 = make_profile("mango", "recall", 2_000);
    let p3 = make_profile("zebra", "recall", 3_000); // latest, highest id

    // Intentionally unsorted: reverse order.
    let mut candidates = vec![&p3, &p2, &p1];
    sort_fallback_candidates(&mut candidates);

    // After sort: p1 (earliest) must be first, p3 (latest) must be last.
    assert_eq!(
        candidates[0].id, "aardvark",
        "earliest profile must come first"
    );
    assert_eq!(candidates[1].id, "mango");
    assert_eq!(candidates[2].id, "zebra", "latest profile must come last");
}

/// `sort_fallback_candidates` must break ties by `id` (ascending) when
/// `created_at` is equal.
#[test]
fn sort_fallback_candidates_breaks_ties_by_id() {
    let ts = 5_000i64;
    let p_z = make_profile("z-profile", "recall", ts);
    let p_a = make_profile("a-profile", "recall", ts);
    let p_m = make_profile("m-profile", "recall", ts);

    // Deliberately insert in reverse alphabetical order.
    let mut candidates = vec![&p_z, &p_m, &p_a];
    sort_fallback_candidates(&mut candidates);

    assert_eq!(
        candidates[0].id, "a-profile",
        "lowest id must come first on equal timestamps"
    );
    assert_eq!(candidates[1].id, "m-profile");
    assert_eq!(candidates[2].id, "z-profile");
}

/// `BrainState::resolve` selects the earliest-created profile regardless
/// of HashMap insertion order (integration guard on `sort_fallback_candidates`).
#[test]
fn resolve_fallback_is_deterministic() {
    let p_early = make_profile("alpha", "recall", 1_000);
    let p_later = make_profile("zeta", "recall", 2_000);

    let mut state_a = BrainState {
        profiles: HashMap::new(),
        balanced_recall: BalancedRecallState::new(8),
        profile_states: HashMap::new(),
        bindings: Vec::new(),
        section_states: HashMap::new(),
        router_state: HashMap::new(),
        adapter_set: HashMap::new(),
        signals_applied: AtomicU64::new(0),
        snapshot_serializations: AtomicU64::new(0),
    };
    state_a.profiles.insert(p_early.id.clone(), p_early.clone());
    state_a.profiles.insert(p_later.id.clone(), p_later.clone());

    let mut state_b = BrainState {
        profiles: HashMap::new(),
        balanced_recall: BalancedRecallState::new(8),
        profile_states: HashMap::new(),
        bindings: Vec::new(),
        section_states: HashMap::new(),
        router_state: HashMap::new(),
        adapter_set: HashMap::new(),
        signals_applied: AtomicU64::new(0),
        snapshot_serializations: AtomicU64::new(0),
    };
    // Insert in the opposite order.
    state_b.profiles.insert(p_later.id.clone(), p_later.clone());
    state_b.profiles.insert(p_early.id.clone(), p_early.clone());

    let result_a = state_a.resolve(None, None, "recall");
    let result_b = state_b.resolve(None, None, "recall");

    let id_a = result_a.map(|p| p.id.clone());
    let id_b = result_b.map(|p| p.id.clone());
    assert_eq!(id_a, Some("alpha".to_owned()));
    assert_eq!(id_a, id_b, "fallback resolution must be deterministic");
}
