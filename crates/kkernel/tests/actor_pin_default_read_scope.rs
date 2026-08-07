//! Regression for the round-2 follow-up on the `--actor` pin fix: a default
//! (no explicit `namespace=`) multi-record read must expose exactly `local`
//! plus the *currently pinned* identity's namespace, never a project-config
//! actor the pin displaced.
//!
//! Drives `run_exec` end-to-end (its normal in-process fallback — the same
//! entry point `kkernel exec` uses) against a project `[actor] id =
//! "lambda:fallback"` config with no `visible_namespaces` configured. Seeds
//! one record each into `local`, `lambda:fallback`, and `lambda:pinned` via
//! an explicit per-op `namespace=` write escape, then reads back with
//! `--actor lambda:pinned` and again with `--actor local`, asserting each
//! read's namespace-derived scope.
//!
//! `lambda:pinned` and `lambda:fallback` are the established fixture names
//! this branch's existing actor-pin unit tests already use
//! (`crates/kkernel/src/exec.rs`'s
//! `actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback`); kept
//! consistent here rather than inventing new labels.
//!
//! Lives as its own integration-test binary (rather than inside `exec.rs`'s
//! `#[cfg(test)] mod tests`) for the same reason as
//! `config_discovery_reload_anchor.rs`: it mutates the process-wide cwd via
//! `std::env::set_current_dir`, which must stay isolated from unrelated
//! cwd-sensitive tests running concurrently in the unit-test binary.

use std::path::PathBuf;

use kkernel::exec::{run_exec, ExecArgs};
use serial_test::serial;

/// Snapshot + restore guard for the ambient process state this test mutates
/// (`HOME` and cwd), matching `config_discovery_reload_anchor.rs`'s pattern.
struct EnvGuard {
    prev_home: Option<std::ffi::OsString>,
    prev_cwd: PathBuf,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::env::set_current_dir(&self.prev_cwd);
    }
}

fn base_args(db: &str, actor: Option<&str>, ops: &str, save_file: &str) -> ExecArgs {
    ExecArgs {
        ops: Some(ops.to_string()),
        pending_events: false,
        db: Some(db.to_string()),
        config: None,
        namespace: "local".to_string(),
        actor: actor.map(str::to_string),
        expect_actor: None,
        presentation: Some("verbose".to_string()),
        output_format: None,
        verbose: false,
        save_file: Some(save_file.to_string()),
        ops_file: None,
        dry_run: false,
        atomic: false,
        atomic_max_ops: None,
        strict: true,
    }
}

