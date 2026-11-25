use super::{delete_refused, listing_names};

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
fn a_delete_settles_as_refused_only_on_the_compare() {
    let moved = format!("error: cannot lock ref '{REF}': is at {B} but expected {A}\n");
    let missing =
        format!("error: cannot lock ref '{REF}': reference is missing but expected {A}\n");
    assert!(delete_refused(REF, A, moved.as_bytes()));
    assert!(delete_refused(REF, A, missing.as_bytes()));
    assert!(delete_refused(
        REF,
        &A.to_ascii_uppercase(),
        moved.as_bytes()
    ));
    // The dispatch in the shared runner reaches the same test for the delete's own argv.
    assert!(super::super::cas_refused(
        &["update-ref", "--no-deref", "-d", REF, A],
        moved.as_bytes()
    ));
    // Every other failure leaves the outcome open.
    for stderr in [
        format!("error: cannot lock ref '{REF}': Unable to create '/clone/.git/{REF}.lock': File exists.\n"),
        format!("error: cannot lock ref '{REF}': is at {A} but expected {A}\n"),
        format!("error: cannot lock ref 'refs/heads/other': is at {B} but expected {A}\n"),
        format!("error: cannot lock ref '{REF}': is at {B} but expected {A}\nmore\n"),
        format!("fatal: update_ref failed for ref '{REF}': cannot lock ref '{REF}': is at {B} but expected {A}\n"),
        format!("error: cannot lock ref '{REF}': is at short but expected {A}\n"),
        String::new(),
    ] {
        assert!(!delete_refused(REF, A, stderr.as_bytes()), "{stderr:?}");
    }
    assert!(!delete_refused(REF, A, b"\xff\xfe"));
}
