//! Private process environment for runtime fixtures that change daemon paths.

use khive_storage::test_support::run_exact_test_in_child;

const CHILD_TEST: &str = "KHIVE_RUNTIME_ISOLATED_TEST";

pub(crate) fn run_in_child() -> bool {
    let mut child_fixture = None;
    let in_parent = run_exact_test_in_child(CHILD_TEST, false, |command| {
        let fixture = tempfile::Builder::new()
            .prefix("kh-rt-")
            .tempdir()
            .expect("runtime child fixture");
        let home = fixture.path().join("home");
        std::fs::create_dir(&home).expect("empty child HOME");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", &home)
            .env_remove("LATTICE_MODEL_CACHE")
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_VOLUME_LOCK_DIR", fixture.path().join("volume-locks"))
            .env("KHIVE_LOCK", fixture.path().join("boot.lock"))
            .env(
                "KHIVE_RECOVERER_LOCK",
                fixture.path().join("recoverer.lock"),
            )
            .env("KHIVE_SOCKET", fixture.path().join("s"))
            .env("KHIVE_PID", fixture.path().join("p"));
        // The fixture must outlive the child that runs inside the call.
        child_fixture = Some(fixture);
    });
    if !in_parent {
        return false;
    }
    let fixture = child_fixture.expect("the parent created the runtime child fixture");
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    assert!(
        std::fs::read_dir(fixture.path().join("home"))
            .expect("read child HOME")
            .next()
            .is_none(),
        "runtime child must leave its private HOME empty: {name}"
    );
    true
}
