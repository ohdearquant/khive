//! Black-box regression test for the `kkernel exec --pending-events`
//! resolved-actor disclosure.

use std::process::Command;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

#[test]
fn exec_pending_events_discloses_resolved_actor_before_drain() {
    let home = tempfile::Builder::new()
        .prefix("kkernel-pending-events-home-")
        .tempdir_in(std::env::temp_dir())
        .expect("isolated HOME under /private/tmp");
    let db_path = home.path().join("scratch.db");

    let output = Command::new(kkernel_bin())
        .args([
            "exec",
            "--pending-events",
            "--db",
            db_path.to_str().expect("utf8 db path"),
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_ACTOR", "lambda:pending-events-test")
        .env("RUST_LOG", "error")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_PACKS")
        .env_remove("KHIVE_REQUIRE_ATTRIBUTED_ACTOR")
        .output()
        .expect("run kkernel exec --pending-events");

    assert!(
        output.status.success(),
        "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = "actor: \"lambda:pending-events-test\" (resolved; attributed)";
    let count = stderr.matches(expected).count();
    assert_eq!(
        count, 1,
        "expected exactly one resolved-actor disclosure line; stderr={stderr}"
    );
}
