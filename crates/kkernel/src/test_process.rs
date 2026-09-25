//! Environment-interposing fixtures execute as the sole test in a child.

use std::process::Command;

const CHILD_TEST: &str = "KKERNEL_ISOLATED_TEST";

/// Call before fixture setup or environment changes. Only the exact child
/// continues; unrelated tests never share its environment or recovery lock.
pub(crate) fn run_in_child() -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    let arguments = ["--exact", name, "--nocapture", "--test-threads=1"];
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            arguments,
            "isolated child must run exactly its one named test"
        );
        return false;
    }

    let root = tempfile::tempdir().expect("isolated test fixture");
    let home = root.path().join("home");
    std::fs::create_dir(&home).expect("private child HOME");
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    let output = command
        .args(arguments)
        .env(CHILD_TEST, name)
        .env("HOME", &home)
        // Native model initialization must not escape the checked child HOME.
        .env_remove("LATTICE_MODEL_CACHE")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_LOCK", root.path().join("khived.recovery.lock"))
        .output()
        .expect("spawn isolated test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated test must execute exactly one passing case:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_dir(&home)
            .expect("read child HOME")
            .next()
            .is_none(),
        "isolated test must leave its child HOME empty: {name}"
    );
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn environment_changes_stay_in_the_child() {
        let original_home = std::env::var_os("HOME");
        let original_lock = std::env::var_os("KHIVE_LOCK");
        let original_cwd = std::env::current_dir().unwrap();
        if super::run_in_child() {
            assert_eq!(std::env::var_os("HOME"), original_home);
            assert_eq!(std::env::var_os("KHIVE_LOCK"), original_lock);
            assert_eq!(std::env::current_dir().unwrap(), original_cwd);
            return;
        }
        let other = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", other.path());
        std::env::remove_var("KHIVE_LOCK");
        std::env::set_current_dir(other.path()).unwrap();
        std::env::set_current_dir(original_cwd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_lock_is_private() {
        if super::run_in_child() {
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let lock = std::path::PathBuf::from(
            std::env::var_os("KHIVE_LOCK")
                .expect("isolated child must receive a private recovery lock"),
        );
        assert_eq!(
            home.parent(),
            lock.parent(),
            "child HOME and recovery lock must belong to the same private fixture"
        );
        assert!(
            !lock.starts_with(&home),
            "recovery lock must be outside HOME"
        );
        let _guard = khive_runtime::daemon::acquire_recovery_lock().expect("private recovery lock");
        assert!(lock.is_file(), "recovery lock must use the fixture path");
    }

    #[test]
    fn child_marker_requires_the_exact_test_arguments() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_process::tests::environment_changes_stay_in_the_child",
            ])
            .env("HOME", home.path())
            .env(
                super::CHILD_TEST,
                "test_process::tests::environment_changes_stay_in_the_child",
            )
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "a forged child marker must be rejected before fixture setup"
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("isolated child must run exactly its one named test"));
        assert!(std::fs::read_dir(home.path()).unwrap().next().is_none());
    }
}
