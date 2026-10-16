// ── Actor / namespace precedence matrix (ADR-007 amendment) ──────────────────
//
// These tests exercise the 4-tier resolution order without a live server, using
// the same config-loading primitives that main.rs calls.  Each test covers one
// isolated conflict tier to lock in the precedence-matrix regression cases.

/// Tier 4 (hard default): no CLI actor, no env, no config file → "local".
#[test]
fn actor_precedence_default_local_with_no_config() {
    use khive_runtime::{Namespace, RuntimeConfig};

    let config = RuntimeConfig::default();
    assert_eq!(
        config.default_namespace,
        Namespace::parse("local").unwrap(),
        "RuntimeConfig::default() must produce namespace 'local' (tier-4 hard default)"
    );
}

/// Tier 3 (config file): no CLI, config has actor.id. Under ADR-007 Rev 4 Rule 0
/// the config `[actor] id` must NOT route the storage `default_namespace` — writes
/// stay pinned to `local`. Note: a non-`'local'` `actor.id` IS folded into the
/// default READ visible-set (ADR-007 Rev 4 Rule 3b), but that does not change
/// `default_namespace`. This test asserts only the write-routing invariant.
#[test]
fn actor_precedence_config_actor_id_does_not_route_namespace() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");
    assert_eq!(khive_cfg.actor.id.as_deref(), Some("lambda:from-config"));

    // No CLI actor → base stays at "local" default; config actor.id must not move it.
    let base = RuntimeConfig::default();
    let resolved = runtime_config_from_khive_config(&khive_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("local").unwrap(),
        "config actor.id must NOT become default_namespace (ADR-007 Rev 4 Rule 0); \
         writes stay pinned to local (actor.id does contribute to READ visible-set per Rule 3b, \
         but that does not affect default_namespace)"
    );
}

/// Tier 2 (--namespace / KHIVE_NAMESPACE with explicit value "local"): explicit
/// --namespace local must win over a conflicting config actor.
///
/// This is the regression case: previously the value
/// comparison `args.namespace != "local"` treated `--namespace local` as
/// identical to the absent default, letting config override it.  Now that
/// `namespace` is `Option<String>`, `Some("local")` is correctly explicit.
#[test]
fn actor_precedence_explicit_namespace_local_wins_over_config() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");

    // Simulate: --namespace local supplied → cli_namespace_explicit = true.
    // Caller nullifies config actor before calling runtime_config_from_khive_config.
    let mut effective_cfg = khive_cfg;
    effective_cfg.actor.id = None; // CLI wins — suppress config actor.

    let base = RuntimeConfig {
        default_namespace: Namespace::parse("local").unwrap(), // explicit CLI value
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let resolved = runtime_config_from_khive_config(&effective_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("local").unwrap(),
        "--namespace local (explicit) must win over config actor.id"
    );
}

/// Tier 1 (--actor / KHIVE_ACTOR): explicit --actor value wins over config actor.
#[test]
fn actor_precedence_cli_actor_wins_over_config() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");

    // Simulate: --actor lambda:cli-actor supplied → cli_namespace_explicit = true.
    let mut effective_cfg = khive_cfg;
    effective_cfg.actor.id = None; // CLI wins — suppress config actor.

    let base = RuntimeConfig {
        default_namespace: Namespace::parse("lambda:cli-actor").unwrap(),
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let resolved = runtime_config_from_khive_config(&effective_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("lambda:cli-actor").unwrap(),
        "--actor lambda:cli-actor must win over config actor.id"
    );
}

/// Invalid config actor.id must be caught at load time (not silently downgraded).
///
/// This is the regression case: previously an invalid
/// actor.id logged a warning and fell back to the base namespace.  Now it is a
/// hard startup error via ConfigError::InvalidActorId.
#[test]
fn actor_invalid_config_id_fails_at_load() {
    use khive_runtime::{ConfigError, KhiveConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"bad namespace\"\n"
    )
    .unwrap();

    let err = KhiveConfig::load(Some(&path)).expect_err("invalid actor.id must fail at load");
    let root = match &err {
        ConfigError::InFile { source, .. } => source.as_ref(),
        other => other,
    };
    assert!(
        matches!(root, ConfigError::InvalidActorId { .. }),
        "expected ConfigError::InvalidActorId, got {err:?}"
    );
}

