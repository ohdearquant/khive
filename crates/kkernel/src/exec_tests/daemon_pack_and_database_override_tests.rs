/// A CONCRETE override on a single-backend invocation (no `[[backends]]`
/// declared) must reach the spawn seam: the spawned daemon has no
/// config-declared database path and would otherwise bind
/// `$HOME/.khive/khive.db`, never matching the client's override-anchored
/// frame. (The redundant multi-backend concrete case is withheld — see
/// `inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict`.)
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn single_backend_concrete_db_override_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    // No [[backends]] declared — the single-backend shape.
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let override_path = dir.path().join("override.db");
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(override_path.to_str().expect("utf8")),
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(override_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
            config: Some(config_path),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(override_path.display().to_string()),
        "the single-backend concrete override must reach the daemon spawn seam so a \
             spawned daemon binds the operator's file instead of the default database"
    );
}

/// khive-oss#1941: a client whose environment sets `KHIVE_PACKS` must
/// forward that resolved pack list to any daemon it spawns, or the fresh
/// daemon defaults to the built-in pack set, its `config_id` disagrees
/// with every caller expecting the wider selection, and those callers
/// permanently fall back to in-process dispatch instead of the warm
/// daemon.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn env_khive_packs_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = EnvAndCwdGuard::capture();
    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::set_var("KHIVE_PACKS", "kg,gtd,memory");
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve exec-shaped config from KHIVE_PACKS env");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
        Some(vec![
            "kg".to_string(),
            "gtd".to_string(),
            "memory".to_string()
        ]),
        "the KHIVE_PACKS-resolved pack list must reach the daemon spawn seam so a \
             spawned daemon serves the same packs this client resolved"
    );
}

/// Control for `env_khive_packs_reaches_daemon_spawn_seam`: with no
/// `KHIVE_PACKS` (or other pack-selection input) set, the client resolves
/// the built-in production pack set and forwards exactly that — a
/// spawned daemon re-deriving the same built-in default independently
/// would reach the identical outcome, so this proves the fix is additive
/// and does not change the unconfigured default path's effective result.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn no_env_control_forwards_built_in_default_packs_to_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    // Declared first so it drops LAST (reverse declaration order):
    // constructing the guard here, before either tempdir is created and
    // before any process-global mutation, means every panic from this
    // point on — including a `tempdir()`/`set_current_dir()` setup panic,
    // not just a later assertion — has a live guard whose Drop restores
    // KHIVE_PACKS/HOME/cwd. Because it drops last, `home_dir` and
    // `empty_project_root` are removed BEFORE that restore runs, i.e.
    // `empty_project_root` is removed while it is still the process cwd
    // (verified empirically below: `TempDir::drop` tolerates removing the
    // current working directory on macOS and ignores its own errors).
    let _guard = EnvAndCwdGuard::capture();

    // Isolate both HOME and cwd: with `db: None` config discovery falls
    // through to tier-2 (`<cwd>/khive.toml`) then tier-4
    // (`~/.khive/config.toml`) — either one declaring `[runtime].packs`
    // would satisfy this control incidentally instead of proving the
    // built-in-default path, so neither may be the ambient machine's.
    // Creating these tempdirs mutates nothing process-global by itself
    // (a panic here would be caught by the guard above, harmlessly
    // restoring values that were never changed).
    let home_dir = tempfile::tempdir().expect("tempdir for isolated HOME");
    let empty_project_root = tempfile::tempdir().expect("empty project-root tempdir");

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    std::env::remove_var("KHIVE_PACKS");
    std::env::set_var("HOME", home_dir.path());
    std::env::set_current_dir(empty_project_root.path())
        .expect("chdir into isolated project root with no discoverable config");

    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve exec-shaped config with no pack-selection input");

    // Isolating HOME/cwd only keeps the ambient machine's config from
    // leaking in; it does not by itself prove resolution landed on the
    // built-in set rather than some other value. Certify that directly
    // before the forwarding seam is exercised.
    assert_eq!(
        cfg.packs,
        RuntimeConfig::built_in_packs(),
        "the isolated no-selection environment must resolve to the built-in default \
             pack set before the forwarding seam is exercised"
    );

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
        Some(RuntimeConfig::built_in_packs()),
        "with no pack-selection input the client must forward exactly the built-in \
             default pack set — identical to what an independently-spawned daemon would \
             have defaulted to on its own"
    );
}
