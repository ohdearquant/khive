//! Private process environment for runtime fixtures that change daemon paths.

const CHILD_TEST: &str = "KHIVE_RUNTIME_ISOLATED_TEST";

pub(crate) fn run_in_child() -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    let arguments = ["--exact", name, "--nocapture", "--test-threads=1"];
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        assert_eq!(
            std::env::args().skip(1).collect::<Vec<_>>(),
            arguments,
            "runtime child must run exactly its one named test"
        );
        return false;
    }

    let fixture = tempfile::Builder::new()
        .prefix("kh-rt-")
        .tempdir()
        .expect("runtime child fixture");
    let home = fixture.path().join("home");
    std::fs::create_dir(&home).expect("empty child HOME");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    let output = command
        .args(arguments)
        .env(CHILD_TEST, name)
        .env("HOME", &home)
        .env_remove("LATTICE_MODEL_CACHE")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_LOCK", fixture.path().join("boot.lock"))
        .env(
            "KHIVE_RECOVERER_LOCK",
            fixture.path().join("recoverer.lock"),
        )
        .env("KHIVE_SOCKET", fixture.path().join("s"))
        .env("KHIVE_PID", fixture.path().join("p"))
        .output()
        .expect("spawn runtime child");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "runtime child must execute exactly one passing case:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_dir(&home)
            .expect("read child HOME")
            .next()
            .is_none(),
        "runtime child must leave its private HOME empty: {name}"
    );
    true
}