/// Read a `--save-file` JSONL result file back into the single op result it
/// contains and return the list of `content` strings the `list()` call
/// returned. Mirrors `render_list_response`'s two possible shapes (a bare
/// array, or `{"items": [...], ...}` once a limit clamp kicks in).
fn contents_from_save_file(path: &std::path::Path) -> Vec<String> {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read save-file {}: {e}", path.display()));
    let line = raw
        .lines()
        .next()
        .unwrap_or_else(|| panic!("save-file {} has no result line", path.display()));
    let row: serde_json::Value = serde_json::from_str(line).expect("parse result row json");
    assert_eq!(row["ok"], true, "list op must have succeeded: {row}");
    let items = row["result"]
        .as_array()
        .or_else(|| row["result"]["items"].as_array())
        .unwrap_or_else(|| panic!("list result must be an array or {{items: [...]}}: {row}"))
        .clone();
    items
        .iter()
        .filter_map(|item| item.get("content").and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
#[serial]
async fn actor_pin_default_read_exposes_only_local_and_pinned_records() {
    let prev_home = std::env::var_os("HOME");
    let prev_cwd = std::env::current_dir().expect("read current cwd");
    let guard = EnvGuard {
        prev_home,
        prev_cwd,
    };

    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_CONFIG");
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_BRAIN_PROFILE");
    std::env::remove_var("KHIVE_OUTPUT_FORMAT");
    // Pin the pack set for determinism/speed; `kg` is all this test needs.
    std::env::set_var("KHIVE_PACKS", "kg");
    // Force in-process dispatch for the seed/read calls below — no reliance
    // on a warm/absent daemon socket. `run_exec`'s daemon-forward framing
    // seam (what a live daemon would receive) is exercised separately by
    // `daemon_forward_frame_actor_pin.rs`'s
    // `daemon_forward_frame_reflects_actor_pin_not_displaced_fallback`
    // integration test, which asserts the forwarded frame's `actor_id`/
    // `visible_namespaces` directly off the wire, via a hand-rolled fake
    // daemon socket.
    std::env::set_var("KHIVE_NO_DAEMON", "1");

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

    let db_path = khive_dir.join("actor-pin-default-read.db");
    let db_str = db_path.to_str().expect("utf8 db path").to_string();

    std::env::set_current_dir(project_dir.path()).expect("chdir into isolated project dir");

    // ── seed: one record each into local, lambda:fallback, lambda:pinned ────
    // Explicit `namespace=` on each op is the documented write escape
    // (ADR-007 Rev 4): it targets exactly that namespace regardless of the
    // caller's actor/default namespace, so seeding does not depend on the
    // pin behavior under test.
    let seed_ops = r#"[
        create(kind="observation", content="local-record", namespace="local"),
        create(kind="observation", content="fallback-record", namespace="lambda:fallback"),
        create(kind="observation", content="pinned-record", namespace="lambda:pinned")
    ]"#;
    let seed_save = project_dir.path().join("seed-result.jsonl");
    let seed_args = base_args(
        &db_str,
        None,
        seed_ops,
        seed_save.to_str().expect("utf8 save path"),
    );
    let seed_result = run_exec(seed_args).await;
    assert!(
        seed_result.is_ok(),
        "seeding local/fallback/pinned records must succeed: {seed_result:?}"
    );
    let seed_raw = std::fs::read_to_string(&seed_save).expect("read seed save-file");
    assert_eq!(
        seed_raw.lines().filter(|l| !l.trim().is_empty()).count(),
        3,
        "seed batch must have written exactly 3 result rows: {seed_raw}"
    );
    for line in seed_raw.lines().filter(|l| !l.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).expect("parse seed row json");
        assert_eq!(row["ok"], true, "every seed create() must succeed: {row}");
    }

    // ── read #1: --actor lambda:pinned, default (no namespace=) list() ──────
    let pinned_save = project_dir.path().join("pinned-read.jsonl");
    let pinned_args = base_args(
        &db_str,
        Some("lambda:pinned"),
        r#"list(kind="observation")"#,
        pinned_save.to_str().expect("utf8 save path"),
    );
    let pinned_result = run_exec(pinned_args).await;
    assert!(
        pinned_result.is_ok(),
        "default read while pinned to lambda:pinned must succeed: {pinned_result:?}"
    );
    let pinned_contents = contents_from_save_file(&pinned_save);
    assert!(
        pinned_contents.iter().any(|c| c == "local-record"),
        "--actor lambda:pinned default read must include the local record: {pinned_contents:?}"
    );
    assert!(
        pinned_contents.iter().any(|c| c == "pinned-record"),
        "--actor lambda:pinned default read must include the pinned actor's own record: \
         {pinned_contents:?}"
    );
    assert!(
        !pinned_contents.iter().any(|c| c == "fallback-record"),
        "--actor lambda:pinned default read must NOT expose the displaced project-config \
         fallback actor's record: {pinned_contents:?}"
    );
    assert_eq!(
        pinned_contents.len(),
        2,
        "--actor lambda:pinned default read must expose exactly local + pinned, nothing else: \
         {pinned_contents:?}"
    );

    // ── read #2: --actor local, default (no namespace=) list() ──────────────
    let local_save = project_dir.path().join("local-read.jsonl");
    let local_args = base_args(
        &db_str,
        Some("local"),
        r#"list(kind="observation")"#,
        local_save.to_str().expect("utf8 save path"),
    );
    let local_result = run_exec(local_args).await;
    assert!(
        local_result.is_ok(),
        "default read while pinned to local must succeed: {local_result:?}"
    );
    let local_contents = contents_from_save_file(&local_save);
    assert_eq!(
        local_contents,
        vec!["local-record".to_string()],
        "--actor local default read must expose only the local record, retaining neither the \
         displaced fallback actor nor the previously-pinned actor: {local_contents:?}"
    );

    drop(guard);
}
