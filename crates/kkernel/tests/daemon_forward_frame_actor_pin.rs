//! Regression for the round-2 follow-up on the `--actor` pin fix: the
//! daemon-forward request frame must carry the corrected actor-pin identity,
//! never the displaced project-config fallback actor.
//!
//! Complements `actor_pin_default_read_exposes_only_local_and_pinned_records`
//! (`actor_pin_default_read_scope.rs`, which reads real seeded records back
//! through `run_exec`'s in-process fallback): this drives the SAME `--actor`
//! pin through the ACTUAL daemon-forward framing seam and asserts on the real
//! `DaemonRequestFrame` a live daemon would receive — the wire-level proof
//! that a warm daemon would filter reads by the corrected identity too, not
//! just the in-process path.
//!
//! Drives `run_exec` end-to-end (its normal daemon-forward path, entered by
//! leaving `--save-file` unset) against a hand-rolled fake daemon — a bare
//! `UnixListener` bound at `KHIVE_SOCKET`, matching the pattern
//! `mcp_bridge_reexec_protocol_mismatch.rs` already uses to stand in for a
//! warm daemon — rather than injecting a spy at the private
//! `run_exec_inline_with_forward` seam the pre-move version of this test
//! used. Only `kkernel::exec::{run_exec, ExecArgs}` and the public
//! `khive_runtime::daemon` wire types are needed; no crate-private items.
//!
//! Lives as its own integration-test binary (rather than inside `exec.rs`'s
//! `#[cfg(test)] mod tests`) because it mutates the process-wide cwd via
//! `std::env::set_current_dir` — isolating that mutation to a dedicated test
//! binary avoids any risk of it leaking into unrelated cwd-sensitive tests
//! that run concurrently within `kkernel`'s unit-test binary (a bare
//! `#[serial]` only serializes tests that opt in, not the whole binary).
//! Same rationale as `config_discovery_reload_anchor.rs`.

#![cfg(unix)]

use std::path::PathBuf;

use khive_runtime::daemon::{
    read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
};
use kkernel::exec::{run_exec, ExecArgs};
use serial_test::serial;
use tokio::net::UnixListener;

/// Snapshot + restore guard for the ambient process state this test mutates
/// (`HOME`, cwd, and `KHIVE_SOCKET`), matching `config_discovery_reload_anchor.rs`'s
/// pattern.
struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_cwd: PathBuf,
    prev_socket: Option<std::ffi::OsString>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.prev_socket {
            Some(v) => std::env::set_var("KHIVE_SOCKET", v),
            None => std::env::remove_var("KHIVE_SOCKET"),
        }
        // Best-effort: if the original cwd no longer exists for some reason,
        // there is nothing sane left to restore to.
        let _ = std::env::set_current_dir(&self.prev_cwd);
    }
}

fn base_args(db: &str, actor: Option<&str>) -> ExecArgs {
    ExecArgs {
        ops: Some("stats()".to_string()),
        pending_events: false,
        db: Some(db.to_string()),
        namespace: "local".to_string(),
        actor: actor.map(str::to_string),
        expect_actor: None,
        presentation: Some("agent".to_string()),
        output_format: None,
        verbose: false,
        // Leaving `--save-file` unset is what keeps `run_exec`'s daemon-forward
        // fast path live — it is the one branch the code explicitly skips when
        // a save-file sink is set.
        save_file: None,
        ops_file: None,
        dry_run: false,
        atomic: false,
        atomic_max_ops: None,
        strict: false,
    }
}

