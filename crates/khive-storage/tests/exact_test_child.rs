//! `run_exact_test_in_child` re-executes the test binary so a test can run
//! alone in its own process.
//!
//! Every case plays both roles. The parent calls the runner and checks what it
//! does; the re-executed child, recognised by the marker variable, carries out
//! the behaviour under test.

#![cfg(feature = "test-support")]

use std::panic::catch_unwind;
use std::process::Command;

use khive_storage::test_support::run_exact_test_in_child;

const MARKER: &str = "KHIVE_STORAGE_EXACT_CHILD_TEST";
const CHILD_REFUSAL: &str = "isolated child must run exactly its one named test";
const PARENT_REFUSAL: &str = "isolated test must execute exactly one passing case";

fn test_name() -> String {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    name.to_owned()
}

fn is_child(name: &str) -> bool {
    std::env::var(MARKER).ok().as_deref() == Some(name)
}

/// The runner reports `true` to the parent and `false` to the child.
fn assert_round_trip(include_ignored: bool) {
    let name = test_name();
    let in_parent = run_exact_test_in_child(MARKER, include_ignored, |_| {});
    assert!(
        in_parent != is_child(&name),
        "the runner must return true in the parent and false in the child"
    );
}

fn fail_the_child() {
    panic!("deliberate child failure");
}

fn exit_cleanly_without_the_verdict() {
    std::process::exit(0);
}

fn exit_nonzero_after_the_verdict() {
    println!("test result: ok. 1 passed; 0 failed;");
    std::process::exit(3);
}

/// The parent must see its assertion panic. The child, which the runner lets
/// continue, then misbehaves in the way under test.
fn assert_parent_rejects_child(misbehave: fn()) {
    let name = test_name();
    let outcome = catch_unwind(|| run_exact_test_in_child(MARKER, false, |_| {}));
    if is_child(&name) {
        assert!(
            !outcome.expect("the child path returns instead of panicking"),
            "the runner must return false in the child"
        );
        misbehave();
        return;
    }
    let payload = outcome.expect_err("the parent must reject the child");
    let message = payload
        .downcast_ref::<String>()
        .expect("the verdict assertion carries a formatted message");
    assert!(
        message.contains(PARENT_REFUSAL),
        "the parent must refuse for the verdict, not for another reason:\n{message}"
    );
}

/// A child whose arguments differ from the runner's own is refused before it
/// reaches the test body.
fn assert_forged_child_is_refused(include_ignored: bool, extra_arguments: &[&str]) {
    let name = test_name();
    if is_child(&name) {
        // Reaching the end of this test means the runner let the child continue.
        run_exact_test_in_child(MARKER, include_ignored, |_| {});
        return;
    }
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", name.as_str()])
        .args(extra_arguments)
        .env(MARKER, &name)
        .output()
        .expect("spawn forged child");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "a forged child must be rejected before the test body:\n{text}"
    );
    assert!(
        text.contains(CHILD_REFUSAL),
        "the child must say why it was rejected:\n{text}"
    );
}

#[test]
fn passing_child_returns_true_in_the_parent_and_false_in_the_child() {
    assert_round_trip(false);
}

#[test]
fn include_ignored_child_returns_true_in_the_parent_and_false_in_the_child() {
    assert_round_trip(true);
}

#[test]
fn failing_child_makes_the_parent_assertion_panic() {
    assert_parent_rejects_child(fail_the_child);
}

#[test]
fn child_that_exits_cleanly_without_the_verdict_is_rejected() {
    assert_parent_rejects_child(exit_cleanly_without_the_verdict);
}

#[test]
fn child_that_exits_nonzero_after_the_verdict_is_rejected() {
    assert_parent_rejects_child(exit_nonzero_after_the_verdict);
}

#[test]
fn child_started_without_the_runner_arguments_panics() {
    assert_forged_child_is_refused(false, &[]);
}

#[test]
fn child_started_without_include_ignored_panics_when_the_runner_expects_it() {
    assert_forged_child_is_refused(true, &["--nocapture", "--test-threads=1"]);
}
