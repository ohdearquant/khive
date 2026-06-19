//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use khive_runtime::{
    config_from_env, run_migrations, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, KhiveConfig, KhiveRuntime, PackRegistry, RuntimeConfig, StorageBackend,
    VerbRegistryBuilder,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

/// Build a server from `args`, then serve it over `--daemon` or the named transport.
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    let server = build_server(&args)?;

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon(server).await?;
        return Ok(());
    }
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    let transport_name = args.transport.as_deref().unwrap_or("stdio");
    let transport = registry.get(transport_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown transport {transport_name:?}; registered: {}",
            registry.names().join(", ")
        )
    })?;
    let opts = ServeOptions {
        bind: args.bind.clone(),
    };
    transport.serve(server, &opts).await
}

/// Build a fully-configured server from parsed args (without serving).
pub fn build_server(args: &Args) -> anyhow::Result<KhiveMcpServer> {
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;

    let config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace: cli_namespace,
        namespace_explicit: cli_namespace_explicit,
        no_embed: args.no_embed,
        packs: if args.pack.is_empty() {
            None
        } else {
            Some(args.pack.clone())
        },
        brain_profile: args.brain_profile.clone(),
    })?;

    // Load the KhiveConfig to check for multi-backend declarations (ADR-028).
    // When no [[backends]] are declared, fall through to the existing single-backend path
    // to preserve byte-for-byte backward compatibility.
    let khive_cfg = KhiveConfig::load_with_home_fallback(args.config.as_deref())
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
        .unwrap_or_default();

    if khive_cfg.backends.is_empty() {
        // Single-backend path — identical to pre-ADR-028 behavior.
        let runtime = KhiveRuntime::new(config)?;
        #[cfg(feature = "bench-embedder")]
        {
            for name in runtime.registered_embedding_model_names() {
                runtime.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        return KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"));
    }

    // Multi-backend path (ADR-028).
    build_server_multi_backend(config, &khive_cfg)
}

/// Open backends, run migrations, build per-pack runtimes, register packs.
///
/// Called only when `[[backends]]` is non-empty in `khive.toml`.
fn build_server_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<KhiveMcpServer> {
    // Open and migrate each declared backend.
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    for backend_cfg in &khive_cfg.backends {
        let backend = open_backend(backend_cfg)?;
        // Run migrations before passing backend to any runtime (risk §8 line 433).
        {
            let mut writer = backend.pool().try_writer().map_err(|e| {
                anyhow::anyhow!("backend {}: migration writer: {e}", backend_cfg.name)
            })?;
            run_migrations(writer.conn_mut())
                .map_err(|e| anyhow::anyhow!("backend {}: migration: {e}", backend_cfg.name))?;
        }
        backends.insert(backend_cfg.name.clone(), Arc::new(backend));
    }

    // Ensure the `main` backend exists (required fallback for unconfigured packs).
    let main_backend = backends
        .get(BackendId::MAIN)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[[backends]] is declared but no backend named \"main\" was found; \
             add a [[backends]] entry with name = \"main\""
            )
        })?
        .clone();

    // Build per-pack runtimes: each pack gets its assigned backend, or `main` as fallback.
    let pack_names = &base_config.packs;
    let mut per_pack_runtimes: HashMap<String, KhiveRuntime> = HashMap::new();
    for pack_name in pack_names {
        let backend_name = khive_cfg
            .packs
            .get(pack_name.as_str())
            .map(|pc| pc.backend.as_str())
            .unwrap_or(BackendId::MAIN);
        let backend = backends
            .get(backend_name)
            .cloned()
            .unwrap_or_else(|| main_backend.clone());
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::new(backend_name);
        per_pack_runtimes.insert(
            pack_name.clone(),
            KhiveRuntime::from_backend(backend, rt_config),
        );
    }

    // Build the default runtime (for the main backend) — used for EventStore wiring.
    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    });

    #[cfg(feature = "bench-embedder")]
    {
        for rt in per_pack_runtimes.values() {
            for name in rt.registered_embedding_model_names() {
                rt.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        for name in default_runtime.registered_embedding_model_names() {
            default_runtime
                .register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
        }
    }

    // Build the VerbRegistry using per-pack runtimes.
    let gate = default_runtime.config().gate.clone();
    let default_namespace = default_runtime.config().default_namespace.clone();
    let config_id = crate::server::compute_config_id(default_runtime.config());
    let visible_namespaces = default_runtime.config().visible_namespaces.clone();

    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_namespace.as_str());
    builder.with_visible_namespaces(visible_namespaces);
    // Thread authenticated actor identity (issue #75) into the multi-backend
    // registry exactly as the single-backend path does (server.rs). Without
    // this, dispatch mints ActorRef::anonymous() and comm.inbox reverts to
    // party-line, silently disabling actor-addressed delivery (ADR-057).
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    // Wire EventStore from the default (main) runtime.
    if let Ok(tok) = default_runtime.authorize(khive_runtime::Namespace::local()) {
        if let Ok(event_store) = default_runtime.events(&tok) {
            builder.with_event_store(event_store);
        }
    }

    PackRegistry::register_packs_with_runtimes(
        pack_names,
        &per_pack_runtimes,
        &default_runtime,
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration: {e}"))?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    // Install edge rules and embedders.
    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    registry.call_register_embedders(&default_runtime);

    // Apply schema plans to each pack's assigned backend.
    let backend_for_pack: HashMap<&str, &StorageBackend> = per_pack_runtimes
        .iter()
        .map(|(name, rt)| (name.as_str(), rt.backend()))
        .collect();
    let main_ref: &StorageBackend = main_backend.as_ref();
    registry.apply_schema_plans_with_map(&backend_for_pack, main_ref);

    Ok(KhiveMcpServer::from_registry_with_meta(
        registry,
        default_namespace.as_str(),
        &config_id,
    ))
}

/// Open a `StorageBackend` from a `BackendConfig`.
fn open_backend(cfg: &BackendConfig) -> anyhow::Result<StorageBackend> {
    match cfg.kind {
        BackendKind::Memory => StorageBackend::memory()
            .map_err(|e| anyhow::anyhow!("backend {}: memory open: {e}", cfg.name)),
        BackendKind::Sqlite => {
            let path = cfg.path.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "backend {}: sqlite backend requires a `path` field",
                    cfg.name
                )
            })?;
            let expanded = expand_tilde(path);
            if let Some(parent) = expanded.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!(
                        "backend {}: cannot create parent dir {}: {e}",
                        cfg.name,
                        parent.display()
                    )
                })?;
            }
            StorageBackend::sqlite(&expanded)
                .map_err(|e| anyhow::anyhow!("backend {}: sqlite open: {e}", cfg.name))
        }
    }
}