/// Accept exactly one connection on `listener`, decode the request frame
/// `run_exec`'s daemon-forward seam sent, reply with a minimal
/// always-succeeding response that echoes the request's `config_id` back
/// (the fail-closed check `map_response` applies before trusting a result),
/// and return the captured request frame for assertions.
async fn accept_and_reply(listener: &UnixListener) -> DaemonRequestFrame {
    let (mut stream, _) = listener
        .accept()
        .await
        .expect("accept fake-daemon connection");
    let req_bytes = read_frame(&mut stream)
        .await
        .expect("read request frame from run_exec's daemon-forward seam");
    let frame: DaemonRequestFrame =
        serde_json::from_slice(&req_bytes).expect("decode captured request frame");
    let response = DaemonResponseFrame {
        ok: true,
        result: Some("{}".to_string()),
        error: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(frame.config_id.clone()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    let payload = serde_json::to_vec(&response).expect("serialize fake-daemon response");
    write_frame(&mut stream, &payload)
        .await
        .expect("write fake-daemon response");
    frame
}

#[tokio::test]
#[serial]
async fn daemon_forward_frame_reflects_actor_pin_not_displaced_fallback() {
    let prev_home = std::env::var_os("HOME");
    let prev_cwd = std::env::current_dir().expect("read current cwd");
    let prev_socket = std::env::var_os("KHIVE_SOCKET");
    let guard = EnvGuard {
        prev_home,
        prev_cwd,
        prev_socket,
    };

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_CONFIG");
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_BRAIN_PROFILE");
    std::env::remove_var("KHIVE_OUTPUT_FORMAT");
    // Pin the pack set for determinism: an ambient `KHIVE_PACKS` naming packs
    // not compiled into this build would fail resolution before the behavior
    // under test (#1276) — `kg` is all this test needs.
    std::env::set_var("KHIVE_PACKS", "kg");
    // This test's whole point is exercising the daemon-forward fast path, so
    // it must NOT be bypassed.
    std::env::remove_var("KHIVE_NO_DAEMON");

    let home_dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    std::env::set_var("HOME", home_dir.path());

    let project_dir = tempfile::tempdir().expect("tempdir for isolated project cwd");
    let khive_dir = project_dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir project .khive");

    // Deliberately no `visible_namespaces` under `[actor]` — the exact shape
    // the round-2 follow-up asked for.
    std::fs::write(
        khive_dir.join("config.toml"),
        r#"
[actor]
id = "lambda:fallback"
"#,
    )
    .expect("write config.toml");

    let db_path = khive_dir.join("frame-identity-test.db");
    let db_str = db_path.to_str().expect("utf8 db path").to_string();

    // Hermeticity: HOME is redirected to an empty tempdir above (tier 4 must
    // never reach a real `~/.khive/config.toml`); chdir into the project dir
    // below (tier 2's `<cwd>/khive.toml` stays absent, and the independent
    // project-actor lookup's tier 3 `<cwd>/.khive/config.toml` resolves to
    // the SAME fixture `config.toml` written above) — both restored on drop.
    std::env::set_current_dir(project_dir.path()).expect("chdir into isolated project dir");

    let sock_dir = tempfile::tempdir().expect("tempdir for fake daemon socket");
    let sock = sock_dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    let listener = UnixListener::bind(&sock).expect("bind fake-daemon socket");

    // ── call #1: no --actor pin — proves the fixture (not an ambient
    // CWD/HOME config) actually feeds this resolution, before any pin could
    // displace it. Otherwise the pinned-call assertions below could pass even
    // if resolution silently picked up a config from outside the fixture. ──
    let (unpinned_frame, unpinned_result) = tokio::join!(
        accept_and_reply(&listener),
        run_exec(base_args(&db_str, None))
    );
    assert!(
        unpinned_result.is_ok(),
        "unpinned dispatch must succeed: {unpinned_result:?}"
    );
    assert_eq!(
        unpinned_frame.actor_id.as_deref(),
        Some("lambda:fallback"),
        "the pre-pin resolved actor must come from the DB-anchored fixture config.toml, \
         not an ambient CWD/HOME config: {:?}",
        unpinned_frame.actor_id
    );
    assert!(
        unpinned_frame
            .visible_namespaces
            .iter()
            .any(|ns| ns == "lambda:fallback"),
        "the pre-pin resolved frame must fold the fixture actor into visible_namespaces: {:?}",
        unpinned_frame.visible_namespaces
    );

    // ── call #2: --actor lambda:pinned — the frame must carry the pinned
    // actor and its namespace, never the displaced project-config fallback
    // actor ───────────────────────────────────────────────────────────────
    let (pinned_frame, pinned_result) = tokio::join!(
        accept_and_reply(&listener),
        run_exec(base_args(&db_str, Some("lambda:pinned")))
    );
    assert!(
        pinned_result.is_ok(),
        "pinned dispatch must succeed: {pinned_result:?}"
    );
    assert_eq!(
        pinned_frame.actor_id.as_deref(),
        Some("lambda:pinned"),
        "the forwarded frame's actor_id must be the pin, not the project-config fallback"
    );
    assert!(
        pinned_frame
            .visible_namespaces
            .iter()
            .any(|ns| ns == "lambda:pinned"),
        "the forwarded frame's visible_namespaces must include the pinned actor: {:?}",
        pinned_frame.visible_namespaces
    );
    assert!(
        !pinned_frame
            .visible_namespaces
            .iter()
            .any(|ns| ns == "lambda:fallback"),
        "the forwarded frame's visible_namespaces must NOT retain the displaced fallback \
         actor: {:?}",
        pinned_frame.visible_namespaces
    );

    // ── call #3: --actor local — the frame must carry no actor and no extra
    // visibility; the fallback must not survive under the anonymous identity
    // either ──────────────────────────────────────────────────────────────
    let (local_frame, local_result) = tokio::join!(
        accept_and_reply(&listener),
        run_exec(base_args(&db_str, Some("local")))
    );
    assert!(
        local_result.is_ok(),
        "local-pin dispatch must succeed: {local_result:?}"
    );
    assert_eq!(
        local_frame.actor_id, None,
        "the forwarded frame's actor_id must be cleared under a local pin"
    );
    assert!(
        local_frame.visible_namespaces.is_empty(),
        "the forwarded frame's visible_namespaces must be empty under a local pin, retaining \
         neither the fallback actor nor the previous pin: {:?}",
        local_frame.visible_namespaces
    );

    drop(listener);
    drop(guard);
}