/// Empty-string actor.id must be caught at load time.
#[test]
fn actor_empty_string_id_fails_at_load() {
    use khive_runtime::{ConfigError, KhiveConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"\"\n"
    )
    .unwrap();

    let err = KhiveConfig::load(Some(&path)).expect_err("empty actor.id must fail at load");
    let root = match &err {
        ConfigError::InFile { source, .. } => source.as_ref(),
        other => other,
    };
    assert!(
        matches!(root, ConfigError::InvalidActorId { .. }),
        "expected ConfigError::InvalidActorId for empty string, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// CLI / env precedence: real Args parsing via clap try_parse_from
//
// These tests exercise the actual clap parser + resolve_cli_namespace so that
// a regression such as `args.namespace != "local"` (the original High finding)
// would cause failures here, not just in the manually-constructed tests above.
// ---------------------------------------------------------------------------

/// RAII guard that unsets the named env vars on construction.
/// Prevents leakage from a prior serial test that may not have cleaned up.
struct ClearEnvGuard {
    vars: Vec<&'static str>,
}

impl ClearEnvGuard {
    fn new(vars: &[&'static str]) -> Self {
        for &v in vars {
            std::env::remove_var(v);
        }
        Self {
            vars: vars.to_vec(),
        }
    }
}

impl Drop for ClearEnvGuard {
    fn drop(&mut self) {
        for &v in &self.vars {
            std::env::remove_var(v);
        }
    }
}

/// Tier 1a: --actor flag → explicit=true, namespace = supplied value.
#[test]
#[serial_test::serial]
fn cli_args_actor_flag_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--actor", "lambda:cli-actor"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit, "--actor must mark namespace as explicit");
    assert_eq!(ns, Namespace::parse("lambda:cli-actor").unwrap());
}

/// Tier 1b: --actor local → explicit=true (regression guard: must NOT be treated as absent).
#[test]
#[serial_test::serial]
fn cli_args_actor_local_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--actor", "local"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "--actor local must be explicit, not treated as absent default"
    );
    assert_eq!(ns, Namespace::parse("local").unwrap());
}

/// Tier 2a: --namespace flag → explicit=true.
#[test]
#[serial_test::serial]
fn cli_args_namespace_flag_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--namespace", "lambda:ns-flag"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit, "--namespace must mark namespace as explicit");
    assert_eq!(ns, Namespace::parse("lambda:ns-flag").unwrap());
}

/// Tier 2b: --namespace local → explicit=true (the original regression case).
#[test]
#[serial_test::serial]
fn cli_args_namespace_local_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--namespace", "local"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "--namespace local must be explicit (regression: was previously treated as absent)"
    );
    assert_eq!(ns, Namespace::parse("local").unwrap());
}

/// Tier 1 wins over Tier 2: --actor beats --namespace when both supplied.
#[test]
#[serial_test::serial]
fn cli_args_actor_wins_over_namespace_when_both_supplied() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from([
        "khive-mcp",
        "--actor",
        "lambda:actor-wins",
        "--namespace",
        "lambda:ns-loses",
    ])
    .unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit);
    assert_eq!(
        ns,
        Namespace::parse("lambda:actor-wins").unwrap(),
        "--actor must win over --namespace when both are supplied"
    );
}

/// Tier 4 (hard default): no CLI flags → explicit=false, namespace = "local".
#[test]
#[serial_test::serial]
fn cli_args_no_flags_gives_local_default() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(!explicit, "no flags must not be treated as explicit");
    assert_eq!(
        ns,
        Namespace::parse("local").unwrap(),
        "default namespace must be 'local' when no CLI flags are supplied"
    );
}

/// KHIVE_NAMESPACE env var → explicit=true (env var has same effect as flag).
///
/// Uses `clap`'s env-source support. `ClearEnvGuard` unsets both
/// `KHIVE_NAMESPACE` and `KHIVE_ACTOR` on construction AND drop, so the env is
/// clean for the parse and restored to clean state after, even on panic.
/// `#[serial]` prevents races with other env-mutating tests.
#[test]
#[serial_test::serial]
fn cli_args_khive_namespace_env_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_NAMESPACE", "KHIVE_ACTOR"]);

    std::env::set_var("KHIVE_NAMESPACE", "lambda:from-env");
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    std::env::remove_var("KHIVE_NAMESPACE");

    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "KHIVE_NAMESPACE env must mark namespace as explicit"
    );
    assert_eq!(ns, Namespace::parse("lambda:from-env").unwrap());
}

/// ADR-096 Fork 2 (PR #657): `KHIVE_ACTOR` env var must NOT
/// occupy the CLI tier at all — it no longer marks the CLI namespace as
/// explicit, and it no longer wins over `KHIVE_NAMESPACE`. `args.rs` used to
/// bind `--actor` to `env = "KHIVE_ACTOR"`, which made a bare shell-level
/// `KHIVE_ACTOR` indistinguishable from an explicit `--actor` flag and let it
/// beat the project-config `[actor]` tier — inverting the ratified
/// precedence (CLI flag > project config > `KHIVE_ACTOR` env > anonymous).
/// The env binding was removed from `args.actor`; `KHIVE_ACTOR` is now read
/// only as the tier-3 `actor_id` fallback in `RuntimeConfig::default()` /
/// `resolve_runtime_config`, never through `resolve_cli_namespace`.
/// `ClearEnvGuard` keeps env state isolated; `#[serial]` prevents races.
#[test]
#[serial_test::serial]
fn cli_args_khive_actor_env_no_longer_occupies_cli_tier() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_NAMESPACE", "KHIVE_ACTOR"]);

    std::env::set_var("KHIVE_ACTOR", "lambda:actor-env");
    std::env::set_var("KHIVE_NAMESPACE", "lambda:ns-env");
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_NAMESPACE");

    assert!(
        args.actor.is_none(),
        "KHIVE_ACTOR env must not populate args.actor — the clap env binding was removed"
    );

    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "KHIVE_NAMESPACE env still marks the (unchanged, out-of-scope) legacy \
         --namespace alias tier as explicit"
    );
    assert_eq!(
        ns,
        Namespace::parse("lambda:ns-env").unwrap(),
        "with KHIVE_ACTOR no longer in the CLI tier, KHIVE_NAMESPACE decides \
         the resolved namespace instead of KHIVE_ACTOR"
    );
}