/// Expand a leading `~` to `$HOME` in a path.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(format!("{home}/{rest}"))
    } else if s == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
    } else {
        path.to_path_buf()
    }
}

/// Inputs for [`resolve_runtime_config`] — the subset of serve-time arguments
/// that determine the resolved [`RuntimeConfig`]. Callers other than
/// `kkernel mcp` (e.g. `kkernel reindex`) supply these directly so they resolve
/// the SAME engines, db path, and actor namespace the MCP server would.
pub struct RuntimeConfigInputs<'a> {
    /// Raw `--db` / `KHIVE_DB` value (`:memory:` sentinel honored).
    pub db: Option<&'a str>,
    /// Explicit `--config` / `KHIVE_CONFIG` path (else home-fallback search).
    pub config: Option<&'a std::path::Path>,
    /// Pre-resolved default namespace.
    pub namespace: khive_runtime::Namespace,
    /// Whether the namespace came from an explicit CLI flag (skips config tier).
    pub namespace_explicit: bool,
    /// Disable embedding entirely (still resolves actor namespace from config).
    pub no_embed: bool,
    /// Packs to register. `None` falls back to `RuntimeConfig::default().packs`.
    pub packs: Option<Vec<String>>,
    /// Explicit brain profile ID (highest-priority tier).
    ///
    /// `None` lets lower tiers (env var, config file, runtime fallback) handle
    /// resolution. Pass `Some(id)` only when the caller holds an explicit CLI value.
    pub brain_profile: Option<String>,
}

