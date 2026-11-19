use super::*;
use std::panic::{self, AssertUnwindSafe};

// The registry is a process-wide singleton, shared across every test in this
// binary (cargo runs `#[test]`s in parallel threads of the same process). A
// module-local lock serializes these tests so one test's entries can't be
// observed as another's "oldest" or leak into another's snapshot assertion.
//
// Acquired with the same poison recovery the registry itself uses. A failing
// assertion panics while holding this lock, so under `.unwrap()` every later
// test in the module dies of `PoisonError` instead of running: one real
// failure presents as a dozen, and the reader has to work out which one is
// the signal. Measured — a single-predicate mutation reddened 13 of 15 tests,
// 10 of them purely as poison cascade.
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn register_reports_oldest_with_label() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let handle = register(Some("test_span".to_string()));
    let (_, _, label) = oldest().expect("expected an open entry");
    assert_eq!(label.as_deref(), Some("test_span"));
    drop(handle);
}

#[test]
fn drop_deregisters() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let handle = register(Some("drop_me".to_string()));
    let id_present_before = snapshot()
        .iter()
        .any(|(_, label)| label.as_deref() == Some("drop_me"));
    assert!(id_present_before);
    drop(handle);
    let id_present_after = snapshot()
        .iter()
        .any(|(_, label)| label.as_deref() == Some("drop_me"));
    assert!(!id_present_after);
}

#[test]
fn oldest_is_genuinely_oldest() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = register(Some("first".to_string()));
    std::thread::sleep(Duration::from_millis(5));
    let second = register(Some("second".to_string()));
    let (first_id, _, label) = oldest().expect("expected an open entry");
    assert_eq!(label.as_deref(), Some("first"));
    drop(first);
    let (second_id, _, label) = oldest().expect("expected an open entry");
    assert_eq!(label.as_deref(), Some("second"));
    assert_ne!(
        first_id, second_id,
        "distinct registrations must carry distinct TxIds"
    );
    drop(second);
}

#[test]
fn snapshot_contains_all_open_entries() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let a = register(Some("snap_a".to_string()));
    let b = register(Some("snap_b".to_string()));
    let labels: Vec<Option<String>> = snapshot().into_iter().map(|(_, label)| label).collect();
    assert!(labels.contains(&Some("snap_a".to_string())));
    assert!(labels.contains(&Some("snap_b".to_string())));
    drop(a);
    drop(b);
}

#[test]
fn oldest_for_partitions_by_origin() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let secondary_id = DbIdentity::new("secondary.db");
    let main_view = TxOriginFilter::Main(main_id.clone());
    let secondary_view = TxOriginFilter::Secondary(secondary_id.clone());

    let main_handle = register_scoped(
        Some("main_span".to_string()),
        TxOrigin::Database(main_id.clone()),
    );
    let secondary_handle = register_scoped(
        Some("secondary_span".to_string()),
        TxOrigin::Database(secondary_id.clone()),
    );

    let main_oldest = oldest_for(&main_view).expect("main span visible in main view");
    assert_eq!(main_oldest.label.as_deref(), Some("main_span"));
    assert_eq!(main_oldest.origin, TxOrigin::Database(main_id));

    let secondary_oldest =
        oldest_for(&secondary_view).expect("secondary span visible in secondary view");
    assert_eq!(secondary_oldest.label.as_deref(), Some("secondary_span"));

    drop(main_handle);
    drop(secondary_handle);
}

#[test]
fn register_delegates_to_unscoped_origin() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id);
    let handle = register(Some("legacy_span".to_string()));

    let oldest = oldest_for(&main_view).expect("register() delegates to Unscoped");
    assert_eq!(oldest.origin, TxOrigin::Unscoped);

    drop(handle);
}

#[test]
fn unscoped_visible_in_main_view_and_oldest() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id);
    let handle = register_scoped(Some("unscoped_span".to_string()), TxOrigin::Unscoped);

    let via_filter = oldest_for(&main_view).expect("unscoped span falls back to main view");
    assert_eq!(via_filter.label.as_deref(), Some("unscoped_span"));
    assert_eq!(via_filter.origin, TxOrigin::Unscoped);

    let via_aggregate = oldest().expect("unscoped span visible in aggregate oldest()");
    assert_eq!(via_aggregate.2.as_deref(), Some("unscoped_span"));

    drop(handle);
}

