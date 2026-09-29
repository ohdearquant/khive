mod timing {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test_support/timing.rs"
    ));
}

use std::ffi::OsStr;
use std::time::Duration;

#[test]
fn tight_bound_is_selected_without_instrumentation() {
    let tight = Duration::from_millis(500);
    let relaxed = Duration::from_millis(2_500);

    assert_eq!(
        timing::select_duration_bound(tight, Some(relaxed), None),
        Some(tight)
    );
    assert_eq!(
        timing::select_duration_bound(tight, None, None),
        Some(tight)
    );
    assert_eq!(timing::duration_bound(tight, Some(tight)), Some(tight));
}

#[test]
fn instrumentation_presence_selects_relaxed_or_skipped_bound() {
    let tight = Duration::from_millis(500);
    let relaxed = Duration::from_millis(2_500);

    for profile_file in [OsStr::new(""), OsStr::new("profile-%p.profraw")] {
        assert_eq!(
            timing::select_duration_bound(tight, Some(relaxed), Some(profile_file)),
            Some(relaxed)
        );
        assert_eq!(
            timing::select_duration_bound(tight, None, Some(profile_file)),
            None
        );
    }
}