/// Resolve a [`RuntimeConfig`] from serve-time inputs, applying the SAME
/// config-file / env / actor-namespace precedence as `kkernel mcp`.
///
/// Extracted from `build_server` so `kkernel reindex` reuses the exact engine
/// and db resolution — otherwise an admin reindex writes vectors for the
/// default/env model set while the MCP server serves recall from the
/// config-file `[[engines]]` set.
pub fn resolve_runtime_config(inputs: RuntimeConfigInputs<'_>) -> anyhow::Result<RuntimeConfig> {
    let db_path = match inputs.db {
        Some(":memory:") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            Some(PathBuf::from(format!("{home}/.khive/khive.db")))
        }
    };

    let packs = inputs
        .packs
        .unwrap_or_else(|| RuntimeConfig::default().packs);

    // Tier-1: explicit CLI --brain-profile only (not env — env is tier-3, after TOML).
    // We must NOT read KHIVE_BRAIN_PROFILE here; RuntimeConfig::default() reads it, so
    // we exclude brain_profile from the default spread and set it to None (CLI-only).
    let cli_brain_profile = inputs.brain_profile.filter(|s| !s.trim().is_empty());

    let base_config = RuntimeConfig {
        db_path,
        default_namespace: inputs.namespace,
        packs,
        // Explicit CLI flag only at this tier — env and config-file tiers are applied
        // below in resolve_config / resolve_actor_from_config and apply_env_brain_profile.
        brain_profile: cli_brain_profile,
        ..RuntimeConfig::default()
    };

    let resolved = if inputs.no_embed {
        let no_embed_base = RuntimeConfig {
            embedding_model: None,
            additional_embedding_models: vec![],
            ..base_config
        };
        resolve_actor_from_config(inputs.config, no_embed_base, inputs.namespace_explicit)?
    } else {
        resolve_config(inputs.config, base_config)?
    };

    // Tier-3 env fallback: KHIVE_BRAIN_PROFILE is applied AFTER CLI (tier-1) and
    // config-file (tier-2) so that a project or global TOML always wins over the env var.
    Ok(apply_env_brain_profile(resolved))
}

/// Apply `KHIVE_BRAIN_PROFILE` env var as the tier-3 fallback for `brain_profile`.
///
/// Called after CLI (tier-1) and config-file (tier-2) have already been applied.
/// Only sets `brain_profile` when neither previous tier produced a value.
fn apply_env_brain_profile(mut cfg: RuntimeConfig) -> RuntimeConfig {
    if cfg.brain_profile.is_none() {
        cfg.brain_profile = std::env::var("KHIVE_BRAIN_PROFILE")
            .ok()
            .filter(|s| !s.trim().is_empty());
    }
    cfg
}

/// Resolve the full config (embedding engines + namespace) from file or env.
///
/// Precedence for the storage namespace (highest to lowest):
/// 1. CLI `--actor` / `--namespace` (carried in `base.default_namespace`)
/// 2. Default "local" from RuntimeConfig
///
/// Config file `[actor] id` does NOT set `default_namespace` — writes stay
/// pinned to `local` (ADR-007 Rev 4 Rule 0). A non-`'local'` `actor.id` IS
/// folded into the default READ visible-set (Rule 3b), but `runtime_config_from_khive_config`
/// preserves `base.default_namespace` regardless of the configured actor.
///
/// Precedence for embedding engines:
/// 1. Config file `[[engines]]`
/// 2. Env vars `KHIVE_EMBEDDING_MODEL` + `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
fn resolve_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if env_primary.is_some() || env_additional.is_some() {
                tracing::warn!(
                    "khive config file is present; KHIVE_EMBEDDING_MODEL and \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are ignored"
                );
            }

            Ok(runtime_config_from_khive_config(&khive_cfg, base))
        }
        None => {
            let env_cfg = config_from_env();
            if env_cfg.engines.is_empty() {
                Ok(base)
            } else {
                Ok(runtime_config_from_khive_config(&env_cfg, base))
            }
        }
    }
}

