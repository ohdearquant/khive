use super::*;

#[test]
fn search_query_omits_updated_qualifier_with_no_floor() {
    assert_eq!(search_query(None), "sort:updated-asc");
}

#[test]
fn search_query_includes_inclusive_updated_floor() {
    assert_eq!(
        search_query(Some("2024-01-01T00:00:00Z")),
        "sort:updated-asc updated:>=2024-01-01T00:00:00Z"
    );
}

#[test]
fn fetch_limit_reserves_only_replayed_boundaries_and_caps_unbounded_budgets() {
    let mut checkpoint = PageCheckpoint::new("local", Some("A".into()));
    let budget = Budget { remaining: Some(1) };
    assert_eq!(page_fetch_limit(&budget, &checkpoint, Some("A")), 1);
    checkpoint.at_floor.insert(10, Uuid::nil());
    checkpoint.at_floor.insert(20, Uuid::nil());
    checkpoint.undated.insert(30, Uuid::nil());
    assert_eq!(page_fetch_limit(&budget, &checkpoint, Some("A")), 4);
    assert_eq!(page_fetch_limit(&budget, &checkpoint, Some("B")), 2);
    for remaining in [None, Some(2000), Some(u64::MAX)] {
        assert_eq!(
            page_fetch_limit(&Budget { remaining }, &checkpoint, Some("A")),
            PAGE_LIMIT
        );
    }
}

#[test]
fn a_full_reduced_page_does_not_prove_remote_exhaustion() {
    assert_eq!(
        decide_page_outcome(1, 1, None, Some("A"), true),
        PageOutcome::StopBudgetExhausted
    );
    assert_eq!(
        decide_page_outcome(2, 3, Some("A"), Some("A"), false),
        PageOutcome::WindowComplete
    );
}

#[test]
fn short_page_proves_window_complete_regardless_of_budget() {
    let outcome = decide_page_outcome(42, PAGE_LIMIT, None, Some("2024-01-01T00:00:00Z"), false);
    assert_eq!(outcome, PageOutcome::WindowComplete);
    assert!(page_outcome_proves_window_complete(outcome));

    // Even a page that runs out of budget mid-way is still a proof of
    // completeness if the page itself was short — the loop always
    // finishes sorting/processing the whole (short) page first.
    let outcome = decide_page_outcome(0, PAGE_LIMIT, None, None, true);
    assert_eq!(outcome, PageOutcome::WindowComplete);
}

/// This is the exact ADR-088 Amendment 1 scenario: a
/// full (`PAGE_LIMIT`-sized) page came back, but the local budget was
/// NOT exhausted (e.g. every record in the page already existed and
/// consumed no budget) and paging is still forced to stop because the
/// floor didn't move. `done` must be false here — the remote window is
/// not proven exhausted just because the local budget wasn't hit.
#[test]
fn full_page_with_stalled_floor_is_not_window_complete_even_with_budget_left() {
    let outcome = decide_page_outcome(PAGE_LIMIT, PAGE_LIMIT, Some("X"), Some("X"), false);
    assert_eq!(outcome, PageOutcome::StopFloorStalled);
    assert!(!page_outcome_proves_window_complete(outcome));
}

#[test]
fn full_page_with_advancing_floor_and_budget_left_continues() {
    let outcome = decide_page_outcome(PAGE_LIMIT, PAGE_LIMIT, Some("A"), Some("B"), false);
    assert_eq!(outcome, PageOutcome::Continue("B".to_string()));
    assert!(!page_outcome_proves_window_complete(outcome));
}

#[test]
fn full_page_with_exhausted_budget_stops_without_proving_completeness() {
    let outcome = decide_page_outcome(PAGE_LIMIT, PAGE_LIMIT, Some("A"), Some("B"), true);
    assert_eq!(outcome, PageOutcome::StopBudgetExhausted);
    assert!(!page_outcome_proves_window_complete(outcome));
}

#[test]
fn full_page_with_no_updated_at_stalls_rather_than_looping_forever() {
    let outcome = decide_page_outcome(PAGE_LIMIT, PAGE_LIMIT, Some("A"), None, false);
    assert_eq!(outcome, PageOutcome::StopFloorStalled);
}
