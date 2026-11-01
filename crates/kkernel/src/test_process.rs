//! Environment-interposing fixtures execute as the sole test in a child.

use khive_storage::test_support::run_exact_test_in_child;

const CHILD_TEST: &str = "KKERNEL_ISOLATED_TEST";

/// Call before fixture setup or environment changes. Only the exact child
/// continues; unrelated tests never share its environment or recovery lock.
pub(crate) fn run_in_child() -> bool {
    run_in_child_with_stack(None)
}

/// Reserve extra stack only for this isolated child, without changing sibling tests.
pub(crate) fn run_in_child_with_stack_size(stack_bytes: usize) -> bool {
    run_in_child_with_stack(Some(stack_bytes))
}

fn run_in_child_with_stack(stack_bytes: Option<usize>) -> bool {
    let mut fixture = None;
    let in_parent = run_exact_test_in_child(CHILD_TEST, false, |command| {
        let root = tempfile::tempdir().expect("isolated test fixture");
        let home = root.path().join("home");
        std::fs::create_dir(&home).expect("private child HOME");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", &home)
            // Native model initialization must not escape the checked child HOME.
            .env_remove("LATTICE_MODEL_CACHE")
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
            .env("KHIVE_LOCK", root.path().join("khived.recovery.lock"))
            .env(
                "KHIVE_RECOVERER_LOCK",
                root.path().join("khived.recoverer.lock"),
            )
            .env("KHIVE_PID", root.path().join("khived.pid"))
            .env("KHIVE_SOCKET", root.path().join("khived.sock"));
        if let Some(stack_bytes) = stack_bytes {
            command.env("RUST_MIN_STACK", stack_bytes.to_string());
        }
        // The fixture must outlive the child that runs inside the call.
        fixture = Some(root);
    });
    if !in_parent {
        return false;
    }
    let root = fixture.expect("the parent created the isolated fixture");
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    assert!(
        std::fs::read_dir(root.path().join("home"))
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
    fn requested_stack_size_is_child_local() {
        let original_stack = std::env::var_os("RUST_MIN_STACK");
        const STACK_BYTES: usize = 4 * 1024 * 1024;
        if super::run_in_child_with_stack_size(STACK_BYTES) {
            assert_eq!(std::env::var_os("RUST_MIN_STACK"), original_stack);
            return;
        }
        assert_eq!(
            std::env::var("RUST_MIN_STACK").ok(),
            Some(STACK_BYTES.to_string()),
            "isolated child must receive its requested stack reservation"
        );
    }

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
        assert!(
            std::fs::read_dir(&home).unwrap().next().is_none(),
            "acquiring the private lock must not create default HOME paths"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rendezvous_paths_share_the_private_fixture() {
        if super::run_in_child() {
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let root = home.parent().unwrap();
        for key in [
            "KHIVE_LOCK",
            "KHIVE_RECOVERER_LOCK",
            "KHIVE_PID",
            "KHIVE_SOCKET",
        ] {
            let path = std::path::PathBuf::from(std::env::var_os(key).expect("private path"));
            assert_eq!(
                path.parent(),
                Some(root),
                "{key} must use the private fixture"
            );
            assert!(!path.starts_with(&home), "{key} must stay outside HOME");
        }
        let marker = khive_runtime::daemon::supervisor_marker_path();
        assert_eq!(marker.parent(), Some(root));
        assert!(!marker.starts_with(&home));
        let _guard = khive_runtime::daemon::acquire_recovery_lock().expect("private recovery lock");
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());
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
