use super::{fetch_refs, fetched_ref_of, lock_held, pull_number};

#[test]
fn fetch_refs_maps_a_branch_and_a_pull_request() {
    assert_eq!(
        fetch_refs("work/topic"),
        Some((
            "refs/heads/work/topic".to_string(),
            "refs/remotes/origin/work/topic".to_string()
        ))
    );
    assert_eq!(
        fetch_refs("pull/6828"),
        Some((
            "refs/pull/6828/head".to_string(),
            "refs/fetched/pull/6828".to_string()
        ))
    );
}

#[test]
fn fetch_refs_refuses_what_the_grammar_does_not_admit() {
    for refused in [
        "",
        "pull/",
        "pull/0",
        "pull/012",
        "pull/12a",
        "pull/+12",
        "pull/12345678901",
        "pull/12/head",
        "-x",
        "a..b",
        "a b",
        "topic.lock",
        "a@{1}",
        "/main",
    ] {
        assert_eq!(fetch_refs(refused), None, "{refused:?}");
    }
    // Ten digits is the bound, and a branch whose name only starts with "pull" is a branch.
    assert!(fetch_refs("pull/1234567890").is_some());
    assert_eq!(
        fetch_refs("pullup").map(|(_, destination)| destination),
        Some("refs/remotes/origin/pullup".to_string())
    );
}

#[test]
fn pull_numbers_are_digits_without_a_leading_zero() {
    assert_eq!(pull_number("7"), Some(7));
    assert_eq!(pull_number("9999999999"), Some(9_999_999_999));
    for refused in ["", "0", "07", "7 ", "٣", "12345678901"] {
        assert_eq!(pull_number(refused), None, "{refused:?}");
    }
}

#[test]
fn only_refs_a_fetch_writes_are_named_as_unfetched() {
    assert_eq!(
        fetched_ref_of("origin/main"),
        Some("refs/remotes/origin/main".to_string())
    );
    assert_eq!(
        fetched_ref_of("refs/remotes/origin/main"),
        Some("refs/remotes/origin/main".to_string())
    );
    assert_eq!(
        fetched_ref_of("refs/fetched/pull/12"),
        Some("refs/fetched/pull/12".to_string())
    );
    for other in [
        "main",
        "HEAD",
        "origin/",
        "refs/remotes/origin/",
        "refs/remotes/upstream/main",
        "refs/fetched/pull/012",
        "refs/heads/origin/main",
    ] {
        assert_eq!(fetched_ref_of(other), None, "{other:?}");
    }
}

#[test]
fn a_held_lock_is_told_from_a_moved_ref_and_from_other_failures() {
    let reference = "refs/remotes/origin/main";
    let locked = format!(
        "fatal: update_ref failed for ref '{reference}': cannot lock ref '{reference}': Unable to create '/clone/.git/{reference}.lock': File exists.\n\nAnother git process seems to be running in this repository.\n"
    );
    assert!(lock_held(reference, locked.as_bytes()));
    // Another ref's lock, a moved ref and an unrelated failure are not this ref's lock.
    assert!(!lock_held("refs/remotes/origin/other", locked.as_bytes()));
    let moved = format!(
        "fatal: update_ref failed for ref '{reference}': cannot lock ref '{reference}': is at {} but expected {}\n",
        "1".repeat(40),
        "2".repeat(40)
    );
    assert!(!lock_held(reference, moved.as_bytes()));
    assert!(!lock_held(reference, b"fatal: not a git repository\n"));
    assert!(!lock_held(reference, &[0xff, 0xfe]));
}
