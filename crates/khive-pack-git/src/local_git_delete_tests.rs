use super::{delete_outcome, listing_names, DeleteOutcome};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REF: &str = "refs/heads/work/one";

#[test]
fn a_listing_names_a_branch_only_through_a_branch_attribute() {
    let listing = format!(
        "worktree /clone\0HEAD {A}\0branch {REF}\0\0worktree /clone/other\0HEAD {B}\0detached\0\0"
    );
    assert!(listing_names(listing.as_bytes(), REF));
    assert!(!listing_names(listing.as_bytes(), "refs/heads/work/on"));
    assert!(!listing_names(listing.as_bytes(), "refs/heads/work"));
    // A worktree whose path holds a line break and a branch line is still one path attribute.
    let forged = format!("worktree /tmp/x\nbranch {REF}\0HEAD {B}\0detached\0\0");
    assert!(!listing_names(forged.as_bytes(), REF));
    assert!(!listing_names(b"", REF));
}

#[test]
fn a_delete_settles_from_the_transactions_status_lines_alone() {
    assert_eq!(
        delete_outcome(0, b"start: ok\nprepare: ok\ncommit: ok\n"),
        DeleteOutcome::Deleted
    );
    // Stopped before the prepare: no ref changed.
    assert_eq!(
        delete_outcome(128, b"start: ok\n"),
        DeleteOutcome::NotCommitted
    );
    assert_eq!(delete_outcome(128, b""), DeleteOutcome::NotCommitted);
    // Prepared and then failed, or a reply it does not know: the outcome stays open.
    for (code, stdout) in [
        (128, &b"start: ok\nprepare: ok\n"[..]),
        (128, b"start: ok\nprepare: ok\ncommit: ok\n"),
        (128, b"start: ok\nstart: ok\n"),
        (128, b"start: ok"),
        (0, b"start: ok\nprepare: ok\n"),
        (0, b""),
        (1, b"start: ok\nprepare: ok\n"),
    ] {
        assert_eq!(
            delete_outcome(code, stdout),
            DeleteOutcome::Unknown,
            "{code} {stdout:?}"
        );
    }
}