/// Resolve only the actor namespace from a config file (no-embed path).
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    cli_namespace_explicit: bool,
) -> anyhow::Result<RuntimeConfig> {
    if cli_namespace_explicit {
        return Ok(base);
    }
    match KhiveConfig::load_with_home_fallback(config_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let resolved = runtime_config_from_khive_config(&khive_cfg, base);
            Ok(RuntimeConfig {
                embedding_model: None,
                additional_embedding_models: vec![],
                ..resolved
            })
        }
        None => Ok(base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::Namespace;
    use serial_test::serial;
    use std::io::Write;

    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("khive.toml");
        let mut f = std::fs::File::create(&path).expect("create config file");
        f.write_all(body.as_bytes()).expect("write config");
        path
    }

    // The resolver MUST honor config-file `[[engines]]` over RuntimeConfig
    // defaults — otherwise `kkernel reindex` embeds for the wrong model set
    // versus what `kkernel mcp` serves recall from. Regression for PR #8
    // blocker.
    #[test]
    #[serial]
    fn resolver_uses_config_file_engines_over_defaults() {
        // Ensure env vars cannot leak into either branch.
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        let default_cfg = RuntimeConfig::default();
        let default_primary = format!("{:?}", default_cfg.embedding_model);
        // Default ships a non-empty additional-engine list (the multilingual
        // model). The single-engine config file below must override it.
        assert!(
            !default_cfg.additional_embedding_models.is_empty(),
            "precondition: default config has additional engines"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        // A single non-default engine that differs from the default primary.
        let path = write_config(
            dir.path(),
            r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config");

        let resolved_primary = format!("{:?}", resolved.embedding_model);
        assert_ne!(
            resolved_primary, default_primary,
            "resolved primary engine must come from the config file, not the default"
        );
        assert!(
            resolved.embedding_model.is_some(),
            "config-file engine must resolve to a primary embedding model"
        );
        assert!(
            resolved.additional_embedding_models.is_empty(),
            "config file declares one engine; additional list must be empty (not the default's)"
        );
        assert_eq!(resolved.db_path, None, ":memory: must map to in-memory db");
    }

    /// Regression for BLOCKER-1 (PR #52 codex review): project-toml brain_profile
    /// MUST win over KHIVE_BRAIN_PROFILE env var.
    ///
    /// Merged ADR-035 §Precedence: CLI > project toml > global toml > env > default.
    /// Before the fix, the env var was bound into the clap `brain_profile` arg and
    /// placed at tier-1 via RuntimeConfig::default() in the base_config spread,
    /// causing env to override TOML.
    #[test]
    #[serial]
    fn brain_profile_config_beats_env() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "project-profile"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None, // no explicit CLI flag
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("project-profile"),
            "project TOML brain_profile must win over KHIVE_BRAIN_PROFILE env var"
        );
    }

    /// Env var is used when no CLI flag and no TOML value are present.
    #[test]
    #[serial]
    fn brain_profile_env_fallback_when_no_toml() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        // Config file without [runtime] brain_profile.
        let path = write_config(
            dir.path(),
            r#"
[[engines]]
name = "primary"
model = "bge-small-en-v1.5"
default = true
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: None,
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("env-profile"),
            "env var must be used when no CLI flag and no TOML brain_profile is set"
        );
    }

    /// CLI flag wins over both TOML and env var.
    #[test]
    #[serial]
    fn brain_profile_cli_wins_over_all() {
        std::env::set_var("KHIVE_BRAIN_PROFILE", "env-profile");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "project-profile"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            no_embed: false,
            packs: None,
            brain_profile: Some("cli-profile".to_string()), // explicit CLI
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_BRAIN_PROFILE");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("cli-profile"),
            "CLI --brain-profile must win over both TOML and KHIVE_BRAIN_PROFILE env var"
        );
    }

    // --- multi-backend boot path (ADR-028) ---

    /// Build a `RuntimeConfig` suitable for multi-backend tests: in-memory db,
    /// AllowAllGate, "local" namespace, no embedder, both kg and comm packs.
    fn base_runtime_config_for_multi_backend() -> RuntimeConfig {
        use khive_runtime::{AllowAllGate, BackendId, Namespace};
        RuntimeConfig {
            db_path: None,
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::parse("local").expect("ns"),
            embedding_model: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        }
    }

    /// Two in-memory backends — `main` plus a second named `secondary`.
    /// The `comm` pack is pinned to `secondary`; `kg` defaults to `main`.
    /// Positive test: `build_server_multi_backend` must return `Ok` and both
    /// packs must be functional.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boots_ok_with_two_memory_backends() {
        use crate::tools::request::RequestParams;
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let server = build_server_multi_backend(base_cfg, &khive_cfg)
            .expect("multi-backend boot must succeed");

        // kg round-trip: create an entity on the main backend.
        let kg_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="concept", name="MultiBackendTestEntity")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
            })
            .await
            .expect("kg dispatch must not error");

        let kg_json: serde_json::Value =
            serde_json::from_str(&kg_resp).expect("kg response is valid JSON");
        // Response shape: {"results": [{ok, tool, result}], "summary": {...}}
        let first_ok = kg_json["results"][0]["ok"].as_bool();
        assert_eq!(
            first_ok,
            Some(true),
            "kg create must succeed; response: {kg_resp}"
        );

        // comm round-trip: send a message on the secondary backend.
        let comm_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="local", content="multi-backend-test")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
            })
            .await
            .expect("comm dispatch must not error");

        let comm_json: serde_json::Value =
            serde_json::from_str(&comm_resp).expect("comm response is valid JSON");
        let first_comm_ok = comm_json["results"][0]["ok"].as_bool();
        assert_eq!(
            first_comm_ok,
            Some(true),
            "comm.send must succeed; response: {comm_resp}"
        );
    }

    /// Regression for B-BLOCKER-1 (HC-7 critic): the multi-backend boot path
    /// MUST thread the configured actor identity (issue #75) into the registry,
    /// exactly as the single-backend path does. If `with_actor_id` is dropped,
    /// dispatch mints `ActorRef::anonymous()` and `comm.inbox` reverts to
    /// party-line — silently re-opening the cross-actor leak #75 fixed. With a
    /// configured actor `"actor-b"`, a message addressed to `"actor-a"` must NOT
    /// appear in `actor-b`'s inbox, while one addressed to `"actor-b"` must.
    #[tokio::test]
    #[serial]
    async fn multi_backend_preserves_actor_filtering() {
        use crate::tools::request::RequestParams;
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        // Configured actor — the value #75 threads end-to-end.
        let base_cfg = RuntimeConfig {
            actor_id: Some("actor-b".to_string()),
            ..base_runtime_config_for_multi_backend()
        };

        let server = build_server_multi_backend(base_cfg, &khive_cfg)
            .expect("multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                let resp = server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: None,
                        presentation_per_op: None,
                    })
                    .await
                    .expect("dispatch must not error");
                serde_json::from_str::<serde_json::Value>(&resp).expect("valid JSON")
            }
        };

        // One message to a different actor, one to ourselves.
        let to_a = dispatch(r#"comm.send(to="actor-a", content="for-a")"#.to_string()).await;
        assert_eq!(to_a["results"][0]["ok"].as_bool(), Some(true), "{to_a}");
        let to_b = dispatch(r#"comm.send(to="actor-b", content="for-b")"#.to_string()).await;
        assert_eq!(to_b["results"][0]["ok"].as_bool(), Some(true), "{to_b}");

        // Inbox for the configured actor (actor-b) must be filtered by to_actor.
        let inbox = dispatch(r#"comm.inbox()"#.to_string()).await;
        let result = &inbox["results"][0]["result"];
        let messages = result["messages"]
            .as_array()
            .expect("inbox returns a messages array");

        let contents: Vec<&str> = messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            contents.contains(&"for-b"),
            "actor-b must see the message addressed to it; got {contents:?}"
        );
        assert!(
            !contents.contains(&"for-a"),
            "actor-b must NOT see the message addressed to actor-a (leak #75 / B-BLOCKER-1); \
             got {contents:?} — actor identity was not threaded into the multi-backend registry"
        );
    }

    /// Negative test: `[[backends]]` is declared but there is no entry named
    /// `"main"`. `build_server_multi_backend` must return an error whose
    /// message mentions `"main"` so operators know what to fix.
    #[test]
    fn multi_backend_missing_main_returns_error_mentioning_main() {
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "secondary".to_string(), // intentionally NOT "main"
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: std::collections::HashMap::new(),
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_server_multi_backend(base_cfg, &khive_cfg);
        assert!(
            result.is_err(),
            "missing main backend must produce an error"
        );
        // Neither unwrap_err nor expect_err work because KhiveMcpServer is not Debug.
        // Extract the error via match instead.
        if let Err(err) = result {
            assert!(
                err.to_string().contains("main"),
                "error message must mention \"main\"; got: {err}"
            );
        }
    }
}