#[test]
fn memory_absent_from_attribution_views_but_present_in_oldest() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let secondary_id = DbIdentity::new("secondary.db");
    let main_view = TxOriginFilter::Main(main_id);
    let secondary_view = TxOriginFilter::Secondary(secondary_id);
    let handle = register_scoped(Some("memory_span".to_string()), TxOrigin::Memory);

    assert!(oldest_for(&main_view).is_none());
    assert!(oldest_for(&secondary_view).is_none());

    let via_aggregate = oldest().expect("memory span visible in aggregate oldest()");
    assert_eq!(via_aggregate.2.as_deref(), Some("memory_span"));

    drop(handle);
}

#[test]
fn database_entries_never_leak_across_views() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let secondary_id = DbIdentity::new("secondary.db");
    let main_view = TxOriginFilter::Main(main_id);
    let secondary_view = TxOriginFilter::Secondary(secondary_id.clone());
    let handle = register_scoped(
        Some("secondary_span".to_string()),
        TxOrigin::Database(secondary_id),
    );

    assert!(oldest_for(&main_view).is_none());
    let via_secondary =
        oldest_for(&secondary_view).expect("secondary span visible in its own view");
    assert_eq!(via_secondary.label.as_deref(), Some("secondary_span"));

    drop(handle);
}

#[test]
fn panic_inside_scope_still_deregisters() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let _handle = register(Some("panics".to_string()));
        panic!("boom");
    }));
    assert!(result.is_err());
    let still_present = snapshot()
        .iter()
        .any(|(_, label)| label.as_deref() == Some("panics"));
    assert!(!still_present);
}

#[test]
fn any_open_labeled_matches_only_the_named_label() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id.clone());
    let handle = register_scoped(
        Some("wanted".to_string()),
        TxOrigin::Database(main_id.clone()),
    );

    assert!(any_open_labeled(&main_view, "wanted"));
    assert!(!any_open_labeled(&main_view, "other"));
    assert!(!any_open_labeled(&main_view, "wante"));

    drop(handle);
    assert!(!any_open_labeled(&main_view, "wanted"));
}

/// The whole reason this predicate exists: an unrelated span open on the
/// same backend satisfies [`oldest_for`] while saying nothing about the
/// span the caller named. A caller asking "is *my* span still open" must
/// not read a different span's presence as its own.
#[test]
fn any_open_labeled_ignores_unrelated_spans_that_oldest_for_reports() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id.clone());
    let unrelated = register_scoped(
        Some("someone_elses_write".to_string()),
        TxOrigin::Database(main_id.clone()),
    );

    assert!(
        oldest_for(&main_view).is_some(),
        "aggregate must observe the unrelated span — otherwise this test \
             proves nothing about the discrimination"
    );
    assert!(!any_open_labeled(&main_view, "mine"));

    drop(unrelated);
}

#[test]
fn any_open_labeled_survives_duplicate_labels() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id.clone());
    let first = register_scoped(Some("dup".to_string()), TxOrigin::Database(main_id.clone()));
    let second = register_scoped(Some("dup".to_string()), TxOrigin::Database(main_id.clone()));

    assert!(any_open_labeled(&main_view, "dup"));
    drop(first);
    assert!(
        any_open_labeled(&main_view, "dup"),
        "one of two same-labeled spans closing must not read as all of them closing"
    );
    drop(second);
    assert!(!any_open_labeled(&main_view, "dup"));
}

#[test]
fn any_open_labeled_never_matches_an_unlabeled_span() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let main_view = TxOriginFilter::Main(main_id.clone());
    let unlabeled = register_scoped(None, TxOrigin::Database(main_id.clone()));

    assert!(
        oldest_for(&main_view).is_some(),
        "the unlabeled span must be registered for this test to discriminate"
    );
    assert!(!any_open_labeled(&main_view, "anything"));

    drop(unlabeled);
}

#[test]
fn any_open_labeled_partitions_by_origin() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let main_id = DbIdentity::new("main.db");
    let secondary_id = DbIdentity::new("secondary.db");
    let main_view = TxOriginFilter::Main(main_id);
    let secondary_view = TxOriginFilter::Secondary(secondary_id.clone());
    let handle = register_scoped(
        Some("secondary_only".to_string()),
        TxOrigin::Database(secondary_id),
    );

    assert!(any_open_labeled(&secondary_view, "secondary_only"));
    assert!(!any_open_labeled(&main_view, "secondary_only"));

    drop(handle);
}
