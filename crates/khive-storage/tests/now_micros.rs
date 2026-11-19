//! `now_micros` reads the wall clock in microseconds since the Unix epoch.

#[test]
fn now_micros_lies_between_two_direct_clock_reads() {
    let before = chrono::Utc::now().timestamp_micros();
    let observed = khive_storage::now_micros();
    let after = chrono::Utc::now().timestamp_micros();

    assert!(
        (before..=after).contains(&observed),
        "now_micros() returned {observed}, outside the clock reads [{before}, {after}]"
    );
}
