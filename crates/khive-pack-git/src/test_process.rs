//! Environment-interposing cases run as the sole test in a child.
//! Their existing setup/restore guards cannot affect sibling test cases.

use std::process::{Command, Output};

const CHILD_TEST: &str = "KHIVE_GIT_ISOLATED_TEST";

pub(crate) fn command(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args([
            "--exact",
            name,
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_TEST, name);
    command
}

pub(crate) fn passed_one_test(output: &Output) -> bool {
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed; 0 failed;")
}

pub(crate) fn assert_success(output: &Output) {
    assert!(
        passed_one_test(output),
        "isolated test must execute exactly one passing case:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Call before any fixture setup, lock, or environment mutation. The parent
/// returns from its test after this completes; only the exact child continues.
pub(crate) fn run_in_child() -> bool {
    let thread = std::thread::current();
    run_in_child_with(command(
        thread.name().expect("libtest names its test threads"),
    ))
}

pub(crate) fn run_in_child_with(mut child: Command) -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    // An inherited marker for another case must not bypass isolation.
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            [
                "--exact",
                name,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1"
            ],
            "isolated child must run exactly its one named test before mutating the environment"
        );
        return false;
    }
    assert_success(&child.output().expect("spawn isolated test"));
    true
}
