// ── exec-path / serve-path config_id parity (#581) ────────────────────────
//
// `run_exec`'s cfg construction (above) and `kkernel mcp`'s `build_server`
// both call `resolve_runtime_config`. These tests prove the two call shapes
// agree on `compute_config_id` for the same database — the acceptance gate
// for the #581 fix — and settle the `namespace_explicit` design question
// empirically rather than by convention.

/// Direct regression guard for #581: a project's tier-3 `.khive/config.toml`
/// `[actor] id` must be visible to `kkernel exec` exactly as it is to
/// `kkernel mcp`, and the two paths' `config_id` must be byte-identical so
/// the daemon accepts exec's forwarded frame instead of rejecting it as a
/// `ConfigMismatch` (which silently falls back to an anonymous in-process
/// dispatch — the reported symptom: `comm.inbox` returning `count=0`).
#[test]
#[serial]
fn exec_config_id_matches_serve_config_id_for_project_toml_actor() {
    const CHILD_MARKER: &str = "KKERNEL_EXEC_PROJECT_CONFIG_TEST_CHILD";
    const TEST_NAME: &str =
        "exec::tests::exec_config_id_matches_serve_config_id_for_project_toml_actor";
    if rerun_in_command_scoped_empty_home(CHILD_MARKER, TEST_NAME) {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let khive_dir = dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    std::fs::write(
        khive_dir.join("config.toml"),
        r#"
[actor]
id = "lambda:test-actor"

[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
    )
    .expect("write config.toml");

    // A db path anchored INSIDE the same `.khive` dir — this is what makes
    // tier-3 discovery agree between a client and a daemon serving the same
    // database, regardless of process cwd (see `project_config_anchor_dir`).
    let db_path = khive_dir.join("exec-parity-test.db");
    let db_str = db_path.to_str().expect("utf8 path").to_string();

    let ns = Namespace::parse("local").expect("ns");

    // Exec-shaped inputs with no explicit config in this scenario and
    // `namespace_explicit: true` (the choice made in `run_exec` above).
    // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
    // from the ambient environment (same rationale as `isolated_server`
    // above, #1276): `RuntimeConfig::default()` reads `KHIVE_PACKS` fresh
    // on every call, so leaving this `None` makes the assertion below
    // depend on two independent env reads observing the same ambient
    // value — a real flake source when a concurrently-running test
    // mutates process env between them (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_str),
        config: None,
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // serve-shaped inputs: mirrors `build_server` when the operator starts
    // `kkernel mcp --daemon` with no explicit --actor/--namespace flag,
    // relying on the config file's `[actor] id` — the common daemon-startup
    // shape (`resolve_cli_namespace` returns `explicit=false` in that case).
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&db_str),
        config: None,
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");

    // The TOML must actually have reached both constructions — the direct
    // regression proxy for #581, verified without a live daemon socket.
    assert_eq!(exec_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
    assert_eq!(serve_cfg.actor_id.as_deref(), Some("lambda:test-actor"));
    assert!(
        exec_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:test-actor").expect("ns")),
        "actor.id must fold into visible_namespaces (ADR-007 Rev 4 Rule 3b)"
    );
    assert!(
        exec_cfg.embedding_model.is_some(),
        "config-file [[engines]] must resolve an embedding model, not env/default"
    );
    assert_eq!(
        format!("{:?}", exec_cfg.embedding_model),
        format!("{:?}", serve_cfg.embedding_model),
    );

    // The acceptance gate: byte-identical config_id, so the daemon accepts
    // exec's forwarded frame instead of rejecting it as a ConfigMismatch.
    assert_eq!(
        compute_config_id(&exec_cfg, None),
        compute_config_id(&serve_cfg, None),
        "exec-path config_id must match the serve/daemon-path config_id for the same db"
    );
}

/// Regression guard: an explicit `--actor` pin must rebuild the
/// actor-derived portion of `visible_namespaces`, not just `actor_id`.
///
/// Builds the config exactly the way `run_exec` does — through
/// `resolve_runtime_config`, from a project `[actor] id = "lambda:fallback"`
/// with no explicit extra visibility — so the displaced actor is folded
/// into `visible_namespaces` (ADR-007 Rev 4 Rule 3b) before the pin is
/// ever applied. A non-local pin must give default reads `local ∪
/// lambda:pinned`; a `local` pin must leave only `local`. Neither case may
/// retain `lambda:fallback`.
#[test]
#[serial]
fn actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback() {
    const CHILD_MARKER: &str = "KKERNEL_ACTOR_PIN_CONFIG_TEST_CHILD";
    const TEST_NAME: &str =
        "exec::tests::actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback";
    if rerun_in_command_scoped_empty_home(CHILD_MARKER, TEST_NAME) {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let khive_dir = dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    std::fs::write(
        khive_dir.join("config.toml"),
        r#"
[actor]
id = "lambda:fallback"
"#,
    )
    .expect("write config.toml");

    let db_path = khive_dir.join("actor-pin-visibility-test.db");
    let db_str = db_path.to_str().expect("utf8 path").to_string();
    let pinned_packs = Some(vec!["kg".to_string()]);

    let resolve = |db: &str| {
        resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: pinned_packs.clone(),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config")
    };

    // Sanity: the fallback actor really is folded into the default read
    // visible-set before any pin is applied — otherwise this test would
    // pass vacuously.
    let baseline = resolve(&db_str);
    assert_eq!(baseline.actor_id.as_deref(), Some("lambda:fallback"));
    assert!(baseline
        .visible_namespaces
        .contains(&Namespace::parse("lambda:fallback").expect("ns")));

    // A non-local pin must replace the fallback's read visibility with the
    // pinned actor's — never both, never neither.
    let mut pinned_cfg = resolve(&db_str);
    apply_actor_pin_and_expectation(&mut pinned_cfg, Some("lambda:pinned"), None).unwrap();
    assert_eq!(pinned_cfg.actor_id.as_deref(), Some("lambda:pinned"));
    assert!(
        pinned_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:pinned").expect("ns")),
        "pinned actor must be added to the default read scope: {:?}",
        pinned_cfg.visible_namespaces
    );
    assert!(
        !pinned_cfg
            .visible_namespaces
            .contains(&Namespace::parse("lambda:fallback").expect("ns")),
        "the displaced fallback actor must not remain visible under the pinned \
             identity: {:?}",
        pinned_cfg.visible_namespaces
    );

    // A `local` pin must authoritatively clear the fallback's visibility
    // without adding a replacement.
    let mut local_cfg = resolve(&db_str);
    apply_actor_pin_and_expectation(&mut local_cfg, Some("local"), None).unwrap();
    assert_eq!(local_cfg.actor_id, None);
    assert!(
        local_cfg.visible_namespaces.is_empty(),
        "pinning to local must leave only local visible, retaining neither the \
             fallback actor nor adding a new one: {:?}",
        local_cfg.visible_namespaces
    );
}

/// Settles the `namespace_explicit` design question by constructing both
/// arms and comparing `compute_config_id` directly, per the decision
/// criterion: does either arm break config_id parity with the daemon?
///
/// No `[actor] id` is present (an explicit EMPTY config file makes
/// this fully deterministic — no dependency on cwd or `$HOME`), and the
/// namespace is a non-"local" value so the actor_id fill-when-None guard in
/// `resolve_runtime_config` (the ONLY place `namespace_explicit` has any
/// effect in the embed path, i.e. `no_embed: false`, which `kkernel exec`
/// always uses) actually fires for one arm and not the other.
#[test]
#[serial]
fn namespace_explicit_changes_actor_id_fill_but_not_config_id() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");

    // A real, EMPTY config file: the explicit tier fails loud on a
    // missing file (ADR-035), so the hermeticity trick must be a real
    // file with no `[actor]` block.
    let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
    let missing_config = empty_config_dir.path().join("config.toml");
    std::fs::write(&missing_config, "").expect("write empty config");
    let ns = Namespace::parse("lambda:custom-ns").expect("ns");
    // Pin packs so the `compute_config_id` comparison below never depends
    // on two independent `KHIVE_PACKS` env reads agreeing (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    let with_explicit_true = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve with namespace_explicit=true");

    let with_explicit_false = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve with namespace_explicit=false");

    // The fill-when-None guard DOES fire differently between the two arms...
    assert_eq!(
        with_explicit_true.actor_id.as_deref(),
        Some("lambda:custom-ns"),
        "namespace_explicit=true + non-local namespace + no config actor.id \
             must fill actor_id from the namespace (ADR-057)"
    );
    assert_eq!(
        with_explicit_false.actor_id, None,
        "namespace_explicit=false must NOT fill actor_id"
    );

    // ...but `compute_config_id` never reads identity fields (`actor_id` or
    // `visible_namespaces`; namespace is carried separately per its own doc
    // comment), so the two configs — which differ ONLY in actor_id — must
    // still produce a byte-identical fingerprint. This is the empirical
    // basis for `run_exec` picking `namespace_explicit: true`: it is the
    // conservative, behavior-preserving choice, and it provably does not
    // affect config_id parity with the daemon either way.
    assert_eq!(
        compute_config_id(&with_explicit_true, None),
        compute_config_id(&with_explicit_false, None),
        "namespace_explicit must not affect the daemon-forwarded config_id"
    );
}

/// D1-R3: the two tests above are inert to the config_id topology-drift
/// bug because they always call `compute_config_id(_, None)` on BOTH
/// sides — omitting the backends topology can never diverge from itself.
/// This test constructs a genuinely multi-backend `KhiveConfig` (mirroring
/// the real hosted shape: a `main` backend plus a separate `sessions`
/// backend, with the `session` pack pinned to it) and proves both that the
/// pre-fix computation diverges and that the post-fix computation is
/// byte-identical.
#[test]
#[serial]
fn exec_config_id_matches_serve_config_id_for_multi_backend_topology() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_runtime::{BackendConfig, BackendKind, PackConfig};

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");

    // An explicit EMPTY config file keeps this fully deterministic
    // regardless of host state (same rationale as the sibling test
    // above; the explicit tier fails loud on a MISSING file — ADR-035 —
    // so the trick must be a real file).
    let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
    let missing_config = empty_config_dir.path().join("multi-backend-config.toml");
    std::fs::write(&missing_config, "").expect("write empty config");
    let ns = Namespace::parse("local").expect("ns");

    let khive_cfg = KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("/tmp/khive-parity-main.db")),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
            BackendConfig {
                name: "sessions".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("/tmp/khive-parity-sessions.db")),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
        ],
        packs: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "session".to_string(),
                PackConfig {
                    backend: "sessions".to_string(),
                    verbs_disabled: Vec::new(),
                    no_embed: false,
                },
            );
            m
        },
        ..KhiveConfig::default()
    };

    // Pin packs so the config_id comparisons below never depend on two
    // independent `KHIVE_PACKS` env reads agreeing (#1356).
    let pinned_packs = Some(vec!["kg".to_string()]);

    // exec-shaped inputs (namespace_explicit: true — the choice `run_exec` makes).
    let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns.clone(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs.clone(),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // serve-shaped inputs (namespace_explicit: false — the daemon-startup shape).
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&missing_config),
        namespace: ns,
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: pinned_packs,
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");

    // Pre-fix proof: the OLD exec-path computation (`compute_config_id(_, None)`,
    // exec.rs:490 before this fix) diverges from the daemon/serve-path
    // computation (`Some(&khive_cfg)`, serve.rs:916) the instant the backends
    // topology is non-empty. This is the exact bug: a legitimately-matching
    // client was rejected as a `ConfigMismatch` and silently fell back to the
    // cold in-process path on every call.
    assert_ne!(
        compute_config_id(&exec_cfg, None),
        compute_config_id(&serve_cfg, Some(&khive_cfg)),
        "pre-fix exec computation (None) must diverge from the daemon computation \
             (Some) for a non-empty backends topology — proves this test catches the \
             real divergence, not a tautology"
    );

    // Post-fix proof: both sides fold the SAME backends topology and produce
    // a byte-identical fingerprint, so the daemon accepts the forwarded frame
    // instead of rejecting it as a ConfigMismatch.
    assert_eq!(
        compute_config_id(&exec_cfg, Some(&khive_cfg)),
        compute_config_id(&serve_cfg, Some(&khive_cfg)),
        "exec-path config_id must match the daemon-path config_id for the same \
             multi-backend topology (D1 fix acceptance gate)"
    );
}
