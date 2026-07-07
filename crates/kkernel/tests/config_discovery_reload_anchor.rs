//! Regression test for #689 (residual, attempt 2): post-resolution config
//! reloads must anchor tier-3 discovery to the fixed `config_discovery_db_anchor`
//! semantics (`None`/cwd for an unset `--db`), never to `$HOME`.
//!
//! Play-gate repro shape this test encodes: a cwd `.khive/config.toml` declares
//! two `[[backends]]` and `--db` is left unset. Before the fix, the
//! post-resolution reloads at `exec.rs`'s `run_exec_inline_with_forward` /
//! `run_exec_ops_file` and `serve.rs`'s `build_server` fed the materialized
//! `$HOME/.khive/khive.db` default back into `KhiveConfig::load_with_home_fallback`,
//! so tier-3 discovery searched `$HOME/.khive/config.toml` instead of the cwd
//! project config — the cwd-declared backends were invisible, and the run fell
//! back to a single, home-anchored default database.
//!
//! This test isolates both `HOME` and the process cwd in a dedicated temp dir
//! each, so it asserts on ACTUAL backend-file creation (not just a resolved,
//! non-empty `KhiveConfig`): the fixed behavior must create both declared
//! backend files under the project dir and must NOT create
//! `$HOME/.khive/khive.db`.
//!
//! Lives as its own integration-test binary (rather than inside `exec.rs`'s
//! `#[cfg(test)] mod tests`) because it mutates the process-wide cwd via
//! `std::env::set_current_dir` — isolating that mutation to a dedicated test
//! binary avoids any risk of it leaking into unrelated cwd-sensitive tests
//! that run concurrently within `kkernel`'s unit-test binary.

use std::path::PathBuf;

use kkernel::exec::{run_exec, ExecArgs};
use serial_test::serial;

/// Snapshot + restore guard for the ambient process state this test mutates
/// (`HOME` and cwd). Restoring via `Drop` keeps the host process clean even
/// if an assertion panics partway through.
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
        // Best-effort: if the original cwd no longer exists for some reason,
        // there is nothing sane left to restore to.
        let _ = std::env::set_current_dir(&self.prev_cwd);
    }
}

#[tokio::test]
#[serial]
async fn unset_db_with_cwd_multi_backend_config_creates_configured_backends_not_home_default() {
    let prev_home = std::env::var_os("HOME");
    let prev_cwd = std::env::current_dir().expect("read current cwd");
    let guard = EnvGuard {
        prev_home,
        prev_cwd,
    };

    // Ambient env vars that could otherwise influence config/actor/embedding
    // resolution and mask the bug this test targets.
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_CONFIG");
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_BRAIN_PROFILE");
    std::env::remove_var("KHIVE_OUTPUT_FORMAT");
    std::env::remove_var("KHIVE_PACKS");
    // Force in-process dispatch — no reliance on a warm/absent daemon socket.
    std::env::set_var("KHIVE_NO_DAEMON", "1");

    // Isolated HOME: must remain untouched by the run (no `.khive` created here).
    let home_dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    std::env::set_var("HOME", home_dir.path());

    // Isolated project dir: this becomes the process cwd, and hosts the
    // multi-backend `.khive/config.toml` the fixed tier-3 discovery must find.
    let project_dir = tempfile::tempdir().expect("tempdir for isolated project cwd");
    let khive_dir = project_dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir project .khive");

    let main_backend_path = khive_dir.join("main.db");
    let sessions_backend_path = khive_dir.join("sessions.db");
    std::fs::write(
        khive_dir.join("config.toml"),
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write multi-backend config.toml");

    std::env::set_current_dir(project_dir.path()).expect("chdir into isolated project dir");

    let args = ExecArgs {
        ops: Some("stats()".to_string()),
        pending_events: false,
        db: None, // the repro shape: --db left unset
        namespace: "local".to_string(),
        presentation: Some("agent".to_string()),
        output_format: None,
        verbose: false,
        save_file: None,
        ops_file: None,
        dry_run: false,
        atomic: false,
        atomic_max_ops: None,
    };

    let result = run_exec(args).await;

    let home_default_db = home_dir.path().join(".khive").join("khive.db");

    assert!(
        result.is_ok(),
        "kkernel exec 'stats()' with an unset --db and a cwd multi-backend config \
         must succeed (RC=0 equivalent): {result:?}"
    );
    assert!(
        main_backend_path.exists(),
        "configured `main` backend file must be created at {} — tier-3 config \
         discovery must have anchored to the cwd project config, not $HOME",
        main_backend_path.display()
    );
    assert!(
        sessions_backend_path.exists(),
        "configured `sessions` backend file must be created at {} — tier-3 config \
         discovery must have anchored to the cwd project config, not $HOME",
        sessions_backend_path.display()
    );
    assert!(
        !home_default_db.exists(),
        "the $HOME/.khive/khive.db fallback must NOT be created when a cwd config \
         declares [[backends]] and --db is unset — found {}",
        home_default_db.display()
    );

    drop(guard);
}
