//! The runtime exact-test child runs with a private HOME and with the
//! documented variables, whatever the parent's environment holds.

#[path = "../src/test_process.rs"]
mod test_process;

use std::path::PathBuf;

#[test]
fn child_receives_a_private_home_and_the_documented_variables() {
    if test_process::run_in_child() {
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("child HOME"));
    let fixture = home.parent().expect("fixture directory");
    let private_paths = [
        ("KHIVE_LOCK", "boot.lock"),
        ("KHIVE_RECOVERER_LOCK", "recoverer.lock"),
        ("KHIVE_SOCKET", "s"),
        ("KHIVE_PID", "p"),
        ("KHIVE_VOLUME_LOCK_DIR", "volume-locks"),
    ];
    for (variable, file) in private_paths {
        let value = std::env::var_os(variable).expect("private path");
        assert_eq!(PathBuf::from(value), fixture.join(file), "{variable}");
    }
    assert_eq!(std::env::var("KHIVE_TEST_HARNESS").as_deref(), Ok("1"));
    assert!(std::env::var_os("LATTICE_MODEL_CACHE").is_none());
    let mut khive_names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| name.starts_with("KHIVE_"))
        .collect();
    khive_names.sort();
    let expected = [
        "KHIVE_LOCK",
        "KHIVE_PID",
        "KHIVE_RECOVERER_LOCK",
        "KHIVE_RUNTIME_ISOLATED_TEST",
        "KHIVE_SOCKET",
        "KHIVE_TEST_HARNESS",
        "KHIVE_VOLUME_LOCK_DIR",
    ];
    assert_eq!(khive_names, expected);
}
