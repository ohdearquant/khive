//! Process-scoped home for tests that exercise daemon defaults or change HOME.

use std::path::PathBuf;

const CHILD_TEST: &str = "KHIVE_MCP_PRIVATE_HOME_TEST";
const CHILD_HOME: &str = "KHIVE_MCP_PRIVATE_HOME_PATH";
const PARENT_PID: &str = "KHIVE_MCP_PRIVATE_HOME_PARENT_PID";
pub(crate) const PARENT_HOME: &str = "KHIVE_MCP_PRIVATE_HOME_PARENT_HOME";

pub(crate) fn rerun_with_private_home() -> bool {
    let thread = std::thread::current();
    let test_name = thread
        .name()
        .expect("MCP isolation requires a named test thread");
    if let Some(selected) = std::env::var_os(CHILD_TEST) {
        assert_eq!(
            selected.as_os_str(),
            std::ffi::OsStr::new(test_name),
            "MCP_CHILD_EXACT_TEST"
        );
        let parent_pid: u32 = std::env::var(PARENT_PID)
            .expect("MCP_CHILD_PARENT_REQUIRED")
            .parse()
            .expect("MCP_CHILD_PARENT_VALID");
        assert_ne!(parent_pid, std::process::id(), "MCP_CHILD_PROCESS_REQUIRED");
        let expected =
            PathBuf::from(std::env::var_os(CHILD_HOME).expect("MCP_CHILD_HOME_REQUIRED"));
        assert!(
            expected.is_absolute() && expected.is_dir(),
            "MCP_CHILD_HOME_PRIVATE"
        );
        assert_eq!(
            std::env::var_os("HOME"),
            Some(expected.clone().into_os_string()),
            "MCP_CHILD_HOME_REDIRECTED"
        );
        assert_eq!(
            std::env::var_os("USERPROFILE"),
            Some(expected.into_os_string()),
            "MCP_CHILD_PROFILE_REDIRECTED"
        );
        return false;
    }

    let parent_home = std::env::var_os("HOME");
    let home = tempfile::tempdir().expect("MCP private child home");
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("MCP test executable"));
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("APPDATA", home.path().join("config"))
        .env("LOCALAPPDATA", home.path().join("cache"))
        .env(CHILD_TEST, test_name)
        .env(CHILD_HOME, home.path())
        .env(PARENT_PID, std::process::id().to_string())
        .env(
            PARENT_HOME,
            parent_home
                .as_deref()
                .unwrap_or_else(|| std::ffi::OsStr::new("")),
        )
        .current_dir(home.path());
    for name in [
        "KHIVE_SOCKET",
        "KHIVE_PID",
        "KHIVE_LOCK",
        "KHIVE_RECOVERER_LOCK",
        "KHIVE_SUPERVISOR_MARKER",
        "KHIVE_PROCESS_REF",
        "KHIVE_DB",
        "KHIVE_CONFIG",
        "KHIVE_NO_DAEMON",
        "KHIVE_SAVE_TO_ROOT",
    ] {
        command.env_remove(name);
    }
    let output = command.output().expect("spawn exact MCP child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "MCP_PRIVATE_CHILD_FAILED: {test_name}\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
        "MCP_CHILD_EXACTLY_ONE_TEST: {test_name}\n{stdout}\n{stderr}"
    );
    assert_eq!(
        std::env::var_os("HOME"),
        parent_home,
        "MCP_PARENT_HOME_UNCHANGED"
    );
    true
}
