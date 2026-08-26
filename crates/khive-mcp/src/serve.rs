//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use khive_runtime::{
    config_from_env, parse_pack_list, runtime_config_from_khive_config, BackendConfig, BackendId,
    BackendKind, BlobHydrator, ConnectionPool, KhiveConfig, KhiveRuntime, OutputFormat,
    RuntimeConfig, StorageBackend,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

/// Output of [`build_registry_for_multi_backend`] — carries the registry and
/// the per-pack runtimes so `kkernel` can build a `BackendRegistry` for the
/// coordinator (ADR-029 Phase 2).
pub struct MultiBackendRegistry {
    /// The assembled [`khive_runtime::VerbRegistry`] ready to be passed to a server.
    pub registry: khive_runtime::VerbRegistry,
    /// Namespace the registry was built for.
    pub default_namespace: String,
    /// Config fingerprint (for daemon matching).
    pub config_id: String,
    /// Pack-name → `Arc<KhiveRuntime>`, one entry per declared pack.
    pub per_pack_runtimes: HashMap<String, Arc<KhiveRuntime>>,
    /// The `main` backend (needed by the coordinator to build the BackendRegistry).
    pub main_backend: Arc<StorageBackend>,
    /// The default runtime this boot built alongside the per-pack runtimes.
    /// A clone (cheap: internal state is `Arc<RwLock<_>>`-shared), kept so
    /// callers — chiefly boot-wiring tests — can assert on its installed
    /// state (e.g. `has_note_write_validator()`) without re-deriving it.
    pub default_runtime: KhiveRuntime,
}

/// Stable machine code for a concrete database override that conflicts with
/// an already-declared multi-backend topology.
pub const DB_OVERRIDE_CONFLICT_CODE: &str = "database_override_conflict";

/// Invocation-level refusal raised before any verb is dispatched when a
/// concrete `--db`/`KHIVE_DB` value would collapse declared backends.
///
/// The `config_source` the envelope reports is the canonicalized selected
/// file path (`diagnostic_config_path`): under symlinks it can diverge from
/// the path the operator typed.
#[derive(Debug)]
pub struct DatabaseOverrideConflict {
    db_override: String,
    backend_count: usize,
    config_source: Option<PathBuf>,
}

impl DatabaseOverrideConflict {
    fn new(
        db_override: &str,
        backend_count: usize,
        config_source: Option<&std::path::Path>,
    ) -> Self {
        Self {
            db_override: db_override.to_owned(),
            backend_count,
            config_source: config_source.map(std::path::Path::to_path_buf),
        }
    }

    /// Stable JSON shape emitted by `kkernel exec` when dispatch never began.
    pub fn envelope(&self) -> serde_json::Value {
        let config_path = self
            .config_source
            .as_deref()
            .map(|path| path.display().to_string());
        serde_json::json!({
            "ok": false,
            "invocation": {
                "started": false,
            },
            "error": {
                "code": DB_OVERRIDE_CONFLICT_CODE,
                "message": self.to_string(),
                "db_override": self.db_override.as_str(),
                "declared_backends": self.backend_count,
                "config_path": config_path,
            },
        })
    }
}

impl std::fmt::Display for DatabaseOverrideConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "--db {:?} (or KHIVE_DB) cannot be combined with [[backends]]: {} \
             backend(s) are already declared in the discovered config, so applying this \
             override here is ambiguous (it could silently collapse distinct declared \
             backends onto a single file). Remedy: edit the backend paths in the \
             discovered config file (searched in order: ./khive.toml, \
             <db-dir>/config.toml, ~/.khive/config.toml), or point at a different config \
             with --config <file> / KHIVE_CONFIG.",
            self.db_override, self.backend_count
        )?;
        if let Some(path) = self.config_source.as_deref() {
            write!(
                formatter,
                " This invocation selected the config file at {}; the backends above \
                 are declared there.",
                path.display()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for DatabaseOverrideConflict {}

/// Return the stable invocation-refusal envelope when `error` carries a
/// [`DatabaseOverrideConflict`] anywhere in its source chain, not only as
/// the top-level error: an intermediate carrier that adds context
/// (`anyhow::Context`) must not silently degrade the documented JSON
/// refusal to a generic error rendering.
pub fn db_override_refusal_envelope(error: &anyhow::Error) -> Option<serde_json::Value> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DatabaseOverrideConflict>())
        .map(DatabaseOverrideConflict::envelope)
}

/// Build a server from `args`, then serve it over `--daemon` or the named transport.
///
/// #667: `build_server` runs migrations and applies pack schema plans (FTS DDL
/// included) while constructing the runtime. Acquiring the boot/recovery lock
/// *before* that call and holding it through daemon bind+pid-write (or
/// dropping it right after construction in non-daemon mode) closes the window
/// where a second concurrently-booting process could run schema DDL against
/// the same database file at the same time — see
/// [`khive_runtime::daemon::run_daemon_with_boot_guard`].
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    if let Some(generation) = args.resumed_generation {
        tracing::warn!(
            generation,
            "bridge self-heal: this process is a resumed generation of an \
             in-place re-exec triggered by a stale daemon-protocol mismatch (#714)"
        );
    }
    // #667: in daemon mode, failing to acquire the boot guard must abort
    // before `build_server` runs migrations/FTS DDL unguarded — see
    // `acquire_daemon_boot_guard`. Non-daemon callers keep the best-effort
    // lock (dropped right after construction below).
    #[cfg(unix)]
    let boot_guard = if args.daemon {
        Some(khive_runtime::daemon::acquire_daemon_boot_guard()?)
    } else {
        khive_runtime::daemon::acquire_recovery_lock()
    };
    let (server, schedule_rt) = build_server(&args).await?;

    #[cfg(feature = "channel-email")]
    spawn_email_channel_loops_if_daemon(&server, &args);
    #[cfg(feature = "channel-telegram")]
    spawn_telegram_channel_loops_if_daemon(&server, &args);
    start_daemon_components_if_daemon(&args, &server, schedule_rt);

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_boot_guard(server, boot_guard).await?;
        return Ok(());
    }
    #[cfg(unix)]
    drop(boot_guard);
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    // ADR-091 Amendment 2 Plank A: every non-daemon process runs the
    // observe-only session sweep (never PASSIVE/TRUNCATE checkpointing —
    // that stays daemon-owned).
    serve_with_session_sweep(server, &args, registry).await
}

/// Whether this process owns the email channel loops (#602).
///
/// Channel loops (IMAP poll + outbox scan) are a daemon-role responsibility:
/// before this gate, `spawn_email_channel_loops` was called unconditionally
/// from EVERY serve entrypoint, so every stdio `kkernel mcp` client process
/// (one per Claude Code session, agent, etc.) spawned its own independent IMAP
/// poll loop against the same mailbox. Nine concurrent pollers exhausted
/// Exchange Online's per-mailbox connection slots and took inbound email down
/// for ~19h on 2026-07-04. `args.daemon` is the same flag `run`/`serve_server`
/// already use to decide whether to hand off to
/// `khive_runtime::daemon::run_daemon`, so gating on it keeps daemon-role
/// detection in one place shared by both boot paths, matching the
/// `checkpoint_pool_for` pattern (#601/#604).
#[cfg(feature = "channel-email")]
fn is_daemon_role(args: &Args) -> bool {
    args.daemon
}

/// Combine process role with the fixed runtime-mode admission captured when
/// the server was built. A daemon flag alone never authorizes background
/// writes: each loop is admitted only when the runtime serving its verbs is
/// writable in the resolved single- or multi-backend topology.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_loop_plan(server: &KhiveMcpServer, args: &Args) -> crate::server::ChannelLoopAdmission {
    if args.daemon {
        server.channel_loop_admission()
    } else {
        crate::server::ChannelLoopAdmission::default()
    }
}

/// Handle for the ADR-091 Amendment 2 Plank A session sweep task. Dropping
/// the sender alone is NOT a sufficient shutdown contract (minor, ADR-091
/// Amendment 2): the sweep task's own clean-shutdown heartbeat
/// removal runs asynchronously after observing the channel close, and the
/// tokio runtime is not guaranteed to poll it to completion before the
/// process exits. [`Self::shutdown`] holds the `JoinHandle` and awaits it
/// (bounded) so the removal has actually run before `serve`/`run` returns.
struct SessionSweepHandle {
    shutdown_tx: tokio::sync::watch::Sender<()>,
    join: tokio::task::JoinHandle<()>,
}

impl SessionSweepHandle {
    async fn shutdown(self) {
        drop(self.shutdown_tx);
        if tokio::time::timeout(std::time::Duration::from_secs(2), self.join)
            .await
            .is_err()
        {
            tracing::warn!(
                "ADR-091 Amendment 2 Plank A: session sweep task did not exit within 2s of \
                 the shutdown signal; its walpin heartbeat removal may not have completed"
            );
        }
    }
}

/// Spawn the ADR-091 Amendment 2 Plank A observe-only session sweep task,
/// fanned out over every file-backed backend this server carries: `pool` as
/// the main backend, plus one entry per pool in `secondary_pools` (ADR-091
/// Amendment 3). Returns `None` only when the server has no file-backed
/// backend at all (a purely in-memory or registry-only server). Returns a
/// [`SessionSweepHandle`] the caller MUST hold for the session's run scope
/// and shut down explicitly (see [`SessionSweepHandle::shutdown`]) — mirrors
/// `run_checkpoint_task`'s shutdown-channel contract on the daemon side.
///
/// Called from BOTH non-daemon serve entrypoints (`run` and `serve_server`,
/// item: sweep coverage, ADR-091 Amendment 2) — `serve_server` is the
/// ADR-029 multi-backend coordinator boot path, and previously never started
/// this sweep at all, leaving every multi-backend session permanently
/// invisible to cross-process WAL-pin attribution.
///
/// Platform-independent (ADR-091 Amendment 2: "Windows is a
/// supported target"): the tx_registry age check and the walpin sidecar
/// write path (`khive_db::walpin`) both run on every platform now — only
/// sidecar-directory *enumeration* (the daemon's TRUNCATE-time attribution
/// read) is Unix-only, and daemon mode itself already requires Unix. A
/// Windows session still registers its beacon and writes heartbeats, so it
/// classifies as `reporting`/`registered-silent` (not a permanent `unknown`)
/// whenever a Unix daemon does enumerate the shared sidecar directory.
fn spawn_session_walpin_sweep(server: &KhiveMcpServer) -> Option<SessionSweepHandle> {
    let mut backends = Vec::new();
    if let Some(pool) = server.pool() {
        backends.push(khive_db::SweepBackend {
            pool,
            is_main: true,
        });
    }
    for pool in server.secondary_pools() {
        backends.push(khive_db::SweepBackend {
            pool,
            is_main: false,
        });
    }
    if backends.is_empty() {
        return None;
    }
    let config = khive_db::SessionSweepConfig::from_env();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let join = tokio::spawn(khive_db::run_session_sweep_task(
        backends,
        config,
        shutdown_rx,
    ));
    tracing::info!("ADR-091 Amendment 2 Plank A: session WAL-registry sweep started");
    Some(SessionSweepHandle { shutdown_tx, join })
}

/// Serve `server` on the transport resolved from `args` with the ADR-091
/// Amendment 2 Plank A session sweep held for exactly the serve scope.
///
/// Both non-daemon serve entrypoints (`run` and `serve_server`) funnel
/// through here so the sweep lifecycle exists in one place. Every serve-path
/// early return — unknown transport, serve error, clean serve return —
/// happens inside the inner future, upstream of the unconditional shutdown
/// below, so a future early return cannot leak the sweep task or skip its
/// clean-shutdown heartbeat removal. Explicit shutdown (not just a dropped
/// sender) means the task's heartbeat removal has actually completed before
/// this function returns.
async fn serve_with_session_sweep(
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
) -> anyhow::Result<()> {
    let session_sweep = spawn_session_walpin_sweep(&server);
    serve_holding_sweep(session_sweep, server, args, registry).await
}

/// Inner half of [`serve_with_session_sweep`], split so tests can inject an
/// observable [`SessionSweepHandle`] and prove the shutdown is awaited on
/// every return path — the completion signal fires happens-before this
/// function returns, which the spawn-composed wrapper cannot demonstrate
/// deterministically (a dropped sender also wakes the task, just not before
/// the caller resumes).
async fn serve_holding_sweep(
    session_sweep: Option<SessionSweepHandle>,
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
) -> anyhow::Result<()> {
    let result = async {
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
    .await;
    if let Some(sweep) = session_sweep {
        sweep.shutdown().await;
    }
    result
}

/// Admit each email channel loop only when this is the daemon process and the
/// runtime that backs that loop's verbs is writable. Shared by both serve
/// entrypoints (`run` and `serve_server`) so neither role nor storage-mode
/// gating can drift between single- and multi-backend boot paths.
///
/// If no daemon is running, mail is simply not polled until one starts — that
/// is the intended behavior, not a silent failure; the log line makes it
/// observable.
#[cfg(feature = "channel-email")]
fn spawn_email_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let admission = channel_loop_plan(server, args);
    if !is_daemon_role(args) {
        tracing::info!("email channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(
            "email channel loops: skipped (assigned comm and kg runtimes do not admit writes)"
        );
        return;
    }
    tracing::info!(
        inbound_poll = admission.inbound_poll,
        outbound_delivery = admission.outbound_delivery,
        "email channel loops: applying daemon/runtime admission"
    );
    spawn_email_channel_loops(server, admission);
}

/// Start ADR-119 daemon components in daemon role only. Non-daemon roles
/// must not start components and stay byte-identical in behavior and output
/// — the silent return keeps client runs unchanged. In daemon role the
/// registry itself always logs the enumerated roster (names + count),
/// including an empty one. A resolved `schedule_rt` contributes the dynamic
/// `schedule-tick` component; `None` leaves it out of the roster.
fn start_daemon_components_if_daemon(
    args: &Args,
    server: &KhiveMcpServer,
    schedule_rt: Option<KhiveRuntime>,
) -> usize {
    if !args.daemon {
        return 0;
    }
    // ADR-170: the main daemon supervises the events daemon — spawn it when
    // the events socket is unreachable, respawn if it dies. Both paths come
    // from the server's own resolved events-split config, so the supervised
    // daemon and the forwarding clients cannot anchor at diverging paths;
    // a socket is present exactly when this host upgraded to forwarding
    // (`enable_events_forwarding_for_daemon`), and the `KHIVE_EVENTS_SPLIT=0`
    // kill-switch already produced no split at resolution.
    // A read-only deployment never supervises an events daemon: the
    // supervised process opens the events sidecar writable, which would
    // create and schema-initialize it on this deployment's behalf. The
    // runtime side independently refuses to forward writes when its backend
    // is read-only, so the two guards fail safe together.
    #[cfg(unix)]
    if server.default_runtime_is_read_only() {
        tracing::info!("read-only deployment: events daemon supervision skipped");
    } else if let Some(split) = server.events_split_config() {
        if let Some(socket) = split.socket_path.clone() {
            khive_runtime::daemon::track_background_task(
                khive_runtime::events_split::supervise_events_daemon(split.db_path.clone(), socket),
            );
        }
    }
    crate::components::start_daemon_components_with_schedule(server, schedule_rt)
}

/// Admit the schedule runtime to daemon supervision only when its own assigned
/// backend is writable. This decision is deliberately per runtime: a read-only
/// main backend must not disable a schedule pack routed to a writable secondary,
/// while a writable main must not accidentally start a ticker for a schedule
/// pack routed to a read-only secondary.
fn writable_schedule_runtime(runtime: Option<KhiveRuntime>) -> Option<KhiveRuntime> {
    runtime.filter(|runtime| !runtime.is_read_only())
}

/// Spawn the email channel polling + outbox loops if the `channel-email`
/// feature is enabled and `KHIVE_EMAIL_*` config resolves. Non-fatal: logs a
/// warning and returns on incomplete config. Only call this with the
/// role-and-runtime admission returned by [`channel_loop_plan`] — use
/// [`spawn_email_channel_loops_if_daemon`], which both serve entrypoints call.
#[cfg(feature = "channel-email")]
fn spawn_email_channel_loops(
    server: &KhiveMcpServer,
    admission: crate::server::ChannelLoopAdmission,
) {
    use khive_channel::ChannelRegistry;
    use khive_channel_email::EmailChannel;
    use std::sync::Arc;

    match EmailChannel::from_env() {
        Ok(email_ch) => {
            let email_ch = Arc::new(email_ch);
            let mut ch_registry = ChannelRegistry::new();
            let dyn_ch: Arc<dyn khive_channel::Channel> = email_ch.clone();
            ch_registry.register(dyn_ch);
            let ch_registry = Arc::new(ch_registry);
            let verb_reg = server.verb_registry_clone();
            let runtime = server.channel_outbox_runtime_clone();
            let ingest_ns = ingest_namespace_from_env();
            let default_actor = default_inbound_actor_from_env();
            let mut allowlist = allowed_recipients_from_env();
            if allowlist.is_empty() {
                allowlist.push(email_ch.maintainer_address().to_string());
            }
            let mailbox = email_ch.mailbox().to_string();

            let ingest_ns_clone = ingest_ns.clone();
            let default_actor_clone = default_actor.clone();
            let verb_reg_poll = verb_reg.clone();
            let verb_reg_outbox = verb_reg.clone();
            let ingest_ns_outbox = ingest_ns.clone();
            let allowlist_clone = allowlist.clone();
            let mailbox_clone = mailbox.clone();
            let email_ch_clone = Arc::clone(&email_ch);
            let runtime_outbox = runtime.clone();

            let spawned = run_if_authorized(&ingest_ns, &verb_reg, || {
                if admission.inbound_poll {
                    tokio::task::spawn(async move {
                        if let Err(error) = ensure_channel_quarantine_storage(&verb_reg_poll).await
                        {
                            tracing::error!(
                                error = %error,
                                "email polling disabled: quarantine blob storage is unavailable"
                            );
                            return;
                        }
                        channel_poll_loop(
                            ch_registry,
                            verb_reg_poll,
                            ingest_ns_clone,
                            default_actor_clone,
                        )
                        .await;
                    });
                    tracing::info!("email channel polling loop started");
                }
                if admission.outbound_delivery {
                    match runtime_outbox {
                        Some(rt) => {
                            tokio::task::spawn(channel_outbox_loop(
                                email_ch_clone,
                                verb_reg_outbox,
                                rt,
                                ingest_ns_outbox,
                                mailbox_clone,
                                allowlist_clone,
                            ));
                            tracing::info!("email channel outbox loop started");
                        }
                        None => {
                            tracing::error!(
                                "email outbox loop was NOT started: server has no KG-routed \
                                 runtime handle, which the loop needs to claim external_id on \
                                 outbound notes; outbound mail will not be sent"
                            );
                        }
                    }
                }
            });
            if !spawned {
                tracing::error!(
                    namespace = %ingest_ns,
                    "email channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "channel-email feature is enabled but configuration is incomplete: {e}; \
                 email polling is disabled"
            );
        }
    }
}

/// Resolve the target namespace for ingested channel messages.
///
/// Reads `KHIVE_EMAIL_INGEST_NAMESPACE`; falls back to `"local"` when the
/// variable is unset or blank. Called once at server startup before the poll
/// loop is spawned.
#[cfg(feature = "channel-email")]
fn ingest_namespace_from_env() -> String {
    std::env::var("KHIVE_EMAIL_INGEST_NAMESPACE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Resolve the default inbound actor for fresh (uncorrelated) email messages.
///
/// Reads `KHIVE_EMAIL_DEFAULT_ACTOR`; falls back to `"lambda:leo"` when the
/// variable is unset or blank. Called once at server startup alongside
/// `ingest_namespace_from_env`.
#[cfg(feature = "channel-email")]
fn default_inbound_actor_from_env() -> String {
    std::env::var("KHIVE_EMAIL_DEFAULT_ACTOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "lambda:leo".to_string())
}

/// Parse the outbox allowlist from `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS`.
///
/// Returns a `Vec` of trimmed, non-empty address strings. When the env var is
/// unset or blank the returned vec is empty; callers should fall back to the
/// channel maintainer address in that case.
#[cfg(feature = "channel-email")]
fn allowed_recipients_from_env() -> Vec<String> {
    std::env::var("KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Run `on_authorized` only when the ingest namespace passes the preflight check.
///
/// Returns `true` when the closure was called (preflight passed), `false`
/// otherwise.  Tests can inject a counting closure to verify the loop is not
/// started when preflight fails (ADR-056 §6 fail-closed contract).
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn run_if_authorized(
    ns_str: &str,
    registry: &khive_runtime::VerbRegistry,
    on_authorized: impl FnOnce(),
) -> bool {
    if preflight_ingest_namespace(ns_str, registry) {
        on_authorized();
        true
    } else {
        false
    }
}

/// Validate and authorize the ingest namespace before spawning the poll loop.
///
/// Returns `true` when `ns_str` parses to a valid namespace AND the registry
/// gate permits it.  Returns `false` on any parse failure or authorization
/// denial, after logging the reason.  The caller must not spawn the poll loop
/// when this returns `false` (fail-closed, ADR-056 §6).
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn preflight_ingest_namespace(ns_str: &str, registry: &khive_runtime::VerbRegistry) -> bool {
    match khive_runtime::Namespace::parse(ns_str) {
        Ok(ns) => match registry.authorize_namespace(ns) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    namespace = %ns_str,
                    error = %e,
                    "ingest namespace authorization denied; email polling will not start"
                );
                false
            }
        },
        Err(e) => {
            tracing::error!(
                namespace = %ns_str,
                error = %e,
                "invalid ingest namespace string; email polling will not start"
            );
            false
        }
    }
}

#[cfg(feature = "channel-email")]
const CHANNEL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
const UNKNOWN_INGEST_QUARANTINE_THRESHOLD: u8 = 5;

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelIngestDisposition {
    Hold { attempt: Option<u8> },
    Quarantine,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_ingest_attempt_key(channel_kind: &str, external_id: Option<&str>) -> Option<String> {
    external_id.map(|external_id| format!("{channel_kind}:{external_id}"))
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn channel_ingest_disposition(
    classification: khive_runtime::ChannelIngestFailureClass,
    channel_kind: &str,
    external_id: Option<&str>,
    unknown_attempts: &mut std::collections::HashMap<String, u8>,
) -> ChannelIngestDisposition {
    use khive_runtime::ChannelIngestFailureClass;

    match classification {
        ChannelIngestFailureClass::Retryable { .. } => {
            ChannelIngestDisposition::Hold { attempt: None }
        }
        ChannelIngestFailureClass::Permanent { .. } => ChannelIngestDisposition::Quarantine,
        ChannelIngestFailureClass::Unknown { .. } => {
            let Some(key) = channel_ingest_attempt_key(channel_kind, external_id) else {
                return ChannelIngestDisposition::Hold { attempt: None };
            };
            let attempt = unknown_attempts.entry(key).or_default();
            *attempt = attempt.saturating_add(1);
            if *attempt >= UNKNOWN_INGEST_QUARANTINE_THRESHOLD {
                ChannelIngestDisposition::Quarantine
            } else {
                ChannelIngestDisposition::Hold {
                    attempt: Some(*attempt),
                }
            }
        }
    }
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn log_channel_quarantine(
    channel_kind: &str,
    classification: khive_runtime::ChannelIngestFailureClass,
    external_id: &str,
) {
    tracing::warn!(
        channel = channel_kind,
        classification = classification.name(),
        reason = classification.reason(),
        external_id,
        "quarantined inbound message after comm.ingest refusal"
    );
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn ensure_channel_quarantine_storage(
    registry: &khive_runtime::VerbRegistry,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    if !registry.has_verb("blob.put") || !registry.has_verb("blob.stat") {
        return Err(khive_runtime::RuntimeError::Unconfigured(
            "channel quarantine requires the blob pack".to_string(),
        ));
    }

    // `blob.stat` is read-only. Probing a valid, absent digest verifies both
    // verb registration and that the pack's BlobStore is installed without
    // publishing a startup artifact.
    registry
        .dispatch(
            "blob.stat",
            json!({"content_ref": "0000000000000000000000000000000000000000000000000000000000000000"}),
        )
        .await?;
    Ok(())
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn quarantine_channel_ingest_failure(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    channel_kind: &str,
    default_inbound_actor: Option<&str>,
    envelope: &khive_channel::ChannelEnvelope,
    classification: khive_runtime::ChannelIngestFailureClass,
) -> Result<(), khive_runtime::RuntimeError> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use serde_json::json;

    let external_id = envelope.external_id.as_deref().ok_or_else(|| {
        khive_runtime::RuntimeError::InvalidInput(
            "cannot quarantine a channel message without external_id".to_string(),
        )
    })?;
    let (replay_bytes, notification_to) = envelope
        .quarantine_replay
        .as_ref()
        .map(|replay| (replay.bytes.as_slice(), replay.notification_to.as_str()))
        .unwrap_or_else(|| (envelope.content.as_bytes(), envelope.to.as_str()));

    let put = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(replay_bytes)}))
        .await?;
    let content_ref = put
        .get("content_ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            khive_runtime::RuntimeError::Internal(
                "blob.put returned no string content_ref for channel quarantine".to_string(),
            )
        })?;

    // Quarantine sender prefix invariant: the sender retains the originating
    // channel prefix because prefix-keyed consumers depend on it for alert
    // visibility. Moving `email:quarantine` outside `email:` hides the alert.
    let quarantine_sender = format!("{channel_kind}:quarantine");
    debug_assert!(
        quarantine_sender.starts_with(&format!("{channel_kind}:")),
        "quarantine sender prefix invariant: prefix-keyed consumers require the channel prefix"
    );

    let mut params = json!({
        "namespace": ingest_namespace,
        "from": quarantine_sender,
        "to": notification_to,
        "content": "Inbound channel message quarantined. Original bytes are available through the attached content reference.",
        "channel_kind": channel_kind,
        "external_id": external_id,
        "metadata": {
            "quarantined": "true",
            "quarantine_classification": classification.name(),
            "quarantine_reason": classification.reason(),
            "quarantine_external_id": external_id,
            "quarantine_content_ref": content_ref,
        },
    });
    if let Some(actor) = default_inbound_actor {
        params["default_inbound_actor"] = json!(actor);
    }
    registry.dispatch("comm.ingest", params).await?;
    log_channel_quarantine(channel_kind, classification, external_id);
    Ok(())
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
async fn handle_channel_ingest_failure(
    registry: &khive_runtime::VerbRegistry,
    ingest_namespace: &str,
    channel_kind: &str,
    default_inbound_actor: Option<&str>,
    envelope: &khive_channel::ChannelEnvelope,
    error: &khive_runtime::RuntimeError,
    unknown_attempts: &mut std::collections::HashMap<String, u8>,
) -> bool {
    let classification = error.channel_ingest_failure_class();
    match channel_ingest_disposition(
        classification,
        channel_kind,
        envelope.external_id.as_deref(),
        unknown_attempts,
    ) {
        ChannelIngestDisposition::Hold { attempt } => {
            tracing::warn!(
                channel = channel_kind,
                classification = classification.name(),
                reason = classification.reason(),
                external_id = envelope.external_id.as_deref(),
                attempt,
                threshold = UNKNOWN_INGEST_QUARANTINE_THRESHOLD,
                "comm.ingest failed; holding channel progress"
            );
            false
        }
        ChannelIngestDisposition::Quarantine => {
            match quarantine_channel_ingest_failure(
                registry,
                ingest_namespace,
                channel_kind,
                default_inbound_actor,
                envelope,
                classification,
            )
            .await
            {
                Ok(()) => true,
                Err(quarantine_error) => {
                    tracing::warn!(
                        channel = channel_kind,
                        classification = classification.name(),
                        reason = classification.reason(),
                        external_id = envelope.external_id.as_deref(),
                        error = %quarantine_error,
                        "channel quarantine failed; holding progress for retry"
                    );
                    false
                }
            }
        }
    }
}

/// Background task that polls all registered channels every 5 seconds and
/// ingests new inbound messages via `comm.ingest`.
///
/// #605: the 5s cadence is the happy-path default only. A connect/auth
/// failure (classified by `khive_channel_email::is_backoff_eligible`) starts
/// a per-channel-kind jittered exponential backoff (`ImapBackoff`,
/// 5s -> 10s -> ... capped at ~10min) instead of retrying flat every 5s; a
/// success resets that channel's backoff to base, and the loop returns to
/// the normal 5s cadence. This is process-side pressure relief on top of the
/// per-credential single-flight guard inside `LiveImap` itself. Eligible
/// failures log via [`log_eligible_poll_failure`]: `warn!` only on an
/// escalation edge, `debug!` while riding the same capped step — never one
/// `warn!` per retry.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
async fn channel_poll_loop(
    channels: std::sync::Arc<khive_channel::ChannelRegistry>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
    default_inbound_actor: String,
) {
    use chrono::{DateTime, Utc};
    use khive_channel_email::{is_backoff_eligible, ImapBackoff};
    use serde_json::json;
    use std::collections::HashMap;
    // Per-channel bootstrap "since" floor (issue #449). This
    // only feeds the date-based SINCE search used while a channel has no
    // committed UID high-water yet (first-ever poll, or a UIDVALIDITY
    // reset); once a checkpoint has a high-water, polling is UID-ranged and
    // this floor is unused for that channel. Each entry only advances to the
    // poll tick's timestamp once that channel's full cycle -- cursor_get,
    // poll_page, every comm.ingest, and cursor_commit -- succeeds this tick.
    // Advancing it unconditionally (as a shared `last_poll` timestamp used
    // to) would drop the earlier floor on any bootstrap-cycle failure, and
    // if that failure spans a calendar-day boundary the next checkpoint-less
    // poll's SINCE clause would use the newer date, permanently skipping the
    // previous day's uncommitted mail.
    let mut bootstrap_since: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
    // One backoff state per (kind, slug) — i.e. per credential (#606).
    // Keying by kind alone would throttle a
    // second same-kind credential (e.g. a second mailbox) whenever the first
    // one's connection fails, even though the two are independent
    // credentials with independent connectivity.
    let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
    // ADR-094: tracks the error class of the most recent unresolved failure
    // per (kind, slug), so `ChannelPollFailed` fires once per failure episode
    // (first failure since success, or a change in error class) rather than
    // once per retry. Cleared on every success.
    let mut last_error_class: HashMap<(String, String), &'static str> = HashMap::new();
    // Unknown ingest failures are bounded per external message ID. Entries
    // remain pinned through quarantine until the page cursor itself commits,
    // so a cursor-commit failure cannot restart the five-attempt wait.
    let mut unknown_ingest_attempts: HashMap<String, u8> = HashMap::new();
    let mut next_interval = CHANNEL_POLL_INTERVAL;
    let event_store = registry.event_store();
    // Captured before the loop's first sleep (issue #449 follow-up).
    // A channel's very first bootstrap floor must reflect
    // when the daemon actually started, not whenever its first tick happens
    // to fire: `tokio::time::sleep` below runs before any polling, so
    // computing `now` after it (as the loop used to) can land on the far
    // side of a calendar-day boundary the daemon started before. Every
    // vacant `bootstrap_since` entry -- on tick 1 or any later tick a
    // channel is first seen on -- uses this single startup timestamp
    // instead of that tick's own `now`.
    let startup_since = Utc::now();

    loop {
        tokio::time::sleep(next_interval).await;
        next_interval = CHANNEL_POLL_INTERVAL;

        let now = Utc::now();

        for (kind, slug, channel) in channels.iter() {
            let backoff_key = (kind.to_string(), slug.to_string());
            let since = *bootstrap_since
                .entry(backoff_key.clone())
                .or_insert(startup_since);
            // Set once this channel's cycle durably completes (a fresh
            // commit, or nothing new to commit); gates whether `since`
            // advances past this tick's `now` for next time.
            let mut bootstrap_floor_advances = false;

            append_channel_lifecycle_event(
                event_store.as_ref(),
                khive_types::EventKind::ChannelPollStarted,
                khive_storage::ChannelPollStartedPayload {
                    channel_kind: kind.to_string(),
                    channel_slug: slug.to_string(),
                    since_rfc3339: since.to_rfc3339(),
                },
            )
            .await;

            // Durable checkpoint path (issue #449): cursor_get -> poll_page ->
            // every comm.ingest -> cursor_commit, committing only when the
            // whole page durably ingested. A cursor_get failure means we
            // cannot trust what progress to poll from, so this channel is
            // skipped for the cycle rather than risk polling from an empty
            // checkpoint and silently discarding durable state.
            let checkpoint = match load_channel_cursor(&registry, kind, slug).await {
                Ok(cp) => cp,
                Err(e) => {
                    tracing::warn!(
                        channel = kind,
                        "comm.cursor_get failed; skipping this channel's poll this cycle: {e}"
                    );
                    continue;
                }
            };

            match channel.poll_page(since, checkpoint.as_ref()).await {
                Ok(page) => {
                    let prior_attempt =
                        backoffs.get(&backoff_key).map(|b| b.attempt()).unwrap_or(0);
                    if let Some(backoff) = backoffs.get_mut(&backoff_key) {
                        backoff.record_success();
                    }
                    last_error_class.remove(&backoff_key);

                    // Only a recovery from a prior failure/backoff episode is
                    // an interesting lifecycle transition — an unbroken
                    // string of healthy polls never had ChannelPollFailed
                    // fire, so there is nothing to report recovering from.
                    if prior_attempt > 0 {
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollSucceeded,
                            khive_storage::ChannelPollSucceededPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                envelope_count: page.envelopes.len(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelBackoffReset,
                            khive_storage::ChannelBackoffResetPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                previous_backoff_attempt: prior_attempt,
                            },
                        )
                        .await;
                    }

                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Success,
                        event_store.as_ref(),
                    )
                    .await;

                    // Every envelope in the page must durably ingest before
                    // the cursor is allowed to advance past it (issue #449):
                    // a partial-page ingest failure must leave
                    // the checkpoint untouched so the next poll re-selects
                    // the whole page -- comm.ingest's `INSERT OR IGNORE`
                    // dedup then skips re-storing the messages that already
                    // succeeded, and only the failed one is retried.
                    let page_attempt_keys: Vec<String> = page
                        .envelopes
                        .iter()
                        .filter_map(|env| {
                            channel_ingest_attempt_key(kind, env.external_id.as_deref())
                        })
                        .collect();
                    let mut page_fully_ingested = true;
                    for env in page.envelopes {
                        let params = json!({
                            "namespace": ingest_namespace,
                            "from": env.from.clone(),
                            "to": env.to.clone(),
                            "content": env.content.clone(),
                            "subject": env.subject.clone(),
                            "channel_kind": kind,
                            "channel_slug": slug,
                            "external_id": env.external_id.clone(),
                            "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
                            "correlation_external_id": env.correlation_external_id.clone(),
                            "default_inbound_actor": default_inbound_actor,
                            "wire_message_id": env.wire_message_id.clone(),
                            "wire_references": env.wire_references.clone(),
                            "metadata": env.metadata.clone(),
                        });
                        if let Err(error) = registry.dispatch("comm.ingest", params).await {
                            let handled = handle_channel_ingest_failure(
                                &registry,
                                &ingest_namespace,
                                kind,
                                Some(&default_inbound_actor),
                                &env,
                                &error,
                                &mut unknown_ingest_attempts,
                            )
                            .await;
                            if !handled {
                                page_fully_ingested = false;
                            }
                        }
                    }

                    if page_fully_ingested {
                        match page.next_checkpoint {
                            Some(next_checkpoint) => {
                                match commit_channel_cursor(&registry, kind, slug, &next_checkpoint)
                                    .await
                                {
                                    Ok(()) => bootstrap_floor_advances = true,
                                    Err(e) => {
                                        tracing::warn!(
                                            channel = kind,
                                            "comm.cursor_commit failed; progress not durably \
                                             advanced, next poll will retry: {e}"
                                        );
                                    }
                                }
                            }
                            // Nothing new to commit this tick is not a
                            // failure -- safe to advance the bootstrap floor.
                            None => bootstrap_floor_advances = true,
                        }
                        if bootstrap_floor_advances {
                            for key in page_attempt_keys {
                                unknown_ingest_attempts.remove(&key);
                            }
                        }
                    } else {
                        tracing::warn!(
                            channel = kind,
                            "not committing IMAP cursor: at least one message in this page \
                             failed comm.ingest; the whole page will be retried next poll"
                        );
                    }
                }
                Err(e) => {
                    let class = channel_error_class(&e);
                    record_channel_heartbeat(
                        &registry,
                        kind,
                        slug,
                        HeartbeatOutcome::Failure {
                            class,
                            message: e.to_string(),
                        },
                        event_store.as_ref(),
                    )
                    .await;

                    // First failure since success or since the error class
                    // changed — a run of identical retries at the same class
                    // does not re-fire this event.
                    if last_error_class.get(&backoff_key) != Some(&class) {
                        last_error_class.insert(backoff_key.clone(), class);
                        append_channel_lifecycle_event(
                            event_store.as_ref(),
                            khive_types::EventKind::ChannelPollFailed,
                            khive_storage::ChannelPollFailedPayload {
                                channel_kind: kind.to_string(),
                                channel_slug: slug.to_string(),
                                error_class: class.to_string(),
                                error_message: e.to_string(),
                            },
                        )
                        .await;
                    }

                    if is_backoff_eligible(&e) {
                        let backoff = backoffs.entry(backoff_key).or_default();
                        let tick = backoff.record_failure();
                        log_eligible_poll_failure(kind, &e, &tick);
                        next_interval = next_interval.max(tick.delay);

                        if tick.should_warn {
                            append_channel_lifecycle_event(
                                event_store.as_ref(),
                                khive_types::EventKind::ChannelBackoffArmed,
                                khive_storage::ChannelBackoffArmedPayload {
                                    channel_kind: kind.to_string(),
                                    channel_slug: slug.to_string(),
                                    attempt: tick.attempt,
                                    step_ms: tick.step.as_millis() as u64,
                                    delay_ms: tick.delay.as_millis() as u64,
                                },
                            )
                            .await;
                        }
                    } else {
                        // Non-eligible failures (config/gate errors, never
                        // produced by poll/connect in practice) are not
                        // connectivity pressure, so they keep the pre-#605
                        // warn-every-retry behavior at the normal cadence.
                        tracing::warn!(channel = kind, "channel poll failed: {e}");
                    }
                }
            }

            if bootstrap_floor_advances {
                bootstrap_since.insert((kind.to_string(), slug.to_string()), now);
            }
        }
    }
}

/// Append one ADR-094 channel lifecycle event, namespaced and attributed the
/// same way as `record_channel_heartbeat`'s persisted rows.
///
/// Best-effort: `store == None` is a no-op, and a serialize/append failure is
/// logged and swallowed — no lifecycle-append error may ever interrupt or
/// slow down channel polling.
#[cfg(feature = "channel-email")]
async fn append_channel_lifecycle_event<P: serde::Serialize>(
    store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
    kind: khive_types::EventKind,
    payload: P,
) {
    let Some(store) = store else {
        return;
    };
    let payload_value = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_kind = %kind.name(),
                "failed to serialize channel lifecycle event payload"
            );
            return;
        }
    };
    let event = khive_storage::Event::new(
        khive_pack_comm::CHANNEL_HEALTH_NAMESPACE,
        "channel.poll_lifecycle",
        kind,
        khive_types::SubstrateKind::Event,
        "daemon:channel_poll_loop",
    )
    .with_payload(payload_value);
    if let Err(err) = store.append_event(event).await {
        tracing::warn!(
            error = %err,
            event_kind = %kind.name(),
            "channel lifecycle event append failed"
        );
    }
}

/// One poll attempt's outcome, as reported to `comm.heartbeat` (#606).
#[cfg(feature = "channel-email")]
enum HeartbeatOutcome {
    Success,
    Failure {
        class: &'static str,
        message: String,
    },
}

/// Map a [`khive_channel::ChannelError`] to the `comm.heartbeat` `error_class`
/// open string enum (#606: `auth | transport | config`
/// in v1, callers must tolerate unknown classes). `Auth`/`Transport` are the
/// connectivity classes `is_backoff_eligible` already distinguishes;
/// `Config`/`UnauthorizedSender`/`InvalidEnvelope` are static/attribution
/// failures, never produced by `poll`/`connect` in practice (see
/// `is_backoff_eligible`'s doc comment), so they all map to `"config"`.
#[cfg(feature = "channel-email")]
fn channel_error_class(err: &khive_channel::ChannelError) -> &'static str {
    match err {
        khive_channel::ChannelError::Auth(_) => "auth",
        khive_channel::ChannelError::Transport(_) => "transport",
        khive_channel::ChannelError::Config(_)
        | khive_channel::ChannelError::UnauthorizedSender(_)
        | khive_channel::ChannelError::InvalidEnvelope(_) => "config",
    }
}

/// Persist one poll attempt's outcome via the `comm.heartbeat` subhandler
/// (#606). Best-effort: a failed write is logged, never interrupts the poll
/// loop. Takes NO `namespace` param — heartbeat rows are always dispatched
/// against `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` regardless of the
/// daemon's configured `KHIVE_EMAIL_INGEST_NAMESPACE` (2026-07-04); an
/// explicitly-scoped `comm.health` read may see a different namespace
/// (khive #877).
#[cfg(feature = "channel-email")]
async fn record_channel_heartbeat(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    outcome: HeartbeatOutcome,
    event_store: Option<&std::sync::Arc<dyn khive_storage::EventStore>>,
) {
    use serde_json::json;

    let namespace = khive_pack_comm::CHANNEL_HEALTH_NAMESPACE;
    let params = match &outcome {
        HeartbeatOutcome::Success => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "success",
        }),
        HeartbeatOutcome::Failure { class, message } => json!({
            "namespace": namespace,
            "channel_kind": channel_kind,
            "channel_slug": channel_slug,
            "poll_interval_secs": CHANNEL_POLL_INTERVAL.as_secs(),
            "outcome": "failure",
            "error_class": class,
            "error_message": message,
        }),
    };
    if let Err(e) = registry.dispatch("comm.heartbeat", params).await {
        tracing::warn!(
            channel = channel_kind,
            "comm.heartbeat failed to persist poll outcome: {e}"
        );
        append_channel_lifecycle_event(
            event_store,
            khive_types::EventKind::ChannelHeartbeatPersistFailed,
            khive_storage::ChannelHeartbeatPersistFailedPayload {
                channel_kind: channel_kind.to_string(),
                channel_slug: channel_slug.to_string(),
                error: e.to_string(),
            },
        )
        .await;
    }
}

/// Load the durable poll checkpoint for `(channel_kind, channel_slug)` via
/// `comm.cursor_get` (issue #449). Returns `Ok(None)` on first-run
/// (`comm.cursor_get` returns JSON `null`). A dispatch failure or a
/// malformed response is returned as `Err` so the caller skips this
/// channel's poll for the cycle rather than risk polling with empty
/// progress and silently discarding durable state.
#[cfg(feature = "channel-email")]
async fn load_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
) -> Result<Option<khive_channel::StoredChannelCheckpoint>, khive_runtime::RuntimeError> {
    use serde_json::json;

    let value = registry
        .dispatch(
            "comm.cursor_get",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
            }),
        )
        .await?;
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(|e| {
        khive_runtime::RuntimeError::Internal(format!(
            "comm.cursor_get returned a malformed checkpoint: {e}"
        ))
    })
}

/// Persist the durable poll checkpoint for `(channel_kind, channel_slug)`
/// via `comm.cursor_commit` (issue #449).
///
/// Callers MUST only call this after every envelope in the page has
/// returned `Ok` from `comm.ingest` -- see `channel_poll_loop`. Committing
/// on a partial page would advance the cursor past a message that was never
/// durably ingested, permanently skipping it.
#[cfg(feature = "channel-email")]
async fn commit_channel_cursor(
    registry: &khive_runtime::VerbRegistry,
    channel_kind: &str,
    channel_slug: &str,
    checkpoint: &khive_channel::ChannelCheckpoint,
) -> Result<(), khive_runtime::RuntimeError> {
    use serde_json::json;

    registry
        .dispatch(
            "comm.cursor_commit",
            json!({
                "channel_kind": channel_kind,
                "channel_slug": channel_slug,
                "source": checkpoint.source,
                "generation": checkpoint.generation,
                "high_water": checkpoint.high_water,
            }),
        )
        .await?;
    Ok(())
}

/// Log a backoff-eligible poll failure at the level ADR-091's `crossing_warn`
/// discipline calls for: `warn!` only on an escalation edge
/// (`tick.should_warn`, i.e. the computed step just changed), `debug!` on a
/// repeat at the same step. Regression fix (2026-07-04): the poll loop
/// previously emitted a generic `warn!` on every eligible retry in addition
/// to the escalation-edge warn, so sustained pressure spammed warn-level logs
/// once per retry instead of once per escalation. Extracted to a standalone
/// function so the level decision is unit-testable without driving the full
/// poll loop.
#[cfg(feature = "channel-email")]
fn log_eligible_poll_failure(
    kind: &str,
    err: &khive_channel::ChannelError,
    tick: &khive_channel_email::BackoffTick,
) {
    if tick.should_warn {
        tracing::warn!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "IMAP poll backoff escalating after connect/auth failure: {err}"
        );
    } else {
        tracing::debug!(
            channel = kind,
            attempt = tick.attempt,
            delay_secs = tick.delay.as_secs_f64(),
            "channel poll failed, holding at current backoff step: {err}"
        );
    }
}

/// True if a note's `delivered_at` property marks it as already delivered.
///
/// Must match the `list` query predicate's null handling (`list.rs`): a
/// present-but-null `delivered_at` is undelivered, not delivered. Checking
/// `.is_some()` alone would treat an explicit null (e.g. left by a curation
/// `update`) as delivered and strand the note in the outbox forever.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn note_already_delivered(props: &serde_json::Map<String, serde_json::Value>) -> bool {
    props
        .get("delivered_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Background task that delivers undelivered outbound email notes every 5 seconds.
///
/// Implements AT-LEAST-ONCE delivery: the `external_id` (= RFC 822 Message-ID) is
/// persisted to the note BEFORE sending. A crash between the SMTP success and the
/// `delivered_at` write causes a duplicate send on restart; the duplicate carries
/// the same Message-ID so receiving MTAs typically collapse it.
///
/// The `external_id` claim goes through `runtime`'s non-wire
/// `claim_outbound_message_external_id` rather than a `registry.dispatch("update",
/// ...)` call: `external_id` is one of the owner-established properties the
/// generic `update` verb refuses to patch on a pack-owned note kind, so a caller
/// patch through `dispatch` is rejected by design and would leave every note
/// stuck retrying forever.
///
/// Only compiled when the `channel-email` feature is enabled.
#[cfg(feature = "channel-email")]
async fn channel_outbox_loop(
    email_channel: std::sync::Arc<khive_channel_email::EmailChannel>,
    registry: khive_runtime::VerbRegistry,
    runtime: khive_runtime::KhiveRuntime,
    ingest_namespace: String,
    mailbox: String,
    allowlist: Vec<String>,
) {
    let domain = mailbox.split('@').nth(1).unwrap_or("localhost").to_string();
    let namespace = match khive_runtime::Namespace::parse(&ingest_namespace) {
        Ok(ns) => ns,
        Err(e) => {
            tracing::error!(
                namespace = %ingest_namespace,
                error = %e,
                "outbox loop: ingest namespace does not parse; loop will not run"
            );
            return;
        }
    };

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        channel_outbox_once(
            email_channel.as_ref(),
            &registry,
            &runtime,
            &namespace,
            &ingest_namespace,
            &mailbox,
            &domain,
            &allowlist,
        )
        .await;
    }
}

/// Execute one email outbox scan. Kept separate from the five-second loop so
/// routing and owner-claim behavior can be verified without sleeping or
/// opening a network transport.
#[cfg(feature = "channel-email")]
#[allow(clippy::too_many_arguments)]
async fn channel_outbox_once(
    email_channel: &dyn khive_channel::Channel,
    registry: &khive_runtime::VerbRegistry,
    runtime: &khive_runtime::KhiveRuntime,
    namespace: &khive_runtime::Namespace,
    ingest_namespace: &str,
    mailbox: &str,
    domain: &str,
    allowlist: &[String],
) {
    use chrono::Utc;
    use khive_channel::ChannelEnvelope;
    use serde_json::json;

    // Query outbound messages via the registry. The note `list` handler applies
    // the `direction` filter server-side (scanning up to its internal cap) and
    // returns an `items` envelope of full note objects. There is no `delivered_at`
    // or recipient-prefix filter, so the `email:` prefix and the
    // already-delivered check are applied per-note below.
    let list_params = json!({
        "namespace": ingest_namespace,
        "kind": "message",
        "direction": "outbound",
        "delivered": false,
        "limit": 200,
    });
    let list_result = match registry.dispatch("list", list_params).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(error = %error, "outbox loop: list failed");
            return;
        }
    };

    let Some(notes) = list_result
        .get("items")
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for note_val in notes {
        let props = match note_val.get("properties") {
            Some(serde_json::Value::Object(properties)) => properties.clone(),
            _ => continue,
        };

        if props.get("direction").and_then(|value| value.as_str()) != Some("outbound") {
            continue;
        }
        let to_actor = match props.get("to_actor").and_then(|value| value.as_str()) {
            Some(actor) if actor.starts_with("email:") => actor.to_string(),
            _ => continue,
        };
        if note_already_delivered(&props) {
            continue;
        }
        let note_id = match note_val.get("id").and_then(|value| value.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let recipient = to_actor
            .strip_prefix("email:")
            .unwrap_or(to_actor.as_str())
            .to_string();
        if !allowlist.is_empty() && !allowlist.contains(&recipient) {
            tracing::warn!(
                note_id = %note_id,
                recipient = %recipient,
                "outbox loop: recipient not in allowlist; skipping"
            );
            continue;
        }

        let subject = props
            .get("subject")
            .and_then(|value| value.as_str())
            .unwrap_or("(no subject)")
            .to_string();
        let content = match note_val.get("content").and_then(|value| value.as_str()) {
            Some(content) => content.to_string(),
            None => continue,
        };
        let thread_id = props
            .get("thread_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let in_reply_to = props
            .get("in_reply_to_message_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let references = props
            .get("references_chain")
            .and_then(|value| value.as_str())
            .map(str::to_string);

        // Mint-before-send through the KG runtime's owner-only path. Generic
        // `update` correctly refuses caller patches to `external_id`.
        let message_id = match props.get("external_id").and_then(|value| value.as_str()) {
            Some(external_id) if !external_id.is_empty() => external_id.to_string(),
            _ => {
                let message_id = format!("<{note_id}@{domain}>");
                let claim_result = match uuid::Uuid::parse_str(&note_id) {
                    Ok(uuid) => match runtime.authorize(namespace.clone()) {
                        Ok(token) => {
                            runtime
                                .claim_outbound_message_external_id(
                                    &token,
                                    uuid,
                                    message_id.clone(),
                                )
                                .await
                        }
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(khive_runtime::RuntimeError::InvalidInput(format!(
                        "note id {note_id} is not a valid UUID: {error}"
                    ))),
                };
                if let Err(error) = claim_result {
                    tracing::warn!(
                        note_id = %note_id,
                        error = %error,
                        "outbox loop: failed to claim external_id; skipping"
                    );
                    continue;
                }
                message_id
            }
        };

        let mut envelope = ChannelEnvelope::new(
            format!("email:{mailbox}"),
            format!("email:{recipient}"),
            content,
        )
        .with_subject(subject)
        .with_message_id(message_id.clone());
        if let Some(thread_id) = thread_id {
            envelope = envelope.with_correlation(thread_id);
        }
        if let Some(in_reply_to) = in_reply_to {
            envelope = envelope.with_in_reply_to(in_reply_to);
        }
        if let Some(references) = references {
            envelope = envelope.with_references(references);
        }

        match email_channel.send(envelope).await {
            Ok(()) => {
                let delivered_at = Utc::now().to_rfc3339();
                match registry
                    .dispatch(
                        "update",
                        json!({
                            "namespace": ingest_namespace,
                            "id": note_id,
                            "properties": { "delivered_at": delivered_at },
                        }),
                    )
                    .await
                {
                    Ok(_) => tracing::info!(
                        note_id = %note_id,
                        recipient = %recipient,
                        message_id = %message_id,
                        "outbox loop: delivered"
                    ),
                    Err(error) => tracing::warn!(
                        note_id = %note_id,
                        error = %error,
                        "outbox loop: failed to set delivered_at (AT-LEAST-ONCE: will retry)"
                    ),
                }
            }
            Err(error) => tracing::warn!(
                note_id = %note_id,
                recipient = %recipient,
                error = %error,
                "outbox loop: send failed; will retry next cycle"
            ),
        }
    }
}

/// Apply the same independent daemon/runtime admission as the email adapter:
/// Telegram polling follows the comm runtime, while outbound delivery follows
/// the kg runtime that must durably mark `delivered_at`.
#[cfg(feature = "channel-telegram")]
fn spawn_telegram_channel_loops_if_daemon(server: &KhiveMcpServer, args: &Args) {
    let admission = channel_loop_plan(server, args);
    if !args.daemon {
        tracing::info!("telegram channel loops: skipped (client role; daemon owns channel loops)");
        return;
    }
    if !admission.inbound_poll && !admission.outbound_delivery {
        tracing::info!(
            "telegram channel loops: skipped (assigned comm and kg runtimes do not admit writes)"
        );
        return;
    }
    tracing::info!(
        inbound_poll = admission.inbound_poll,
        outbound_delivery = admission.outbound_delivery,
        "telegram channel loops: applying daemon/runtime admission"
    );
    spawn_telegram_channel_loops(server, admission);
}

/// Spawn the Telegram channel polling + outbox loops if the `channel-telegram`
/// feature is enabled and `KHIVE_TELEGRAM_*` config resolves. Non-fatal: logs
/// a warning and returns on incomplete config. Only call this with the
/// role-and-runtime admission returned by [`channel_loop_plan`] — use
/// [`spawn_telegram_channel_loops_if_daemon`].
///
/// Unlike the email adapter, Telegram's poll offset is held in memory inside
/// `TelegramChannel` itself (ADR-056 Amendment 2026-07-05, "Poll offset and
/// restart durability") — there is no per-channel checkpoint/cursor
/// persistence, backoff escalation, or ADR-094 lifecycle-event surface for
/// this adapter; those are email-specific hardening (#605/#606/ADR-094)
/// this ADR explicitly does not require for Telegram's simpler getUpdates
/// durability model.
#[cfg(feature = "channel-telegram")]
fn spawn_telegram_channel_loops(
    server: &KhiveMcpServer,
    admission: crate::server::ChannelLoopAdmission,
) {
    use khive_channel_telegram::TelegramChannel;
    use std::sync::Arc;

    match TelegramChannel::from_env() {
        Ok(tg_ch) => {
            let tg_ch = Arc::new(tg_ch);
            let verb_reg = server.verb_registry_clone();
            let ingest_ns = telegram_ingest_namespace_from_env();

            let verb_reg_poll = verb_reg.clone();
            let verb_reg_outbox = verb_reg.clone();
            let ingest_ns_poll = ingest_ns.clone();
            let ingest_ns_outbox = ingest_ns.clone();
            let tg_ch_poll = Arc::clone(&tg_ch);
            let tg_ch_outbox = Arc::clone(&tg_ch);

            let spawned = run_if_authorized(&ingest_ns, &verb_reg, || {
                if admission.inbound_poll {
                    tokio::task::spawn(async move {
                        if let Err(error) = ensure_channel_quarantine_storage(&verb_reg_poll).await
                        {
                            tracing::error!(
                                error = %error,
                                "telegram polling disabled: quarantine blob storage is unavailable"
                            );
                            return;
                        }
                        telegram_poll_loop(tg_ch_poll, verb_reg_poll, ingest_ns_poll).await;
                    });
                    tracing::info!("telegram channel polling loop started");
                }
                if admission.outbound_delivery {
                    tokio::task::spawn(telegram_outbox_loop(
                        tg_ch_outbox,
                        verb_reg_outbox,
                        ingest_ns_outbox,
                    ));
                    tracing::info!("telegram channel outbox loop started");
                }
            });
            if !spawned {
                tracing::error!(
                    namespace = %ingest_ns,
                    "telegram channel loops NOT started: ingest namespace authorization failed (fail-closed)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "channel-telegram feature is enabled but configuration is incomplete: {e}; \
                 telegram polling is disabled"
            );
        }
    }
}

/// Resolve the target namespace for ingested Telegram messages.
///
/// Reads `KHIVE_TELEGRAM_INGEST_NAMESPACE`; falls back to `"local"` when the
/// variable is unset or blank. Called once at server startup before the poll
/// loop is spawned.
#[cfg(feature = "channel-telegram")]
fn telegram_ingest_namespace_from_env() -> String {
    std::env::var("KHIVE_TELEGRAM_INGEST_NAMESPACE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Background task that polls the Telegram channel via `getUpdates` long
/// polling and ingests new inbound messages via `comm.ingest`. No
/// backoff/heartbeat/lifecycle-event surface — see
/// [`spawn_telegram_channel_loops`]'s doc comment for why this is a
/// deliberately smaller loop than `channel_poll_loop`.
///
/// The Bot API `getUpdates` call itself blocks server-side for the
/// connector's long-poll timeout awaiting new updates (ADR-056 Amendment
/// 2026-07-05 requires long polling, not short polling), so the success path
/// adds no extra sleep between requests — the long poll paces the loop.
/// Only the error path sleeps, so a failing Bot API does not hot-loop.
///
/// A fetched batch's offset is committed (acknowledged to Telegram) only
/// after every authorized envelope in it durably ingests via `comm.ingest`
/// — mirrors the IMAP cursor-commit discipline at `channel_poll_loop`
/// without importing its IMAP-specific machinery (issue #113).
#[cfg(feature = "channel-telegram")]
async fn telegram_poll_loop(
    telegram_channel: std::sync::Arc<khive_channel_telegram::TelegramChannel>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
) {
    use chrono::Utc;
    use khive_channel::Channel;
    use serde_json::json;

    const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
    let mut unknown_ingest_attempts = std::collections::HashMap::<String, u8>::new();

    loop {
        match telegram_channel.poll(Utc::now()).await {
            Ok(envelopes) => {
                let kind = telegram_channel.kind();
                let slug = telegram_channel.slug();
                let batch_attempt_keys: Vec<String> = envelopes
                    .iter()
                    .filter_map(|env| channel_ingest_attempt_key(kind, env.external_id.as_deref()))
                    .collect();
                let mut all_ingested = true;
                for env in envelopes {
                    let params = json!({
                        "namespace": ingest_namespace,
                        "from": env.from.clone(),
                        "to": env.to.clone(),
                        "content": env.content.clone(),
                        "channel_kind": kind,
                        "channel_slug": &slug,
                        "external_id": env.external_id.clone(),
                        "sent_at": env.sent_at.as_ref().map(|ts| ts.to_rfc3339()),
                    });
                    if let Err(error) = registry.dispatch("comm.ingest", params).await {
                        let handled = handle_channel_ingest_failure(
                            &registry,
                            &ingest_namespace,
                            kind,
                            None,
                            &env,
                            &error,
                            &mut unknown_ingest_attempts,
                        )
                        .await;
                        if !handled {
                            all_ingested = false;
                        }
                    }
                }

                if all_ingested {
                    telegram_channel.commit_offset();
                    for key in batch_attempt_keys {
                        unknown_ingest_attempts.remove(&key);
                    }
                } else {
                    tracing::warn!(
                        channel = kind,
                        "not committing telegram offset: at least one message in this batch \
                         failed comm.ingest; the whole batch will be retried next poll"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    channel = telegram_channel.kind(),
                    "telegram channel poll failed: {e}"
                );
                tokio::time::sleep(ERROR_BACKOFF).await;
            }
        }
    }
}

/// Background task that delivers undelivered outbound notes addressed to a
/// `telegram:` recipient every 5 seconds. Mirrors `channel_outbox_loop`'s
/// note-scan/send/mark-delivered shape without the Message-ID minting logic
/// (Telegram has no RFC 822 Message-ID concept).
#[cfg(feature = "channel-telegram")]
async fn telegram_outbox_loop(
    telegram_channel: std::sync::Arc<khive_channel_telegram::TelegramChannel>,
    registry: khive_runtime::VerbRegistry,
    ingest_namespace: String,
) {
    use chrono::Utc;
    use khive_channel::{Channel, ChannelEnvelope};
    use serde_json::json;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let list_params = json!({
            "namespace": ingest_namespace,
            "kind": "message",
            "direction": "outbound",
            "delivered": false,
            "limit": 200,
        });
        let list_result = match registry.dispatch("list", list_params).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "telegram outbox loop: list failed");
                continue;
            }
        };

        let notes = match list_result
            .get("items")
            .and_then(serde_json::Value::as_array)
        {
            Some(arr) => arr.clone(),
            None => continue,
        };

        for note_val in notes {
            let props = match note_val.get("properties") {
                Some(serde_json::Value::Object(m)) => m.clone(),
                _ => continue,
            };

            if props.get("direction").and_then(|v| v.as_str()) != Some("outbound") {
                continue;
            }

            let to_actor = match props.get("to_actor").and_then(|v| v.as_str()) {
                Some(a) if a.starts_with("telegram:") => a.to_string(),
                _ => continue,
            };

            if note_already_delivered(&props) {
                continue;
            }

            let note_id = match note_val.get("id").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };

            let content = match note_val.get("content").and_then(|v| v.as_str()) {
                Some(c) => c.to_string(),
                None => continue,
            };

            let env = ChannelEnvelope::new("telegram:bot", to_actor, content);

            match telegram_channel.send(env).await {
                Ok(()) => {
                    let delivered_at = Utc::now().to_rfc3339();
                    let mark_result = registry
                        .dispatch(
                            "update",
                            json!({
                                "namespace": ingest_namespace,
                                "id": note_id,
                                "properties": { "delivered_at": delivered_at },
                            }),
                        )
                        .await;
                    match mark_result {
                        Ok(_) => {
                            tracing::info!(note_id = %note_id, "telegram outbox loop: delivered");
                        }
                        Err(e) => {
                            tracing::warn!(
                                note_id = %note_id,
                                error = %e,
                                "telegram outbox loop: failed to set delivered_at (AT-LEAST-ONCE: will retry)"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        note_id = %note_id,
                        error = %e,
                        "telegram outbox loop: send failed; will retry next cycle"
                    );
                }
            }
        }
    }
}

/// Serve a pre-built server (ADR-029 Phase 2 boot path).
///
/// Extracted from `run()` so that `kkernel`'s `Command::Mcp` arm can build a
/// coordinator-equipped server and then call this to drive the
/// daemon/transport dispatch. The `Args` object is still needed for `--daemon`,
/// `--transport`, and `--bind` flags.
///
/// `boot_guard` is the recovery lock the caller acquired *before* building
/// `server` (#667) — building a multi-backend coordinator server also runs
/// migrations and applies pack schema plans, so the same
/// acquire-before-construct/hold-through-bind pattern used in [`run`] applies
/// here. Pass `None` only if the caller could not acquire the lock.
///
/// `schedule_rt` is the caller's resolved `"schedule"`-pack runtime handle
/// (ADR-106) — see `start_daemon_components_if_daemon`. `kkernel`'s
/// coordinator-attached multi-backend boot path resolves this from the same
/// `MultiBackendRegistry.per_pack_runtimes` map it uses to build `server`
/// itself, so the tick drains the identical backend/actor/pack configuration
/// the live server serves.
pub async fn serve_server(
    server: KhiveMcpServer,
    args: &Args,
    registry: &TransportRegistry,
    boot_guard: Option<std::fs::File>,
    schedule_rt: Option<KhiveRuntime>,
) -> anyhow::Result<()> {
    if let Some(generation) = args.resumed_generation {
        tracing::warn!(
            generation,
            "bridge self-heal: this process is a resumed generation of an \
             in-place re-exec triggered by a stale daemon-protocol mismatch (#714)"
        );
    }
    #[cfg(feature = "channel-email")]
    spawn_email_channel_loops_if_daemon(&server, args);
    #[cfg(feature = "channel-telegram")]
    spawn_telegram_channel_loops_if_daemon(&server, args);
    start_daemon_components_if_daemon(args, &server, schedule_rt);

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon_with_boot_guard(server, boot_guard).await?;
        return Ok(());
    }
    drop(boot_guard);
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    // ADR-091 Amendment 2 Plank A: every non-daemon process runs the
    // observe-only session sweep — including this ADR-029 multi-backend
    // coordinator boot path (sweep coverage, ADR-091 Amendment 2).
    // Without this spawn, every multi-backend session is permanently
    // invisible to cross-process WAL-pin attribution.
    serve_with_session_sweep(server, args, registry).await
}

/// Build the VerbRegistry and per-pack runtimes for a multi-backend deployment
/// (ADR-028 + ADR-029 Phase 2).
///
/// Returns a [`MultiBackendRegistry`] that `kkernel` uses to both:
/// 1. Construct the `KhiveMcpServer` (via `from_registry_with_meta`), and
/// 2. Build the `BackendRegistry` for the `SubstrateCoordinator`.
///
/// This is an asynchronous host-boot boundary, not a pure registry constructor.
/// Before it returns, every distinct secondary backend has been inventoried and
/// the canonical main backend has completed the application-assisted V21
/// attachment cutover. No runtime or attachment-only GC surface is exposed while
/// that work is pending or incomplete.
///
/// This is a refactor-extraction of the registry-building logic from
/// `build_server_multi_backend`, keeping the existing tests intact.
///
/// `cli_db_override` is the raw, pre-resolution `--db` / `KHIVE_DB` value (issue
/// #553). `[[backends]]` in `khive.toml` otherwise wins unconditionally, so an
/// operator's `--db :memory:` isolation request was silently discarded whenever
/// any backend was declared. `Some(":memory:")` forces every declared backend to
/// in-memory for this invocation (loudly logged). A concrete path matching the
/// declared `main` backend is accepted as a no-op; any other concrete path is
/// rejected rather than silently collapsing distinct declared backends onto one
/// caller-supplied file.
pub async fn build_registry_for_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override).await
}

/// Build the coordinated multi-backend registry while checking a canonical
/// database anchor captured earlier in config discovery.
///
/// This has the same secondary-inventory and V21-completion guarantee as
/// [`build_registry_for_multi_backend`].
pub async fn build_registry_for_multi_backend_with_db_anchor(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> anyhow::Result<MultiBackendRegistry> {
    // Regression fence: `base_config.db_path` feeds `compute_config_id` below,
    // so it must agree with the canonical anchor for this same `--db` input.
    // This is the shared choke point both multi-backend boot paths funnel
    // through — `build_server_multi_backend` in this file and `kkernel`'s
    // `Command::Mcp` coordinator-attached branch — so the guard lives here
    // once instead of at each caller.
    khive_runtime::assert_captured_db_anchor_consistent(base_config.db_path.as_deref(), db_anchor)?;

    build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override).await
}

/// One backend's schema result from [`migrate_configured_storage_topology`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSchemaMigrationStatus {
    /// Configured backend name (`main` for the implicit single backend).
    pub backend: String,
    /// Applied canonical core-schema version after coordination.
    pub applied_version: u32,
    /// Whether this backend advanced only as a prerequisite for the selected
    /// canonical-main target.
    pub prerequisite: bool,
}

#[derive(Debug, Clone)]
struct ConfiguredStorageTargetPlan {
    effective_backends: Vec<BackendConfig>,
    full_topology: bool,
}

fn effective_backend_configs(backends: &[BackendConfig], force_memory: bool) -> Vec<BackendConfig> {
    backends
        .iter()
        .map(|backend| {
            if force_memory {
                BackendConfig {
                    kind: BackendKind::Memory,
                    path: None,
                    ..backend.clone()
                }
            } else {
                backend.clone()
            }
        })
        .collect()
}

fn validate_effective_backend_alias_modes(backends: &[BackendConfig]) -> anyhow::Result<()> {
    let mut physical_sqlite: HashMap<std::path::PathBuf, (&str, bool)> = HashMap::new();
    for backend in backends {
        let Some(canonical) = canonical_backend_path(backend)? else {
            continue;
        };
        if let Some((first_name, first_read_only)) = physical_sqlite.get(&canonical) {
            if *first_read_only != backend.read_only {
                anyhow::bail!(
                    "backend {} aliases {} (already declared by backend {}) but declares \
                     read_only={} while the same physical database is declared read_only={}; \
                     every alias of one database must use the same access mode",
                    backend.name,
                    canonical.display(),
                    first_name,
                    backend.read_only,
                    first_read_only,
                );
            }
        } else {
            physical_sqlite.insert(canonical, (&backend.name, backend.read_only));
        }
    }
    Ok(())
}

fn plan_configured_storage_targets(
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<ConfiguredStorageTargetPlan> {
    reject_conflicting_db_override_with_source(cli_db_override, &khive_cfg.backends, None)?;
    let force_memory = cli_db_override == Some(":memory:");
    let effective_backends = effective_backend_configs(&khive_cfg.backends, force_memory);
    validate_effective_backend_alias_modes(&effective_backends)?;

    let main = effective_backends
        .iter()
        .find(|backend| backend.name == BackendId::MAIN)
        .ok_or_else(|| anyhow::anyhow!("configured topology has no \"main\" backend"))?;
    let full_topology = match target_backend {
        None => true,
        Some(target) => {
            let selected = effective_backends
                .iter()
                .find(|backend| backend.name == target)
                .ok_or_else(|| {
                    let defined = effective_backends
                        .iter()
                        .map(|backend| backend.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow::anyhow!("unknown backend {target:?}; defined backends: {defined}")
                })?;
            match (
                canonical_backend_path(selected)?,
                canonical_backend_path(main)?,
            ) {
                (Some(selected), Some(main)) => selected == main,
                // A force-memory override intentionally creates one distinct
                // ephemeral backend per configured name; only the literal
                // main name is the canonical-main target in that mode.
                (None, None) => selected.name == main.name,
                _ => false,
            }
        }
    };

    Ok(ConfiguredStorageTargetPlan {
        effective_backends,
        full_topology,
    })
}

/// Return the configured backend names a read-only schema check must inspect.
///
/// This shares the migration command's physical-alias planning. Selecting
/// `main` (or a SQLite alias of it) therefore includes every secondary
/// prerequisite; selecting an independent secondary inspects only that target.
pub fn configured_storage_check_targets(
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    if khive_cfg.backends.is_empty() {
        if let Some(target) = target_backend {
            if target != BackendId::MAIN {
                anyhow::bail!(
                    "unknown backend {target:?}; the implicit single-backend topology contains \
                     only \"main\""
                );
            }
        }
        return Ok(vec![BackendId::MAIN.to_string()]);
    }

    let plan = plan_configured_storage_targets(khive_cfg, cli_db_override, target_backend)?;
    if plan.full_topology {
        Ok(plan
            .effective_backends
            .into_iter()
            .map(|backend| backend.name)
            .collect())
    } else {
        Ok(vec![target_backend
            .expect("a partial topology plan always has a selected target")
            .to_string()])
    }
}

/// Migrate storage without constructing pack runtimes or loading embedders.
///
/// When `[[backends]]` is present this executes the same deduplicated,
/// secondary-first inventory and main V21 cutover barrier as normal MCP boot.
/// With no declared topology it uses the single-backend boot coordinator. No
/// runtime is exposed and no pack DDL is applied.
pub async fn migrate_configured_storage_topology(
    mut base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    target_backend: Option<&str>,
) -> anyhow::Result<Vec<BackendSchemaMigrationStatus>> {
    if khive_cfg.backends.is_empty() {
        if let Some(target) = target_backend {
            if target != BackendId::MAIN {
                anyhow::bail!(
                    "unknown backend {target:?}; the implicit single-backend topology contains \
                     only \"main\""
                );
            }
        }
        let backend = prepare_single_backend_for_schema_admin(&base_config, khive_cfg).await?;
        let applied_version = read_applied_schema_version(backend.sql().as_ref()).await?;
        return Ok(vec![BackendSchemaMigrationStatus {
            backend: BackendId::MAIN.to_string(),
            applied_version,
            prerequisite: false,
        }]);
    }

    let plan = plan_configured_storage_targets(khive_cfg, cli_db_override, target_backend)?;
    if !plan.full_topology {
        let target = target_backend.expect("a partial topology plan always has a target");
        let _force_memory = normalize_redundant_db_override(
            &mut base_config,
            cli_db_override,
            &khive_cfg.backends,
        )?;
        let selected = plan
            .effective_backends
            .iter()
            .find(|backend| backend.name == target)
            .expect("the planner validated the selected backend")
            .clone();
        let backend = Arc::new(open_backend(&selected)?);
        prepare_core_schema_for_boot(Arc::clone(&backend), format!("backend {}", selected.name))
            .await?;
        crate::attachment_cutover::require_secondary_attachment_empty(
            Arc::clone(&backend),
            &selected.name,
        )
        .await?;
        crate::attachment_cutover::coordinate_empty_secondary_attachment_cutover(
            Arc::clone(&backend),
            &selected.name,
        )
        .await?;
        return Ok(vec![BackendSchemaMigrationStatus {
            backend: selected.name,
            applied_version: read_applied_schema_version(backend.sql().as_ref()).await?,
            prerequisite: false,
        }]);
    }

    let prepared = prepare_configured_storage_topology(
        base_config,
        khive_cfg,
        cli_db_override,
        StorageTopologyPurpose::SchemaAdministration,
    )
    .await?;
    let selected_target = target_backend
        .and_then(|target| prepared.backends.get(target))
        .cloned();
    let mut statuses = Vec::with_capacity(khive_cfg.backends.len());
    for configured in &khive_cfg.backends {
        let backend = prepared.backends.get(&configured.name).ok_or_else(|| {
            anyhow::anyhow!(
                "configured backend {:?} disappeared during schema coordination",
                configured.name
            )
        })?;
        statuses.push(BackendSchemaMigrationStatus {
            backend: configured.name.clone(),
            applied_version: read_applied_schema_version(backend.sql().as_ref()).await?,
            prerequisite: selected_target
                .as_ref()
                .is_some_and(|selected| !Arc::ptr_eq(selected, backend)),
        });
    }
    Ok(statuses)
}

async fn read_applied_schema_version(sql: &dyn khive_storage::SqlAccess) -> anyhow::Result<u32> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let mut reader = sql
        .reader()
        .await
        .map_err(|error| anyhow::anyhow!("open schema-version reader: {error}"))?;
    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations".into(),
            params: vec![],
            label: Some("read_applied_schema_version".into()),
        })
        .await
        .map_err(|error| anyhow::anyhow!("read applied schema version: {error}"))?;
    match value {
        Some(SqlValue::Integer(version)) if version >= 0 => Ok(version as u32),
        other => anyhow::bail!("schema-version query returned an invalid value: {other:?}"),
    }
}

/// Validate a `--db`/`KHIVE_DB` override against a non-empty `[[backends]]`
/// declaration WITHOUT opening any backend — the same rule
/// `build_registry_for_multi_backend_inner` enforces, factored out so a
/// caller that hasn't yet decided whether it will construct backends in this
/// process can apply the check up front.
///
/// This closes #1226: `kkernel exec`'s daemon-forward fast path (inline ops,
/// used whenever a warm daemon answers) never called into this guard at all
/// — only the in-process fallback did — so an inline invocation with a
/// conflicting override silently forwarded to the daemon's own already-open
/// backends instead of being rejected, while the same override on
/// `--ops-file` (always in-process by design) correctly bailed. The two call
/// forms disagreed about whether the override was legal because only one of
/// them ever ran this check. Returns `Ok(true)` when the override forces
/// every backend to in-memory (`:memory:`), `Ok(false)` when there is no
/// override to apply or the concrete override already names the declared
/// `main` backend.
pub fn validate_db_override_against_backends(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    validate_db_override_against_backends_with_source(cli_db_override, backends, None)
}

/// Source-preserving form of [`validate_db_override_against_backends`].
/// `config_source` is diagnostic only; it never participates in routing.
pub fn validate_db_override_against_backends_with_source(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let backend_count = backends.len();
    reject_conflicting_db_override_with_source(cli_db_override, backends, config_source)?;
    match cli_db_override {
        Some(":memory:") => {
            tracing::warn!(
                "--db :memory: (or KHIVE_DB=:memory:) is forcing {backend_count} configured \
                 [[backends]] entries to ephemeral in-memory storage for this invocation; \
                 the backend paths declared in the discovered config (./khive.toml, \
                 <db-dir>/config.toml, or ~/.khive/config.toml) will not be used, and nothing \
                 written this run persists after the process exits"
            );
            Ok(true)
        }
        Some(other) => {
            if backends.is_empty() {
                tracing::info!(
                    "--db {other:?} (or KHIVE_DB) with no declared [[backends]]: the \
                     override names the database directly (ordinary single-backend case)"
                );
            } else {
                tracing::info!(
                    "--db {other:?} (or KHIVE_DB) matches the path declared for the \
                     \"main\" backend in khive.toml; proceeding because the override is a no-op"
                );
            }
            Ok(false)
        }
        None => Ok(false),
    }
}

/// Fail only for a conflicting concrete override, without logging accepted
/// `:memory:` or redundant-main cases. Boot paths use this before their shared
/// builder so a refusal retains its config source without duplicating the
/// builder's acceptance logs.
pub fn reject_conflicting_db_override_with_source(
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Self-guard (not a caller contract): with no declared backends there is
    // nothing to collapse, so a concrete override is the ordinary
    // single-backend case and must pass. Without this, the main-backend
    // lookup below finds nothing and EVERY concrete override would be
    // misclassified as ambiguous.
    if backends.is_empty() {
        return Ok(());
    }
    let Some(other) = cli_db_override.filter(|path| *path != ":memory:") else {
        return Ok(());
    };
    if override_matches_declared_main_backend(other, backends)? {
        return Ok(());
    }
    Err(DatabaseOverrideConflict::new(other, backends.len(), config_source).into())
}

/// Validate a database override and normalize a redundant concrete override
/// to the same fingerprint anchor used when no override is supplied.
///
/// Once a concrete path is proven to name the declared `main` backend, the
/// backend topology fully identifies storage and the override has no remaining
/// semantic effect. Retaining it in `RuntimeConfig.db_path` would nevertheless
/// change `compute_config_id` and prevent sharing the default warm daemon.
pub fn normalize_redundant_db_override(
    config: &mut RuntimeConfig,
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    normalize_redundant_db_override_with_source(config, cli_db_override, backends, None)
}

/// Source-preserving form of [`normalize_redundant_db_override`].
pub fn normalize_redundant_db_override_with_source(
    config: &mut RuntimeConfig,
    cli_db_override: Option<&str>,
    backends: &[BackendConfig],
    config_source: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let force_memory = validate_db_override_against_backends_with_source(
        cli_db_override,
        backends,
        config_source,
    )?;
    // The anchor rewrite is only sound once the override has been proven
    // redundant against a declared `main` backend. With no declared backends
    // the concrete override is the ordinary single-backend case: it names the
    // database directly and must be preserved, not collapsed to the
    // no-override anchor.
    if !backends.is_empty() && matches!(cli_db_override, Some(path) if path != ":memory:") {
        config.db_path = khive_runtime::resolve_db_anchor(None);
    }
    Ok(force_memory)
}

fn override_matches_declared_main_backend(
    override_path: &str,
    backends: &[BackendConfig],
) -> anyhow::Result<bool> {
    let Some(main) = backends
        .iter()
        .find(|backend| backend.name == BackendId::MAIN)
    else {
        return Ok(false);
    };
    if main.kind != BackendKind::Sqlite {
        return Ok(false);
    }
    let Some(main_path) = main.path.as_ref() else {
        return Ok(false);
    };

    Ok(canonical_path_no_side_effects(main_path)?
        == canonical_path_no_side_effects(std::path::Path::new(override_path))?)
}

/// Refuse a declared writable SQLite backend whose current filesystem mode is
/// already read-only, without opening SQLite or creating any path.
///
/// The multi-backend boot path performs the same check after open so its
/// captured runtime identity remains authoritative. Pre-open clients must run
/// this narrower probe before daemon forwarding as well: a warm daemon may
/// still hold a write-capable handle acquired before a later chmod, and the
/// declaration-only topology fingerprint would otherwise route the request to
/// that retained writer. A force-memory override supersedes every declared
/// path and must skip this helper entirely at its call site.
pub fn validate_declared_backend_access_modes(backends: &[BackendConfig]) -> anyhow::Result<()> {
    for backend in backends {
        if backend.kind != BackendKind::Sqlite || backend.read_only {
            continue;
        }
        let Some(path) = backend.path.as_ref() else {
            continue;
        };
        let expanded = khive_runtime::expand_tilde(path);
        let metadata = match std::fs::metadata(&expanded) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "backend {}: cannot inspect filesystem access mode for {} before daemon \
                     forwarding: {error}",
                    backend.name,
                    expanded.display(),
                ));
            }
        };
        if metadata.permissions().readonly() {
            anyhow::bail!(
                "backend {}: path {} has no filesystem write bits; declare `read_only = true` \
                 so backend topology and daemon config identity describe the snapshot-inspection \
                 mode explicitly (request was refused before daemon forwarding)",
                backend.name,
                expanded.display(),
            );
        }
    }
    Ok(())
}

struct PreparedStorageTopology {
    base_config: RuntimeConfig,
    backends: HashMap<String, Arc<StorageBackend>>,
    main_backend: Arc<StorageBackend>,
    shared_hydrator: Option<Arc<BlobHydrator>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageTopologyPurpose {
    Serving,
    SchemaAdministration,
}

async fn schema_admin_requires_blob_hydrator(backend: Arc<StorageBackend>) -> anyhow::Result<bool> {
    let status_backend = Arc::clone(&backend);
    let status = tokio::task::spawn_blocking(move || status_backend.attachment_cutover_status())
        .await
        .context("schema-admin attachment status task panicked")?
        .context("inspect schema-admin attachment status")?;
    if status == khive_db::migrations::AttachmentCutoverStatus::Complete {
        return Ok(false);
    }

    Ok(
        khive_pack_moodboard::legacy_preference_model_count(backend.sql().as_ref())
            .await
            .context("count legacy moodboard models before resolving blob storage")?
            != 0,
    )
}

async fn prepare_configured_storage_topology(
    mut base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    purpose: StorageTopologyPurpose,
) -> anyhow::Result<PreparedStorageTopology> {
    let force_memory =
        normalize_redundant_db_override(&mut base_config, cli_db_override, &khive_cfg.backends)?;
    let effective_backends = effective_backend_configs(&khive_cfg.backends, force_memory);
    // Validate every physical alias before opening the first database. A
    // targeted admin command and full server boot must reject the same mixed
    // read-only/read-write alias topology without creating a partial set of
    // database files.
    validate_effective_backend_alias_modes(&effective_backends)?;

    // ADR-170: the events split must anchor beside the store that actually
    // holds this deployment's data. The resolver derived it from
    // `base_config.db_path`, which for declared-backend configs is the
    // materialized `$HOME/.khive/khive.db` default rather than the declared
    // main backend — re-anchor beside main's declared file here, preserving
    // the resolver/daemon-host mode decision (direct vs forwarding) by
    // re-deriving the socket beside the moved events db. An in-memory main
    // (including a force-memory override) carries no event-plane split.
    if let Some(split) = base_config.events_split.take() {
        let main_path = effective_backends
            .iter()
            .find(|b| b.name == BackendId::MAIN && b.kind == BackendKind::Sqlite)
            .and_then(|b| b.path.as_ref());
        if let Some(main_path) = main_path {
            let expanded = khive_runtime::expand_tilde(main_path);
            let db_path = khive_runtime::events_split::events_db_path_beside(&expanded);
            let socket_path = split
                .socket_path
                .is_some()
                .then(|| khive_runtime::events_split::events_socket_path_beside(&db_path));
            base_config.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
                db_path,
                socket_path,
            });
        }
    }

    // Open each declared backend, deduplicating SQLite backends by canonical
    // path (ADR-028 §8). Schema preparation is deliberately deferred until
    // after main is identified: every distinct secondary must be inventoried
    // before main can atomically enable attachment-only GC at V21.
    let mut backends: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    let mut path_to_backend: HashMap<std::path::PathBuf, Arc<StorageBackend>> = HashMap::new();
    for backend_cfg in &effective_backends {
        let canonical = canonical_backend_path(backend_cfg)?;
        if let Some(ref canon) = canonical {
            if let Some(existing) = path_to_backend.get(canon) {
                if existing.is_read_only() != backend_cfg.read_only {
                    anyhow::bail!(
                        "backend {} aliases {} but declares read_only={} while the same \
                         physical database was already opened with read_only={}; every alias \
                         of one database must use the same access mode",
                        backend_cfg.name,
                        canon.display(),
                        backend_cfg.read_only,
                        existing.is_read_only(),
                    );
                }
                backends.insert(backend_cfg.name.clone(), existing.clone());
                continue;
            }
        }
        let backend = open_backend(backend_cfg)?;
        let arc = Arc::new(backend);
        if let Some(canon) = canonical {
            path_to_backend.insert(canon, arc.clone());
        }
        backends.insert(backend_cfg.name.clone(), arc);
    }

    let main_backend = backends
        .get(BackendId::MAIN)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[[backends]] is declared but no backend named \"main\" was found; \
             add a [[backends]] entry with name = \"main\""
            )
        })?
        .clone();

    // ADR-160 D4 chooses one liveness authority: attachment-bearing records
    // live only on main. Preflight every distinct secondary before main can
    // expose a durable incomplete marker, then finish any empty interrupted
    // V21 stage there so every runtime starts on one exact schema.
    let mut checked_secondary = HashSet::new();
    let mut unique_secondary = Vec::new();
    for configured in &khive_cfg.backends {
        let backend_name = configured.name.as_str();
        let backend = backends.get(backend_name).ok_or_else(|| {
            anyhow::anyhow!(
                "configured backend {backend_name:?} disappeared during topology preparation"
            )
        })?;
        if Arc::ptr_eq(backend, &main_backend) {
            continue;
        }
        let identity = Arc::as_ptr(backend) as usize;
        if checked_secondary.insert(identity) {
            prepare_core_schema_for_boot(Arc::clone(backend), format!("backend {backend_name}"))
                .await?;
            crate::attachment_cutover::require_secondary_attachment_empty(
                Arc::clone(backend),
                backend_name,
            )
            .await?;
            unique_secondary.push((backend_name.to_string(), Arc::clone(backend)));
        }
    }
    for (backend_name, backend) in unique_secondary {
        crate::attachment_cutover::coordinate_empty_secondary_attachment_cutover(
            backend,
            &backend_name,
        )
        .await?;
    }

    // Only now may main advance. In particular, a zero-ref V20 main must not
    // take the ordinary atomic V21 fast path until every secondary has proved
    // it anchors no process-shared blob.
    prepare_core_schema_for_boot(Arc::clone(&main_backend), "backend main").await?;

    // Serving always resolves the process-wide BlobStore/hydration pair.
    // Core-only schema administration does so only when an unfinished V21
    // cutover has legacy moodboard evidence to authenticate. Exact-current
    // and non-moodboard migrations must remain independent of unused blob
    // configuration (and side-effect free for read-only snapshots).
    let resolve_hydrator = match purpose {
        StorageTopologyPurpose::Serving => true,
        StorageTopologyPurpose::SchemaAdministration => {
            schema_admin_requires_blob_hydrator(Arc::clone(&main_backend)).await?
        }
    };
    let shared_hydrator = if resolve_hydrator {
        // Resolve before staging main: an explicit invalid [storage.blob]
        // selection required for verified migration must fail without leaving
        // a new incomplete marker. The blob pack's backend mode governs
        // read-only wrapping during serving boot.
        let blob_governing_backend = if base_config.packs.iter().any(|pack| pack == "blob") {
            match khive_cfg.packs.get("blob") {
                Some(pack) => backends
                    .get(pack.backend.as_str())
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "[packs.blob].backend = {:?} references an unknown backend",
                            pack.backend
                        )
                    })?,
                None => main_backend.clone(),
            }
        } else {
            main_backend.clone()
        };
        resolve_blob_hydrator_for_boot(
            &base_config,
            khive_cfg,
            main_backend.as_ref(),
            blob_governing_backend.as_ref(),
        )?
    } else {
        None
    };
    crate::attachment_cutover::coordinate_attachment_cutover(
        Arc::clone(&main_backend),
        shared_hydrator.clone(),
    )
    .await?;

    Ok(PreparedStorageTopology {
        base_config,
        backends,
        main_backend,
        shared_hydrator,
    })
}

async fn build_registry_for_multi_backend_inner(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<MultiBackendRegistry> {
    let PreparedStorageTopology {
        base_config,
        backends,
        main_backend,
        shared_hydrator,
    } = prepare_configured_storage_topology(
        base_config,
        khive_cfg,
        cli_db_override,
        StorageTopologyPurpose::Serving,
    )
    .await?;

    let pack_names = &base_config.packs;
    let mut per_pack_runtimes_local: HashMap<String, KhiveRuntime> = HashMap::new();
    for pack_name in pack_names {
        let (backend_name, backend) = match khive_cfg.packs.get(pack_name.as_str()) {
            None => (BackendId::MAIN, main_backend.clone()),
            Some(pack_cfg) => {
                let backend_name = pack_cfg.backend.as_str();
                let backend = backends.get(backend_name).cloned().ok_or_else(|| {
                    let defined = backends.keys().cloned().collect::<Vec<_>>().join(", ");
                    anyhow::anyhow!(
                        "[packs.{pack_name}].backend = {backend_name:?} references an unknown backend; defined backends: {defined}"
                    )
                })?;
                (backend_name, backend)
            }
        };
        let mut rt_config = base_config.clone();
        rt_config.backend_id = BackendId::new(backend_name);
        per_pack_runtimes_local.insert(
            pack_name.clone(),
            build_pack_runtime(backend, backend_name, rt_config, &main_backend),
        );
    }

    let default_runtime = KhiveRuntime::from_backend(main_backend.clone(), {
        let mut cfg = base_config.clone();
        cfg.backend_id = BackendId::main();
        cfg
    });

    // ADR-160 D3: resolve the config-selected `BlobStore` once, pair it with
    // exactly one aggregate hydration budget, and install the same immutable
    // hydrator `Arc` on every runtime handle this boot produces. `core()`
    // clones share each runtime's one-shot slot, so they see this same pair.
    if let Some(hydrator) = shared_hydrator {
        // The shared install: the hydrator's mode was decided above from the
        // blob pack's backend, and the receiving handles legitimately mix
        // modes (a writable blob secondary beside a read-only main is a
        // documented topology), so each handle's own domain-store mode must
        // not gate this install.
        default_runtime.install_shared_blob_hydrator(Arc::clone(&hydrator))?;
        for rt in per_pack_runtimes_local.values() {
            rt.install_shared_blob_hydrator(Arc::clone(&hydrator))?;
        }
    }

    #[cfg(feature = "bench-embedder")]
    {
        for rt in per_pack_runtimes_local.values() {
            for name in rt.registered_embedding_model_names() {
                rt.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        for name in default_runtime.registered_embedding_model_names() {
            default_runtime
                .register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
        }
    }

    enforce_strict_actor_mode(
        default_runtime.config().actor_id.as_deref(),
        &default_runtime.config().packs,
    )?;
    if should_warn_unattributed(
        default_runtime.config().actor_id.as_deref(),
        &default_runtime.config().packs,
    ) {
        tracing::warn!(
            "actor identity resolved to \"local\": comm sends will be stamped from \
             \"local\" (unattributed) and comm.inbox will be unscoped (party-line). \
             Set KHIVE_ACTOR or --actor to this lambda's id."
        );
    }

    let gate = default_runtime.config().gate.clone();
    let default_namespace = default_runtime.config().default_namespace.clone();
    let config_id = crate::server::compute_config_id_with_runtime_policies(
        default_runtime.config(),
        Some(khive_cfg),
        default_runtime.ann_fresh_tail_enabled(),
        default_runtime.is_read_only(),
    );
    let visible_namespaces = default_runtime.config().visible_namespaces.clone();

    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.with_gate(gate);
    builder.with_default_namespace(default_namespace.as_str());
    builder.with_visible_namespaces(visible_namespaces);
    builder.with_actor_id(default_runtime.config().actor_id.clone());

    if default_runtime.is_read_only() {
        builder.with_read_only_audit_store();
    } else if let Ok(tok) = default_runtime.authorize(khive_runtime::Namespace::local()) {
        if let Ok(event_store) = default_runtime.events(&tok) {
            builder.with_event_store(event_store);
        }
    }

    khive_runtime::PackRegistry::register_packs_with_runtimes(
        pack_names,
        &per_pack_runtimes_local,
        &default_runtime,
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration: {e}"))?;

    let registry = builder
        .build()
        .map_err(|e| anyhow::anyhow!("registry build: {e}"))?;

    default_runtime.install_edge_rules(registry.all_edge_rules());
    for rt in per_pack_runtimes_local.values() {
        rt.install_edge_rules(registry.all_edge_rules());
    }
    registry.call_register_embedders(&default_runtime);
    registry.call_register_entity_type_validators(&default_runtime);
    // #750: install pack-owned note-mutation hooks (currently
    // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
    // update/delete verbs notify caching packs even though there is no
    // crate-level dependency between them.
    registry.call_register_note_mutation_hooks(&default_runtime);
    // Note-write identity: install the pack-owned kind set and the pack-owned
    // note-write validator so identity properties are derived at the write and
    // preserved through merge/update on every path, including the ones that
    // reach no pack verb. Each per-pack runtime is constructed independently
    // in the multi-backend boot path (unlike the single-backend
    // `KhiveMcpServer::with_packs` path), so both must be installed on every
    // runtime that could actually serve a generic `create`/`update`/`merge`
    // for a pack-owned kind, not just `default_runtime`.
    let owned_note_kinds: Vec<String> = registry
        .pack_owned_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    default_runtime.install_pack_owned_note_kinds(owned_note_kinds.clone());
    for rt in per_pack_runtimes_local.values() {
        rt.install_pack_owned_note_kinds(owned_note_kinds.clone());
    }
    // The validator is installed on every runtime the kind list reaches, not
    // just the default: each per-pack runtime is built independently, so none
    // of them shares the default's validator slot, and `core()` clones the
    // secondary's own slots rather than the default's. A runtime that has the
    // kind list but no validator enforces half the rule.
    registry.call_register_note_write_validators(&default_runtime);
    for rt in per_pack_runtimes_local.values() {
        registry.call_register_note_write_validators(rt);
    }

    let backend_for_pack: HashMap<&str, &StorageBackend> = per_pack_runtimes_local
        .iter()
        .map(|(name, rt)| (name.as_str(), rt.backend()))
        .collect();
    let main_ref: &StorageBackend = main_backend.as_ref();
    registry
        .apply_schema_plans_with_map(&backend_for_pack, main_ref)
        .map_err(|e| anyhow::anyhow!("pack schema boot failure: {e}"))?;

    // Wrap runtimes in Arc for the coordinator's BackendRegistry.
    let per_pack_runtimes_arc: HashMap<String, Arc<KhiveRuntime>> = per_pack_runtimes_local
        .into_iter()
        .map(|(k, v)| (k, Arc::new(v)))
        .collect();

    Ok(MultiBackendRegistry {
        registry,
        default_namespace: default_namespace.as_str().to_string(),
        config_id,
        per_pack_runtimes: per_pack_runtimes_arc,
        main_backend,
        default_runtime,
    })
}

/// Return true when the actor identity will produce unattributed comm sends and
/// a party-line inbox.
///
/// Fires when:
/// - `actor_id` is `None` (not configured) or `"local"` (the default fallback), AND
/// - the loaded pack list includes `"comm"`.
///
/// Pure predicate — no I/O, no logging. Callers emit the warning.
///
/// Delegates to the shared actor-identity policy (#567) so this predicate,
/// the gate's actor resolution, and storage-token minting can never disagree
/// about what counts as "unattributed".
pub(crate) fn should_warn_unattributed(actor_id: Option<&str>, loaded_packs: &[String]) -> bool {
    khive_runtime::should_warn_unattributed_actor(actor_id, loaded_packs)
}

/// Return true when strict actor-attribution mode is active.
///
/// Set `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` to opt in. When active, starting the
/// server with the `comm` pack loaded and no actor identity configured is a fatal
/// error instead of a warning. Default is OFF to preserve OSS single-actor
/// behaviour.
///
/// This closes the #199/#200 misconfiguration window for cloud deployments where
/// an operator who misses the startup warning would silently expose a party-line
/// inbox to all tenants.
pub(crate) fn is_strict_actor_mode() -> bool {
    std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Enforce the strict-actor mode contract at server construction time.
///
/// When `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`:
///   - If `actor_id` is `None`/`"local"` AND `"comm"` is in the pack list →
///     return `Err` with a clear message. The server must NOT be constructed.
///
/// When strict mode is OFF (default): return `Ok(())` unconditionally — the
/// caller is still responsible for emitting the non-fatal `should_warn_unattributed`
/// warning.
///
/// # Scope: dispatch paths only
///
/// This function MUST be called from every **SERVING/DISPATCH** construction path —
/// the paths that will actually route verb calls and read or write comm/tenant data:
/// - `build_server` and `build_server_multi_backend` in this file (the `kkernel mcp` paths)
/// - `build_registry_for_multi_backend` in this file (the ADR-029 coordinator path)
/// - `kkernel exec` (`crates/kkernel/src/exec.rs`) — dispatches arbitrary ops
/// - `khive_mcp::pending_events::run_pending_events` — drains and dispatches
///   scheduled events
///
/// **Pure-introspection registry construction is intentionally EXEMPT**
/// (`build_registry` in `crates/kkernel/src/pack_introspect.rs`,
/// `build_taxonomy` in `crates/kkernel/src/kg/validate.rs`) because it never
/// dispatches verbs or reads comm/tenant data — an operator must still be
/// able to introspect a strict-mode deployment.
pub fn enforce_strict_actor_mode(
    actor_id: Option<&str>,
    loaded_packs: &[String],
) -> anyhow::Result<()> {
    if is_strict_actor_mode() && should_warn_unattributed(actor_id, loaded_packs) {
        anyhow::bail!(
            "KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1 is set but no actor identity is \
             configured. Set KHIVE_ACTOR or --actor to this lambda's id before \
             starting in strict mode (comm pack requires an attributed actor to \
             prevent party-line inbox exposure)."
        );
    }
    Ok(())
}

/// Build a fully-configured server from parsed args (without serving).
///
/// This is the supported production boot path for a single-backend server. The
/// returned future does not complete until bounded blob hydration is installed
/// and any resumable V21 attachment cutover has reached `Complete`; callers must
/// not replace it with direct `KhiveRuntime`/`KhiveMcpServer` construction for a
/// legacy database.
///
/// Returns, alongside the server, the resolved writable [`KhiveRuntime`]
/// handle the `"schedule"` pack is bound to. It is `None` when the resolved
/// pack set omits `"schedule"` or that pack's own assigned backend is
/// read-only, because every ticker pass begins with reclaim DML. This is the
/// SAME runtime the server itself dispatches through, never an independently
/// re-resolved one (PR #782 — see
/// `crates/khive-mcp/docs/api/pending-events.md`).
///
/// Thin wrapper over [`build_server_with_explicit_namespace`]: derives the
/// `(namespace, namespace_explicit)` pair from a real CLI parse and, because
/// this is the genuine `--actor`/`--namespace` CLI flag path, also treats
/// that explicitness as a real actor override.
pub async fn build_server(args: &Args) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;
    build_server_with_explicit_namespace(
        args,
        cli_namespace,
        cli_namespace_explicit,
        cli_namespace_explicit,
    )
    .await
}

/// Build a fully-configured server from parsed args plus an independently
/// resolved `(namespace, namespace_explicit, actor_explicit)` triple.
///
/// Like [`build_server`], this is an asynchronous host-boot boundary and returns
/// only after the attachment cutover is complete.
///
/// Extracted from [`build_server`] (PR #782) so non-interactive-CLI callers
/// (e.g. the `--pending-events` one-shot drain wrapper) can supply a
/// namespace default without it being misread as a genuine `--actor`
/// override. `build_server` derives `namespace_explicit` from a real CLI
/// parse, where "a namespace value is present" and "the operator explicitly
/// overrode the actor identity" are the same fact by construction. A caller
/// that synthesizes an `Args` value programmatically does not get to make
/// that inference — pass `actor_explicit: false` while `namespace_explicit`
/// is still `true` (the `kkernel exec` / `kkernel reindex` shape; see
/// `RuntimeConfigInputs::actor_explicit`'s field doc).
pub async fn build_server_with_explicit_namespace(
    args: &Args,
    namespace: khive_runtime::Namespace,
    namespace_explicit: bool,
    actor_explicit: bool,
) -> anyhow::Result<(KhiveMcpServer, Option<KhiveRuntime>)> {
    let (config, db_anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: args.config.as_deref(),
        namespace,
        namespace_explicit,
        actor_explicit,
        no_embed: args.no_embed,
        packs: if args.pack.is_empty() {
            None
        } else {
            Some(args.pack.clone())
        },
        brain_profile: args.brain_profile.clone(),
    })?;
    let config = {
        let mut config = config;
        if args.daemon {
            enable_events_forwarding_for_daemon(&mut config);
        }
        config
    };

    // Regression fence: `config.db_path` must agree with what the canonical
    // resolver derives from this same `--db` input, or `config_id` (computed
    // from `config.db_path` below) would silently desynchronize this process
    // from any daemon/peer anchored on the same database.
    khive_runtime::assert_captured_db_anchor_consistent(
        config.db_path.as_deref(),
        db_anchor.as_deref(),
    )?;

    // Load the KhiveConfig to check for multi-backend declarations (ADR-028).
    // When no [[backends]] are declared, fall through to the existing single-backend path
    // to preserve byte-for-byte backward compatibility.
    //
    // Deliberately `config_discovery_db_anchor(args.db.as_deref())`, NOT
    // `config.db_path` — `config.db_path` (already resolved above) materializes
    // the `$HOME/.khive/khive.db` default when `--db` is unset (#689), which
    // would re-anchor this reload's tier-3 project-local config discovery to
    // the home directory instead of the process cwd. This keeps the reload in
    // agreement with the discovery anchor `resolve_runtime_config` already used
    // to produce `config` above.
    let db_path_for_config = config_discovery_db_anchor(args.db.as_deref());
    let loaded_config = KhiveConfig::load_with_home_fallback_and_source(
        args.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
    let config_source = loaded_config.as_ref().map(|(_, source)| source.as_path());
    let khive_cfg = loaded_config
        .as_ref()
        .map(|(config, _)| config.clone())
        .unwrap_or_default();

    if !khive_cfg.backends.is_empty() {
        reject_conflicting_db_override_with_source(
            args.db.as_deref(),
            &khive_cfg.backends,
            config_source,
        )?;
    }

    // Issue #1586: disclose the resolved database target once at startup so a
    // no-override invocation's silent default (`$HOME/.khive/khive.db`) is
    // visible in the operator's log alongside the other startup facts. The
    // backends slice keeps the line truthful in multi-backend mode, where the
    // config-declared backend paths — not `config.db_path` — receive writes.
    tracing::info!(target: "khive.boot", "{}", resolved_database_disclosure(config.db_path.as_deref(), &khive_cfg.backends));

    if khive_cfg.backends.is_empty() {
        let runtime = build_single_backend_runtime(config, &khive_cfg).await?;
        #[cfg(feature = "bench-embedder")]
        {
            for name in runtime.registered_embedding_model_names() {
                runtime.register_embedder(crate::bench_embedder::FeatureHashProvider::new(name));
            }
        }
        enforce_strict_actor_mode(
            runtime.config().actor_id.as_deref(),
            &runtime.config().packs,
        )?;
        if should_warn_unattributed(
            runtime.config().actor_id.as_deref(),
            &runtime.config().packs,
        ) {
            tracing::warn!(
                "actor identity resolved to \"local\": comm sends will be stamped from \
                 \"local\" (unattributed) and comm.inbox will be unscoped (party-line). \
                 Set KHIVE_ACTOR or --actor to this lambda's id."
            );
        }
        let schedule_rt = writable_schedule_runtime(
            runtime
                .config()
                .packs
                .iter()
                .any(|p| p == "schedule")
                .then(|| runtime.clone()),
        );
        let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        let server = KhiveMcpServer::new(runtime)
            .map(|s| s.with_default_output_format(fmt))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok((server, schedule_rt));
    }

    // Multi-backend path (ADR-028).
    let multi = build_registry_for_multi_backend_with_db_anchor(
        config,
        &khive_cfg,
        args.db.as_deref(),
        db_anchor.as_deref(),
    )
    .await?;
    let schedule_rt = writable_schedule_runtime(
        multi
            .per_pack_runtimes
            .get("schedule")
            .map(|rt| (**rt).clone()),
    );
    let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
    Ok((server, schedule_rt))
}

/// Canonicalize a SQLite backend path for deduplication (ADR-028 §8).
///
/// The database file may not exist yet at boot time, so this uses the shared
/// no-side-effect path resolver rather than creating the parent as part of
/// identity resolution. That distinction is mandatory for a missing
/// read-only snapshot: boot must fail without materializing either the parent
/// or database. `None` is returned for in-memory backends, which are never
/// deduplicated.
fn canonical_backend_path(cfg: &BackendConfig) -> anyhow::Result<Option<PathBuf>> {
    if cfg.kind == BackendKind::Memory {
        return Ok(None);
    }
    let path = match cfg.path.as_ref() {
        Some(p) => khive_runtime::expand_tilde(p),
        None => return Ok(None),
    };
    canonical_path_no_side_effects(&path)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("backend {}: cannot resolve path: {e}", cfg.name))
}

/// Bound on final-component symlink hops [`canonical_path_no_side_effects`]
/// will follow before giving up, mirroring the kernel's `ELOOP` limit — high
/// enough for any real alias chain, low enough to fail fast on a cycle.
const MAX_SYMLINK_HOPS: u32 = 40;

pub(crate) fn canonical_path_no_side_effects(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let expanded = khive_runtime::expand_tilde(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("cannot resolve current directory: {e}"))?
            .join(expanded)
    };

    // A dangling final-component symlink (the alias exists, its target does
    // not yet) makes `Path::exists()` below report `false` — it follows
    // symlinks, so a missing target reads as "nothing here". Read the link
    // manually first and resolve through it, so a declared alias like
    // `link.db -> target.db` compares equal to `target.db` even before either
    // file has been created. Iterate rather than recurse, and cap the hop
    // count, so a symlink cycle (a self-link or a mutual a<->b pair) fails
    // loud instead of recursing until the stack overflows.
    let mut current = absolute;
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(link_target) = std::fs::read_link(&current) else {
            break;
        };
        current = if link_target.is_absolute() {
            link_target
        } else {
            match current.parent() {
                Some(parent) => parent.join(&link_target),
                None => link_target,
            }
        };
    }
    if std::fs::read_link(&current).is_ok() {
        anyhow::bail!(
            "too many levels of symbolic links resolving {}",
            path.display()
        );
    }
    let absolute = current;

    if absolute.exists() {
        return absolute
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("cannot canonicalize {}: {e}", absolute.display()));
    }

    // Neither `absolute` nor its immediate parent may exist yet (a fresh
    // install with several undeclared directory levels). Walk up to the
    // deepest ancestor that does exist, canonicalize only that (resolving any
    // symlinked directory component along the way), then rejoin the missing
    // tail lexically — never creating anything on disk. `Path::file_name`
    // returns `None` for a `..` component, so the walk records the real last
    // component instead; `.`/`..` in the missing tail are collapsed lexically
    // below, which is sound only for components with no filesystem identity
    // at all. `Path::exists` follows symlinks, so a DANGLING ancestor symlink
    // also reads as nonexistent there — but `missing/..` through a symlink
    // resolves relative to the link's target, not its location, so lexical
    // collapse would be wrong. Probe `symlink_metadata` (which succeeds on a
    // dangling link) and fail loud instead: such a path cannot be opened or
    // created through until the link target exists.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = absolute.as_path();
    let existing_ancestor = loop {
        let Some(parent) = probe.parent() else {
            break None;
        };
        tail.push(
            probe
                .components()
                .next_back()
                .map(|c| c.as_os_str().to_os_string())
                .unwrap_or_default(),
        );
        if parent.exists() {
            break Some(parent.to_path_buf());
        }
        if std::fs::symlink_metadata(parent).is_ok() {
            anyhow::bail!(
                "dangling symbolic link {} while resolving {}",
                parent.display(),
                path.display()
            );
        }
        probe = parent;
    };

    let Some(existing_ancestor) = existing_ancestor else {
        return Ok(absolute);
    };

    let canonical_ancestor = existing_ancestor
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot canonicalize {}: {e}", existing_ancestor.display()))?;

    Ok(tail
        .into_iter()
        .rev()
        .fold(canonical_ancestor, |mut acc, component| {
            if component == *".." {
                acc.pop();
                acc
            } else if component.is_empty() || component == *"." {
                acc
            } else {
                acc.join(component)
            }
        }))
}

/// Build a fully-wired multi-backend `KhiveMcpServer` (ADR-028).
///
/// Before this future resolves, all distinct secondary databases have passed the
/// no-attachment-authority inventory and the canonical main database has reached
/// V21 `Complete`. This ordering prevents a main-database GC sweep from becoming
/// eligible while a secondary still carries legacy blob liveness.
///
/// Called only when `[[backends]]` is non-empty in `khive.toml`. Delegates
/// registry assembly to [`build_registry_for_multi_backend`] and finishing
/// (pool + output format) to [`build_server_from_multi_backend_registry`] —
/// this function's entire body used to duplicate both (#603); it is now a
/// thin pass-through so a future wiring addition lands in exactly one place.
///
/// `pub` so `kkernel`'s coordinator-attached boot path can be compared
/// against it directly in the #603 parity regression test — both call sites
/// must produce servers with an identical wiring surface for the same config.
pub async fn build_server_multi_backend(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> anyhow::Result<KhiveMcpServer> {
    khive_runtime::assert_db_anchor_consistent(base_config.db_path.as_deref(), cli_db_override)?;
    let multi =
        build_registry_for_multi_backend_inner(base_config, khive_cfg, cli_db_override).await?;
    Ok(build_server_from_multi_backend_registry(
        multi, khive_cfg, None,
    ))
}

/// Build a coordinated multi-backend server while checking a canonical
/// database anchor captured earlier in config discovery.
///
/// This has the same secondary-inventory and V21-completion guarantee as
/// [`build_server_multi_backend`].
pub async fn build_server_multi_backend_with_db_anchor(
    base_config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> anyhow::Result<KhiveMcpServer> {
    // The db-anchor consistency guard runs inside `build_registry_for_multi_backend`
    // (the shared choke point every multi-backend boot path funnels through),
    // so it is not duplicated here.
    let multi = build_registry_for_multi_backend_with_db_anchor(
        base_config,
        khive_cfg,
        cli_db_override,
        db_anchor,
    )
    .await?;
    Ok(build_server_from_multi_backend_registry(
        multi, khive_cfg, None,
    ))
}

/// Finish constructing a `KhiveMcpServer` from an already-built
/// [`MultiBackendRegistry`] (#603).
///
/// This is the ONE place that applies every wiring step a multi-backend boot
/// needs on top of the registry: the ADR-078 output-format default, the
/// ADR-091 Planks 0+2 checkpoint pool, and — only for callers that pass one —
/// the cross-backend coordinator (ADR-029 Phase 2). [`build_server_multi_backend`]
/// (this file, `coordinator: None`) and `kkernel`'s `Command::Mcp` multi-backend
/// branch (`crates/kkernel/src/main.rs`, `coordinator: Some(..)`) both call this
/// instead of hand-assembling the server, so a future wiring addition (the
/// fourth `pool`-style patch) is a change to this one function, not to two
/// call sites — #503, ADR-078's inline output-format patch, and #601 each
/// missed wiring by landing only in the hand-copied kkernel branch.
pub fn build_server_from_multi_backend_registry(
    multi: MultiBackendRegistry,
    khive_cfg: &KhiveConfig,
    coordinator: Option<Arc<dyn crate::coordinator::CoordinatorService>>,
) -> KhiveMcpServer {
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let channel_loop_admission = crate::server::ChannelLoopAdmission::for_pack_runtimes(
        multi.per_pack_runtimes.get("kg").map(Arc::as_ref),
        multi.per_pack_runtimes.get("comm").map(Arc::as_ref),
    );
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let channel_outbox_runtime = multi
        .per_pack_runtimes
        .get("kg")
        .map(|runtime| runtime.as_ref().clone());
    // Wire the main backend's pool for background WAL checkpointing. The pool is
    // only present for file-backed databases; in-memory backends return None here
    // so that checkpoint_once never runs on a non-WAL connection.
    let pool = checkpoint_pool_for(multi.main_backend.as_ref());
    // ADR-091 Amendment 3: every OTHER file-backed backend this registry
    // wired, so the session sweep (`spawn_session_walpin_sweep`) and the
    // daemon's checkpoint task can attribute and checkpoint them instead of
    // leaving them permanently invisible to cross-process WAL-pin attribution.
    let secondary_pools = secondary_file_backed_pools(&multi);
    let fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
    let default_runtime = multi.default_runtime.clone();

    let server = KhiveMcpServer::from_registry_with_meta(
        multi.registry,
        &multi.default_namespace,
        &multi.config_id,
    )
    .with_default_output_format(fmt)
    .with_secondary_pools(secondary_pools)
    .with_runtime(default_runtime);

    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    let server = server
        .with_channel_outbox_runtime(channel_outbox_runtime)
        .with_channel_loop_admission(channel_loop_admission);

    let server = match coordinator {
        Some(c) => server.with_coordinator(c),
        None => server,
    };

    match pool {
        Some(p) => server.with_pool(p),
        None => server,
    }
}

/// Distinct file-backed backend pools among `multi`'s per-pack runtimes,
/// excluding the main backend's own pool (wired separately via
/// [`checkpoint_pool_for`]) — ADR-091 Amendment 3 fan-out needs exactly one
/// entry per additional file-backed backend the registry wired, not one per
/// pack, since several packs can share a backend.
///
/// Dedup is by canonical database identity (each pool's [`TxOrigin::Database`]
/// origin, minted from its canonical path — see `ConnectionPool::origin`),
/// never by pool pointer: two backends configured with alias spellings of
/// the SAME file (a direct path and a symlinked path, say) mint two distinct
/// `Arc<ConnectionPool>` but converge on one canonical sidecar, and admitting
/// both would race two `SweepBackend`s on the same heartbeat file.
fn secondary_file_backed_pools(multi: &MultiBackendRegistry) -> Vec<Arc<ConnectionPool>> {
    use khive_storage::tx_registry::TxOrigin;

    let mut seen: HashSet<khive_storage::tx_registry::DbIdentity> = HashSet::new();
    if let TxOrigin::Database(id) = multi.main_backend.pool_arc().origin() {
        seen.insert(id);
    }
    let mut pools = Vec::new();
    for rt in multi.per_pack_runtimes.values() {
        let backend = rt.backend();
        if !backend.is_file_backed() || backend.is_read_only() {
            continue;
        }
        let pool = backend.pool_arc();
        let TxOrigin::Database(id) = pool.origin() else {
            // A file-backed backend always mints a `Database` origin
            // (`ConnectionPool::origin`'s own contract); anything else here
            // has no canonical identity to dedup on, so it cannot be
            // admitted as a secondary fan-out target.
            continue;
        };
        if seen.insert(id) {
            pools.push(pool);
        }
    }
    pools
}

/// Construction-time facts that every multi-backend boot path must agree on
/// for identical input config (#603) — the parity contract the shared
/// [`build_server_from_multi_backend_registry`] constructor exists to
/// guarantee. Extend this struct (not the call sites) when a future wiring
/// addition needs its own parity coverage.
#[derive(Debug, PartialEq, Eq)]
pub struct WiringSurface {
    /// Whether a checkpoint pool was wired (#601/#604 — ADR-091 Planks 0+2).
    pub has_checkpoint_pool: bool,
    /// The resolved ADR-078 default output format.
    pub output_format: OutputFormat,
    /// Whether the default ingest namespace passes the email loop's gate
    /// preflight (#503/#602). This captures only the existing public
    /// authorization surface; runtime-mode admission remains private server
    /// wiring and independently prevents read-only-backed tasks from starting.
    /// Only meaningful when the `channel-email` feature is compiled in.
    #[cfg(feature = "channel-email")]
    pub channel_loop_eligible: bool,
}

impl WiringSurface {
    /// Capture the wiring surface of an already-built server.
    pub fn capture(server: &KhiveMcpServer) -> Self {
        Self {
            has_checkpoint_pool: server.pool().is_some(),
            output_format: server.default_output_format(),
            #[cfg(feature = "channel-email")]
            channel_loop_eligible: preflight_ingest_namespace(
                &ingest_namespace_from_env(),
                &server.verb_registry_clone(),
            ),
        }
    }
}

/// Derive the checkpoint pool for a multi-backend boot's `main` backend
/// (ADR-091 Planks 0+2). The pool is only present for file-backed databases;
/// in-memory backends must never drive `checkpoint_once` on a non-WAL
/// connection.
///
/// Called from exactly one place now: [`build_server_from_multi_backend_registry`]
/// (#603) — both multi-backend boot paths (`build_server_multi_backend` in this
/// file and `kkernel`'s `Command::Mcp` coordinator branch) go through that shared
/// constructor, so this derivation is no longer hand-copied at each call site
/// (#601, #604).
pub fn checkpoint_pool_for(main_backend: &StorageBackend) -> Option<Arc<ConnectionPool>> {
    if main_backend.is_file_backed() && !main_backend.is_read_only() {
        Some(main_backend.pool_arc())
    } else {
        None
    }
}

/// Resolve `khive.toml`'s `[storage.blob]` selection against `backend`
/// (ADR-111 Amendment 2's boot-wiring requirement).
///
/// Returns the resolved store/hydration pair on success so multi-backend
/// callers can install the exact same aggregate budget on every runtime
/// without re-resolving or reconstructing it.
///
/// An **explicit** `[storage.blob]` section that fails to resolve (an `s3`
/// backend with no AWS credentials in the environment, an invalid prefix,
/// etc.) aborts boot: silently falling back to `FsBlobStore` would defeat
/// the point of declaring `backend = "s3"`. When `[storage.blob]` is
/// **absent**, a resolution failure (e.g. an in-memory backend with no root
/// to default beside — every `--db :memory:` invocation and most unit
/// tests) is non-fatal and leaves `KhiveRuntime::blob_store()` unset:
/// nothing yet consumes it, and forcing a filesystem root onto every
/// in-memory boot would be a behavior change nobody asked for.
///
fn resolve_blob_hydrator_for_boot(
    config: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
    backend: &StorageBackend,
    governing_backend: &StorageBackend,
) -> anyhow::Result<Option<Arc<BlobHydrator>>> {
    // Resolution and construction are fused inside the runtime so the mode
    // is DERIVED from the governing backend's own access mode (the backend
    // the blob pack maps to, ADR-160 D3) rather than declared here, and the
    // hydrator comes back stamped as governed — the only kind the shared
    // install seam accepts. Read-only derivation opens an existing root and
    // never creates one; the extra wrap over an already-wrapped store is
    // documented harmless (reads delegate, mutators refuse).
    match BlobHydrator::resolve_for_governing_backend(
        khive_cfg,
        backend,
        governing_backend,
        config.blob_hydration_bytes,
    ) {
        Ok(hydrator) => Ok(Some(Arc::new(hydrator))),
        Err(khive_runtime::GovernedBlobError::Resolve(error))
            if khive_cfg.storage.blob.is_none() =>
        {
            tracing::debug!(
                error = %error,
                "no usable BlobStore for this backend and no [storage.blob] configured; \
                 leaving KhiveRuntime::blob_store() unset"
            );
            Ok(None)
        }
        Err(error) => Err(anyhow::anyhow!(
            "[storage.blob] configuration error: {error}"
        )),
    }
}

/// Open, migrate, coordinate V21, and construct one single-backend runtime.
///
/// Both `serve` and `kkernel exec` use this real-host async choke point. The
/// runtime is not exposed until the attachment/GC cutover is complete, and no
/// temporary Tokio runtime or writer-task lifecycle domain is introduced.
pub async fn build_single_backend_runtime(
    config: RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<KhiveRuntime> {
    let backend = Arc::new(open_single_backend(&config)?);
    prepare_core_schema_for_boot(Arc::clone(&backend), "single backend").await?;

    let hydrator =
        resolve_blob_hydrator_for_boot(&config, khive_cfg, backend.as_ref(), backend.as_ref())?;
    crate::attachment_cutover::coordinate_attachment_cutover(
        Arc::clone(&backend),
        hydrator.clone(),
    )
    .await?;

    let runtime = KhiveRuntime::from_prepared_backend(backend, config)?;
    if let Some(hydrator) = hydrator {
        runtime.install_blob_hydrator(hydrator)?;
    }
    Ok(runtime)
}

fn open_single_backend(config: &RuntimeConfig) -> anyhow::Result<StorageBackend> {
    let backend = match &config.db_path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    anyhow::anyhow!(
                        "cannot create database parent directory {}: {error}",
                        parent.display()
                    )
                })?;
            }
            StorageBackend::sqlite(path)
                .map_err(|error| anyhow::anyhow!("open single SQLite backend: {error}"))?
        }
        None => StorageBackend::memory()
            .map_err(|error| anyhow::anyhow!("open single in-memory backend: {error}"))?,
    };
    Ok(backend)
}

async fn prepare_single_backend_for_schema_admin(
    config: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> anyhow::Result<Arc<StorageBackend>> {
    let backend = Arc::new(open_single_backend(config)?);
    prepare_core_schema_for_boot(Arc::clone(&backend), "single backend").await?;

    let hydrator = if schema_admin_requires_blob_hydrator(Arc::clone(&backend)).await? {
        resolve_blob_hydrator_for_boot(config, khive_cfg, backend.as_ref(), backend.as_ref())?
    } else {
        None
    };
    crate::attachment_cutover::coordinate_attachment_cutover(Arc::clone(&backend), hydrator)
        .await?;
    Ok(backend)
}

async fn prepare_core_schema_for_boot(
    backend: Arc<StorageBackend>,
    label: impl Into<String>,
) -> anyhow::Result<u32> {
    let label = label.into();
    tokio::task::spawn_blocking(move || backend.prepare_core_schema())
        .await
        .map_err(|error| anyhow::anyhow!("{label}: schema preparation task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("{label}: schema preparation: {error}"))
}

/// Resolve and install blob hydration on an already prepared runtime.
///
/// This low-level compatibility helper does **not** run or validate the
/// application-assisted V21 attachment cutover. Production host boot should use
/// [`build_single_backend_runtime`], [`build_server`], or the async multi-backend
/// builders instead. Calling this helper is sound only when `backend` is the
/// runtime's backend and its attachment-cutover status is already `Complete`;
/// the same-backend precondition is now enforced by identity rather than
/// documented, since the resolved mode derives from `backend` and a mismatch
/// would silently resolve the wrong store mode for the receiving runtime.
pub fn install_resolved_blob_store(
    rt: &KhiveRuntime,
    khive_cfg: &KhiveConfig,
    backend: &StorageBackend,
) -> anyhow::Result<Option<Arc<BlobHydrator>>> {
    if !std::ptr::eq(rt.backend(), backend) {
        anyhow::bail!(
            "install_resolved_blob_store requires the runtime's own backend: the supplied \
             backend reference is not the runtime's"
        );
    }
    let hydrator = resolve_blob_hydrator_for_boot(rt.config(), khive_cfg, backend, backend)?;
    if let Some(hydrator) = hydrator.as_ref() {
        rt.install_blob_hydrator(Arc::clone(hydrator))?;
    }
    Ok(hydrator)
}

/// Construct one per-pack runtime, wiring `core_backend` for secondary-backend packs.
///
/// Centralizing this in one helper ensures that both `build_registry_for_multi_backend`
/// and `build_server_multi_backend` apply the same ADR-073 wiring. Without it, a
/// secondary pack served via `build_server_multi_backend` would receive
/// `core_backend = None`, causing `core()` to fall back to `self.clone()` and write
/// linkable records to the secondary backend instead of main.
fn build_pack_runtime(
    backend: Arc<StorageBackend>,
    backend_name: &str,
    rt_config: RuntimeConfig,
    main_backend: &Arc<StorageBackend>,
) -> KhiveRuntime {
    let rt = KhiveRuntime::from_backend(backend, rt_config);
    if backend_name != BackendId::MAIN {
        rt.with_core_backend(main_backend.clone())
    } else {
        rt
    }
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
            let expanded = khive_runtime::expand_tilde(path);
            if !cfg.read_only {
                if let Some(parent) = expanded.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        anyhow::anyhow!(
                            "backend {}: cannot create parent dir {}: {e}",
                            cfg.name,
                            parent.display()
                        )
                    })?;
                }
            }
            if cfg.read_only {
                StorageBackend::sqlite_read_only(&expanded).map_err(|e| {
                    anyhow::anyhow!("backend {}: sqlite read-only open: {e}", cfg.name)
                })
            } else {
                let backend = StorageBackend::sqlite(&expanded)
                    .map_err(|e| anyhow::anyhow!("backend {}: sqlite open: {e}", cfg.name))?;
                if backend.is_read_only() {
                    anyhow::bail!(
                        "backend {}: path {} has no filesystem write bits; declare \
                         `read_only = true` so backend topology and daemon config identity \
                         describe the snapshot-inspection mode explicitly",
                        cfg.name,
                        expanded.display()
                    );
                }
                Ok(backend)
            }
        }
    }
}

/// Resolve the `--db`/`KHIVE_DB` value into the anchor used for tier-3
/// project-local `.khive/config.toml` DISCOVERY — as distinct from
/// [`khive_runtime::resolve_db_anchor`], which always materializes a concrete
/// anchor (defaulting to `$HOME/.khive/khive.db`) for the database that is
/// actually about to be opened.
///
/// An explicit `--db`/`KHIVE_DB` still anchors discovery to that path, for the
/// same config_id-coherence reason `resolve_db_anchor` documents. But when no
/// db was supplied, this returns `None` instead of the materialized home
/// default (#689): passing the home-default path into
/// `KhiveConfig::load_with_home_fallback`'s `db_path` collapses tier 3 onto
/// `$HOME/.khive/config.toml`, silently skipping the project-local
/// `<cwd>/.khive/config.toml` that `project_config_anchor_dir` documents as
/// the `db_path == None` fallback.
pub fn config_discovery_db_anchor(db: Option<&str>) -> Option<std::path::PathBuf> {
    db.and_then(|d| khive_runtime::resolve_db_anchor(Some(d)))
}

/// One-line disclosure of the database target(s) this process will write to,
/// for CLI entry points that resolve a database path. Returns a sentence
/// naming the resolved target: the concrete file path, the ephemeral
/// in-memory marker when the resolved path is `:memory:`, or — when the
/// loaded config declares `[[backends]]` — the config-declared backend
/// targets, which are what actually receive writes in multi-backend mode
/// (the single resolved anchor path is a discovery/fingerprint input there,
/// not a write target).
///
/// The `:memory:` arm (resolved path `None`) wins even when backends are
/// declared: a `:memory:` override forces every declared backend ephemeral
/// (`force_memory` in [`validate_db_override_against_backends`]'s caller), so
/// the ephemeral line is the truthful one.
///
/// Issue #1586: with no `--db`/`KHIVE_DB` override the resolver silently
/// targets the default `$HOME/.khive/khive.db` — the production database for
/// most installs. Naming the resolved target once at startup makes that
/// implicit choice visible without adding a prompt or refusal. Callers decide
/// the channel: `kkernel mcp` logs it at INFO with its other startup facts;
/// `kkernel exec` prints it to stderr (its default log level is `warn`, so an
/// INFO record would never surface there).
pub fn resolved_database_disclosure(
    resolved_db_path: Option<&std::path::Path>,
    backends: &[BackendConfig],
) -> String {
    match resolved_db_path {
        None => "database: :memory: (ephemeral in-memory; nothing persists)".to_string(),
        Some(_) if !backends.is_empty() => {
            let targets: Vec<String> = backends
                .iter()
                .map(|backend| match (&backend.kind, backend.path.as_deref()) {
                    // Kind decides first: a `memory` backend's `path` is
                    // ignored by construction (see `BackendConfig::path`), so
                    // a stray configured path must not be presented as a
                    // write target.
                    (BackendKind::Memory, _) => format!("{}=:memory:", backend.name),
                    (BackendKind::Sqlite, Some(path)) => {
                        format!("{}={}", backend.name, path.display())
                    }
                    (BackendKind::Sqlite, None) => format!("{}=<unresolved>", backend.name),
                })
                .collect();
            format!(
                "database: config-declared backends govern storage targets: {}",
                targets.join(", ")
            )
        }
        Some(path) => format!("database: {} (resolved)", path.display()),
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
    /// Whether the caller holds a GENUINE explicit actor/identity override —
    /// i.e. an operator actually typed `--actor` / `--namespace` (ADR-057).
    ///
    /// Distinct from `namespace_explicit`: `kkernel exec` and `kkernel reindex`
    /// set `namespace_explicit: true` unconditionally (their `--namespace` arg
    /// has no `Option` to distinguish "typed" from "default"), but they have no
    /// `--actor` flag and must NOT suppress the project/db actor-id tiers when
    /// their namespace happens to resolve to `"local"`. Only `kkernel mcp`
    /// (`build_server`, via `resolve_cli_namespace`) sets this to a value that
    /// can suppress those tiers — everyone else passes `false`.
    pub actor_explicit: bool,
    /// Disable embedding entirely (still resolves actor namespace from config).
    pub no_embed: bool,
    /// Explicit CLI packs to register. `None` falls through to `KHIVE_PACKS`,
    /// `[runtime].packs`, then the built-in production set.
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
    let (config, _) = resolve_runtime_config_with_db_anchor(inputs)?;
    Ok(config)
}

/// Resolve a [`RuntimeConfig`] and return the database anchor captured at the
/// same construction boundary. Server boot paths thread this value through
/// consistency validation and registry construction without re-reading HOME.
pub fn resolve_runtime_config_with_db_anchor(
    inputs: RuntimeConfigInputs<'_>,
) -> anyhow::Result<(RuntimeConfig, Option<PathBuf>)> {
    let db_anchor = khive_runtime::resolve_db_anchor(inputs.db);
    let db_path = db_anchor.clone();

    let cli_packs = inputs.packs.filter(|packs| !packs.is_empty());
    let env_packs = std::env::var("KHIVE_PACKS")
        .ok()
        .map(|value| parse_pack_list(&value))
        .filter(|packs| !packs.is_empty());
    let packs_overridden = cli_packs.is_some() || env_packs.is_some();
    let packs = cli_packs
        .or(env_packs)
        .unwrap_or_else(RuntimeConfig::built_in_packs);

    // Tier-1: explicit CLI --brain-profile only (not env — env is tier-3, after TOML).
    // We must NOT read KHIVE_BRAIN_PROFILE here; RuntimeConfig::default() reads it, so
    // we exclude brain_profile from the default spread and set it to None (CLI-only).
    let cli_brain_profile = inputs.brain_profile.filter(|s| !s.trim().is_empty());

    // Threaded into the config-file resolvers so tier-3 project-local config
    // discovery anchors to the resolved database's directory rather than the
    // process cwd when an explicit `--db`/`KHIVE_DB` is given (kills config_id
    // drift between a client and the daemon serving the same database at a
    // different working directory). Deliberately NOT the base config's own
    // `db_path` (which materializes the `$HOME/.khive/khive.db` default when
    // unset, #689) — an unset db must fall through to cwd-anchored discovery
    // instead of silently searching the home directory.
    let db_path_for_config = config_discovery_db_anchor(inputs.db);

    let resolved = if inputs.no_embed {
        // `RuntimeConfig::no_embeddings()` is the canonical "zero embedders"
        // constructor (issue #396) — it clears `embedding_model` and
        // `additional_embedding_models` together, unlike a manual two-field
        // override which can leave `additional_embedding_models` populated
        // from `KHIVE_ADDITIONAL_EMBEDDING_MODELS`.
        let no_embed_base = RuntimeConfig {
            db_path,
            default_namespace: inputs.namespace,
            packs,
            // Explicit CLI flag only at this tier — env and config-file tiers are applied
            // below in resolve_actor_from_config and apply_env_brain_profile.
            brain_profile: cli_brain_profile,
            ..RuntimeConfig::no_embeddings()
        };
        resolve_actor_from_config(
            inputs.config,
            no_embed_base,
            db_path_for_config.as_deref(),
            packs_overridden,
        )?
    } else {
        let base_config = RuntimeConfig {
            db_path,
            default_namespace: inputs.namespace,
            packs,
            // Explicit CLI flag only at this tier — env and config-file tiers are applied
            // below in resolve_config and apply_env_brain_profile.
            brain_profile: cli_brain_profile,
            ..RuntimeConfig::default()
        };
        resolve_config(
            inputs.config,
            base_config,
            db_path_for_config.as_deref(),
            packs_overridden,
        )?
    };

    // ADR-096 Fork 2 — per-connection `actor_id` precedence chain (highest to
    // lowest), ratified 2026-07-05:
    //
    //   1. Explicit CLI `--actor` / `--namespace` flag (ADR-057), threaded via
    //      `inputs.namespace` / `inputs.actor_explicit` (`resolve_cli_namespace`,
    //      only `build_server` sets `actor_explicit` from a real CLI parse —
    //      see the field doc on `RuntimeConfigInputs::actor_explicit`).
    //      `args.actor` no longer carries a `KHIVE_ACTOR` env-arg alias (the
    //      clap `env` binding was removed from the tier-1 field — see
    //      `args.rs`), so this tier is CLI-flag-only; a bare shell-level
    //      `KHIVE_ACTOR` can no longer masquerade as an explicit flag. When
    //      genuinely explicit, tiers 2-3 below are NOT consulted at all — an
    //      explicit `--actor local` must resolve to anonymous (`None`), not
    //      fall through to a project/db/env actor (the gap this block also
    //      closes). `kkernel exec`/`reindex` force
    //      `namespace_explicit: true` for unrelated reasons (no `Option` on
    //      their `--namespace` arg) but always pass `actor_explicit: false`,
    //      so they keep falling through to tiers 2-3 exactly as before.
    //   2. Project/cwd-anchored config `[actor].id`, resolved INDEPENDENTLY of
    //      the database-anchored config load above (`resolve_project_actor_id`).
    //      Commit 10d9c92c (#651) anchored tier-3 `.khive/config.toml` discovery
    //      to the resolved database's own directory — correct for `config_id`
    //      coherence between a client and a daemon sharing one database, but it
    //      also relocated `[actor]` discovery away from the connecting process's
    //      own project. This tier restores it as a SEPARATE lookup.
    //   3. Whatever `resolved.actor_id` already carries from the
    //      database-anchored config load / `KHIVE_ACTOR` env direct-read
    //      (`resolve_config` / `resolve_actor_from_config` / `RuntimeConfig::
    //      default()` above) — the pre-#651-drift fallback tier. This is the
    //      ONLY place `KHIVE_ACTOR` env feeds `actor_id`; it never touches
    //      `default_namespace`.
    //   4. Anonymous (`None`).
    //
    // Attribution-only: none of these tiers may feed `config_id` (`actor_id` is
    // not read by `compute_config_id`) or `default_namespace` (tier 1 already
    // sets `default_namespace` via `inputs.namespace` — unchanged pre-existing
    // behavior; tiers 2-4 never touch it, per ADR-007 Rev 4 Rule 0).
    let resolved = {
        let mut resolved = resolved;
        let ns = resolved.default_namespace.as_str().to_string();
        if inputs.namespace_explicit && ns != "local" {
            // An explicit non-"local" namespace (CLI `--actor`/`--namespace`,
            // or `kkernel exec`/`reindex`'s forced-explicit `--namespace`)
            // fills `actor_id` directly from the namespace — unchanged
            // pre-existing ADR-057 fill behavior, kept keyed on
            // `namespace_explicit` (not `actor_explicit`) so exec/reindex
            // keep resolving a non-local `--namespace` to that actor.
            resolved.actor_id = Some(ns);
        } else if inputs.actor_explicit {
            // Genuinely explicit CLI actor tier requesting anonymous
            // (`--actor local` / `--namespace local`) is authoritative: do
            // not fall through to project/db/env actor tiers just because
            // "local" also looks like "unset". Gated on `actor_explicit`
            // (not the broader `namespace_explicit`) so `kkernel exec`/
            // `reindex` — which force `namespace_explicit: true` for
            // unrelated reasons and have no `--actor` flag — keep falling
            // through exactly as before.
            resolved.actor_id = None;
        } else {
            let project_actor = khive_runtime::resolve_project_actor_id(inputs.config)
                .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
            resolved.actor_id = project_actor.or(resolved.actor_id);
        }
        resolved
    };

    // ADR-170: events-daemon split. Every file-backed resolution routes event
    // persistence to the events database beside the main store, in DIRECT
    // mode: this resolver serves one-shot hosts (`kkernel exec`, `reindex`,
    // ingest) and tests, which have no events daemon to talk to. Resident
    // daemon hosts upgrade the resolved config to socket forwarding
    // themselves — see [`enable_events_forwarding_for_daemon`] — because
    // only they supervise an events daemon at the derived socket.
    // In-memory resolutions (tests) keep the legacy main-store event plane.
    // `KHIVE_EVENTS_SPLIT=0` is the deployment kill-switch back to legacy.
    let resolved = {
        let mut resolved = resolved;
        let kill_switch = std::env::var("KHIVE_EVENTS_SPLIT").is_ok_and(|v| v.trim() == "0");
        if !kill_switch {
            if let Some(main_db) = resolved.db_path.as_deref() {
                resolved.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
                    db_path: khive_runtime::events_split::events_db_path_beside(main_db),
                    socket_path: None,
                });
            }
        }
        resolved
    };

    // Tier-3 env fallback: KHIVE_BRAIN_PROFILE is applied AFTER CLI (tier-1) and
    // config-file (tier-2) so that a project or global TOML always wins over the env var.
    Ok((apply_env_brain_profile(resolved), db_anchor))
}

/// Upgrade a resolved config's event plane from direct (embedded) mode to
/// socket forwarding — the resident-daemon half of the ADR-170 split.
///
/// `resolve_runtime_config_with_db_anchor` always resolves the event plane in
/// direct mode because most of its callers (one-shot CLI, tests) have no
/// events daemon. The two daemon hosts (`build_server_with_explicit_namespace`
/// under `--daemon`, and `kkernel mcp`'s multi-backend arm) call this after
/// resolution; they are exactly the processes that also supervise an events
/// daemon at the derived socket (`start_daemon_components_if_daemon`), so the
/// socket this routes to is the one that same host keeps alive.
pub fn enable_events_forwarding_for_daemon(config: &mut RuntimeConfig) {
    if let Some(split) = config.events_split.as_mut() {
        split.socket_path = Some(khive_runtime::events_split::events_socket_path_beside(
            &split.db_path,
        ));
    }
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

/// Resolve the server-level default output format (ADR-078 §2 precedence tier 2-3).
///
/// Precedence (highest to lowest — called AFTER CLI tier is handled at request time):
/// 1. `KHIVE_OUTPUT_FORMAT` env var (tier 2)
/// 2. `khive_cfg.runtime.default_output_format` from TOML (tier 3)
/// 3. Builtin `OutputFormat::Json` (tier 4)
///
/// Returns the resolved [`OutputFormat`] to wire into the server via
/// `with_default_output_format`.
pub fn apply_env_output_format(toml_default: Option<OutputFormat>) -> OutputFormat {
    // Env var (tier 2) overrides TOML (tier 3).
    if let Ok(val) = std::env::var("KHIVE_OUTPUT_FORMAT") {
        match val.trim() {
            "json" => return OutputFormat::Json,
            "auto" => return OutputFormat::Auto,
            "table" => return OutputFormat::Table,
            _ => {
                tracing::warn!(
                    value = %val,
                    "KHIVE_OUTPUT_FORMAT has unknown value; falling back to TOML / builtin default"
                );
            }
        }
    }
    // TOML default (tier 3) or builtin (tier 4).
    toml_default.unwrap_or(OutputFormat::Json)
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
///
/// `db_path` is the already-resolved database path (or `None` for an in-memory
/// database); it anchors tier-3 project-local config discovery to the
/// database's own directory instead of the process cwd.
fn resolve_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    db_path: Option<&std::path::Path>,
    packs_overridden: bool,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path, db_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let base = apply_config_pack_selection(&khive_cfg, base, packs_overridden);
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if !khive_cfg.engines.is_empty() && (env_primary.is_some() || env_additional.is_some())
            {
                tracing::warn!(
                    "khive config [[engines]] present; KHIVE_EMBEDDING_MODEL / \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are overridden"
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

/// Resolve configuration without enabling embedding engines (no-embed path).
///
/// `db_path` anchors tier-3 project-local config discovery to the database's
/// own directory instead of the process cwd (see [`resolve_config`]). The
/// caller-owned namespace remains in `base`, while non-actor sections such as
/// `[git_write]` are still loaded and validated.
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    db_path: Option<&std::path::Path>,
    packs_overridden: bool,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path, db_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            let base = apply_config_pack_selection(&khive_cfg, base, packs_overridden);
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

fn apply_config_pack_selection(
    khive_cfg: &KhiveConfig,
    mut base: RuntimeConfig,
    packs_overridden: bool,
) -> RuntimeConfig {
    if !packs_overridden {
        if let Some(packs) = khive_cfg
            .runtime
            .packs
            .as_ref()
            .filter(|packs| !packs.is_empty())
        {
            base.packs.clone_from(packs);
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{BlobConfig, Namespace, StorageSectionConfig};

    #[tokio::test(flavor = "current_thread")]
    async fn schema_prepare_waits_for_gc_owner_off_the_async_worker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let database = dir.path().join("owner-wait.db");
        let backend = Arc::new(StorageBackend::sqlite(&database).expect("open backend"));
        let owner = khive_db::stores::blob::acquire_database_gc_owner(backend.sql().as_ref())
            .await
            .expect("pre-hold database GC owner");
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released_by_task = Arc::clone(&released);

        tokio::spawn(async move {
            tokio::task::yield_now().await;
            released_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(owner);
        });

        let version = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            prepare_core_schema_for_boot(Arc::clone(&backend), "owner wait regression"),
        )
        .await
        .expect("current-thread runtime must keep scheduling while schema preparation waits")
        .expect("schema preparation after owner release");

        assert_eq!(
            version,
            khive_db::MIGRATIONS
                .last()
                .expect("migration ledger")
                .version
        );
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "the owner-release task must run before schema preparation can complete"
        );
    }

    // Freeze lingering `-wal`/`-shm` sidecars left by a writable fixture whose
    // connections close asynchronously; read-only admission rejects a writable
    // `-shm` as potentially live.
    #[cfg(unix)]
    fn freeze_snapshot_sidecars(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["-wal", "-shm"] {
            let mut name = path.file_name().expect("db file name").to_os_string();
            name.push(suffix);
            let sidecar = path.parent().expect("db parent dir").join(name);
            if sidecar.exists() {
                let mut permissions = std::fs::metadata(&sidecar)
                    .expect("sidecar metadata")
                    .permissions();
                permissions.set_mode(0o444);
                std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
            }
        }
    }
    use serial_test::serial;
    use std::io::Write;

    // #689: `config_discovery_db_anchor` is a pure function (no env/cwd
    // dependency), so its explicit-vs-unset contract is covered here without
    // the env-mutation isolation the cwd/HOME-dependent tests below require.
    #[test]
    fn config_discovery_db_anchor_unset_is_none() {
        assert_eq!(
            config_discovery_db_anchor(None),
            None,
            "unset --db must not anchor discovery on the materialized home default"
        );
    }

    #[test]
    fn config_discovery_db_anchor_explicit_matches_resolve_db_anchor() {
        assert_eq!(
            config_discovery_db_anchor(Some("/tmp/explicit.db")),
            khive_runtime::resolve_db_anchor(Some("/tmp/explicit.db")),
            "an explicit --db must anchor discovery identically to resolve_db_anchor"
        );
    }

    #[test]
    fn config_discovery_db_anchor_memory_sentinel_is_none() {
        assert_eq!(config_discovery_db_anchor(Some(":memory:")), None);
    }

    // #1586: the resolved-database disclosure must name the concrete resolved
    // path (or the ephemeral in-memory marker) so a default-targeted write is
    // visible at startup.
    #[test]
    fn resolved_database_disclosure_names_file_path() {
        let line = resolved_database_disclosure(
            Some(std::path::Path::new("/home/op/.khive/khive.db")),
            &[],
        );
        assert!(
            line.contains("/home/op/.khive/khive.db"),
            "disclosure must carry the resolved path; got: {line}"
        );
        assert!(
            line.starts_with("database:"),
            "disclosure is a single labelled startup line; got: {line}"
        );
    }

    #[test]
    fn resolved_database_disclosure_marks_memory_ephemeral() {
        let line = resolved_database_disclosure(None, &[]);
        assert!(
            line.contains(":memory:") && line.contains("ephemeral"),
            "in-memory disclosure must say the target is ephemeral; got: {line}"
        );
    }

    // Multi-backend mode: writes go to the config-declared backend paths, not
    // the resolved anchor path — the disclosure must name the real targets and
    // must NOT present the anchor as the write target.
    #[test]
    fn resolved_database_disclosure_names_backend_targets_in_multi_backend_mode() {
        let backends = vec![
            BackendConfig {
                name: "main".into(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("/data/main.db")),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            },
            BackendConfig {
                name: "scratch".into(),
                kind: BackendKind::Memory,
                // Stray path on a memory backend: ignored by construction, so
                // the disclosure must not present it as a write target.
                path: Some(std::path::PathBuf::from("/data/ignored-stray.db")),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            },
        ];
        let anchor = std::path::Path::new("/home/op/.khive/khive.db");
        let line = resolved_database_disclosure(Some(anchor), &backends);
        assert!(
            line.contains("main=/data/main.db") && line.contains("scratch=:memory:"),
            "multi-backend disclosure must name each declared target; got: {line}"
        );
        assert!(
            !line.contains("/home/op/.khive/khive.db"),
            "multi-backend disclosure must not present the anchor path as a write target; got: {line}"
        );
        assert!(
            !line.contains("ignored-stray"),
            "a memory backend's stray configured path is ignored by construction and must not be disclosed; got: {line}"
        );
    }

    // A `:memory:` override with declared backends forces every backend
    // ephemeral (force_memory), so the ephemeral line wins over the backend
    // listing.
    #[test]
    fn resolved_database_disclosure_memory_override_wins_over_backends() {
        let backends = vec![BackendConfig {
            name: "main".into(),
            kind: BackendKind::Sqlite,
            path: Some(std::path::PathBuf::from("/data/main.db")),
            cache_mb: None,
            journal_mode: None,
            read_only: false,
        }];
        let line = resolved_database_disclosure(None, &backends);
        assert!(
            line.contains("ephemeral") && !line.contains("/data/main.db"),
            "memory-forced run must disclose ephemerality, not the overridden file targets; got: {line}"
        );
    }

    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("khive.toml");
        let mut f = std::fs::File::create(&path).expect("create config file");
        f.write_all(body.as_bytes()).expect("write config");
        path
    }

    fn kg_test_packs() -> Vec<String> {
        vec!["kg".to_string()]
    }

    fn resolve_packs(
        config: &std::path::Path,
        cli_packs: Option<Vec<String>>,
        no_embed: bool,
    ) -> Vec<String> {
        resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed,
            packs: cli_packs,
            brain_profile: None,
        })
        .expect("resolve config")
        .packs
    }

    /// ADR-170 host-class contract: the shared resolver must emit the events
    /// split in DIRECT mode (socket-less), because most of its callers —
    /// one-shot CLI hosts and every test that builds a server — have no
    /// events daemon to forward to. A resolver that emits a socket here
    /// routes those hosts' events at a daemon that does not exist: appends
    /// are silently dropped and synchronous provenance reads fail closed
    /// (measured: the schedule drain refused to dispatch a due event).
    /// Resident daemon hosts get forwarding only through the explicit
    /// upgrade, at a socket derived beside the events db (never a global
    /// socket, which would cross-wire a second database's events).
    #[test]
    #[serial]
    fn resolver_emits_direct_events_mode_and_daemon_upgrade_derives_socket_beside_db() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("khive.db");
        let mut resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db.to_str().expect("utf8")),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        let split = resolved
            .events_split
            .as_ref()
            .expect("file-backed resolution must configure the events split");
        // The sidecar derives from the full main-db file name with the
        // parent canonicalized (macOS temp dirs live behind /var symlinks).
        let dir_real = dir.path().canonicalize().expect("canonicalize temp dir");
        assert_eq!(split.db_path, dir_real.join("khive.db.events.db"));
        assert_eq!(
            split.socket_path, None,
            "the shared resolver must emit direct mode; only daemon hosts upgrade"
        );

        enable_events_forwarding_for_daemon(&mut resolved);
        let split = resolved.events_split.as_ref().expect("split still set");
        assert_eq!(
            split.socket_path.as_deref(),
            Some(dir_real.join("khive.db.events.sock").as_path()),
            "daemon upgrade must derive the socket beside the events db, not globally"
        );

        // In-memory resolutions carry no event-plane split at all.
        let in_memory = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve in-memory config");
        assert!(in_memory.events_split.is_none());
    }

    #[test]
    #[serial]
    fn config_pack_selection_applies_to_both_embedding_paths() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "[runtime]\npacks = [\"kg\", \"gtd\"]\n");

        assert_eq!(resolve_packs(&path, None, false), vec!["kg", "gtd"]);
        assert_eq!(resolve_packs(&path, None, true), vec!["kg", "gtd"]);
    }

    #[test]
    #[serial]
    fn env_pack_selection_overrides_config_file() {
        let _env = ClearedKhiveEnvGuard::clear();
        std::env::set_var("KHIVE_PACKS", "kg, gtd");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "[runtime]\npacks = [\"kg\", \"memory\"]\n");

        assert_eq!(resolve_packs(&path, None, true), vec!["kg", "gtd"]);
    }

    #[test]
    #[serial]
    fn cli_pack_selection_overrides_env_and_config_file() {
        let _env = ClearedKhiveEnvGuard::clear();
        std::env::set_var("KHIVE_PACKS", "kg,gtd");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "[runtime]\npacks = [\"kg\", \"memory\"]\n");

        assert_eq!(
            resolve_packs(&path, Some(vec!["kg".to_string()]), true),
            vec!["kg"]
        );
    }

    #[test]
    #[serial]
    fn empty_cli_and_env_pack_layers_fall_through_to_config() {
        let _env = ClearedKhiveEnvGuard::clear();
        std::env::set_var("KHIVE_PACKS", " ,  ");
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "[runtime]\npacks = [\"kg\", \"memory\"]\n");

        assert_eq!(
            resolve_packs(&path, Some(Vec::new()), true),
            vec!["kg", "memory"]
        );
    }

    #[test]
    #[serial]
    fn empty_config_pack_selection_preserves_built_in_default() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(dir.path(), "[runtime]\npacks = []\n");

        assert_eq!(
            resolve_packs(&path, None, true),
            RuntimeConfig::built_in_packs()
        );
    }

    #[test]
    #[serial]
    fn backend_assignment_table_does_not_select_packs() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            "[runtime]\npacks = [\"kg\"]\n\n[packs.gtd]\nbackend = \"main\"\n",
        );

        assert_eq!(resolve_packs(&path, None, true), vec!["kg"]);
    }

    // The resolver MUST honor config-file `[[engines]]` over RuntimeConfig
    // defaults — otherwise `kkernel reindex` embeds for the wrong model set
    // versus what `kkernel mcp` serves recall from. Regression for PR #8
    // blocker.
    #[test]
    #[serial]
    fn resolver_uses_config_file_engines_over_defaults() {
        // Ensure a stale ambient value cannot leak into either branch.
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        // The shipped default is single-engine, so leaving the additional list
        // unset would make "the config file overrode the default" and "there
        // was nothing to override" produce the same empty result, and the
        // final assertion below would stop discriminating. Declare one
        // deliberately so the override remains observable.
        std::env::set_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS", "paraphrase");

        let default_cfg = RuntimeConfig::default();
        let default_primary = format!("{:?}", default_cfg.embedding_model);
        assert!(
            !default_cfg.additional_embedding_models.is_empty(),
            "precondition: default config must carry an additional engine for this test to discriminate"
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
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
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

        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    }

    /// Regression for #379: when the loaded config file has NO `[[engines]]`
    /// block, `KHIVE_EMBEDDING_MODEL` is genuinely used as the fallback — it
    /// must resolve into `RuntimeConfig::embedding_model`, not be discarded.
    /// The startup warning must not fire in this case either (the env pair is
    /// applied, not overridden) — see the `resolve_config` fix.
    #[test]
    #[serial]
    fn resolver_falls_back_to_env_when_config_has_no_engines() {
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::set_var("KHIVE_EMBEDDING_MODEL", "bge-small-en-v1.5");

        let dir = tempfile::tempdir().expect("temp dir");
        // Config file present, but with no [[engines]] block at all.
        let path = write_config(
            dir.path(),
            r#"
[runtime]
brain_profile = "unrelated"
"#,
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        std::env::remove_var("KHIVE_EMBEDDING_MODEL");

        assert_eq!(
            format!("{:?}", resolved.embedding_model),
            "Some(BgeSmallEnV15)",
            "KHIVE_EMBEDDING_MODEL must be applied as the fallback when the \
             config file has no [[engines]] block, not treated as ignored"
        );
    }

    /// Regression for PR #52: project-toml brain_profile
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
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
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
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
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
            actor_explicit: false,
            no_embed: false,
            packs: Some(kg_test_packs()),
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

    /// Regression for #203: the `--actor` / `--namespace`
    /// CLI flag must set `actor_id`, not just `default_namespace`. Before the fix,
    /// `--actor lambda:x` with no `KHIVE_ACTOR` env and no config-file `[actor] id`
    /// left actor_id None → anonymous token → degraded ADR-057 comm + false warning.
    #[test]
    #[serial]
    fn cli_actor_flag_populates_actor_id() {
        std::env::remove_var("KHIVE_ACTOR");

        // ADR-096 Fork 2: an explicit EMPTY config file (rather than `None`)
        // keeps this test hermetic against whatever the real `$HOME/.khive/config.toml`
        // on the machine running the suite happens to contain — the project-actor
        // tier (`resolve_project_actor_id`) now runs unconditionally and would
        // otherwise pick up a real machine's global `[actor]`, if one is set.
        // (The explicit tier fails loud on a MISSING file — ADR-035 — so the
        // hermeticity trick must be a real, empty file.)
        let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
        let missing_config = empty_config_dir.path().join("config.toml");
        std::fs::write(&missing_config, "").expect("write empty config");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: Namespace::parse("lambda:agent-x").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:agent-x"),
            "--actor flag must populate actor_id (flag==env parity), not just default_namespace"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "lambda:agent-x",
            "the flag still sets the write namespace"
        );
    }

    #[test]
    #[serial]
    fn no_embed_explicit_actor_preserves_git_write_config() {
        std::env::remove_var("KHIVE_ACTOR");
        let repo = tempfile::tempdir().expect("repo tempdir");
        std::fs::create_dir(repo.path().join(".git")).expect("create .git");
        let dir = tempfile::tempdir().expect("config tempdir");
        let path = write_config(
            dir.path(),
            &format!(
                "[[git_write.allowed]]\nrepo = {:?}\nbranches = [\"feat/*\"]\n",
                repo.path().display().to_string()
            ),
        );

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("lambda:cli-actor").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve no-embed config");

        assert_eq!(resolved.default_namespace.as_str(), "lambda:cli-actor");
        assert_eq!(resolved.actor_id.as_deref(), Some("lambda:cli-actor"));
        assert_eq!(resolved.git_write.allowed.len(), 1);
        assert_eq!(
            resolved.git_write.allowed[0].repo,
            repo.path().display().to_string()
        );
        assert_eq!(resolved.git_write.allowed[0].branches, vec!["feat/*"]);
    }

    /// The `"local"` default namespace must stay anonymous (actor_id None) even when
    /// passed explicitly, so `should_warn_unattributed` still flags an unset actor.
    #[test]
    #[serial]
    fn cli_actor_flag_local_stays_anonymous() {
        std::env::remove_var("KHIVE_ACTOR");

        // See the hermeticity note in `cli_actor_flag_populates_actor_id` above.
        let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
        let missing_config = empty_config_dir.path().join("config.toml");
        std::fs::write(&missing_config, "").expect("write empty config");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id, None,
            "explicit --actor local must remain anonymous (no actor_id) so the \
             unattributed-comm warning still fires"
        );
    }

    // --- ADR-096 Fork 2: project/cwd-anchored actor restore ---
    //
    // These tests exercise the REAL config-discovery path (`std::env::current_dir`
    // / `HOME`), which #651 anchored to the resolved database's own directory for
    // `config_id` purposes. Because process cwd and `HOME` are global process
    // state, each test below temporarily redirects both via `SeatEnv` (a small
    // RAII guard) and is marked `#[serial]` so it never races another `#[serial]`
    // test in this file. No other test in this module reads `config: None`
    // (everything else pins an explicit path or a nonexistent one), so these are
    // the only tests in this binary that legitimately depend on process cwd/HOME.

    /// RAII guard: temporarily redirects process cwd to `project_root` and `HOME`
    /// to an isolated, empty tempdir (so tier 4 — `~/.khive/config.toml` — never
    /// reaches whatever the real machine running this suite happens to have
    /// configured globally). Restores both on drop, even on panic/unwind.
    struct SeatEnv {
        original_cwd: PathBuf,
        original_home: Option<std::ffi::OsString>,
        _isolated_home: tempfile::TempDir,
    }

    impl SeatEnv {
        fn enter(project_root: &std::path::Path) -> Self {
            let original_cwd = std::env::current_dir().expect("read cwd");
            let original_home = std::env::var_os("HOME");
            let isolated_home = tempfile::tempdir().expect("isolated HOME tempdir");
            std::env::set_current_dir(project_root).expect("chdir into seat project root");
            std::env::set_var("HOME", isolated_home.path());
            Self {
                original_cwd,
                original_home,
                _isolated_home: isolated_home,
            }
        }
    }

    impl Drop for SeatEnv {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
            match &self.original_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Unit-level proof that `resolve_project_actor_id` reads the cwd-anchored
    /// project config — the pre-#651 tier-3 location — independently of any
    /// database directory. This is the primitive Fork 2 restores.
    #[test]
    #[serial]
    fn resolve_project_actor_id_reads_cwd_anchored_project_config() {
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());

        assert_eq!(
            khive_runtime::resolve_project_actor_id(None).expect("no config error"),
            Some("lambda:seat-actor".to_string()),
            "resolve_project_actor_id must read the cwd-anchored .khive/config.toml \
             regardless of any database directory"
        );
    }

    /// ADR-096 Fork 2 pinning regression test — the exact regression class that
    /// broke the fleet: a seat-shaped connection whose cwd carries its own
    /// `.khive/config.toml` with an `[actor] id`, while the resolved database (and
    /// its own db-anchored config directory) lives ELSEWHERE and carries no
    /// `[actor]` at all — exactly how daemon-multiplexed seats run in production
    /// (every seat's own project dir vs. one shared home database).
    ///
    /// Exercises the REAL discovery path end-to-end through `resolve_runtime_config`
    /// (not a synthetic roots-based helper), so a future change to config discovery
    /// that re-collapses this fails THIS test loudly instead of silently reducing
    /// every seat's attribution to `"local"` / anonymous.
    #[test]
    #[serial]
    fn seat_shaped_project_actor_resolves_through_full_tier_chain() {
        std::env::remove_var("KHIVE_ACTOR");

        // The seat: a project directory with its own `[actor] id`.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        // The shared database: a DIFFERENT directory, with no config.toml at its
        // own db-anchored location (the shared-home-database fleet case).
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve seat-shaped config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:seat-actor"),
            "a seat-shaped cwd with its own [actor] must resolve that actor through \
             the full discovery path even when the shared db-anchored config \
             location carries none — got {:?}",
            resolved.actor_id
        );
        assert_ne!(
            resolved.actor_id.as_deref(),
            Some("local"),
            "must not collapse to the literal namespace string"
        );
    }

    /// #689 regression: an unset `--db`/`KHIVE_DB` must anchor tier-3
    /// `.khive/config.toml` discovery on the process cwd, not on
    /// `resolve_db_anchor(None)`'s materialized `$HOME/.khive/khive.db`
    /// default. Before the fix, `db_path_for_config` was cloned straight from
    /// `base_config.db_path`, so an unset db collapsed tier 3 onto
    /// `$HOME/.khive/config.toml` and silently ignored a real project-local
    /// config with no error of any kind.
    ///
    /// Uses `[runtime].brain_profile` — read from the db-anchored config load
    /// (`resolve_config`/`runtime_config_from_khive_config`), unlike `[actor]`
    /// which is resolved through a separate, always-cwd-anchored tier (see
    /// `seat_shaped_project_actor_resolves_through_full_tier_chain` above) and
    /// so cannot observe this bug on its own.
    #[test]
    #[serial]
    fn resolve_runtime_config_unset_db_discovers_cwd_config_over_home() {
        std::env::remove_var("KHIVE_ACTOR");

        let project_dir = tempfile::tempdir().expect("project tempdir");
        std::fs::create_dir_all(project_dir.path().join(".khive")).expect("mkdir project .khive");
        std::fs::write(
            project_dir.path().join(".khive/config.toml"),
            "[runtime]\nbrain_profile = \"cwd-profile\"\n",
        )
        .expect("write project config");

        let seat_env = SeatEnv::enter(project_dir.path());

        // A conflicting $HOME/.khive/config.toml — must NOT win when --db is unset.
        std::fs::create_dir_all(seat_env._isolated_home.path().join(".khive"))
            .expect("mkdir home .khive");
        std::fs::write(
            seat_env._isolated_home.path().join(".khive/config.toml"),
            "[runtime]\nbrain_profile = \"home-profile\"\n",
        )
        .expect("write home config");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve unset-db config");

        assert_eq!(
            resolved.brain_profile.as_deref(),
            Some("cwd-profile"),
            "unset --db must resolve tier-3 discovery against the project cwd, \
             not $HOME/.khive/khive.db's directory — got {:?}",
            resolved.brain_profile
        );
    }

    /// CLI `--actor` (tier 1) must win over a discovered project-config `[actor]`
    /// (tier 2), per the ratified full precedence chain (ADR-096 Fork 2:
    /// CLI > project-config > KHIVE_ACTOR env > anonymous).
    #[test]
    #[serial]
    fn cli_actor_flag_wins_over_project_config_actor() {
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:project-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace: Namespace::parse("lambda:cli-actor").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:cli-actor"),
            "an explicit --actor flag must win over a discovered project-config actor"
        );
    }

    /// Project-config `[actor] id` (tier 2) must win over `KHIVE_ACTOR` env
    /// (tier 3) when both are present, and env must still be used as a fallback
    /// when no project config exists — the precedence this ADR restores.
    #[test]
    #[serial]
    fn project_actor_config_beats_khive_actor_env_which_falls_back_to_anonymous() {
        std::env::remove_var("KHIVE_ACTOR");

        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_config(
            dir.path(),
            r#"
[actor]
id = "lambda:project-actor"
"#,
        );

        std::env::set_var("KHIVE_ACTOR", "lambda:env-actor");

        let with_project_config = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config with project actor");

        // A real, EMPTY file stands in for "no project config": the explicit
        // tier fails loud on a missing file (ADR-035), so the hermeticity
        // trick cannot be a nonexistent path.
        let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
        let empty_config = empty_config_dir.path().join("config.toml");
        std::fs::write(&empty_config, "").expect("write empty config");
        let without_project_config = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&empty_config),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config without project actor");

        std::env::remove_var("KHIVE_ACTOR");

        assert_eq!(
            with_project_config.actor_id.as_deref(),
            Some("lambda:project-actor"),
            "a project-config [actor] id must win over KHIVE_ACTOR env"
        );
        assert_eq!(
            without_project_config.actor_id.as_deref(),
            Some("lambda:env-actor"),
            "KHIVE_ACTOR env must still be used when no project config actor exists"
        );
    }

    /// PR #657: drives the REAL `clap` parse of `Args`
    /// (not a hand-built `RuntimeConfigInputs`) to prove a bare shell-level
    /// `KHIVE_ACTOR` no longer occupies the tier-1 CLI slot. Before the fix,
    /// `args.rs` bound `--actor` to `env = "KHIVE_ACTOR"`, so this env var
    /// alone made `resolve_cli_namespace` report `explicit = true` and
    /// therefore beat the project-config tier — inverting the ratified
    /// chain (CLI flag > project config > `KHIVE_ACTOR` env > anonymous).
    #[test]
    #[serial]
    fn real_clap_path_khive_actor_env_no_longer_wins_over_project_config() {
        use clap::Parser;
        std::env::remove_var("KHIVE_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:project-actor\"\n",
        )
        .expect("write seat config");

        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::set_var("KHIVE_ACTOR", "lambda:env-actor");

        // The real arg vector `kkernel mcp` parses — no `--actor` flag, so a
        // pre-fix `env = "KHIVE_ACTOR"` binding would populate `args.actor`.
        let args = Args::try_parse_from(["mcp"]).expect("parse real mcp args");
        let (namespace_explicit, namespace) =
            resolve_cli_namespace(&args).expect("resolve cli namespace");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace,
            namespace_explicit,
            actor_explicit: namespace_explicit,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        });

        std::env::remove_var("KHIVE_ACTOR");
        let resolved = resolved.expect("resolve config");

        assert!(
            !namespace_explicit,
            "KHIVE_ACTOR env alone must NOT make the CLI namespace tier explicit"
        );
        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:project-actor"),
            "project-config [actor] id must win over KHIVE_ACTOR env on the real clap path"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "KHIVE_ACTOR env must never set default_namespace, only actor_id"
        );
    }

    /// PR #657, second case: with no project config and
    /// no `--actor` flag, `KHIVE_ACTOR` must still land as the tier-3
    /// `actor_id` fallback (it is read directly by `RuntimeConfig::default()`,
    /// independent of the removed clap `env` binding) — and must still leave
    /// `default_namespace` at `"local"`.
    #[test]
    #[serial]
    fn real_clap_path_khive_actor_env_falls_back_to_tier3_actor_id() {
        use clap::Parser;
        std::env::remove_var("KHIVE_ACTOR");

        // No project config anywhere on the discovery path: an isolated,
        // empty seat dir + isolated HOME (SeatEnv), so tier 2 and tier 4
        // (~/.khive/config.toml) both come up empty.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::set_var("KHIVE_ACTOR", "lambda:env-only-actor");

        let args = Args::try_parse_from(["mcp"]).expect("parse real mcp args");
        let (namespace_explicit, namespace) =
            resolve_cli_namespace(&args).expect("resolve cli namespace");

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: None,
            namespace,
            namespace_explicit,
            actor_explicit: namespace_explicit,
            no_embed: true,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        });

        std::env::remove_var("KHIVE_ACTOR");
        let resolved = resolved.expect("resolve config");

        assert!(
            !namespace_explicit,
            "KHIVE_ACTOR env alone must NOT make the CLI namespace tier explicit"
        );
        assert_eq!(
            resolved.actor_id.as_deref(),
            Some("lambda:env-only-actor"),
            "KHIVE_ACTOR env must still land as the tier-3 actor_id fallback \
             when no project config exists"
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "KHIVE_ACTOR env must never set default_namespace, only actor_id"
        );
    }

    /// PR #657: an explicit `--actor local` (an operator
    /// request for the anonymous identity) must suppress BOTH the project-config
    /// and the db-anchored-config actor tiers, not just the missing-flag default.
    /// Before the fix, `resolve_runtime_config`'s tier-3 fold used
    /// `cli_actor.or(project_actor).or(resolved.actor_id)` unconditionally, so an
    /// explicit `local` (which maps to `cli_actor = None`) still fell through to
    /// whatever project or db-anchored `[actor]` happened to be discovered.
    #[test]
    #[serial]
    fn explicit_actor_local_suppresses_project_and_db_actor_tiers() {
        std::env::remove_var("KHIVE_ACTOR");

        // The seat: a project directory with its own `[actor] id`.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        std::fs::create_dir_all(seat_dir.path().join(".khive")).expect("mkdir seat .khive");
        std::fs::write(
            seat_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:seat-actor\"\n",
        )
        .expect("write seat config");

        // A DIFFERENT db-anchored directory that ALSO carries its own `[actor]`
        // (the db-anchored config load in `resolve_config` applies this
        // unconditionally, regardless of the CLI explicit flag).
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        std::fs::write(
            khive_dir.join("config.toml"),
            "[actor]\nid = \"lambda:db-actor\"\n",
        )
        .expect("write db-anchored config");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        let _seat_env = SeatEnv::enter(seat_dir.path());

        let resolved = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: false,
            packs: Some(kg_test_packs()),
            brain_profile: None,
        })
        .expect("resolve config");

        assert_eq!(
            resolved.actor_id, None,
            "explicit --actor local must resolve to anonymous even when both a \
             project-config and a db-anchored config declare an [actor] id — got {:?}",
            resolved.actor_id
        );
        assert_eq!(
            resolved.default_namespace.as_str(),
            "local",
            "explicit --actor local must keep default_namespace local"
        );
    }

    /// `config_id` must stay byte-identical across two connections that share ONE
    /// database but declare DIFFERENT `[actor]` ids via their own project/cwd
    /// config (ADR-096 Fork 2 hard invariant — `actor_id` must never feed
    /// `compute_config_id`, and neither may the identity-derived
    /// `visible_namespaces` fold-in). `default_namespace` must also stay
    /// `"local"` for both (ADR-007 Rev 4 Rule 0), independent of the configured actor.
    ///
    /// Deliberately does NOT use an explicit `--config` override for the two
    /// connections: an explicit path is tier 1 and would make the db-anchored
    /// config load (which DOES fold its own `[actor]` into `visible_namespaces`,
    /// unchanged pre-existing behavior) and the new project-actor tier read the
    /// identical file, conflating "two different db-anchored configs" (a
    /// different, pre-existing concern) with "two different project-anchored
    /// actors on one shared db-anchored config" (what Fork 2 must keep
    /// config_id-inert). Real seats share ONE db-anchored config; only their
    /// project-anchored actor differs — this test mirrors that shape via `SeatEnv`.
    #[test]
    #[serial]
    fn config_id_byte_identical_across_different_actor_ids() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        // ONE shared database, anchored in its own directory with NO `[actor]` at
        // that db-anchored config location — mirrors the real fleet shape, where
        // every seat's project config differs but the shared home database's own
        // config carries no actor.
        let db_dir = tempfile::tempdir().expect("db tempdir");
        let khive_dir = db_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir db .khive");
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").expect("touch db file");
        let db_str = db_path.to_str().expect("utf8 path").to_string();

        // Two different seat project directories, each with its OWN distinct
        // [actor] id.
        let seat_a = tempfile::tempdir().expect("seat a");
        std::fs::create_dir_all(seat_a.path().join(".khive")).expect("mkdir seat a .khive");
        std::fs::write(
            seat_a.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:actor-a\"\n",
        )
        .expect("write seat a config");

        let seat_b = tempfile::tempdir().expect("seat b");
        std::fs::create_dir_all(seat_b.path().join(".khive")).expect("mkdir seat b .khive");
        std::fs::write(
            seat_b.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:actor-b\"\n",
        )
        .expect("write seat b config");

        let cfg_a = {
            let _seat_env = SeatEnv::enter(seat_a.path());
            resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_str),
                config: None,
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: false,
                actor_explicit: false,
                no_embed: true,
                packs: Some(kg_test_packs()),
                brain_profile: None,
            })
            .expect("resolve config a")
        };

        let cfg_b = {
            let _seat_env = SeatEnv::enter(seat_b.path());
            resolve_runtime_config(RuntimeConfigInputs {
                db: Some(&db_str),
                config: None,
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: false,
                actor_explicit: false,
                no_embed: true,
                packs: Some(kg_test_packs()),
                brain_profile: None,
            })
            .expect("resolve config b")
        };

        assert_eq!(cfg_a.actor_id.as_deref(), Some("lambda:actor-a"));
        assert_eq!(cfg_b.actor_id.as_deref(), Some("lambda:actor-b"));
        assert_ne!(
            cfg_a.actor_id, cfg_b.actor_id,
            "precondition: the two connections must actually declare different actors"
        );

        assert_eq!(
            cfg_a.default_namespace.as_str(),
            "local",
            "default_namespace must stay local regardless of the configured actor"
        );
        assert_eq!(
            cfg_b.default_namespace.as_str(),
            "local",
            "default_namespace must stay local regardless of the configured actor"
        );

        assert_eq!(
            crate::server::compute_config_id(&cfg_a, None),
            crate::server::compute_config_id(&cfg_b, None),
            "config_id must be byte-identical across connections that differ ONLY \
             in [actor] id and folded visibility — identity fields must never feed compute_config_id"
        );
    }

    // --- multi-backend boot path (ADR-028) ---

    /// Build a `RuntimeConfig` suitable for multi-backend tests: in-memory db,
    /// AllowAllGate, "local" namespace, no embedder, both kg and comm packs.
    ///
    /// `db_path` mirrors what `resolve_runtime_config` sets for a `--db`-unset
    /// invocation (every call site below passes `cli_db_override: None` to
    /// `build_server_multi_backend`/`build_registry_for_multi_backend`) — the
    /// db-anchor consistency guard those functions run requires `db_path` to
    /// agree with `resolve_db_anchor` for the same input.
    fn base_runtime_config_for_multi_backend() -> RuntimeConfig {
        use khive_runtime::{AllowAllGate, BackendId, Namespace};
        RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(None),
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::parse("local").expect("ns"),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        }
    }

    #[tokio::test]
    async fn secondary_legacy_ref_blocks_before_zero_ref_main_can_enable_v21_gc() {
        use khive_db::migrations::AttachmentCutoverStatus;
        use khive_db::stores::blob::FsBlobStore;
        use khive_storage::BlobStore as _;

        let temp = tempfile::tempdir().unwrap();
        let main_path = temp.path().join("main.db");
        let secondary_path = temp.path().join("secondary.db");
        let (main_entity, _) = crate::attachment_cutover::create_v20_database_fixture(
            &main_path,
            "visual_asset",
            None,
        );
        rusqlite::Connection::open(&main_path)
            .unwrap()
            .execute(
                "DELETE FROM entities WHERE id = ?1",
                [main_entity.to_string()],
            )
            .unwrap();
        let (secondary_entity, _) = crate::attachment_cutover::create_v20_database_fixture(
            &secondary_path,
            "visual_asset",
            Some(2),
        );

        let store = FsBlobStore::new(temp.path().join("blobs"), 0).unwrap();
        let bytes = b"secondary-owned legacy blob".to_vec();
        let live_ref = store.put(bytes).await.unwrap();
        rusqlite::Connection::open(&secondary_path)
            .unwrap()
            .execute(
                "UPDATE entities SET content_ref = ?1 WHERE id = ?2",
                rusqlite::params![live_ref.as_str(), secondary_entity.to_string()],
            )
            .unwrap();

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            ..KhiveConfig::default()
        };
        let error = match build_registry_for_multi_backend(
            base_runtime_config_for_multi_backend(),
            &khive_cfg,
            None,
        )
        .await
        {
            Ok(_) => panic!("secondary liveness must block main cutover"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("legacy_refs=1"));

        let main = StorageBackend::sqlite(&main_path).unwrap();
        assert_eq!(
            main.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Pending,
            "secondary rejection must leave main at V20 without a marker"
        );
        store
            .transactional_orphan_sweep(main.sql().as_ref(), false)
            .await
            .expect_err("a V20 main must refuse attachment-only GC");
        assert!(store.exists(&live_ref).await.unwrap());
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

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend boot must succeed");

        // kg round-trip: create an entity on the main backend.
        let kg_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="concept", name="MultiBackendTestEntity")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
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
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
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

    /// ADR-124 note-write identity guard: the pack-owned note kind set must
    /// be installed on the multi-backend boot path, not only on
    /// `KhiveMcpServer::with_packs` (single-backend). Routes `kg` and `comm`
    /// to the same "main" backend through the real `build_server_multi_backend`
    /// builder — no manual `install_pack_owned_note_kinds` call, unlike
    /// `build_registry_with_owned_kinds` in the `khive-pack-comm` integration
    /// tests — so this test exercises the actual boot wiring rather than a
    /// hand-simulated one. Without the install this fix added in `serve.rs`
    /// (alongside the edge-rules/note-mutation-hook installs), a generic
    /// `update(properties={from_actor: ...})` on an inbound message note
    /// would silently succeed on a served multi-backend instance. Verified by
    /// temporarily reverting the `install_pack_owned_note_kinds` calls in
    /// `build_registry_for_multi_backend_inner` and re-running this test: it
    /// fails (`from_actor` forgery succeeds) without the fix and passes with
    /// it restored.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_installs_owned_note_kinds_so_update_is_refused() {
        use crate::tools::request::RequestParams;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend boot must succeed");

        let send_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="local", content="adr-124 multi-backend boot probe")"#
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("comm.send dispatch must not error");
        let send_json: serde_json::Value =
            serde_json::from_str(&send_resp).expect("send response is valid JSON");
        assert_eq!(
            send_json["results"][0]["ok"].as_bool(),
            Some(true),
            "comm.send must succeed; response: {send_resp}"
        );
        let full_id = send_json["results"][0]["result"]["full_id"]
            .as_str()
            .expect("send must return full_id")
            .to_string();

        let get_before_resp = server
            .dispatch_request_local(RequestParams {
                ops: format!(r#"get(id="{full_id}")"#),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("get dispatch must not error");
        let get_before_json: serde_json::Value =
            serde_json::from_str(&get_before_resp).expect("get response is valid JSON");
        let original_from_actor =
            get_before_json["results"][0]["result"]["properties"]["from_actor"].clone();

        let update_resp = server
            .dispatch_request_local(RequestParams {
                ops: format!(
                    r#"update(id="{full_id}", properties={{"from_actor": "forged-actor"}})"#
                ),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("update dispatch must not error");
        let update_json: serde_json::Value =
            serde_json::from_str(&update_resp).expect("update response is valid JSON");
        assert_eq!(
            update_json["results"][0]["ok"].as_bool(),
            Some(false),
            "update forging `from_actor` on a message note must be refused on a served \
             multi-backend instance; response: {update_resp}"
        );
        let error_msg = update_json["results"][0]["error"]
            .as_str()
            .unwrap_or_default();
        assert!(
            error_msg.contains("from_actor"),
            "refusal error must name `from_actor`; got: {error_msg}"
        );

        let get_after_resp = server
            .dispatch_request_local(RequestParams {
                ops: format!(r#"get(id="{full_id}")"#),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("get dispatch must not error");
        let get_after_json: serde_json::Value =
            serde_json::from_str(&get_after_resp).expect("get response is valid JSON");
        assert_eq!(
            get_after_json["results"][0]["result"]["properties"]["from_actor"], original_from_actor,
            "stored from_actor must be unchanged after the refused forgery attempt"
        );
    }

    /// ADR-124 boot-occupancy regression (multi-backend twin of
    /// `server::tests::single_runtime_boot_installs_note_write_validator`):
    /// `has_note_write_validator` exists specifically so a transport's own
    /// tests can assert, per boot path, that the documented startup
    /// sequence actually filled the slot — but nothing called it for the
    /// multi-backend builder either. Each per-pack runtime is constructed
    /// independently in this boot path (unlike single-backend
    /// `with_packs`), so the validator must be installed on the default
    /// runtime AND on every per-pack runtime, not just one — installing it
    /// on only the default would leave `kg`'s own per-pack runtime (which
    /// actually serves the generic `create` verb below) unenforced. Asserts
    /// occupancy directly on all of them through the real
    /// `build_registry_for_multi_backend_inner` builder, then proves the
    /// slot is wired, not just occupied: a generic `create` naming a forged
    /// `from_actor` on a `message` note must come back derived to the same
    /// actor a legitimate `comm.send` on this server stamps, not the forged
    /// value. Sensitivity verified by temporarily commenting out both
    /// `registry.call_register_note_write_validators(...)` calls in
    /// `build_registry_for_multi_backend_inner` and re-running: all
    /// assertions fail without them (occupancy false; forged value
    /// survives) and pass with them restored.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_installs_note_write_validator_on_every_runtime() {
        use crate::tools::request::RequestParams;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend_inner(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry build must succeed");

        assert!(
            multi.default_runtime.has_note_write_validator(),
            "multi-backend boot must install the note-write validator on the \
             default runtime"
        );
        for (pack_name, rt) in &multi.per_pack_runtimes {
            assert!(
                rt.has_note_write_validator(),
                "multi-backend boot must install the note-write validator on \
                 the per-pack runtime for {pack_name:?}"
            );
        }

        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);

        let send_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="local", content="adr-124 boot-occupancy actor probe")"#
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("comm.send dispatch must not error");
        let send_json: serde_json::Value =
            serde_json::from_str(&send_resp).expect("send response is valid JSON");
        assert_eq!(
            send_json["results"][0]["ok"].as_bool(),
            Some(true),
            "comm.send must succeed; response: {send_resp}"
        );
        let full_id = send_json["results"][0]["result"]["full_id"]
            .as_str()
            .expect("send must return full_id")
            .to_string();

        let get_resp = server
            .dispatch_request_local(RequestParams {
                ops: format!(r#"get(id="{full_id}")"#),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("get dispatch must not error");
        let get_json: serde_json::Value =
            serde_json::from_str(&get_resp).expect("get response is valid JSON");
        let legitimate_actor = get_json["results"][0]["result"]["properties"]["from_actor"].clone();

        let create_resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"create(kind="message", content="adr-124 boot-occupancy create probe", properties={"from_actor": "forged-actor"})"#
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("create dispatch must not error");
        let create_json: serde_json::Value =
            serde_json::from_str(&create_resp).expect("create response is valid JSON");
        assert_eq!(
            create_json["results"][0]["ok"].as_bool(),
            Some(true),
            "create must succeed; response: {create_resp}"
        );
        assert_eq!(
            create_json["results"][0]["result"]["properties"]["from_actor"], legitimate_actor,
            "a forged from_actor on a generic create must come back derived to \
             the same actor a legitimate comm.send on this server stamps, not \
             the forged value; response: {create_resp}"
        );
    }

    /// #658 multi-backend regression: `build_registry_for_multi_backend` — the
    /// production multi-backend wiring path — must also wire the brain
    /// dispatch hook produced by `PackFactory::create_install`, observing the
    /// same `BrainPack` instance the registry dispatches `brain.*` verbs to.
    /// Mirrors `server::tests::brain_dispatch_hook_updates_state_visible_through_same_instance`
    /// (single-backend path) using this file's multi-backend entry point instead.
    #[tokio::test]
    #[serial]
    async fn multi_backend_brain_dispatch_hook_updates_state_visible_through_same_instance() {
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let mut base_cfg = base_runtime_config_for_multi_backend();
        base_cfg.packs = vec!["kg".to_string(), "brain".to_string()];

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry build must succeed");

        multi
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state loads the default namespace into the active slot");

        multi
            .registry
            .dispatch("stats", serde_json::json!({}))
            .await
            .expect("kg.stats dispatch succeeds");

        let state = multi
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state dispatch");
        let total_events = state["balanced_recall"]["total_events"]
            .as_u64()
            .unwrap_or(0);
        assert!(
            total_events > 0,
            "multi-backend dispatch hook must update the same BrainPack instance \
             the registry dispatches brain.* verbs to; got snapshot {state:?}"
        );
    }

    /// Regression for #601, adapted for #603: both multi-backend boot paths —
    /// `build_server_multi_backend` (this file) and `kkernel`'s `Command::Mcp`
    /// coordinator branch — now finish through the single
    /// [`build_server_from_multi_backend_registry`] constructor instead of each
    /// hand-assembling `from_registry_with_meta` + `with_pool`. This test calls
    /// that shared constructor directly (`coordinator: None`, the same value
    /// `build_server_multi_backend` passes) rather than re-deriving the
    /// `is_file_backed`/`pool_arc` logic inline, so a regression in the shared
    /// constructor itself — or its callers drifting back to hand-assembly —
    /// fails here directly. The kkernel-vs-`build_server_multi_backend` parity
    /// itself is covered end-to-end by `kkernel`'s own
    /// `multi_backend_boot_paths_share_identical_wiring_surface` test, which
    /// exercises the actual coordinator branch.
    #[tokio::test]
    #[serial]
    async fn kkernel_multi_backend_path_wires_pool_for_file_backed_main() {
        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("main.db");

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry build must succeed");
        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);

        assert!(
            server.pool().is_some(),
            "file-backed multi-backend main must wire a checkpoint pool onto the server"
        );
    }

    /// Sibling guard: an in-memory main backend must never carry a checkpoint pool
    /// (checkpoint_once must never run on a non-WAL, in-memory connection). Also
    /// exercises `build_server_from_multi_backend_registry` — see the note on the
    /// sibling test above.
    #[tokio::test]
    #[serial]
    async fn kkernel_multi_backend_path_leaves_pool_none_for_in_memory_main() {
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry build must succeed");
        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);

        assert!(
            server.pool().is_none(),
            "in-memory multi-backend main must never carry a checkpoint pool"
        );
    }

    // ── ADR-111 Amendment 2: `resolve_blob_store` must
    // actually be reached from the real boot paths, not only its own unit
    // tests. Both tests below assert against the credential-env error
    // `S3BlobStore::new` raises with no AWS creds in the environment --
    // exactly the technique `khive-runtime`'s own `resolve_blob_store` tests
    // use -- but reached through `build_server`/`build_registry_for_multi_backend`
    // themselves, proving the boot path resolves and installs the configured
    // `S3BlobStore` rather than silently keeping the default `FsBlobStore`.

    #[tokio::test]
    #[serial]
    async fn single_backend_boot_wires_configured_s3_blob_store() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = write_config(
            dir.path(),
            r#"
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
region = "us-east-1"
"#,
        );

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            ":memory:",
            "--pack",
            "kg",
            "--config",
            config_path.to_str().expect("utf8 path"),
        ]);

        let result = build_server(&args).await;

        match prev_access_key {
            Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
            None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
        }
        match prev_secret_key {
            Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
            None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
        }

        let err = result.err().expect(
            "an s3 blob backend with no AWS credentials must fail boot through the real \
             single-backend path -- a silent fs fallback would return Ok here instead",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("AWS_ACCESS_KEY_ID"),
            "expected the credential-env error surfaced through build_server, got: {msg}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_wires_configured_s3_blob_store() {
        let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::S3 {
                    bucket: "khive-blobs".to_string(),
                    region: "us-east-1".to_string(),
                    endpoint: None,
                    prefix: None,
                    allow_http: None,
                }),
            },
            ..KhiveConfig::default()
        };
        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None).await;

        match prev_access_key {
            Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
            None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
        }
        match prev_secret_key {
            Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
            None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
        }

        let err = result.err().expect(
            "an s3 blob backend with no AWS credentials must fail boot through the real \
             multi-backend path -- a silent fs fallback would return Ok here instead",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("AWS_ACCESS_KEY_ID"),
            "expected the credential-env error surfaced through \
             build_registry_for_multi_backend, got: {msg}"
        );
    }

    // ── ADR-111 Amendment 2: the two tests above only
    // prove the fail-closed error path. The three tests below exercise the
    // successful construction-and-install branch of `install_resolved_blob_store`
    // (the real call site: `:1826` single-backend, `:1567` multi-backend) plus
    // the no-`[storage.blob]` filesystem-default boot promised by ADR-111
    // Amendment 2. `BlobStore` carries a `Debug` supertrait (khive-storage)
    // for exactly this purpose: it lets these tests tell which concrete
    // backend got installed behind `Arc<dyn BlobStore>` via
    // `format!("{store:?}")` without adding a downcast/type-name method to
    // the production trait surface.

    /// Isolated dummy (non-secret, never-valid) AWS credentials for the
    /// success-path tests below. `S3BlobStore::new` only builds an
    /// `AmazonS3` client (`object_store`'s `AmazonS3Builder::build`); it
    /// performs no network I/O, so a syntactically-valid dummy key pair is
    /// enough to reach a successful `Ok` construction.
    const DUMMY_AWS_ACCESS_KEY_ID: &str = "AKIADUMMYWITNESSKEY00";
    const DUMMY_AWS_SECRET_ACCESS_KEY: &str = "dummy-witness-secret-access-key-never-real";

    /// RAII guard: sets the two AWS credential env vars to isolated dummy
    /// values for the duration of the test, restoring whatever was
    /// previously present (usually nothing) on drop. Paired with `#[serial]`
    /// on every test that uses it, matching the convention the two boot
    /// tests above already established for this same pair of env vars.
    struct DummyAwsCredsGuard {
        prev_access_key: Option<String>,
        prev_secret_key: Option<String>,
    }

    impl DummyAwsCredsGuard {
        fn set() -> Self {
            let prev_access_key = std::env::var("AWS_ACCESS_KEY_ID").ok();
            let prev_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
            std::env::set_var("AWS_ACCESS_KEY_ID", DUMMY_AWS_ACCESS_KEY_ID);
            std::env::set_var("AWS_SECRET_ACCESS_KEY", DUMMY_AWS_SECRET_ACCESS_KEY);
            Self {
                prev_access_key,
                prev_secret_key,
            }
        }
    }

    impl Drop for DummyAwsCredsGuard {
        fn drop(&mut self) {
            match self.prev_access_key.take() {
                Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
            match self.prev_secret_key.take() {
                Some(v) => std::env::set_var("AWS_SECRET_ACCESS_KEY", v),
                None => std::env::remove_var("AWS_SECRET_ACCESS_KEY"),
            }
        }
    }

    /// RAII guard: clears the `KHIVE_*` variables that would otherwise
    /// override the temp `khive.toml` the boot tests write, restoring each
    /// prior value (or absence) on drop, even on panic/unwind. `#[serial]`
    /// serializes access but does not restore process-global state; this
    /// guard does.
    struct ClearedKhiveEnvGuard {
        prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ClearedKhiveEnvGuard {
        const VARS: [&'static str; 5] = [
            "KHIVE_DB",
            "KHIVE_ACTOR",
            "KHIVE_PACKS",
            "KHIVE_REQUIRE_ATTRIBUTED_ACTOR",
            "KHIVE_BLOB_ROOT",
        ];

        fn clear() -> Self {
            let prev = Self::VARS
                .iter()
                .map(|name| {
                    let value = std::env::var_os(name);
                    std::env::remove_var(name);
                    (*name, value)
                })
                .collect();
            Self { prev }
        }
    }

    impl Drop for ClearedKhiveEnvGuard {
        fn drop(&mut self) {
            for (name, value) in self.prev.drain(..) {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn s3_blob_config() -> BlobConfig {
        BlobConfig::S3 {
            bucket: "khive-blobs".to_string(),
            region: "us-east-1".to_string(),
            endpoint: None,
            prefix: None,
            allow_http: None,
        }
    }

    /// Positive counterpart to `single_backend_boot_wires_configured_s3_blob_store`:
    /// with valid (dummy) AWS credentials present, the single-backend startup
    /// path's `install_resolved_blob_store` call (`:1826`) must actually
    /// install an `S3BlobStore`, not merely fail closed when credentials are
    /// absent. Round-4 remediation: drives the real `build_server` boot entry
    /// (not `KhiveRuntime::new` + a direct `install_resolved_blob_store` call)
    /// via a temporary `khive.toml` + parsed `Args`, selecting the `schedule`
    /// pack so its already-installed runtime (`:1847`) is returned for
    /// inspection.
    #[tokio::test]
    #[serial]
    async fn single_backend_boot_installs_s3_blob_store_on_successful_selection() {
        let _env = ClearedKhiveEnvGuard::clear();
        let _creds = DummyAwsCredsGuard::set();

        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = write_config(
            dir.path(),
            r#"
[storage.blob]
backend = "s3"
bucket = "khive-blobs"
region = "us-east-1"
"#,
        );

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            ":memory:",
            "--pack",
            "kg",
            "--pack",
            "schedule",
            "--config",
            config_path.to_str().expect("utf8 path"),
        ]);

        let (_server, schedule_rt) = build_server(&args).await.expect(
            "valid dummy AWS credentials must resolve and install an S3BlobStore through the \
             real single-backend boot path",
        );
        let runtime = schedule_rt
            .expect("the schedule pack was selected so its installed runtime must be returned");

        let installed = runtime.blob_store().expect(
            "install_resolved_blob_store must call KhiveRuntime::install_blob_store at the \
             real :1826 call site",
        );
        let debug = format!("{installed:?}");
        assert!(
            debug.contains("S3BlobStore"),
            "expected the installed store to be an S3BlobStore, got: {debug}"
        );
    }

    /// A read-only runtime resolving an EXISTING blob root must be able to
    /// install it for bounded reads: the boot helper's hydrator carries its
    /// mode from construction, so the runtime's read-only install seam
    /// accepts it, serves reads, and refuses mutation. Without the
    /// mode-aware construction, the install refuses the hydrator the boot
    /// just resolved and a read-only snapshot cannot serve blobs at all.
    #[tokio::test]
    #[serial]
    async fn install_resolved_blob_store_read_only_runtime_gets_bounded_reads() {
        let _env = ClearedKhiveEnvGuard::clear();
        use khive_storage::BlobStore as _;
        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("snapshot.db");
        prepare_current_snapshot_source(&main_path);

        // Seed an existing root while writable; the read-only boot below
        // must open it without creating anything.
        let blob_root = dir.path().join("blobs");
        let seeded = {
            let seed_store =
                khive_db::stores::blob::FsBlobStore::new(blob_root.clone(), 0).expect("seed store");
            seed_store.put(b"seed".to_vec()).await.expect("seed put")
        };

        let khive_cfg = KhiveConfig {
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(blob_root.display().to_string()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let backend =
            Arc::new(StorageBackend::sqlite_read_only(&main_path).expect("read-only backend"));
        assert!(backend.is_read_only());
        let runtime = khive_runtime::KhiveRuntime::from_backend(
            Arc::clone(&backend),
            base_runtime_config_for_multi_backend(),
        );
        assert!(runtime.is_read_only());

        let _hydrator = install_resolved_blob_store(&runtime, &khive_cfg, backend.as_ref())
            .expect("a read-only runtime must install its resolved existing root")
            .expect("the configured root resolves to a store");

        let installed = runtime.blob_store().expect("installed store");
        assert!(
            installed.exists(&seeded).await.expect("exists"),
            "bounded reads must be served from the existing root"
        );
        let err = installed
            .put(b"post".to_vec())
            .await
            .expect_err("mutation must refuse on the read-only snapshot");
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    /// Positive counterpart to `multi_backend_boot_wires_configured_s3_blob_store`:
    /// with valid (dummy) AWS credentials present, the multi-backend startup
    /// path must resolve the configured `S3BlobStore` once (`:1567`) and
    /// install it on every per-pack runtime this boot produces.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_installs_s3_blob_store_on_successful_selection() {
        let _creds = DummyAwsCredsGuard::set();

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            storage: StorageSectionConfig {
                blob: Some(s3_blob_config()),
            },
            ..KhiveConfig::default()
        };
        let base_cfg = base_runtime_config_for_multi_backend();

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("valid dummy AWS credentials must resolve through the multi-backend path");

        assert!(
            !multi.per_pack_runtimes.is_empty(),
            "precondition: the base config declares at least one pack"
        );
        for (pack_name, rt) in &multi.per_pack_runtimes {
            let store = rt.blob_store().unwrap_or_else(|| {
                panic!("pack {pack_name:?} must have the S3 selection installed on its runtime")
            });
            let debug = format!("{store:?}");
            assert!(
                debug.contains("S3BlobStore"),
                "pack {pack_name:?}: expected the installed store to be an S3BlobStore, got: {debug}"
            );
        }
    }

    #[tokio::test]
    async fn multi_backend_boot_shares_one_hydrator_across_default_core_blob_and_moodboard() {
        let blob_root = tempfile::tempdir().expect("blob root");
        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(blob_root.path().to_string_lossy().into_owned()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let mut base_cfg = base_runtime_config_for_multi_backend();
        base_cfg.packs = vec!["kg".into(), "blob".into(), "moodboard".into()];
        base_cfg.blob_hydration_bytes = khive_storage::MAX_BLOB_WHOLE_BYTES;

        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry build");
        let expected = multi
            .default_runtime
            .blob_hydrator()
            .expect("default runtime hydrator");
        assert_eq!(expected.budget_bytes(), khive_storage::MAX_BLOB_WHOLE_BYTES);

        for pack_name in ["kg", "blob", "moodboard"] {
            let runtime = multi
                .per_pack_runtimes
                .get(pack_name)
                .unwrap_or_else(|| panic!("missing {pack_name} runtime"));
            let installed = runtime
                .blob_hydrator()
                .unwrap_or_else(|| panic!("missing {pack_name} hydrator"));
            assert!(
                Arc::ptr_eq(&installed, &expected),
                "{pack_name} must share the default runtime's aggregate budget"
            );
            let core = runtime.core().blob_hydrator().expect("core hydrator");
            assert!(
                Arc::ptr_eq(&core, &expected),
                "{pack_name}.core() must share the same aggregate budget"
            );
        }

        let blob_runtime = multi.per_pack_runtimes.get("blob").expect("blob runtime");
        let moodboard_runtime = multi
            .per_pack_runtimes
            .get("moodboard")
            .expect("moodboard runtime");
        let content_ref = blob_runtime
            .blob_store()
            .expect("raw store")
            .put(vec![b'x'])
            .await
            .expect("publish contention fixture");

        let held = blob_runtime
            .core()
            .blob_hydrator()
            .expect("blob core hydrator")
            .hydrate_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await
            .expect("reserve the full shared budget");

        let contender_hydrator = moodboard_runtime
            .blob_hydrator()
            .expect("moodboard hydrator");
        let contender_ref = content_ref.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let mut contender = tokio::spawn(async move {
            let _ = entered_tx.send(());
            contender_hydrator.hydrate_verified(&contender_ref, 1).await
        });
        entered_rx.await.expect("contender entered task");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut contender)
                .await
                .is_err(),
            "moodboard hydration must wait while blob/core holds the shared budget"
        );

        drop(held);
        let contender_blob = tokio::time::timeout(std::time::Duration::from_secs(1), contender)
            .await
            .expect("contender should wake when the shared lease drops")
            .expect("contender task")
            .expect("contender hydration");
        assert_eq!(contender_blob.bytes(), b"x");
    }

    /// Guards the ADR-111 Amendment 2 fs-default promise (`docs/adr/ADR-111-blob-store.md:538-541`):
    /// with no `[storage.blob]` section at all, the single-backend startup
    /// path must still install a usable `FsBlobStore` rooted beside the
    /// database file, and that store must actually round-trip a blob --
    /// not merely construct without error. Round-4 remediation: drives the
    /// real `build_server` boot entry via a temporary (sectionless)
    /// `khive.toml` + parsed `Args`, selecting the `schedule` pack so its
    /// already-installed runtime (`:1847`) is returned for inspection.
    #[tokio::test]
    #[serial]
    async fn single_backend_boot_default_fs_blob_store_is_usable_without_storage_section() {
        let _env = ClearedKhiveEnvGuard::clear();

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("main.db");
        let config_path = write_config(dir.path(), "");

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            db_path.to_str().expect("utf8 path"),
            "--pack",
            "kg",
            "--pack",
            "schedule",
            "--config",
            config_path.to_str().expect("utf8 path"),
        ]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("absent [storage.blob] must resolve the fs default through the real single-backend boot path");
        let runtime = schedule_rt
            .expect("the schedule pack was selected so its installed runtime must be returned");

        let installed = runtime.blob_store().expect(
            "install_resolved_blob_store must call KhiveRuntime::install_blob_store at the \
             real :1826 call site for a file-backed backend",
        );
        let debug = format!("{installed:?}");
        assert!(
            debug.contains("FsBlobStore"),
            "expected the default store to be an FsBlobStore, got: {debug}"
        );

        // The absent-section default keeps FsBlobStore's 100 GB free-space
        // floor — that default is exactly what this test locks in, and a CI
        // runner legitimately may not clear it. A CapacityFloor rejection can
        // only come from inside FsBlobStore::put, so it is equally valid
        // proof that the boot path wired a live fs-default store; round-trip
        // only when the volume has room.
        match installed
            .put(b"adr-111 fs-default regression".to_vec())
            .await
        {
            Ok(content_ref) => {
                let round_tripped = installed
                    .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
                    .await
                    .expect("fs-default store must serve back what it just accepted");
                assert_eq!(
                    round_tripped, b"adr-111 fs-default regression",
                    "fs-default store must round-trip the exact bytes written"
                );
            }
            Err(khive_storage::StorageError::CapacityFloor { .. }) => {}
            Err(other) => panic!("fs-default store must accept a write: {other:?}"),
        }
    }

    fn prepare_current_snapshot_source(path: &std::path::Path) {
        let backend = StorageBackend::sqlite(path).expect("create snapshot source");
        backend
            .prepare_core_schema()
            .expect("prepare exact-current migration ledger");
        drop(backend);
        let gc_lock = std::path::PathBuf::from(format!(
            "{}.khive-blob-gc.lock",
            path.as_os_str().to_string_lossy()
        ));
        if gc_lock.exists() {
            std::fs::remove_file(&gc_lock)
                .expect("snapshot fixture must start without a writable GC-lock sidecar");
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(path);
    }

    fn blob_only_runtime_config() -> RuntimeConfig {
        RuntimeConfig {
            packs: vec!["blob".to_string()],
            ..base_runtime_config_for_multi_backend()
        }
    }

    /// Schema administration does not construct a serving runtime. Once the
    /// attachment cutover is already complete, an otherwise-valid read-only
    /// snapshot therefore must not need (or materialize) its configured blob
    /// backend merely to report a no-op migration.
    #[tokio::test]
    #[serial]
    async fn exact_current_read_only_schema_admin_skips_unused_blob_resolution() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("snapshot.db");
        let missing_blob_root = dir.path().join("must-not-be-created");
        let gc_lock = std::path::PathBuf::from(format!(
            "{}.khive-blob-gc.lock",
            main_path.as_os_str().to_string_lossy()
        ));
        prepare_current_snapshot_source(&main_path);
        let before = std::fs::read(&main_path).expect("read exact-current snapshot");

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: BackendId::MAIN.to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                read_only: true,
            }],
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(missing_blob_root.display().to_string()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };

        let statuses = migrate_configured_storage_topology(
            base_runtime_config_for_multi_backend(),
            &khive_cfg,
            None,
            Some(BackendId::MAIN),
        )
        .await
        .expect("an exact-current admin migration must not resolve unused blob storage");

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].backend, BackendId::MAIN);
        assert_eq!(
            statuses[0].applied_version,
            khive_db::MIGRATIONS
                .last()
                .expect("migration ledger")
                .version
        );
        assert_eq!(
            std::fs::read(&main_path).expect("re-read exact-current snapshot"),
            before,
            "a no-op read-only migration must preserve database bytes"
        );
        assert!(
            !missing_blob_root.exists(),
            "schema administration must not materialize an unused blob root"
        );
        assert!(
            !gc_lock.exists(),
            "an exact-current no-op must not acquire write-side GC ownership"
        );
    }

    /// The default fs root is optional when no `[storage.blob]` section was
    /// declared. A snapshot boot must not create that directory merely by
    /// installing the blob pack, and `blob.put` must report the pack runtime's
    /// read-only mode before attempting any physical store write.
    #[tokio::test]
    #[serial]
    async fn read_only_single_backend_neither_creates_blob_root_nor_accepts_blob_put() {
        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("snapshot.db");
        let blob_root = dir.path().join("blobs");
        prepare_current_snapshot_source(&main_path);

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: BackendId::MAIN.to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path),
                cache_mb: None,
                journal_mode: None,
                read_only: true,
            }],
            ..KhiveConfig::default()
        };
        let multi = build_registry_for_multi_backend(blob_only_runtime_config(), &khive_cfg, None)
            .await
            .expect("read-only blob-pack registry must boot without creating a store");
        assert!(
            !blob_root.exists(),
            "snapshot boot must not materialize the default FsBlobStore root"
        );

        let error = multi
            .registry
            .dispatch("blob.put", serde_json::json!({"bytes": "YQ=="}))
            .await
            .expect_err("blob.put must reject on its read-only pack runtime");
        assert!(error.to_string().contains("read-only"), "{error}");
        assert!(
            !blob_root.exists(),
            "the rejected put must remain side-effect free"
        );
    }

    /// Mixed topology is governed by the runtime assigned to the blob pack,
    /// not by the main audit backend. A writable main must not accidentally
    /// make a read-only blob secondary writable or create its default fs root.
    #[tokio::test]
    #[serial]
    async fn read_only_blob_secondary_refuses_put_beside_writable_main() {
        use khive_runtime::PackConfig;

        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.db");
        let archive_path = dir.path().join("blob-snapshot.db");
        let archive_gc_lock = {
            let mut path = archive_path.as_os_str().to_os_string();
            path.push(".khive-blob-gc.lock");
            std::path::PathBuf::from(path)
        };
        let blob_root = dir.path().join("blobs");
        prepare_current_snapshot_source(&main_path);
        prepare_current_snapshot_source(&archive_path);
        assert!(!archive_gc_lock.exists());

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "blob-snapshot".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(archive_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: true,
                },
            ],
            packs: HashMap::from([(
                "blob".to_string(),
                PackConfig {
                    backend: "blob-snapshot".to_string(),
                },
            )]),
            ..KhiveConfig::default()
        };
        let multi = build_registry_for_multi_backend(blob_only_runtime_config(), &khive_cfg, None)
            .await
            .expect("mixed topology must boot");
        let error = multi
            .registry
            .dispatch("blob.put", serde_json::json!({"bytes": "YQ=="}))
            .await
            .expect_err("read-only blob secondary must reject put");
        assert!(error.to_string().contains("read-only"), "{error}");
        assert!(
            !blob_root.exists(),
            "main writability must not create storage for a read-only blob pack"
        );
        assert!(
            !archive_gc_lock.exists(),
            "an exact-current read-only secondary must be inventoried without acquiring a write-side GC lock file"
        );
    }

    /// Positive mixed-topology counterpart: a read-only main does not disable
    /// a blob pack explicitly routed to a writable secondary.
    #[tokio::test]
    #[serial]
    async fn writable_blob_secondary_accepts_put_beside_read_only_main() {
        use khive_runtime::PackConfig;

        let _env = ClearedKhiveEnvGuard::clear();
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main-snapshot.db");
        let blob_db = dir.path().join("blob-writable.db");
        let blob_root = dir.path().join("writable-blobs");
        prepare_current_snapshot_source(&main_path);
        prepare_current_snapshot_source(&blob_db);

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: true,
                },
                BackendConfig {
                    name: "blob-writable".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(blob_db),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: HashMap::from([(
                "blob".to_string(),
                PackConfig {
                    backend: "blob-writable".to_string(),
                },
            )]),
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(blob_root.display().to_string()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let multi = build_registry_for_multi_backend(blob_only_runtime_config(), &khive_cfg, None)
            .await
            .expect("writable blob secondary must boot beside read-only main");
        let result = multi
            .registry
            .dispatch("blob.put", serde_json::json!({"bytes": "YQ=="}))
            .await;
        assert!(
            result.is_ok(),
            "writable blob secondary must accept put: {result:?}"
        );
        assert!(
            blob_root.exists(),
            "writable blob storage may materialize its root"
        );
    }

    /// Regression for ADR-073: a pack assigned to a secondary backend must
    /// have `core_backend` wired at boot so that `rt.core().backend_id()` returns "main".
    ///
    /// Before the fix, `build_server_multi_backend` called `KhiveRuntime::from_backend`
    /// directly (without `with_core_backend`), so `core()` fell back to `self.clone()` and
    /// returned the secondary-backend handle — silently defeating the ADR-073 contract.
    /// Both boot paths now delegate to `build_pack_runtime`, which applies the wiring in
    /// one place and prevents any future path from drifting.
    #[tokio::test]
    #[serial]
    async fn secondary_pack_runtime_core_resolves_to_main_after_build_registry() {
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

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry must boot");

        let comm_rt = result
            .per_pack_runtimes
            .get("comm")
            .expect("comm pack runtime must be present in per_pack_runtimes");

        // Own backend_id is "secondary" — not main.
        assert_eq!(
            comm_rt.backend_id().as_str(),
            "secondary",
            "comm pack runtime's own backend_id must be \"secondary\""
        );

        // ADR-073 contract: core() on a secondary-backend pack must return a
        // main-bound handle, not a clone of self. Failure here means the
        // build_pack_runtime wiring was not applied.
        assert_eq!(
            comm_rt.core().backend_id().as_str(),
            BackendId::MAIN,
            "secondary-backend pack must have core_backend wired to main (ADR-073); \
             core().backend_id() returned {:?} — build_pack_runtime wiring missing",
            comm_rt.core().backend_id().as_str()
        );
    }

    /// ADR-091 Amendment 3 fan-out regression: two backends declared at
    /// alias spellings of the SAME database file (a direct path and a
    /// symlinked path) must mint the same canonical `DbIdentity` and
    /// therefore dedup to exactly one secondary pool. The pointer-identity
    /// dedup this replaced would have kept both `Arc<ConnectionPool>`
    /// instances distinct, letting two `SweepBackend`s race on one
    /// heartbeat file.
    #[tokio::test]
    #[serial]
    #[cfg(unix)]
    async fn secondary_pools_dedup_by_canonical_identity_across_alias_spellings() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let real_path = dir.path().join("khive.db");
        std::fs::write(&real_path, b"").unwrap();
        let alias_path = dir.path().join("khive_alias.db");
        std::os::unix::fs::symlink(&real_path, &alias_path).unwrap();

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
                    name: "direct".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(real_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(alias_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "kg".to_string(),
                    PackConfig {
                        backend: "direct".to_string(),
                    },
                );
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "alias".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();
        let multi = build_registry_for_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend registry with alias-spelled backends must boot");

        let secondary = secondary_file_backed_pools(&multi);
        assert_eq!(
            secondary.len(),
            1,
            "two backends aliasing the same database file must dedup to exactly one \
             secondary pool by canonical identity, got {} pools",
            secondary.len()
        );

        let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
        let mut backends = Vec::new();
        if let Some(pool) = server.pool() {
            backends.push(khive_db::SweepBackend {
                pool,
                is_main: true,
            });
        }
        for pool in server.secondary_pools() {
            backends.push(khive_db::SweepBackend {
                pool,
                is_main: false,
            });
        }
        assert_eq!(
            backends.len(),
            1,
            "exactly one SweepBackend must survive dedup for the alias pair — the \
             in-memory main backend contributes no pool of its own"
        );
    }

    /// Issue #553: `--db :memory:` (or `KHIVE_DB=:memory:`) must not be silently
    /// ignored just because `[[backends]]` declares real sqlite backends. Passing
    /// `Some(":memory:")` as `cli_db_override` must force every declared backend
    /// in-memory for this invocation, and the declared sqlite paths must never be
    /// created on disk.
    #[tokio::test]
    #[serial]
    async fn memory_override_forces_all_backends_in_memory_and_never_creates_sqlite_file() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main_should_never_be_created.db");
        let secondary_path = dir.path().join("secondary_should_never_be_created.db");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path.clone()),
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

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, Some(":memory:")).await;
        if let Err(ref e) = result {
            panic!(
                "--db :memory: override must force both declared sqlite backends \
                 in-memory and boot successfully; got: {e}"
            );
        }

        assert!(
            !main_path.exists(),
            "main backend's declared sqlite path must never be created on disk when \
             --db :memory: overrides it; found file at {main_path:?}"
        );
        assert!(
            !secondary_path.exists(),
            "secondary backend's declared sqlite path must never be created on disk \
             when --db :memory: overrides it; found file at {secondary_path:?}"
        );
    }

    fn sqlite_multi_backend_config(main_path: PathBuf, secondary_path: PathBuf) -> KhiveConfig {
        use khive_runtime::PackConfig;

        KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut packs = std::collections::HashMap::new();
                packs.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "secondary".to_string(),
                    },
                );
                packs
            },
            ..KhiveConfig::default()
        }
    }

    #[tokio::test]
    #[serial]
    async fn concrete_db_override_matching_declared_main_backend_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.db");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let declared_main_path = nested.join("..").join("main.db");
        assert_ne!(declared_main_path, main_path);
        let secondary_path = dir.path().join("secondary.db");
        let khive_cfg = sqlite_multi_backend_config(declared_main_path, secondary_path);
        let override_value = main_path.to_str().unwrap();
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some(override_value)),
            ..base_runtime_config_for_multi_backend()
        };

        let result =
            build_registry_for_multi_backend(base_cfg, &khive_cfg, Some(override_value)).await;

        if let Err(error) = result {
            panic!(
                "a concrete database override resolving to the declared main backend must be accepted: {error}"
            );
        }
    }

    #[test]
    #[serial]
    fn concrete_db_override_diverging_from_declared_main_backend_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main.db");
        let secondary_path = dir.path().join("secondary.db");
        let override_path = dir.path().join("override.db");
        let khive_cfg = sqlite_multi_backend_config(main_path, secondary_path);
        let override_value = override_path.to_str().unwrap();
        let config_path = dir.path().join("config.toml");

        let error = validate_db_override_against_backends_with_source(
            Some(override_value),
            &khive_cfg.backends,
            Some(&config_path),
        )
        .expect_err("a divergent concrete database override must remain ambiguous");

        assert!(error
            .to_string()
            .contains(&config_path.display().to_string()));
        let envelope = db_override_refusal_envelope(&error)
            .expect("database override conflict must carry a stable refusal envelope");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["invocation"]["started"], false);
        assert_eq!(envelope["error"]["code"], DB_OVERRIDE_CONFLICT_CODE);
        assert_eq!(envelope["error"]["db_override"], override_value);
        assert_eq!(envelope["error"]["declared_backends"], 2);
        assert_eq!(
            envelope["error"]["config_path"],
            config_path.display().to_string()
        );

        let msg = error.to_string();
        assert!(
            msg.contains("./khive.toml")
                && msg.contains("<db-dir>/config.toml")
                && msg.contains("~/.khive/config.toml"),
            "remedy must name every searched config filename; got: {msg}"
        );
        assert!(
            msg.contains("--config <file>") && msg.contains("KHIVE_CONFIG"),
            "remedy must name the --config/KHIVE_CONFIG escape; got: {msg}"
        );
        assert!(
            !msg.contains(":memory:"),
            "remedy must not recommend the discarding :memory: override for ingest-shaped work; got: {msg}"
        );
        assert!(
            !override_path.exists(),
            "rejecting an override must not create its database path"
        );
    }

    /// An intermediate error carrier (a `std::error::Error` whose `source()`
    /// is the typed conflict) one level deep must still yield the refusal
    /// envelope: the top-level `downcast_ref` sees only the carrier itself,
    /// so the lookup walks the error's source chain.
    #[test]
    fn refusal_envelope_survives_one_level_of_error_wrapping() {
        /// One-level carrier: its `source()` IS the typed conflict.
        #[derive(Debug)]
        struct FrameCarrier(DatabaseOverrideConflict);

        impl std::fmt::Display for FrameCarrier {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "daemon spawn refused the frame")
            }
        }

        impl std::error::Error for FrameCarrier {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = anyhow::Error::from(FrameCarrier(DatabaseOverrideConflict::new(
            "/tmp/other.db",
            2,
            None,
        )));

        assert!(
            wrapped.downcast_ref::<DatabaseOverrideConflict>().is_none(),
            "precondition: the top-level downcast must NOT see through an intermediate carrier"
        );
        let envelope = db_override_refusal_envelope(&wrapped)
            .expect("the refusal envelope must survive a one-level error wrapper");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["invocation"]["started"], false);
        assert_eq!(envelope["error"]["code"], DB_OVERRIDE_CONFLICT_CODE);
        assert_eq!(envelope["error"]["db_override"], "/tmp/other.db");
        assert_eq!(envelope["error"]["declared_backends"], 2);
    }

    /// The conflict guard must not fire when no backends are declared at all:
    /// there is nothing for a concrete override to collapse, and the
    /// main-backend lookup finds nothing — without the helper's own
    /// empty-backends guard, EVERY single-backend override would be
    /// misclassified as ambiguous.
    #[test]
    fn reject_conflicting_db_override_accepts_any_concrete_override_with_no_backends() {
        reject_conflicting_db_override_with_source(
            Some("/tmp/single-backend-override.db"),
            &[],
            None,
        )
        .expect("no declared backends means no possible conflict");
    }

    /// With no declared backends a concrete override names the database
    /// directly, so normalization must PRESERVE the override-derived anchor
    /// rather than collapsing it to the no-override anchor — the rewrite is
    /// only proven sound against a declared `main` backend. Break the
    /// `!backends.is_empty()` guard in
    /// `normalize_redundant_db_override_with_source` and this test fails.
    #[test]
    fn normalize_preserves_concrete_override_with_no_backends() {
        let override_value = "/tmp/single-backend-override.db";
        let mut config = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some(override_value)),
            ..RuntimeConfig::default()
        };
        let anchor_before = config.db_path.clone();
        assert_ne!(
            anchor_before,
            khive_runtime::resolve_db_anchor(None),
            "fixture must start from an override-derived anchor distinct from the default"
        );

        let force_memory = normalize_redundant_db_override_with_source(
            &mut config,
            Some(override_value),
            &[],
            None,
        )
        .expect("a concrete override with no declared backends must be accepted");

        assert!(!force_memory, "a concrete override never forces :memory:");
        assert_eq!(
            config.db_path, anchor_before,
            "override-derived anchor must survive normalization when no backends are declared"
        );
    }

    /// A declared `main` backend that is a symlink whose target does not
    /// exist yet (first-open alias, e.g. `link.db -> target.db` written by a
    /// provisioning step before khive has ever opened the database) must
    /// still accept a `--db` override naming the symlink's target directly —
    /// SQLite opens both spellings as the same file. The equivalence check
    /// used to call `Path::exists()` first, which follows symlinks and
    /// reports `false` for a dangling one, so it never resolved the link and
    /// compared the literal alias path against the literal target path
    /// instead, rejecting a legitimate no-op override as ambiguous.
    #[tokio::test]
    #[serial]
    #[cfg(unix)]
    async fn concrete_db_override_matching_declared_main_backend_via_dangling_symlink_is_accepted()
    {
        let dir = tempfile::tempdir().unwrap();
        let target_path = dir.path().join("target.db");
        let link_path = dir.path().join("link.db");
        std::os::unix::fs::symlink("target.db", &link_path).unwrap();
        assert!(
            !target_path.exists(),
            "target.db must not exist yet — this is the first-open case"
        );
        let secondary_path = dir.path().join("secondary.db");
        let khive_cfg = sqlite_multi_backend_config(link_path, secondary_path);
        let override_value = target_path.to_str().unwrap();
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some(override_value)),
            ..base_runtime_config_for_multi_backend()
        };

        let result =
            build_registry_for_multi_backend(base_cfg, &khive_cfg, Some(override_value)).await;

        if let Err(error) = result {
            panic!(
                "a --db override naming the file a dangling symlink's declared main \
                 backend points at must be accepted as the same database: {error}"
            );
        }
    }

    /// The no-side-effects equivalence check used to give up after checking
    /// only the immediate parent directory: if that parent did not exist yet,
    /// it returned the literal (non-canonicalized) absolute path instead of
    /// continuing up to find and resolve a symlinked ancestor further up.
    /// Reaching the same not-yet-created file through a symlinked directory
    /// two levels up and through the real directory must canonicalize to the
    /// identical path.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn canonical_path_no_side_effects_resolves_symlinked_ancestor_before_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let linked_dir = dir.path().join("linked");
        std::os::unix::fs::symlink(&real_dir, &linked_dir).unwrap();

        let via_symlink = linked_dir.join("sub").join("main.db");
        let via_real = real_dir.join("sub").join("main.db");
        assert!(!via_symlink.parent().unwrap().exists());
        assert!(!via_real.parent().unwrap().exists());

        let resolved_via_symlink = canonical_path_no_side_effects(&via_symlink).unwrap();
        let resolved_via_real = canonical_path_no_side_effects(&via_real).unwrap();

        assert_eq!(
            resolved_via_symlink, resolved_via_real,
            "a symlinked ancestor directory two levels above a not-yet-created file \
             must canonicalize to the same target as the real directory: {resolved_via_symlink:?} \
             vs {resolved_via_real:?}"
        );
    }

    /// A `..` inside the not-yet-created tail used to be dropped (recorded as
    /// an empty component), so `missing/../main.db` resolved under `missing/`
    /// instead of collapsing back to the base directory — falsely conflicting
    /// with a plain `main.db` override naming the same file. The missing tail
    /// must collapse `.`/`..` lexically, which is safe precisely because a
    /// nonexistent component cannot be a symlink.
    #[test]
    #[serial]
    fn canonical_path_no_side_effects_collapses_parent_refs_in_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let via_parent_ref = dir.path().join("missing").join("..").join("main.db");
        let direct = dir.path().join("main.db");
        assert!(!dir.path().join("missing").exists());

        let resolved_via_parent_ref = canonical_path_no_side_effects(&via_parent_ref).unwrap();
        let resolved_direct = canonical_path_no_side_effects(&direct).unwrap();

        assert_eq!(
            resolved_via_parent_ref, resolved_direct,
            "a parent-directory reference through a not-yet-created directory must \
             collapse to the same canonical path as the direct spelling"
        );
        assert!(
            !resolved_via_parent_ref
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "the canonical form must not retain `..` components: {resolved_via_parent_ref:?}"
        );
    }

    /// A DANGLING symlink in the middle of the path must fail loud rather
    /// than participate in the lexical `..` collapse: `missing/..` through a
    /// symlink resolves relative to the link's TARGET, so treating it as a
    /// plain nonexistent directory would silently equate two different files.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn canonical_path_no_side_effects_rejects_dangling_ancestor_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("missing");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();

        let via_dangling = dangling.join("..").join("main.db");
        let result = canonical_path_no_side_effects(&via_dangling);

        assert!(
            result.is_err(),
            "a dangling ancestor symlink must fail loud, not collapse lexically: \
             {result:?}"
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("dangling symbolic link"),
            "the error must name the dangling link: {message}"
        );
    }

    /// A symlink pointing at itself used to recurse in
    /// `canonical_path_no_side_effects` until the stack overflowed. The
    /// bounded hop count must instead return an error naming the path.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn canonical_path_no_side_effects_rejects_self_referential_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link_path = dir.path().join("link.db");
        std::os::unix::fs::symlink("link.db", &link_path).unwrap();

        let result = canonical_path_no_side_effects(&link_path);

        assert!(
            result.is_err(),
            "a self-referential symlink must fail loud, not hang or crash"
        );
    }

    /// The two-hop variant of the self-loop above: `a -> b -> a`. Must also
    /// fail loud rather than recursing indefinitely.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn canonical_path_no_side_effects_rejects_two_link_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let link_a = dir.path().join("a.db");
        let link_b = dir.path().join("b.db");
        std::os::unix::fs::symlink("b.db", &link_a).unwrap();
        std::os::unix::fs::symlink("a.db", &link_b).unwrap();

        let result = canonical_path_no_side_effects(&link_a);

        assert!(
            result.is_err(),
            "a two-link symlink cycle must fail loud, not hang or crash"
        );
    }

    /// Issue #553: a concrete `--db` path override combined with declared
    /// `[[backends]]` is ambiguous (which of N declared backends should it apply
    /// to?) and must fail loud with a selectable-config remedy rather than
    /// silently collapsing distinct backends onto one path.
    #[tokio::test]
    #[serial]
    async fn concrete_db_override_with_backends_declared_is_rejected() {
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

        // `db_path` matches the concrete override passed below (the db-anchor
        // consistency guard requires this pairing) — the ambiguity rejection
        // this test exercises is a downstream check inside
        // `build_registry_for_multi_backend`, distinct from anchor drift.
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some("/tmp/some-explicit-override.db")),
            ..base_runtime_config_for_multi_backend()
        };

        let result = build_registry_for_multi_backend(
            base_cfg,
            &khive_cfg,
            Some("/tmp/some-explicit-override.db"),
        )
        .await;
        assert!(
            result.is_err(),
            "a concrete --db path override combined with declared [[backends]] must \
             be rejected as ambiguous"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("./khive.toml")
                    && msg.contains("<db-dir>/config.toml")
                    && msg.contains("~/.khive/config.toml"),
                "remedy must name every searched config filename; got: {msg}"
            );
            assert!(
                msg.contains("--config <file>") && msg.contains("KHIVE_CONFIG"),
                "remedy must name the --config/KHIVE_CONFIG escape; got: {msg}"
            );
        }
    }

    /// Regression: the multi-backend boot path
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

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                let resp = server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: None,
                        presentation_per_op: None,
                        save_to: None,
                        format: None,
                        format_per_op: None,
                        request_id: None,
                    })
                    .await
                    .expect("dispatch must not error");
                serde_json::from_str::<serde_json::Value>(&resp).expect("valid JSON")
            }
        };

        // One message to a different actor, one explicit message to ourselves.
        let to_a = dispatch(r#"comm.send(to="actor-a", content="for-a")"#.to_string()).await;
        assert_eq!(to_a["results"][0]["ok"].as_bool(), Some(true), "{to_a}");
        let to_b =
            dispatch(r#"comm.send(to="actor-b", content="for-b", self_send=true)"#.to_string())
                .await;
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
            "actor-b must NOT see the message addressed to actor-a (leak #75); \
             got {contents:?} — actor identity was not threaded into the multi-backend registry"
        );
    }

    /// Negative test: `[[backends]]` is declared but there is no entry named
    /// `"main"`. `build_server_multi_backend` must return an error whose
    /// message mentions `"main"` so operators know what to fix.
    #[tokio::test]
    #[serial]
    async fn multi_backend_missing_main_returns_error_mentioning_main() {
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

        let result = build_server_multi_backend(base_cfg, &khive_cfg, None).await;
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

    /// Regression for MCP-AUD-001 / #419: a pack explicitly configured to a
    /// backend that has no matching `[[backends]]` entry must fail closed
    /// instead of silently falling back to `main`. `build_registry_for_multi_backend`
    /// must return an `Err` mentioning the pack, the requested backend, and the
    /// defined backends.
    #[tokio::test]
    #[serial]
    async fn multi_backend_registry_rejects_undefined_pack_backend() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "archive".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_registry_for_multi_backend(base_cfg, &khive_cfg, None).await;
        assert!(
            result.is_err(),
            "an undeclared configured pack backend must be a startup error, not a silent \
             fallback to main"
        );
        // MultiBackendRegistry does not implement Debug, so expect_err/unwrap_err are
        // unavailable; extract the error via match instead (same pattern as
        // multi_backend_missing_main_returns_error_mentioning_main above).
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("packs.comm"),
                "error must name the pack; got: {msg}"
            );
            assert!(
                msg.contains("archive"),
                "error must name the undeclared backend; got: {msg}"
            );
            assert!(
                msg.contains("main"),
                "error must list the defined backends; got: {msg}"
            );
        }
    }

    /// Same regression as `multi_backend_registry_rejects_undefined_pack_backend`
    /// but through the `build_server_multi_backend` public builder, which has its
    /// own independent per-pack backend resolution loop.
    #[tokio::test]
    #[serial]
    async fn multi_backend_server_rejects_undefined_pack_backend() {
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "archive".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let result = build_server_multi_backend(base_cfg, &khive_cfg, None).await;
        assert!(
            result.is_err(),
            "an undeclared configured pack backend must be a startup error, not a silent \
             fallback to main"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("packs.comm"),
                "error must name the pack; got: {msg}"
            );
            assert!(
                msg.contains("archive"),
                "error must name the undeclared backend; got: {msg}"
            );
            assert!(
                msg.contains("main"),
                "error must list the defined backends; got: {msg}"
            );
        }
    }

    /// B-SHOULD-FIX-1 (SAFETY): A backend opened with `read_only = true` must
    /// reject write operations. Verified by opening the file backend read-only and
    /// confirming that writing through `apply_pack_ddl_statements` errors (the
    /// writer has PRAGMA query_only = ON).
    #[test]
    fn read_only_backend_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ro_test.db");

        // Create a writable backend first so the file exists.
        let rw = StorageBackend::sqlite(&db_path).expect("rw backend");
        rw.apply_pack_ddl_statements(&[
            "CREATE TABLE IF NOT EXISTS ro_check (id INTEGER PRIMARY KEY)",
        ])
        .expect("DDL on rw backend");
        drop(rw);
        #[cfg(unix)]
        freeze_snapshot_sidecars(&db_path);

        // Re-open read-only and confirm writes fail.
        let ro = StorageBackend::sqlite_read_only(&db_path).expect("ro backend");
        let result = ro.apply_pack_ddl_statements(&["INSERT INTO ro_check (id) VALUES (1)"]);
        assert!(
            result.is_err(),
            "write to a read-only backend must fail; got Ok(())"
        );
        assert!(
            checkpoint_pool_for(&ro).is_none(),
            "read-only backends must not drive checkpoint or WAL sweep writers"
        );
    }

    #[test]
    fn read_only_backend_open_does_not_create_missing_parent_or_database() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("missing-parent");
        let db_path = parent.join("missing.db");
        let config = BackendConfig {
            name: "archive".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(db_path.clone()),
            cache_mb: None,
            journal_mode: None,
            read_only: true,
        };

        let canonical = canonical_backend_path(&config)
            .expect("read-only identity resolution must be lexical and side-effect free");
        assert!(canonical.is_some());
        assert!(
            !parent.exists(),
            "canonical backend identity must not create a missing read-only parent"
        );

        let error = match open_backend(&config) {
            Ok(_) => panic!("missing read-only snapshot must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("read-only open"),
            "error must identify the read-only open: {error}"
        );
        assert!(!parent.exists(), "read-only boot must not create parents");
        assert!(
            !db_path.exists(),
            "read-only boot must not create a database"
        );
    }

    #[cfg(unix)]
    #[test]
    fn multi_backend_requires_read_only_mode_to_be_declared_explicitly() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("chmod_snapshot.db");
        drop(StorageBackend::sqlite(&db_path).expect("create snapshot source"));

        let mut permissions = std::fs::metadata(&db_path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&db_path, permissions).unwrap();
        freeze_snapshot_sidecars(&db_path);

        let config = BackendConfig {
            name: "archive".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(db_path),
            cache_mb: None,
            journal_mode: None,
            read_only: false,
        };
        let error = match open_backend(&config) {
            Ok(_) => panic!("an undeclared multi-backend storage-mode change must fail closed"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("read_only = true"), "{message}");
        assert!(message.contains("config identity"), "{message}");
    }

    #[tokio::test]
    #[serial]
    async fn multi_backend_read_only_construction_and_pack_schema_paths_acquire_no_writer() {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main-snapshot.db");
        let comm_path = dir.path().join("comm-snapshot.db");
        let config_for = |read_only: bool| KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only,
                },
                BackendConfig {
                    name: "comm-store".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(comm_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only,
                },
            ],
            packs: HashMap::from([(
                "comm".to_string(),
                PackConfig {
                    backend: "comm-store".to_string(),
                },
            )]),
            ..KhiveConfig::default()
        };

        let writable = build_registry_for_multi_backend_inner(
            base_runtime_config_for_multi_backend(),
            &config_for(false),
            None,
        )
        .await
        .expect("prepare exact-current core and pack schemas");
        let mut writer_joins = Vec::new();
        if let Some(join) = writable
            .default_runtime
            .backend()
            .pool()
            .take_writer_task_join()
        {
            writer_joins.push(join);
        }
        for runtime in writable.per_pack_runtimes.values() {
            if let Some(join) = runtime.backend().pool().take_writer_task_join() {
                writer_joins.push(join);
            }
        }
        drop(writable);
        for join in writer_joins {
            tokio::time::timeout(std::time::Duration::from_secs(5), join)
                .await
                .expect("writer task must settle before freezing the snapshot")
                .expect("writer task exits cleanly");
        }

        for path in [&main_path, &comm_path] {
            let conn = rusqlite::Connection::open(path).unwrap();
            let mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "delete");
        }

        let snapshot = build_registry_for_multi_backend_inner(
            base_runtime_config_for_multi_backend(),
            &config_for(true),
            None,
        )
        .await
        .expect("open both declared backends for inspection");

        assert_eq!(
            snapshot
                .default_runtime
                .backend()
                .pool()
                .writer_acquisition_snapshot(),
            khive_db::pool::WriterAcquisitionSnapshot::default(),
            "main snapshot construction, exact-ledger validation, and kg pack boot must stay \
             writer-free"
        );
        assert!(
            snapshot.default_runtime.backend().pool().max_readers() > 0,
            "the main rollback-journal snapshot must retain a dedicated reader pool"
        );
        for (pack, runtime) in &snapshot.per_pack_runtimes {
            assert_eq!(
                runtime.backend().pool().writer_acquisition_snapshot(),
                khive_db::pool::WriterAcquisitionSnapshot::default(),
                "pack {pack:?} snapshot boot must keep its construction-inclusive writer \
                 baseline at zero"
            );
            assert!(
                runtime.backend().pool().max_readers() > 0,
                "pack {pack:?} must read its rollback-journal snapshot through a dedicated \
                 reader"
            );
        }
    }

    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    #[tokio::test]
    #[serial]
    async fn mixed_topology_channel_admission_follows_the_runtime_that_backs_each_loop() {
        use clap::Parser;
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main-channel.db");
        let comm_path = dir.path().join("comm-channel.db");
        let config_for = |main_read_only: bool, comm_read_only: bool| KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: main_read_only,
                },
                BackendConfig {
                    name: "comm-store".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(comm_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: comm_read_only,
                },
            ],
            packs: HashMap::from([(
                "comm".to_string(),
                PackConfig {
                    backend: "comm-store".to_string(),
                },
            )]),
            ..KhiveConfig::default()
        };

        let seeded = build_registry_for_multi_backend_inner(
            base_runtime_config_for_multi_backend(),
            &config_for(false, false),
            None,
        )
        .await
        .expect("seed exact-current snapshots");
        drop(seeded);
        #[cfg(unix)]
        freeze_snapshot_sidecars(&comm_path);
        let daemon = Args::parse_from(["mcp", "--daemon"]);

        let comm_snapshot = build_registry_for_multi_backend_inner(
            base_runtime_config_for_multi_backend(),
            &config_for(false, true),
            None,
        )
        .await
        .expect("writable kg plus read-only comm topology");
        let server =
            build_server_from_multi_backend_registry(comm_snapshot, &config_for(false, true), None);
        let admission = channel_loop_plan(&server, &daemon);
        assert!(
            !admission.inbound_poll,
            "comm.ingest/cursor/heartbeat are backed by the read-only comm runtime"
        );
        assert!(
            admission.outbound_delivery,
            "list/update are backed by the writable kg runtime"
        );
        drop(server);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            freeze_snapshot_sidecars(&main_path);
            // The second arm reopens comm writable, so its sidecars must thaw.
            for suffix in ["-wal", "-shm"] {
                let mut name = comm_path.file_name().expect("db file name").to_os_string();
                name.push(suffix);
                let sidecar = comm_path.parent().expect("db parent dir").join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar)
                        .expect("sidecar metadata")
                        .permissions();
                    permissions.set_mode(0o644);
                    std::fs::set_permissions(&sidecar, permissions).expect("thaw sidecar");
                }
            }
        }

        let kg_snapshot = build_registry_for_multi_backend_inner(
            base_runtime_config_for_multi_backend(),
            &config_for(true, false),
            None,
        )
        .await
        .expect("read-only kg plus writable comm topology");
        let server =
            build_server_from_multi_backend_registry(kg_snapshot, &config_for(true, false), None);
        let admission = channel_loop_plan(&server, &daemon);
        assert!(
            admission.inbound_poll,
            "comm.ingest/cursor/heartbeat are backed by the writable comm runtime"
        );
        assert!(
            !admission.outbound_delivery,
            "a read-only kg runtime cannot durably claim or mark delivery, so no external send \
             task may start"
        );
    }

    /// RAII guard: redirects `HOME` and restores the prior value on drop.
    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn redirect_to(dir: &std::path::Path) -> Self {
            let original = std::env::var_os("HOME");
            std::env::set_var("HOME", dir);
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn duplicate_sqlite_path_config(db_path: &std::path::Path) -> KhiveConfig {
        use khive_runtime::PackConfig;

        KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.to_path_buf()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(db_path.to_path_buf()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut packs = std::collections::HashMap::new();
                packs.insert(
                    "comm".to_string(),
                    PackConfig {
                        backend: "alias".to_string(),
                    },
                );
                packs
            },
            ..KhiveConfig::default()
        }
    }

    #[tokio::test]
    async fn targeted_secondary_rejects_conflicting_physical_alias_modes_before_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let aliased = dir.path().join("must-not-be-created.db");
        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "archive-ro".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(aliased.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: true,
                },
                BackendConfig {
                    name: "archive-rw".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(aliased.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            ..KhiveConfig::default()
        };

        let error = migrate_configured_storage_topology(
            base_runtime_config_for_multi_backend(),
            &khive_cfg,
            None,
            Some("archive-rw"),
        )
        .await
        .expect_err("targeted migration must apply full-topology alias validation");
        assert!(error.to_string().contains("same access mode"), "{error:#}");
        assert!(
            !aliased.exists(),
            "alias validation must fail before opening or creating the selected database"
        );
    }

    #[tokio::test]
    async fn main_alias_target_marks_physical_target_names_not_prerequisite() {
        let dir = tempfile::tempdir().expect("temp dir");
        let main = dir.path().join("main.db");
        let secondary = dir.path().join("secondary.db");
        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: BackendId::MAIN.to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "main-alias".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            ..KhiveConfig::default()
        };

        let statuses = migrate_configured_storage_topology(
            base_runtime_config_for_multi_backend(),
            &khive_cfg,
            None,
            Some("main-alias"),
        )
        .await
        .expect("an alias of main must run the full prerequisite topology");
        let status = |name: &str| {
            statuses
                .iter()
                .find(|status| status.backend == name)
                .unwrap_or_else(|| panic!("missing status for {name}"))
        };
        assert!(!status(BackendId::MAIN).prerequisite);
        assert!(!status("main-alias").prerequisite);
        assert!(status("secondary").prerequisite);
    }

    #[test]
    fn force_memory_makes_a_file_alias_target_independent_from_main() {
        let dir = tempfile::tempdir().expect("temp dir");
        let khive_cfg = duplicate_sqlite_path_config(&dir.path().join("unused.db"));
        assert_eq!(
            configured_storage_check_targets(&khive_cfg, Some(":memory:"), Some("alias"))
                .expect("forced-memory target plan"),
            vec!["alias".to_string()],
            "forced-memory configured names are distinct ephemeral databases"
        );
    }

    fn memory_main_backend_config() -> KhiveConfig {
        KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Memory,
                path: None,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        }
    }

    fn assert_db_anchor_drift<T>(result: anyhow::Result<T>) {
        match result {
            Err(error) => assert!(
                error.to_string().contains("db-path resolution drift"),
                "legacy builder must reject raw db input that disagrees with the resolved config: {error}"
            ),
            Ok(_) => panic!("legacy builder accepted raw db input that disagrees with the resolved config"),
        }
    }

    #[tokio::test]
    async fn legacy_registry_rejects_mismatched_explicit_db_override() {
        let base_cfg = RuntimeConfig {
            db_path: Some(PathBuf::from("/tmp/khive-resolved.db")),
            ..base_runtime_config_for_multi_backend()
        };

        assert_db_anchor_drift(
            build_registry_for_multi_backend(
                base_cfg,
                &memory_main_backend_config(),
                Some("/tmp/khive-raw.db"),
            )
            .await,
        );
    }

    #[tokio::test]
    async fn legacy_server_rejects_mismatched_explicit_db_override() {
        let base_cfg = RuntimeConfig {
            db_path: Some(PathBuf::from("/tmp/khive-resolved.db")),
            ..base_runtime_config_for_multi_backend()
        };

        assert_db_anchor_drift(
            build_server_multi_backend(
                base_cfg,
                &memory_main_backend_config(),
                Some("/tmp/khive-raw.db"),
            )
            .await,
        );
    }

    #[tokio::test]
    #[serial]
    async fn legacy_registry_rejects_unset_db_after_home_changes() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let base_cfg = base_runtime_config_for_multi_backend();
        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());

        assert_db_anchor_drift(
            build_registry_for_multi_backend(base_cfg, &memory_main_backend_config(), None).await,
        );
    }

    #[tokio::test]
    #[serial]
    async fn legacy_server_rejects_unset_db_after_home_changes() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let base_cfg = base_runtime_config_for_multi_backend();
        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());

        assert_db_anchor_drift(
            build_server_multi_backend(base_cfg, &memory_main_backend_config(), None).await,
        );
    }

    /// B-SHOULD-FIX-2 (data safety): Two [[backends]] entries whose sqlite paths
    /// canonicalize to the same file must share a single Arc<StorageBackend> and
    /// run migrations only once. Verified by using two names that differ only by
    /// `./` prefix while pointing at the same absolute path.
    #[tokio::test]
    #[serial]
    async fn duplicate_sqlite_paths_deduplicated_to_single_backend() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared.db");
        let khive_cfg = duplicate_sqlite_path_config(&db_path);

        let base_cfg = base_runtime_config_for_multi_backend();

        // Must boot successfully (dedup prevents double-migration / SQLITE_BUSY).
        let result = build_server_multi_backend(base_cfg, &khive_cfg, None).await;
        if let Err(ref e) = result {
            panic!(
                "two backends with the same canonical path must share one Arc and boot ok; got: {e}"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn duplicate_sqlite_aliases_reject_conflicting_read_only_modes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("shared-mode.db");
        let mut khive_cfg = duplicate_sqlite_path_config(&db_path);
        assert!(khive_cfg.backends.len() >= 2);
        khive_cfg.backends[1].read_only = true;

        let error = match build_server_multi_backend(
            base_runtime_config_for_multi_backend(),
            &khive_cfg,
            None,
        )
        .await
        {
            Ok(_) => panic!("one physical database cannot be both writable and read-only"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("same physical database"), "{message}");
        assert!(message.contains("read_only"), "{message}");
    }

    /// Regression for #720: changing `HOME` after runtime-config resolution but
    /// before multi-backend registry construction must not change the database
    /// anchor used by the consistency guard.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_uses_anchor_captured_by_runtime_config() {
        let first_home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::redirect_to(first_home.path());
        let config_path = first_home.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write empty config");
        let (base_cfg, db_anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
            db: None,
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("namespace"),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve runtime config before HOME changes");

        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("shared.db");
        let khive_cfg = duplicate_sqlite_path_config(&db_path);

        let second_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", second_home.path());
        let result = build_server_multi_backend_with_db_anchor(
            base_cfg,
            &khive_cfg,
            None,
            db_anchor.as_deref(),
        )
        .await;
        if let Err(error) = result {
            panic!(
                "multi-backend construction must retain the anchor captured by \
                 resolve_runtime_config instead of re-reading HOME: {error}"
            );
        }
    }

    /// Issue #553 sibling gap: `build_server_multi_backend` is reachable from
    /// `build_server` -> `main.rs` whenever `[[backends]]` is non-empty (e.g.
    /// exactly one declared backend, which still routes through `build_server`'s
    /// "single-backend, zero-change path" in main.rs since that dispatch only
    /// checks `backends.len() <= 1`, while `build_server` itself checks
    /// `is_empty()`). Before this fix, `build_server_multi_backend` took no
    /// db-override parameter at all, so `--db :memory:` / `KHIVE_DB=:memory:`
    /// was silently discarded on this path exactly as issue #553 described.
    /// Passing `Some(":memory:")` as `cli_db_override` must force every
    /// declared backend in-memory for this invocation, and the declared sqlite
    /// paths must never be created on disk.
    #[tokio::test]
    #[serial]
    async fn memory_override_forces_all_backends_in_memory_and_never_creates_sqlite_file_via_build_server_multi_backend(
    ) {
        use khive_runtime::PackConfig;

        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("main_should_never_be_created.db");
        let secondary_path = dir.path().join("secondary_should_never_be_created.db");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "secondary".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(secondary_path.clone()),
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

        let result = build_server_multi_backend(base_cfg, &khive_cfg, Some(":memory:")).await;
        if let Err(ref e) = result {
            panic!(
                "--db :memory: override must force both declared sqlite backends \
                 in-memory and boot successfully; got: {e}"
            );
        }

        assert!(
            !main_path.exists(),
            "main backend's declared sqlite path must never be created on disk when \
             --db :memory: overrides it; found file at {main_path:?}"
        );
        assert!(
            !secondary_path.exists(),
            "secondary backend's declared sqlite path must never be created on disk \
             when --db :memory: overrides it; found file at {secondary_path:?}"
        );
    }

    /// Issue #553 sibling gap: a concrete `--db` path override combined with
    /// declared `[[backends]]` is ambiguous (which of N declared backends
    /// should it apply to?) and must fail loud on the `build_server_multi_backend`
    /// path too, with a selectable-config remedy rather than silently
    /// collapsing distinct backends onto one path.
    #[tokio::test]
    #[serial]
    async fn concrete_db_override_with_backends_declared_is_rejected_via_build_server_multi_backend(
    ) {
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

        // `db_path` matches the concrete override passed below (the db-anchor
        // consistency guard requires this pairing) — the ambiguity rejection
        // this test exercises is a downstream check inside
        // `build_registry_for_multi_backend`, distinct from anchor drift.
        let base_cfg = RuntimeConfig {
            db_path: khive_runtime::resolve_db_anchor(Some("/tmp/some-explicit-override.db")),
            ..base_runtime_config_for_multi_backend()
        };

        let result = build_server_multi_backend(
            base_cfg,
            &khive_cfg,
            Some("/tmp/some-explicit-override.db"),
        )
        .await;
        assert!(
            result.is_err(),
            "a concrete --db path override combined with declared [[backends]] must \
             be rejected as ambiguous"
        );
        if let Err(err) = result {
            let msg = err.to_string();
            assert!(
                msg.contains("./khive.toml")
                    && msg.contains("<db-dir>/config.toml")
                    && msg.contains("~/.khive/config.toml"),
                "remedy must name every searched config filename; got: {msg}"
            );
            assert!(
                msg.contains("--config <file>") && msg.contains("KHIVE_CONFIG"),
                "remedy must name the --config/KHIVE_CONFIG escape; got: {msg}"
            );
        }
    }

    // B-SHOULD-FIX-3 collision test lives in khive-runtime/src/pack.rs
    // (apply_schema_plans_with_map_collision_is_an_error) because
    // `VerbRegistryBuilder::register_boxed` is pub(crate) there.

    /// B-SHOULD-FIX-4 (daemon staleness): `compute_config_id` must produce
    /// different ids for two configs that differ only in pack→backend routing.
    /// The empty-backends case must be byte-identical to the pre-change baseline.
    #[test]
    fn config_id_folds_backend_topology_when_non_empty() {
        use khive_runtime::{BackendId, KhiveConfig, Namespace, PackConfig, RuntimeConfig};

        let base_rt = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        };

        // No backends — must be byte-identical to compute_config_id(base_rt, None).
        let id_no_backends = crate::server::compute_config_id(&base_rt, None);
        let id_empty_backends =
            crate::server::compute_config_id(&base_rt, Some(&KhiveConfig::default()));
        assert_eq!(
            id_no_backends, id_empty_backends,
            "empty-backends config_id must be byte-identical to None-config config_id"
        );

        // Two configs differing only in pack→backend assignment.
        let mut packs_a = std::collections::HashMap::new();
        packs_a.insert(
            "comm".to_string(),
            PackConfig {
                backend: "secondary".to_string(),
            },
        );

        let cfg_a = KhiveConfig {
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
            packs: packs_a,
            ..KhiveConfig::default()
        };

        // cfg_b: no pack assignments — comm falls back to main.
        let cfg_b = KhiveConfig {
            backends: cfg_a.backends.clone(),
            packs: std::collections::HashMap::new(),
            ..KhiveConfig::default()
        };

        let id_a = crate::server::compute_config_id(&base_rt, Some(&cfg_a));
        let id_b = crate::server::compute_config_id(&base_rt, Some(&cfg_b));

        assert_ne!(
            id_a, id_b,
            "configs differing only in pack→backend routing must produce different config_ids; \
             both produced: {id_a}"
        );
    }

    /// Physical isolation guard: a record written through a pack pinned to backend B's
    /// SQLite file MUST NOT appear in backend A's file, and vice versa.
    ///
    /// This is the "billing data must not mix with agent memory" guarantee.
    /// The test opens each file independently with rusqlite after the server is
    /// dropped to confirm cross-file absence in both directions.
    ///
    /// Schema facts discovered from crates/khive-db/sql/:
    ///   entities table — column `name` holds the entity name (entities-ddl.sql)
    ///   notes table    — column `content` holds the message body; `kind` = "message"
    ///                    for comm.send output (notes-ddl.sql + comm handlers.rs)
    ///
    /// Relies on `base_runtime_config_for_multi_backend` leaving `embedding_model`
    /// unset: no embedder means no `vec0` virtual table is created, so the plain
    /// `rusqlite::Connection::open` below (which does not load the vec0 extension)
    /// can read both files. If an embedder is ever added to that helper, this test
    /// must load the extension or query through a runtime instead.
    #[tokio::test]
    #[serial]
    async fn multi_backend_isolates_pack_data_to_separate_files() {
        use crate::tools::request::RequestParams;
        use khive_runtime::PackConfig;
        use rusqlite::Connection;

        let dir = tempfile::tempdir().expect("temp dir");
        let main_path = dir.path().join("main.db");
        let second_path = dir.path().join("second.db");

        let khive_cfg = KhiveConfig {
            backends: vec![
                BackendConfig {
                    name: "main".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(main_path.clone()),
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                BackendConfig {
                    name: "second".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(second_path.clone()),
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
                        backend: "second".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = base_runtime_config_for_multi_backend();

        let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
            .await
            .expect("multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: None,
                        presentation_per_op: None,
                        save_to: None,
                        format: None,
                        format_per_op: None,
                        request_id: None,
                    })
                    .await
                    .expect("dispatch must not error")
            }
        };

        // kg → main.db: create an entity
        let kg_resp =
            dispatch(r#"create(kind="concept", name="MainOnlyEntity")"#.to_string()).await;
        let kg_json: serde_json::Value =
            serde_json::from_str(&kg_resp).expect("kg response is valid JSON");
        assert_eq!(
            kg_json["results"][0]["ok"].as_bool(),
            Some(true),
            "kg create must succeed; response: {kg_resp}"
        );

        // comm → second.db: send a message
        let comm_resp =
            dispatch(r#"comm.send(to="local", content="SecondOnlyMsg")"#.to_string()).await;
        let comm_json: serde_json::Value =
            serde_json::from_str(&comm_resp).expect("comm response is valid JSON");
        assert_eq!(
            comm_json["results"][0]["ok"].as_bool(),
            Some(true),
            "comm.send must succeed; response: {comm_resp}"
        );

        // Drop the server so WAL is checkpointed and files are fully flushed
        // before we open them with rusqlite.
        drop(server);

        // --- Verify main.db ---
        let main_conn = Connection::open(&main_path).expect("open main.db");

        let main_entity_count: i64 = main_conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity' AND deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("query entities in main.db");
        assert_eq!(
            main_entity_count, 1,
            "main.db MUST contain MainOnlyEntity (written via kg pack); got count={main_entity_count}"
        );

        let main_msg_count: i64 = main_conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE kind = 'message'",
                [],
                |row| row.get(0),
            )
            .expect("query notes in main.db");
        assert_eq!(
            main_msg_count, 0,
            "main.db MUST NOT contain any message notes (comm is pinned to second.db); \
             got count={main_msg_count}"
        );

        // --- Verify second.db ---
        let second_conn = Connection::open(&second_path).expect("open second.db");

        let second_msg_count: i64 = second_conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE kind = 'message' AND content = 'SecondOnlyMsg'",
                [],
                |row| row.get(0),
            )
            .expect("query notes in second.db");
        assert_eq!(
            second_msg_count, 2,
            "second.db MUST contain SecondOnlyMsg (dual-write: 1 outbound + 1 inbound copy); \
             got count={second_msg_count}"
        );

        let second_entity_count: i64 = second_conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity'",
                [],
                |row| row.get(0),
            )
            .expect("query entities in second.db");
        assert_eq!(
            second_entity_count, 0,
            "second.db MUST NOT contain MainOnlyEntity (kg is pinned to main.db); \
             got count={second_entity_count}"
        );
    }

    /// Tilde divergence regression (blocking security defect): a `~/`-prefixed
    /// `--db`/`KHIVE_DB` override must fingerprint identically to the
    /// equivalent absolute path. Before the fix, `resolve_db_anchor` left a
    /// leading `~` unexpanded in `RuntimeConfig.db_path`, so single-backend
    /// boot opened a literal `./~/...` file while `compute_config_id`
    /// canonicalized (and so expanded) the same raw string separately —
    /// letting two processes that open different files still collide on one
    /// `config_id` and get dispatched to each other's database by the daemon.
    #[test]
    #[serial]
    fn config_id_matches_for_tilde_and_equivalent_absolute_db_override() {
        let original_home = std::env::var_os("HOME");
        let home_dir = tempfile::tempdir().expect("home tempdir");
        std::env::set_var("HOME", home_dir.path());

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tilde_anchor = khive_runtime::resolve_db_anchor(Some("~/data.db"))
                .expect("an explicit path always anchors");
            let expected = home_dir.path().join("data.db");
            assert_eq!(
                tilde_anchor, expected,
                "resolve_db_anchor must expand a leading ~ before compute_config_id ever \
                 sees it"
            );

            let absolute_anchor = khive_runtime::resolve_db_anchor(Some(
                expected.to_str().expect("utf8 tempdir path"),
            ))
            .expect("an explicit path always anchors");
            assert_eq!(tilde_anchor, absolute_anchor);

            let make_config = |db_path: std::path::PathBuf| RuntimeConfig {
                db_path: Some(db_path),
                default_namespace: khive_runtime::Namespace::local(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::no_embeddings()
            };

            let tilde_cfg = make_config(tilde_anchor);
            let abs_cfg = make_config(absolute_anchor);

            let id_tilde = crate::server::compute_config_id(&tilde_cfg, None);
            let id_abs = crate::server::compute_config_id(&abs_cfg, None);
            assert_eq!(
                id_tilde, id_abs,
                "a config resolved from a ~-prefixed override must fingerprint identically \
                 to one resolved from the equivalent absolute path"
            );
        }));

        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        outcome.expect("test body panicked");
    }

    // --- ingest_namespace_from_env (Fix 4: namespace env var) ---

    #[cfg(feature = "channel-email")]
    mod ingest_ns_tests {
        use super::*;

        #[test]
        #[serial]
        fn ingest_namespace_defaults_to_local() {
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ingest_namespace_from_env(), "local");
        }

        #[test]
        #[serial]
        fn ingest_namespace_reads_env_var() {
            std::env::set_var("KHIVE_EMAIL_INGEST_NAMESPACE", "lambda:mybot");
            let ns = ingest_namespace_from_env();
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ns, "lambda:mybot");
        }

        #[test]
        #[serial]
        fn ingest_namespace_ignores_blank_env_var() {
            std::env::set_var("KHIVE_EMAIL_INGEST_NAMESPACE", "  ");
            let ns = ingest_namespace_from_env();
            std::env::remove_var("KHIVE_EMAIL_INGEST_NAMESPACE");
            assert_eq!(ns, "local", "blank env var must fall back to default");
        }

        #[test]
        fn preflight_fails_on_invalid_namespace_string() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build empty registry");
            // An empty string is not a valid namespace; parse must fail.
            assert!(
                !preflight_ingest_namespace("", &registry),
                "preflight must return false for an invalid namespace string"
            );
        }

        #[test]
        fn preflight_fails_when_gate_denies_namespace() {
            use khive_runtime::{Gate, GateDecision, GateError, GateRequest};
            use std::fmt;

            #[derive(Debug)]
            struct AlwaysDenyGate;
            impl fmt::Display for AlwaysDenyGate {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "AlwaysDenyGate")
                }
            }
            impl Gate for AlwaysDenyGate {
                fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                    Ok(GateDecision::deny("test: always deny"))
                }
            }

            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_gate(std::sync::Arc::new(AlwaysDenyGate));
            let registry = builder.build().expect("build registry with deny gate");
            assert!(
                !preflight_ingest_namespace("local", &registry),
                "preflight must return false when the gate denies the namespace"
            );
        }

        #[test]
        fn preflight_succeeds_with_allow_gate_and_valid_namespace() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build registry with default allow-all gate");
            assert!(
                preflight_ingest_namespace("local", &registry),
                "preflight must return true for a valid namespace when the gate allows"
            );
        }

        // --- spawn-seam tests: verify the loop is NOT started on preflight failure ---

        #[test]
        fn spawn_not_called_when_gate_denies() {
            use khive_runtime::{Gate, GateDecision, GateError, GateRequest};
            use std::fmt;

            #[derive(Debug)]
            struct AlwaysDenyGate2;
            impl fmt::Display for AlwaysDenyGate2 {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "AlwaysDenyGate2")
                }
            }
            impl Gate for AlwaysDenyGate2 {
                fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                    Ok(GateDecision::deny("spawn seam test: always deny"))
                }
            }

            let mut builder = khive_runtime::VerbRegistryBuilder::new();
            builder.with_gate(std::sync::Arc::new(AlwaysDenyGate2));
            let registry = builder.build().expect("build registry with deny gate");

            let mut spawn_count = 0usize;
            let authorized = run_if_authorized("local", &registry, || {
                spawn_count += 1;
            });

            assert!(
                !authorized,
                "run_if_authorized must return false when gate denies"
            );
            assert_eq!(
                spawn_count, 0,
                "spawn must not be called when preflight fails"
            );
        }

        #[test]
        fn spawn_not_called_when_namespace_invalid() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("build empty registry");

            let mut spawn_count = 0usize;
            let authorized = run_if_authorized("", &registry, || {
                spawn_count += 1;
            });

            assert!(
                !authorized,
                "run_if_authorized must return false for invalid namespace"
            );
            assert_eq!(
                spawn_count, 0,
                "spawn must not be called when namespace is invalid"
            );
        }
    }

    #[cfg(feature = "channel-email")]
    mod channel_ingest_disposition_tests {
        use super::*;
        use khive_runtime::ChannelIngestFailureClass;
        use std::collections::HashMap;

        #[tokio::test]
        async fn quarantine_storage_preflight_rejects_a_missing_blob_pack() {
            let registry = khive_runtime::VerbRegistryBuilder::new()
                .build()
                .expect("empty registry builds");
            let error = ensure_channel_quarantine_storage(&registry)
                .await
                .expect_err("channel polling must fail closed without blob storage");
            assert!(matches!(
                error,
                khive_runtime::RuntimeError::Unconfigured(_)
            ));
        }

        #[test]
        fn unknown_failure_holds_four_attempts_then_quarantines_on_five() {
            let mut attempts = HashMap::new();
            let classification = ChannelIngestFailureClass::Unknown {
                reason: "InvalidInput",
            };

            for expected_attempt in 1..UNKNOWN_INGEST_QUARANTINE_THRESHOLD {
                assert_eq!(
                    channel_ingest_disposition(
                        classification,
                        "email",
                        Some("imap:h:1:7"),
                        &mut attempts,
                    ),
                    ChannelIngestDisposition::Hold {
                        attempt: Some(expected_attempt),
                    }
                );
            }
            assert_eq!(
                channel_ingest_disposition(
                    classification,
                    "email",
                    Some("imap:h:1:7"),
                    &mut attempts,
                ),
                ChannelIngestDisposition::Quarantine,
                "the fifth unknown failure must close the retry livelock"
            );
        }

        #[test]
        fn retryable_never_increments_and_permanent_quarantines_immediately() {
            let mut attempts = HashMap::new();
            assert_eq!(
                channel_ingest_disposition(
                    ChannelIngestFailureClass::Retryable { reason: "Storage" },
                    "email",
                    Some("imap:h:1:8"),
                    &mut attempts,
                ),
                ChannelIngestDisposition::Hold { attempt: None }
            );
            assert!(
                attempts.is_empty(),
                "retryable failures must not consume the unknown budget"
            );

            assert_eq!(
                channel_ingest_disposition(
                    ChannelIngestFailureClass::Permanent {
                        reason: "SecretDetected",
                    },
                    "email",
                    Some("imap:h:1:9"),
                    &mut attempts,
                ),
                ChannelIngestDisposition::Quarantine
            );
            assert!(
                attempts.is_empty(),
                "permanent failures bypass the unknown budget"
            );
        }
    }

    // --- log_eligible_poll_failure: edge-triggered warn ---

    #[cfg(feature = "channel-email")]
    mod eligible_poll_failure_log_tests {
        use super::*;
        use khive_channel::ChannelError;
        use khive_channel_email::BackoffTick;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tracing::field::{Field, Visit};

        #[derive(Clone, Debug, Default)]
        struct CapturedEvent {
            level: Option<tracing::Level>,
            message: Option<String>,
            reason: Option<String>,
        }

        #[derive(Default)]
        struct CapturedEventVisitor {
            message: Option<String>,
            reason: Option<String>,
        }

        impl Visit for CapturedEventVisitor {
            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "message" => self.message = Some(value.to_string()),
                    "reason" => self.reason = Some(value.to_string()),
                    _ => {}
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if matches!(field.name(), "message" | "reason") {
                    let formatted = format!("{value:?}");
                    let value = formatted
                        .trim_start_matches('"')
                        .trim_end_matches('"')
                        .to_string();
                    match field.name() {
                        "message" => self.message = Some(value),
                        "reason" => self.reason = Some(value),
                        _ => {}
                    }
                }
            }
        }

        /// Minimal `tracing::Subscriber` capturing (level, message) pairs into
        /// a thread-local vec, installed via `tracing::subscriber::with_default`.
        /// Mirrors `khive-db/src/checkpoint.rs`'s `CaptureSubscriber` (same
        /// ADR-091 `crossing_warn` test discipline: prove the log fires on
        /// the escalation edge and stays silent on a same-step repeat).
        struct CaptureSubscriber {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = CapturedEventVisitor::default();
                event.record(&mut visitor);
                self.events.lock().unwrap().push(CapturedEvent {
                    level: Some(*event.metadata().level()),
                    message: visitor.message,
                    reason: visitor.reason,
                });
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        fn tick(should_warn: bool, attempt: u32) -> BackoffTick {
            BackoffTick {
                delay: Duration::from_secs(10),
                step: Duration::from_secs(10),
                attempt,
                should_warn,
            }
        }

        #[test]
        fn escalation_edge_logs_warn_not_debug() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Transport("boom".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 1));
            });

            let events = buffer.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "expected exactly one log event, got {events:?}"
            );
            assert_eq!(events[0].level, Some(tracing::Level::WARN));
        }

        #[test]
        fn same_step_repeat_logs_debug_not_warn() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Transport("boom".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(false, 2));
            });

            let events = buffer.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "expected exactly one log event, got {events:?}"
            );
            assert_eq!(events[0].level, Some(tracing::Level::DEBUG));
        }

        #[test]
        fn sustained_capped_pressure_produces_exactly_one_warn() {
            // Simulate one escalation edge followed by several repeats at the
            // same (capped) step, as the poll loop would emit them across
            // consecutive ticks, reproducing the "riding the cap" scenario
            // that previously spammed a WARN per retry.
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Auth("authenticated but not connected".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 8)); // escalation edge
                for attempt in 9..=15 {
                    log_eligible_poll_failure("email", &err, &tick(false, attempt));
                    // repeats at cap
                }
            });

            let events = buffer.lock().unwrap();
            let warn_count = events
                .iter()
                .filter(|e| e.level == Some(tracing::Level::WARN))
                .count();
            let debug_count = events
                .iter()
                .filter(|e| e.level == Some(tracing::Level::DEBUG))
                .count();
            assert_eq!(
                warn_count, 1,
                "exactly one WARN expected across the whole sequence, got {warn_count} in {events:?}"
            );
            assert_eq!(
                debug_count, 7,
                "the 7 same-step repeats must log at debug, not warn"
            );
        }

        #[test]
        fn warn_message_contains_error_text() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };
            let err = ChannelError::Auth("IMAP LOGIN failed: slot exhausted".into());

            tracing::subscriber::with_default(subscriber, || {
                log_eligible_poll_failure("email", &err, &tick(true, 1));
            });

            let events = buffer.lock().unwrap();
            let message = events[0].message.as_deref().unwrap_or_default();
            assert!(
                message.contains("slot exhausted"),
                "escalation warn must carry the underlying error text, got: {message}"
            );
        }

        #[test]
        fn quarantine_warn_names_typed_reason() {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = CaptureSubscriber {
                events: Arc::clone(&buffer),
            };

            tracing::subscriber::with_default(subscriber, || {
                log_channel_quarantine(
                    "email",
                    khive_runtime::ChannelIngestFailureClass::Permanent {
                        reason: "SecretDetected",
                    },
                    "imap:h:11:7",
                );
            });

            let events = buffer.lock().unwrap();
            assert_eq!(events.len(), 1, "quarantine must emit exactly one event");
            assert_eq!(events[0].level, Some(tracing::Level::WARN));
            assert_eq!(
                events[0].reason.as_deref(),
                Some("SecretDetected"),
                "quarantine WARN must name the typed refusal variant"
            );
        }
    }

    // --- should_warn_unattributed predicate ---

    fn packs(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn warn_when_actor_is_none_and_comm_loaded() {
        assert!(should_warn_unattributed(None, &packs(&["kg", "comm"])));
    }

    #[test]
    fn warn_when_actor_is_local_and_comm_loaded() {
        assert!(should_warn_unattributed(
            Some("local"),
            &packs(&["kg", "comm"])
        ));
    }

    #[test]
    fn no_warn_when_actor_is_configured() {
        assert!(!should_warn_unattributed(
            Some("lambda:khive"),
            &packs(&["kg", "comm"])
        ));
    }

    #[test]
    fn no_warn_when_comm_not_loaded() {
        assert!(!should_warn_unattributed(Some("local"), &packs(&["kg"])));
    }

    #[test]
    fn no_warn_when_actor_none_and_no_comm() {
        assert!(!should_warn_unattributed(None, &packs(&["kg", "memory"])));
    }

    // --- is_strict_actor_mode predicate ---
    // All three tests mutate the process-global KHIVE_REQUIRE_ATTRIBUTED_ACTOR;
    // #[serial] prevents races under parallel test execution.

    #[test]
    #[serial]
    fn strict_mode_off_by_default() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        assert!(
            !is_strict_actor_mode(),
            "strict mode must be OFF when KHIVE_REQUIRE_ATTRIBUTED_ACTOR is unset"
        );
        if let Some(v) = prev {
            std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v);
        }
    }

    #[test]
    #[serial]
    fn strict_mode_on_when_env_var_is_1() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        assert!(
            is_strict_actor_mode(),
            "strict mode must be ON when KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1"
        );
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
    }

    #[test]
    #[serial]
    fn strict_mode_off_when_env_var_is_not_1() {
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "0");
        assert!(
            !is_strict_actor_mode(),
            "strict mode must be OFF when KHIVE_REQUIRE_ATTRIBUTED_ACTOR=0"
        );
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
    }

    // --- enforce_strict_actor_mode: shared seam regression tests ---
    // These cover the enforcement seam itself (regression guard).

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_returns_err_when_strict_and_no_actor() {
        // Strict mode ON + no actor + comm pack = Err.
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "comm", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_err(),
            "enforce_strict_actor_mode must return Err when strict mode is ON \
             and no actor is configured (comm pack loaded)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
            "error message must name the env var; got: {msg}"
        );
        assert!(
            msg.contains("KHIVE_ACTOR"),
            "error message must name the remedy; got: {msg}"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_and_actor_configured() {
        // Strict mode ON + proper actor = Ok (comm pack present is irrelevant).
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(Some("lambda:tenant-x"), &packs(&["kg", "comm"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when actor is properly configured"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_off_and_no_actor() {
        // Strict mode OFF + no actor = Ok (the DEFAULT / OSS path must be unchanged).
        // This is the most critical regression guard: ensure the default-off path
        // never fires the guard.
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "comm", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when strict mode is OFF \
             (default OSS path must be completely unchanged)"
        );
    }

    #[test]
    #[serial]
    fn enforce_strict_actor_mode_ok_when_strict_on_but_no_comm_pack() {
        // Strict mode ON but comm pack not loaded = Ok (no risk of party-line).
        let prev = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        let result = enforce_strict_actor_mode(None, &packs(&["kg", "memory"]));
        match prev {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }
        assert!(
            result.is_ok(),
            "enforce_strict_actor_mode must return Ok when comm pack is not loaded \
             (no party-line risk even without actor)"
        );
    }

    // --- build_server's returned schedule-tick runtime (ADR-106, PR #782) ---
    //
    // Before this fix, the daemon-resident tick (`schedule_tick_loop`)
    // reconstructed its OWN `RuntimeConfig::default()` from raw `args.db` and
    // an inferred namespace, discarding everything `build_server` resolves
    // from `--config`/`[[backends]]`/`--actor`/`--pack`. These regressions
    // exercise `build_server` itself (the exact function `run()` calls) and
    // assert the runtime it hands back for the tick to drain against carries
    // the SAME resolved db path, actor identity, and pack set the live
    // server itself was built with — not a silently different one.
    //
    // All use `SeatEnv` (defined above, ADR-096 Fork 2 section) to isolate
    // cwd/HOME so no ambient developer-machine `~/.khive/config.toml` or
    // project `.khive/config.toml` can leak into the resolution, and clear
    // every `KHIVE_*` env var these tests care about so a shell-level export
    // in the test-runner's environment cannot silently change the resolved
    // config out from under the assertion.

    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_uses_the_configured_backend_not_the_home_default() {
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let configured_db = seat_dir.path().join("configured-schedule-backend.db");

        use clap::Parser;
        let args = Args::parse_from(["mcp", "--db", configured_db.to_str().expect("utf8 path")]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("build_server must succeed");
        let rt = schedule_rt
            .expect("the default pack set includes \"schedule\" — a runtime must be returned");

        assert_eq!(
            rt.config().db_path.as_deref(),
            Some(configured_db.as_path()),
            "the tick's runtime must target the exact --db this daemon was configured with, \
             not RuntimeConfig::default()'s $HOME/.khive/khive.db fallback"
        );
    }

    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_uses_the_configured_actor_identity() {
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            ":memory:",
            "--actor",
            "lambda:adr106-tick-actor",
        ]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("build_server must succeed");
        let rt = schedule_rt.expect("schedule pack is loaded by default");

        assert_eq!(
            rt.config().actor_id.as_deref(),
            Some("lambda:adr106-tick-actor"),
            "the tick's runtime must carry the daemon's own resolved --actor identity, \
             not RuntimeConfig::default()'s unattributed actor_id=None"
        );
    }

    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_is_none_when_schedule_pack_is_not_in_the_restricted_pack_set(
    ) {
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        use clap::Parser;
        // Restrict to a pack set that deliberately excludes "schedule".
        let args = Args::parse_from(["mcp", "--db", ":memory:", "--pack", "kg"]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("build_server must succeed");
        assert!(
            schedule_rt.is_none(),
            "when the operator restricts --pack to exclude \"schedule\", the tick must have \
             nothing to drain against — never silently falling back to a runtime that can \
             dispatch through a pack the daemon was not configured to load"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn default_read_only_server_omits_schedule_tick_and_warms_without_a_writer() {
        use std::os::unix::fs::PermissionsExt;

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let db = seat_dir.path().join("read-only-schedule.db");
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("create migrated snapshot source");
        let mut permissions = std::fs::metadata(&db).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&db, permissions).unwrap();
        freeze_snapshot_sidecars(&db);

        use clap::Parser;
        let args = Args::parse_from(["mcp", "--db", db.to_str().expect("utf8 path"), "--no-embed"]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("read-only server must build");
        assert!(
            schedule_rt.is_none(),
            "the default pack set must not return a writer-dependent ticker runtime for a snapshot"
        );

        // Read-only pools are intentionally omitted from `server.pool()` so
        // checkpoint ownership cannot see them. Build the same default pack
        // registry over a retained runtime clone to inspect its actual pool
        // while exercising the identical `warm_all` implementation.
        let runtime = KhiveRuntime::new_readonly(RuntimeConfig {
            db_path: Some(db),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("retained read-only runtime");
        let pool = runtime.backend().pool_arc();
        let warm_server = KhiveMcpServer::new(runtime).expect("default read-only pack registry");
        let before = pool.writer_acquisition_snapshot();
        warm_server.warm_all().await;
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            before,
            "default daemon pack warm must remain outside the read-only writer plane"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn multi_backend_schedule_tick_and_warm_use_each_assigned_backend_mode() {
        use std::os::unix::fs::PermissionsExt;

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let main_db = seat_dir.path().join("read-only-main.db");
        let schedule_db = seat_dir.path().join("writable-schedule.db");
        for path in [&main_db, &schedule_db] {
            KhiveRuntime::new(RuntimeConfig {
                db_path: Some(path.clone()),
                ..RuntimeConfig::no_embeddings()
            })
            .expect("create migrated backend");
        }
        let mut permissions = std::fs::metadata(&main_db).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&main_db, permissions).unwrap();
        freeze_snapshot_sidecars(&main_db);

        let config_path = write_config(
            seat_dir.path(),
            &format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{main}"
read_only = true

[[backends]]
name = "schedule-backend"
kind = "sqlite"
path = "{schedule}"

[packs.schedule]
backend = "schedule-backend"
"#,
                main = main_db.display(),
                schedule = schedule_db.display(),
            ),
        );

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--no-embed",
            "--pack",
            "kg",
            "--pack",
            "schedule",
        ]);
        let (server, schedule_rt) = build_server(&args)
            .await
            .expect("mixed-mode server must build");
        assert!(
            schedule_rt.is_some(),
            "a read-only main backend must not suppress schedule when schedule's own backend is writable"
        );
        let schedule_pool = schedule_rt
            .as_ref()
            .expect("writable schedule runtime")
            .backend()
            .pool_arc();
        let schedule_before = schedule_pool.writer_acquisition_snapshot();
        server.warm_all().await;
        assert_eq!(
            schedule_pool.writer_acquisition_snapshot(),
            schedule_before,
            "warming other packs must not add schedule-backend writer traffic"
        );

        let read_only_schedule_db = seat_dir.path().join("read-only-schedule-secondary.db");
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(read_only_schedule_db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("create migrated read-only schedule source");
        let mut permissions = std::fs::metadata(&read_only_schedule_db)
            .unwrap()
            .permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&read_only_schedule_db, permissions).unwrap();
        freeze_snapshot_sidecars(&read_only_schedule_db);
        let writable_main_db = seat_dir.path().join("writable-main.db");
        KhiveRuntime::new(RuntimeConfig {
            db_path: Some(writable_main_db.clone()),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("create migrated writable main");
        let config_path = write_config(
            seat_dir.path(),
            &format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{main}"

[[backends]]
name = "schedule-backend"
kind = "sqlite"
path = "{schedule}"
read_only = true

[packs.schedule]
backend = "schedule-backend"
"#,
                main = writable_main_db.display(),
                schedule = read_only_schedule_db.display(),
            ),
        );
        let args = Args::parse_from([
            "mcp",
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--no-embed",
            "--pack",
            "kg",
            "--pack",
            "schedule",
        ]);
        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("mixed-mode server must build");
        assert!(
            schedule_rt.is_none(),
            "a writable main backend must not enable schedule when schedule's own backend is read-only"
        );
    }

    #[test]
    fn client_role_never_starts_the_schedule_component() {
        use clap::Parser;
        let args = Args::parse_from(["mcp"]);
        let cfg = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "schedule".to_string()],
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        assert_eq!(
            start_daemon_components_if_daemon(&args, &server, Some(rt)),
            0,
            "stdio/client role must not own background schedule work"
        );
    }

    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_runtime_satisfies_strict_actor_mode_like_the_live_server() {
        // Regression for the exact "strict actor mode can make every tick
        // fail" scenario this fix addressed: before this fix, the
        // tick's separately-reconstructed `RuntimeConfig::default()` carried
        // NO actor regardless of what `--actor` the daemon itself was given,
        // so a strict-mode daemon's tick would trip `enforce_strict_actor_mode`
        // on every single pass even though the live server's own actor was
        // configured correctly. `build_server` must both (a) succeed under
        // strict mode when an actor IS configured, and (b) hand back a
        // schedule-tick runtime carrying that SAME actor — proving the tick
        // no longer performs its own, separately-failing resolution.
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--db",
            ":memory:",
            "--actor",
            "lambda:strict-mode-tenant",
            "--pack",
            "kg",
            "--pack",
            "comm",
            "--pack",
            "schedule",
        ]);

        let result = build_server(&args).await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }

        let (_server, schedule_rt) = result.expect(
            "build_server must succeed under strict mode when --actor is properly configured",
        );
        let rt = schedule_rt.expect("\"schedule\" pack was explicitly requested");
        assert_eq!(
            rt.config().actor_id.as_deref(),
            Some("lambda:strict-mode-tenant"),
            "the tick's runtime must carry the same actor identity that satisfied strict \
             mode at daemon boot, not a separately-resolved, unattributed default"
        );
    }

    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_uses_the_declared_multi_backend_not_main() {
        // Multi-backend (ADR-028 [[backends]]) config-backed targeting: the
        // "schedule" pack is explicitly routed to its OWN backend, distinct
        // from "main". `build_server`'s returned schedule-tick runtime must
        // WRITE INTO that declared backend's file, not main's — proving the
        // correct per-pack runtime is threaded through for
        // multi-backend boots too, not only the single-backend common case.
        // (`RuntimeConfig.db_path` is not itself a reliable signal here —
        // per-pack multi-backend runtimes only override `backend_id`, not
        // `db_path` — so this test verifies the actual bound storage file by
        // writing a marker row and re-opening both declared backend files
        // independently.)
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let main_db = seat_dir.path().join("main.db");
        let schedule_db = seat_dir.path().join("schedule-backend.db");
        let config_path = write_config(
            seat_dir.path(),
            &format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{main}"

[[backends]]
name = "schedule-backend"
kind = "sqlite"
path = "{schedule}"

[packs.schedule]
backend = "schedule-backend"
"#,
                main = main_db.display(),
                schedule = schedule_db.display(),
            ),
        );

        use clap::Parser;
        let args = Args::parse_from(["mcp", "--config", config_path.to_str().expect("utf8 path")]);

        let (_server, schedule_rt) = build_server(&args)
            .await
            .expect("build_server must succeed");
        let rt = schedule_rt.expect("schedule pack is loaded by default and declared here");

        let marker_content = "adr106-multi-backend-schedule-marker";
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize schedule runtime");
        let store = rt.notes(&token).expect("notes store");
        store
            .upsert_note(khive_storage::note::Note::new(
                "local",
                "observation",
                marker_content,
            ))
            .await
            .expect("write marker note through the tick's runtime");

        // Re-open each declared backend file independently (the original
        // `_server`/`rt` are dropped-in-scope-still-alive but this is a
        // sequential, not concurrent, re-open) and confirm the marker landed
        // in "schedule-backend.db" only.
        let count_marker_notes = |path: std::path::PathBuf| async move {
            let cfg = RuntimeConfig {
                db_path: Some(path),
                default_namespace: Namespace::parse("local").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                ..RuntimeConfig::default()
            };
            let probe_rt = KhiveRuntime::new(cfg).expect("reopen backend file");
            let ns = Namespace::parse("local").unwrap();
            let token = probe_rt.authorize(ns).expect("authorize probe");
            let store = probe_rt.notes(&token).expect("notes store");
            let page = store
                .query_notes(
                    "local",
                    Some("observation"),
                    khive_storage::types::PageRequest {
                        limit: 10,
                        offset: 0,
                    },
                )
                .await
                .expect("query observation notes");
            page.items
                .into_iter()
                .filter(|n| n.content == marker_content)
                .count()
        };

        assert_eq!(
            count_marker_notes(schedule_db.clone()).await,
            1,
            "the marker written through the tick's runtime must be present in the declared \
             \"schedule-backend\" backend file"
        );
        assert_eq!(
            count_marker_notes(main_db.clone()).await,
            0,
            "the marker must be ABSENT from \"main\" — the tick's runtime must not have \
             silently written into the main backend instead of the declared schedule backend"
        );
    }

    /// Multi-backend ACTION-DISPATCH routing (PR #782):
    /// `schedule` defaults to "main" (no `[packs.schedule]`
    /// entry declared), while `kg` — the pack whose `create` verb the stored
    /// action below replays — is routed to a SEPARATE declared backend. A due
    /// scheduled event whose action writes through `kg` must land its side
    /// effect in `kg`'s OWN declared backend, never "main".
    ///
    /// This is the regression the prior fix was missing: it
    /// fixed SCANNING (`scheduled_event` rows now correctly read from
    /// `schedule`'s own backend, proven by
    /// `build_server_schedule_tick_uses_the_declared_multi_backend_not_main`
    /// above) but not DISPATCH — the drain replayed every stored action
    /// through a throwaway `KhiveMcpServer::new(schedule_rt.clone())`, which
    /// registers EVERY pack against the schedule backend alone. This test
    /// drives the drain through `run_pending_events_on(&rt, &server, ..)`
    /// with the daemon's REAL, fully-wired `server` (as
    /// supervised schedule component now captures it) and asserts the
    /// replayed action's own write shows up only in `kg`'s declared backend.
    #[tokio::test]
    #[serial]
    async fn build_server_schedule_tick_dispatches_actions_through_the_declared_multi_backend_not_schedule(
    ) {
        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        let _seat_env = SeatEnv::enter(seat_dir.path());
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let main_db = seat_dir.path().join("main.db");
        let kg_db = seat_dir.path().join("kg-backend.db");
        let config_path = write_config(
            seat_dir.path(),
            &format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{main}"

[[backends]]
name = "kg-backend"
kind = "sqlite"
path = "{kg}"

[packs.kg]
backend = "kg-backend"
"#,
                main = main_db.display(),
                kg = kg_db.display(),
            ),
        );

        use clap::Parser;
        let args = Args::parse_from([
            "mcp",
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--no-embed",
        ]);

        let (server, schedule_rt) = build_server(&args)
            .await
            .expect("build_server must succeed");
        // No `[packs.schedule]` entry above, so it defaults to "main".
        let rt = schedule_rt.expect("schedule pack is loaded by default");
        assert!(
            rt.config().embedding_model.is_none()
                && rt.config().additional_embedding_models.is_empty(),
            "the backend-routing fixture must remain independent of external embedding models"
        );

        let marker = "adr106-multi-backend-dispatch-marker";
        let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
        let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        let repeat: Option<&str> = None;
        let fired_at: Option<&str> = None;
        let cancelled_at: Option<&str> = None;
        let props = serde_json::json!({
            "trigger_at": past,
            "repeat": repeat,
            "status": "pending",
            "event_type": "schedule",
            "payload": action_dsl,
            "fired_at": fired_at,
            "cancelled_at": cancelled_at,
            // The schedule pack stamps the creating token's actor; this
            // fixture writes through the runtime directly, so it must carry
            // the provenance itself — the drain fail-closes without it.
            "created_by_actor": "local",
        });
        let ns = Namespace::parse("local").expect("ns");
        let token = rt.authorize(ns).expect("authorize schedule runtime");
        let scheduled = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                &action_dsl,
                None,
                Some(props),
                vec![],
            )
            .await
            .expect("create scheduled_event through the schedule runtime");
        rt.events(&token)
            .expect("schedule event store")
            .append_event(
                khive_storage::Event::new(
                    "local",
                    khive_pack_schedule::CREATOR_PROVENANCE_VERB,
                    khive_types::EventKind::Audit,
                    khive_types::SubstrateKind::Note,
                    format!("{}:{}", token.actor().kind, token.actor().id),
                )
                .with_target(scheduled.id)
                .with_payload(serde_json::json!({
                    "provenance": khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1,
                    "event_type": "schedule",
                })),
            )
            .await
            .expect("append schedule creator provenance");

        let summary = crate::pending_events::run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");
        assert_eq!(
            summary.fired + summary.advanced,
            1,
            "the due event must be dispatched, got summary={summary:?}"
        );
        assert_eq!(summary.failed, 0, "dispatch must not fail: {summary:?}");

        let count_marker_notes = |path: std::path::PathBuf| async move {
            let cfg = RuntimeConfig {
                db_path: Some(path),
                default_namespace: Namespace::parse("local").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                ..RuntimeConfig::default()
            };
            let probe_rt = KhiveRuntime::new(cfg).expect("reopen backend file");
            let ns = Namespace::parse("local").unwrap();
            let token = probe_rt.authorize(ns).expect("authorize probe");
            let store = probe_rt.notes(&token).expect("notes store");
            let page = store
                .query_notes(
                    "local",
                    Some("observation"),
                    khive_storage::types::PageRequest {
                        limit: 10,
                        offset: 0,
                    },
                )
                .await
                .expect("query observation notes");
            page.items
                .into_iter()
                .filter(|n| n.content == marker)
                .count()
        };

        assert_eq!(
            count_marker_notes(kg_db.clone()).await,
            1,
            "the replayed create(kind=\"observation\") action must land in the kg pack's OWN \
             declared backend (\"kg-backend\"), not the schedule backend"
        );
        assert_eq!(
            count_marker_notes(main_db.clone()).await,
            0,
            "the marker must be ABSENT from \"main\" (the schedule backend) — dispatching \
             through a throwaway single-runtime server built from the schedule runtime alone \
             would have written it here instead"
        );
    }

    // --- channel_error_class / record_channel_heartbeat (khive #606) ---

    #[cfg(feature = "channel-email")]
    mod channel_heartbeat_tests {
        use super::*;
        use khive_channel::ChannelError;
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

        #[test]
        fn auth_maps_to_auth_class() {
            assert_eq!(channel_error_class(&ChannelError::Auth("x".into())), "auth");
        }

        #[test]
        fn transport_maps_to_transport_class() {
            assert_eq!(
                channel_error_class(&ChannelError::Transport("x".into())),
                "transport"
            );
        }

        #[test]
        fn config_maps_to_config_class() {
            assert_eq!(
                channel_error_class(&ChannelError::Config("x".into())),
                "config"
            );
        }

        #[test]
        fn unauthorized_sender_and_invalid_envelope_map_to_config_class() {
            // Never produced by poll/connect in practice (see is_backoff_eligible's doc
            // comment), but the mapping must still be total and defensible if it ever
            // does surface from a future adapter.
            assert_eq!(
                channel_error_class(&ChannelError::UnauthorizedSender("x".into())),
                "config"
            );
            assert_eq!(
                channel_error_class(&ChannelError::InvalidEnvelope("x".into())),
                "config"
            );
        }

        /// `record_channel_heartbeat` must be best-effort: a heartbeat dispatch
        /// failure (comm pack not loaded) must not panic the poll loop.
        #[tokio::test]
        async fn record_heartbeat_is_best_effort_when_comm_pack_absent() {
            let registry = VerbRegistryBuilder::new()
                .build()
                .expect("empty registry builds");

            // comm pack is not loaded, so "comm.heartbeat" is an unknown verb — this
            // must return without panicking (the caller only logs a warning).
            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
                None,
            )
            .await;
        }

        /// End-to-end: a successful poll outcome persists via `comm.heartbeat` and
        /// is readable via `comm.health` — the same wiring the live poll loop uses.
        #[tokio::test]
        async fn record_heartbeat_success_is_visible_via_comm_health() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
                None,
            )
            .await;

            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(channels.len(), 1);
            assert_eq!(channels[0]["channel_kind"].as_str(), Some("email"));
            assert_eq!(
                channels[0]["channel_slug"].as_str(),
                Some("recipient@example.com")
            );
            assert_eq!(channels[0]["poll_interval_secs"].as_u64(), Some(5));
            assert_eq!(channels[0]["stalled"].as_bool(), Some(false));
        }

        /// #606: a daemon polling under a
        /// non-local `KHIVE_EMAIL_INGEST_NAMESPACE` must not cause a client-role
        /// no-arg `comm.health()` to report empty state. `record_channel_heartbeat`
        /// takes no `namespace` parameter — the write is unconditionally pinned to
        /// `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` — so this regression proves
        /// the heartbeat row is visible via the default (local-scoped) `comm.health`
        /// read even though this daemon's *messages* are configured to ingest into
        /// a completely different namespace.
        #[tokio::test]
        async fn heartbeat_visible_via_health_regardless_of_configured_ingest_namespace() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            // Simulates `KHIVE_EMAIL_INGEST_NAMESPACE=lambda:mybot`: comm.ingest for
            // an inbound message would target this namespace, but the heartbeat
            // write must ignore it entirely.
            let configured_ingest_namespace = "lambda:mybot";
            let ingest_params = serde_json::json!({
                "namespace": configured_ingest_namespace,
                "from": "email:sender@example.com",
                "to": "email:recipient@example.com",
                "content": "hello",
                "channel_kind": "email",
                "external_id": "test-msg-1",
                "default_inbound_actor": "lambda:leo",
            });
            registry
                .dispatch("comm.ingest", ingest_params)
                .await
                .expect("message ingest into the configured namespace succeeds");

            record_channel_heartbeat(
                &registry,
                "email",
                "recipient@example.com",
                HeartbeatOutcome::Success,
                None,
            )
            .await;

            // A no-arg client-role comm.health() call must see the heartbeat row —
            // it must NOT report role="client" with an empty channels array just
            // because messages are configured to ingest into a non-local namespace.
            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            assert_eq!(health["role"].as_str(), Some("daemon"));
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(
                channels.len(),
                1,
                "heartbeat row must be visible to a no-arg client comm.health() call \
                 regardless of the configured message-ingest namespace"
            );
            assert_eq!(channels[0]["channel_kind"].as_str(), Some("email"));
            assert_eq!(
                channels[0]["channel_slug"].as_str(),
                Some("recipient@example.com")
            );
        }
    }

    // --- ChannelRegistry composite-key production path ---

    #[cfg(feature = "channel-email")]
    mod composite_key_registry_tests {
        use super::*;
        use async_trait::async_trait;
        use chrono::{DateTime, Utc};
        use khive_channel::{Channel, ChannelEnvelope, ChannelError, ChannelRegistry};
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
        use std::sync::Arc;

        /// Two independent inboxes that both report `kind() == "email"` but
        /// distinct `slug()` mailbox addresses, exactly like two configured
        /// `EmailChannel` credentials would.
        struct TwoMailboxChannel {
            slug: String,
        }

        #[async_trait]
        impl Channel for TwoMailboxChannel {
            fn kind(&self) -> &'static str {
                "email"
            }

            fn slug(&self) -> String {
                self.slug.clone()
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                Ok(vec![])
            }
        }

        /// #606 regression guard: a PRODUCTION `ChannelRegistry` populated
        /// through the real `register()` path (not a pack-level dispatch
        /// bypassing registration) with two same-kind, different-slug adapters.
        /// Both must be registered (not collapsed) and both must be pollable by
        /// the production poll loop, producing two independent `comm.health()`
        /// rows and two independent backoff states. This test FAILS against a
        /// `kind`-only-keyed `ChannelRegistry` (both registrations collapse to
        /// `len() == 1`) and PASSES against the `(kind, slug)`-composite-keyed
        /// registry.
        #[tokio::test]
        async fn two_same_kind_channels_both_poll_and_both_persist_health_rows() {
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(TwoMailboxChannel {
                slug: "mailbox-a@example.com".to_string(),
            }));
            ch_registry.register(Arc::new(TwoMailboxChannel {
                slug: "mailbox-b@example.com".to_string(),
            }));
            assert_eq!(
                ch_registry.len(),
                2,
                "two same-kind, different-slug adapters must both register, not collapse"
            );

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            // Drive exactly what the production poll loop does per tick: iterate
            // the registry and record a heartbeat for every (kind, slug, channel).
            for (kind, slug, _channel) in ch_registry.iter() {
                record_channel_heartbeat(&registry, kind, slug, HeartbeatOutcome::Success, None)
                    .await;
            }

            let health = registry
                .dispatch("comm.health", serde_json::json!({}))
                .await
                .expect("health succeeds");
            let channels = health["channels"].as_array().expect("channels array");
            assert_eq!(
                channels.len(),
                2,
                "both mailboxes must produce independent comm.health rows"
            );
            let slugs: std::collections::BTreeSet<&str> = channels
                .iter()
                .map(|c| c["channel_slug"].as_str().expect("channel_slug present"))
                .collect();
            assert_eq!(
                slugs,
                std::collections::BTreeSet::from([
                    "mailbox-a@example.com",
                    "mailbox-b@example.com"
                ])
            );
        }

        /// Backoff state independence: the production poll loop keys its
        /// `HashMap<(String, String), ImapBackoff>` by the same composite
        /// identity, so a failure on one mailbox must never throttle the other.
        #[test]
        fn backoff_state_is_independent_per_kind_slug_pair() {
            use khive_channel_email::ImapBackoff;
            use std::collections::HashMap;

            let mut backoffs: HashMap<(String, String), ImapBackoff> = HashMap::new();
            let key_a = ("email".to_string(), "mailbox-a@example.com".to_string());
            let key_b = ("email".to_string(), "mailbox-b@example.com".to_string());

            let tick_a = backoffs.entry(key_a.clone()).or_default().record_failure();
            assert!(
                !backoffs.contains_key(&key_b),
                "mailbox-b must have no backoff state after only mailbox-a fails"
            );
            assert!(tick_a.delay.as_secs() >= 1, "mailbox-a backoff engaged");

            // mailbox-b independently starts fresh and succeeds immediately.
            let backoff_b = backoffs.entry(key_b).or_default();
            backoff_b.record_success();
            assert_eq!(
                backoffs.get(&key_a).unwrap().attempt(),
                1,
                "mailbox-a's backoff attempt count must be unaffected by mailbox-b's success"
            );
        }
    }

    // --- note_already_delivered: outbox defensive-guard regression ---

    #[cfg(feature = "channel-email")]
    mod outbox_delivered_guard_tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn missing_delivered_at_is_undelivered() {
            let props = json!({}).as_object().unwrap().clone();
            assert!(!note_already_delivered(&props));
        }

        #[test]
        fn explicit_null_delivered_at_is_undelivered() {
            // Regression: a note with delivered_at explicitly set to null (e.g. via a
            // curation `update`) must be treated as undelivered, matching the query
            // predicate in list.rs — not skipped forever by `.is_some()`.
            let props = json!({ "delivered_at": null }).as_object().unwrap().clone();
            assert!(!note_already_delivered(&props));
        }

        #[test]
        fn present_non_null_delivered_at_is_delivered() {
            let props = json!({ "delivered_at": "2026-06-30T12:00:00Z" })
                .as_object()
                .unwrap()
                .clone();
            assert!(note_already_delivered(&props));
        }
    }

    /// #1856 multi-backend regression: the owner-only `external_id` claim
    /// must use the same KG-routed runtime as outbox list/update dispatch.
    /// The default backend deliberately contains no message row; using its
    /// runtime makes the claim fail and suppresses the external send.
    #[cfg(feature = "channel-email")]
    mod routed_email_outbox_tests {
        use super::*;
        use async_trait::async_trait;
        use chrono::{DateTime, Utc};
        use khive_channel::{Channel, ChannelEnvelope, ChannelError};
        use khive_runtime::PackConfig;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingChannel {
            sent: Mutex<Vec<ChannelEnvelope>>,
        }

        #[async_trait]
        impl Channel for RecordingChannel {
            fn kind(&self) -> &'static str {
                "email"
            }

            async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                self.sent.lock().unwrap().push(envelope);
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                Ok(Vec::new())
            }
        }

        #[tokio::test]
        #[serial]
        async fn kg_secondary_runtime_owns_external_id_claim_and_delivery_mark() {
            let dir = tempfile::tempdir().unwrap();
            let main_path = dir.path().join("main.db");
            let kg_path = dir.path().join("kg-secondary.db");
            let khive_cfg = KhiveConfig {
                backends: vec![
                    BackendConfig {
                        name: BackendId::MAIN.to_string(),
                        kind: BackendKind::Sqlite,
                        path: Some(main_path.clone()),
                        cache_mb: None,
                        journal_mode: None,
                        read_only: false,
                    },
                    BackendConfig {
                        name: "kg-store".to_string(),
                        kind: BackendKind::Sqlite,
                        path: Some(kg_path.clone()),
                        cache_mb: None,
                        journal_mode: None,
                        read_only: false,
                    },
                ],
                packs: HashMap::from([
                    (
                        "kg".to_string(),
                        PackConfig {
                            backend: "kg-store".to_string(),
                        },
                    ),
                    (
                        "comm".to_string(),
                        PackConfig {
                            backend: "kg-store".to_string(),
                        },
                    ),
                ]),
                ..KhiveConfig::default()
            };

            let multi = build_registry_for_multi_backend_inner(
                base_runtime_config_for_multi_backend(),
                &khive_cfg,
                None,
            )
            .await
            .expect("mixed-topology registry must build");
            assert_eq!(
                multi.per_pack_runtimes["kg"].backend_id().as_str(),
                "kg-store"
            );
            let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
            let registry = server.verb_registry_clone();
            let owner_runtime = server
                .channel_outbox_runtime_clone()
                .expect("email outbox must retain the KG-routed runtime");
            assert_eq!(owner_runtime.backend_id().as_str(), "kg-store");

            let send = registry
                .dispatch(
                    "comm.send",
                    serde_json::json!({
                        "to": "email:recipient@example.com",
                        "subject": "routed owner claim",
                        "content": "kg-secondary-outbox-probe",
                    }),
                )
                .await
                .expect("comm.send must create the outbound secondary row");
            let note_id = send["full_id"]
                .as_str()
                .expect("comm.send returns full_id")
                .to_string();

            let channel = RecordingChannel::default();
            let namespace = Namespace::parse("local").unwrap();
            channel_outbox_once(
                &channel,
                &registry,
                &owner_runtime,
                &namespace,
                "local",
                "maintainer@example.com",
                "example.com",
                &["recipient@example.com".to_string()],
            )
            .await;
            assert_eq!(channel.sent.lock().unwrap().len(), 1);

            let note = registry
                .dispatch("get", serde_json::json!({ "id": note_id }))
                .await
                .expect("KG-routed get must read the delivered secondary row");
            assert!(
                note["properties"]["external_id"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "owner claim must persist external_id on the KG backend: {note}"
            );
            assert!(
                note["properties"]["delivered_at"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "successful send must persist delivered_at on the KG backend: {note}"
            );

            let main = rusqlite::Connection::open(&main_path).unwrap();
            let main_count: i64 = main
                .query_row(
                    "SELECT COUNT(*) FROM notes WHERE content = 'kg-secondary-outbox-probe'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(main_count, 0, "default backend must not own the message");

            let kg = rusqlite::Connection::open(&kg_path).unwrap();
            let (external_id, delivered_at): (String, String) = kg
                .query_row(
                    "SELECT json_extract(properties, '$.external_id'), \
                            json_extract(properties, '$.delivered_at') \
                     FROM notes WHERE content = 'kg-secondary-outbox-probe' \
                       AND json_extract(properties, '$.direction') = 'outbound'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(!external_id.is_empty());
            assert!(!delivered_at.is_empty());
        }
    }

    // --- spawn_email_channel_loops: shared helper regression (multi-backend gap fix) ---
    //
    // Both `run` and `serve_server` call this same extracted fn (source-verified —
    // see serve.rs's `run` and `serve_server` bodies); a Rust unit test cannot assert
    // "both call sites exist" directly, so this test instead locks in that the
    // extracted helper itself is safe to call in isolation with no `KHIVE_EMAIL_*`
    // env present: it must hit the `Err` arm and return without panicking. No
    // network I/O is exercised (the missing `KHIVE_EMAIL_SMTP_HOST` fails closed
    // before any socket is opened).

    #[cfg(feature = "channel-email")]
    mod spawn_email_channel_loops_tests {
        use super::*;

        const EMAIL_ENV_VARS: [&str; 9] = [
            "KHIVE_EMAIL_SMTP_HOST",
            "KHIVE_EMAIL_IMAP_HOST",
            "KHIVE_EMAIL_USERNAME",
            "KHIVE_EMAIL_MAINTAINER_ADDRESS",
            "KHIVE_EMAIL_AUTHSERV_ID",
            "KHIVE_EMAIL_PASSWORD",
            "KHIVE_EMAIL_OAUTH_TENANT_ID",
            "KHIVE_EMAIL_OAUTH_CLIENT_ID",
            "KHIVE_EMAIL_OAUTH_CLIENT_SECRET",
        ];

        /// RAII guard: snapshots each `KHIVE_EMAIL_*` var's current value, clears it,
        /// and restores the original value (or leaves it removed) on drop — including
        /// on panic, so a failing assertion never leaks env taint to later tests.
        struct EmailEnvGuard {
            snapshot: Vec<(&'static str, Option<String>)>,
        }

        impl EmailEnvGuard {
            fn clear() -> Self {
                let snapshot = EMAIL_ENV_VARS
                    .iter()
                    .map(|&var| (var, std::env::var(var).ok()))
                    .collect();
                for var in EMAIL_ENV_VARS {
                    std::env::remove_var(var);
                }
                Self { snapshot }
            }
        }

        impl Drop for EmailEnvGuard {
            fn drop(&mut self) {
                for (var, prev) in &self.snapshot {
                    match prev {
                        Some(v) => std::env::set_var(var, v),
                        None => std::env::remove_var(var),
                    }
                }
            }
        }

        #[tokio::test]
        #[serial]
        async fn missing_env_hits_err_arm_without_panic() {
            let _env_guard = EmailEnvGuard::clear();

            // Prove the branch the helper depends on is actually taken: with every
            // KHIVE_EMAIL_* var cleared, EmailChannel::from_env() must fail closed.
            // Without this, the test below would pass even if from_env() wrongly hit
            // the Ok arm (it only checks "no panic").
            assert!(
                khive_channel_email::EmailChannel::from_env().is_err(),
                "with KHIVE_EMAIL_* cleared, from_env must fail closed (the Err arm the helper depends on)"
            );

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

            // Must not panic: EmailChannel::from_env() fails closed on the missing
            // KHIVE_EMAIL_SMTP_HOST and the fn logs a warning and returns.
            spawn_email_channel_loops(&server, server.channel_loop_admission());
        }

        /// Regression for #602: `spawn_email_channel_loops_if_daemon` is the
        /// SAME wrapper `run` and `serve_server` call (source-verified — see
        /// those fns' bodies above) — no reimplementation of the role check
        /// here. Actual tokio task spawning cannot be observed from a unit
        /// test, so this pair instead exercises `is_daemon_role` (the pure
        /// predicate) directly against real `Args` values, and drives the
        /// production wrapper through both roles to prove neither branch
        /// panics — the same "no-panic" scope the sibling test above uses.
        #[test]
        fn is_daemon_role_true_for_daemon_args() {
            use clap::Parser;
            let args = Args::parse_from(["mcp", "--daemon"]);
            assert!(
                is_daemon_role(&args),
                "--daemon must resolve to daemon role"
            );
        }

        #[test]
        fn is_daemon_role_false_for_client_args() {
            use clap::Parser;
            let args = Args::parse_from(["mcp"]);
            assert!(
                !is_daemon_role(&args),
                "a plain stdio client (no --daemon) must not resolve to daemon role"
            );
        }

        #[test]
        fn email_plan_gates_poll_and_outbox_on_daemon_role_and_backing_runtime_modes() {
            use clap::Parser;

            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("email-read-only.db");
            let config = RuntimeConfig {
                db_path: Some(path),
                packs: vec!["kg".to_string(), "comm".to_string()],
                ..RuntimeConfig::no_embeddings()
            };
            KhiveRuntime::new(config.clone()).expect("seed exact-current snapshot");
            #[cfg(unix)]
            freeze_snapshot_sidecars(config.db_path.as_ref().expect("db path"));
            let snapshot = KhiveRuntime::new_readonly(config).expect("open read-only snapshot");
            let server = KhiveMcpServer::new(snapshot).expect("build snapshot server");

            let daemon = Args::parse_from(["mcp", "--daemon"]);
            let admitted = channel_loop_plan(&server, &daemon);
            assert!(
                !admitted.inbound_poll,
                "email polling must not start when comm.ingest/cursor/heartbeat resolve to a \
                 read-only runtime"
            );
            assert!(
                !admitted.outbound_delivery,
                "email delivery must not start when list/update resolve to a read-only runtime"
            );

            let writable = KhiveRuntime::memory().expect("writable runtime");
            let server =
                KhiveMcpServer::with_packs(writable, &["kg".to_string(), "comm".to_string()])
                    .expect("build writable server");
            let admitted = channel_loop_plan(&server, &daemon);
            assert!(admitted.inbound_poll);
            assert!(admitted.outbound_delivery);

            let client = Args::parse_from(["mcp"]);
            let admitted = channel_loop_plan(&server, &client);
            assert!(!admitted.inbound_poll);
            assert!(!admitted.outbound_delivery);
        }

        #[tokio::test]
        #[serial]
        async fn daemon_role_gate_spawns_without_panic() {
            use clap::Parser;
            let _env_guard = EmailEnvGuard::clear();
            let args = Args::parse_from(["mcp", "--daemon"]);

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string(), "comm".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg + comm");

            // Daemon role: the wrapper must take the spawn branch (still fails
            // closed on missing KHIVE_EMAIL_* — no network I/O — but must not
            // panic reaching it).
            spawn_email_channel_loops_if_daemon(&server, &args);
        }

        #[tokio::test]
        #[serial]
        async fn client_role_gate_skips_without_panic() {
            use clap::Parser;
            let _env_guard = EmailEnvGuard::clear();
            let args = Args::parse_from(["mcp"]);

            let config = RuntimeConfig {
                db_path: None,
                default_namespace: Namespace::parse("test").unwrap(),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::default()
            };
            let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
            let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

            // Client role: the wrapper must take the skip branch and never
            // attempt to construct an EmailChannel at all.
            spawn_email_channel_loops_if_daemon(&server, &args);
        }
    }

    #[cfg(feature = "channel-telegram")]
    mod telegram_channel_loop_admission_tests {
        use super::*;

        #[test]
        fn telegram_plan_gates_poll_and_outbox_on_backing_runtime_modes() {
            use clap::Parser;

            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("telegram-read-only.db");
            let config = RuntimeConfig {
                db_path: Some(path),
                packs: vec!["kg".to_string(), "comm".to_string()],
                ..RuntimeConfig::no_embeddings()
            };
            KhiveRuntime::new(config.clone()).expect("seed exact-current snapshot");
            #[cfg(unix)]
            freeze_snapshot_sidecars(config.db_path.as_ref().expect("db path"));
            let snapshot = KhiveRuntime::new_readonly(config).expect("open read-only snapshot");
            let server = KhiveMcpServer::new(snapshot).expect("build snapshot server");
            let daemon = Args::parse_from(["mcp", "--daemon"]);

            let admitted = channel_loop_plan(&server, &daemon);
            assert!(
                !admitted.inbound_poll,
                "Telegram getUpdates must not start when comm.ingest resolves to a read-only \
                 runtime"
            );
            assert!(
                !admitted.outbound_delivery,
                "Telegram sendMessage must not start when delivered_at cannot be durably \
                 recorded by list/update's runtime"
            );

            let writable = KhiveRuntime::memory().expect("writable runtime");
            let server =
                KhiveMcpServer::with_packs(writable, &["kg".to_string(), "comm".to_string()])
                    .expect("build writable server");
            let admitted = channel_loop_plan(&server, &daemon);
            assert!(admitted.inbound_poll);
            assert!(admitted.outbound_delivery);
        }
    }

    // --- channel_poll_loop: ADR-094 lifecycle event sequencing (#623) ---

    #[cfg(feature = "channel-email")]
    mod channel_lifecycle_sequencing_tests {
        use super::*;
        use async_trait::async_trait;
        use chrono::{DateTime, Utc};
        use khive_channel::{Channel, ChannelEnvelope, ChannelError, ChannelRegistry};
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        /// A channel whose first `poll()` fails with a backoff-eligible
        /// transport error and whose every later `poll()` succeeds — the
        /// minimal fixture needed to drive the loop through one full
        /// fail-then-recover lifecycle episode.
        struct FlakyOnceChannel {
            call_count: AtomicUsize,
        }

        #[async_trait]
        impl Channel for FlakyOnceChannel {
            fn kind(&self) -> &'static str {
                "mock"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                if self.call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(ChannelError::Transport("synthetic connect failure".into()))
                } else {
                    Ok(vec![])
                }
            }
        }

        /// In-memory `EventStore` fake that just records every appended event
        /// in append order, so the test can inspect exactly what the poll
        /// loop persisted without standing up a SQL backend.
        #[derive(Default)]
        struct FakeEventStore {
            events: Mutex<Vec<khive_storage::Event>>,
        }

        #[async_trait]
        impl khive_storage::EventStore for FakeEventStore {
            async fn append_event(
                &self,
                event: khive_storage::Event,
            ) -> khive_storage::StorageResult<()> {
                self.events.lock().unwrap().push(event);
                Ok(())
            }

            async fn append_events(
                &self,
                events: Vec<khive_storage::Event>,
            ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
                let n = events.len() as u64;
                self.events.lock().unwrap().extend(events);
                Ok(khive_storage::BatchWriteSummary {
                    attempted: n,
                    affected: n,
                    failed: 0,
                    first_error: String::new(),
                })
            }

            async fn get_event(
                &self,
                id: uuid::Uuid,
            ) -> khive_storage::StorageResult<Option<khive_storage::Event>> {
                Ok(self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|e| e.id == id)
                    .cloned())
            }

            async fn query_events(
                &self,
                _filter: khive_storage::EventFilter,
                _page: khive_storage::PageRequest,
            ) -> khive_storage::StorageResult<khive_storage::Page<khive_storage::Event>>
            {
                let items = self.events.lock().unwrap().clone();
                let total = items.len() as u64;
                Ok(khive_storage::Page {
                    items,
                    total: Some(total),
                })
            }

            async fn count_events(
                &self,
                _filter: khive_storage::EventFilter,
            ) -> khive_storage::StorageResult<u64> {
                Ok(self.events.lock().unwrap().len() as u64)
            }

            fn preflight_event(
                &self,
                _event: &khive_storage::Event,
            ) -> khive_storage::StorageResult<()> {
                Ok(())
            }

            async fn append_events_idempotent(
                &self,
                events: Vec<khive_storage::Event>,
            ) -> khive_storage::StorageResult<khive_storage::event::IdempotentEventBatchResult>
            {
                let mut store = self.events.lock().unwrap();
                let mut rows = Vec::with_capacity(events.len());
                for event in events {
                    if let Some(existing) = store.iter().find(|e| e.id == event.id) {
                        if *existing == event {
                            rows.push(
                                khive_storage::event::EventAppendDisposition::AlreadyPresentIdentical,
                            );
                        } else {
                            rows.push(
                                khive_storage::event::EventAppendDisposition::IdentityConflict,
                            );
                        }
                    } else {
                        store.push(event);
                        rows.push(khive_storage::event::EventAppendDisposition::Inserted);
                    }
                }
                Ok(khive_storage::event::IdempotentEventBatchResult { rows })
            }

            fn supports_idempotent_audit_batch(&self) -> bool {
                true
            }
        }

        /// The ADR-094 lifecycle-event subsequence the sequencing test
        /// asserts on. The shared `FakeEventStore` also receives every
        /// dispatch's audit event and each `comm.heartbeat` write, so raw
        /// `store.events.len()` is not a proxy for "how many lifecycle
        /// events landed" -- it inflates far faster than the six events
        /// this test actually cares about, which is why convergence must
        /// be checked against this filtered view, not the raw count.
        fn lifecycle_sequence(store: &FakeEventStore) -> Vec<khive_types::EventKind> {
            store
                .events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.kind)
                .filter(|k| {
                    matches!(
                        k,
                        khive_types::EventKind::ChannelPollStarted
                            | khive_types::EventKind::ChannelPollSucceeded
                            | khive_types::EventKind::ChannelPollFailed
                            | khive_types::EventKind::ChannelBackoffArmed
                            | khive_types::EventKind::ChannelBackoffReset
                    )
                })
                .collect()
        }

        /// Drive the paused virtual clock forward in small steps, yielding
        /// after each one, until the fake store has recorded at least
        /// `target` lifecycle events (see [`lifecycle_sequence`]). A single
        /// big `advance` can outrun a timer the polled task hasn't
        /// registered yet (the task only arms its next `sleep` after
        /// cooperative scheduling lets it run back around the loop), so
        /// this steps forward repeatedly instead of guessing one jump that
        /// is simultaneously long enough to fire the next timer and short
        /// enough not to skip past it unregistered.
        ///
        /// The loop's own `comm.*` dispatches land on `spawn_blocking`
        /// (`khive-db`'s writer runs on tokio's real OS-thread blocking
        /// pool), so how many `advance`/`yield_now` rounds this needs to
        /// converge depends on real thread-pool scheduling latency, not on
        /// virtual time -- a fixed iteration count is really a proxy for
        /// real wall-clock patience, and a bigger fixed count doesn't buy
        /// more of it if the loop itself runs each round near-instantly in
        /// real time. Bounding on an actual wall-clock deadline instead
        /// gives the blocking pool as much real time as it needs under
        /// load, while still failing fast (with a clear message) if the
        /// condition is genuinely never met.
        async fn advance_until(store: &FakeEventStore, target: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if lifecycle_sequence(store).len() >= target {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "lifecycle event sequence did not reach {target} events within 60s of \
                     wall-clock time; got {:?}",
                    lifecycle_sequence(store)
                );
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }
        }

        /// ADR-094 sequencing invariant (#623): a channel that fails once and
        /// then recovers must produce exactly this six-event lifecycle
        /// sequence, in this order — `query_events` in production orders on
        /// `idx_events_ns_created_id`, i.e. append order, which this fake
        /// preserves directly. Swapping any two entries (e.g. emitting
        /// `ChannelBackoffArmed` before `ChannelPollFailed`, or letting the
        /// second `ChannelPollStarted` land after `ChannelPollSucceeded`)
        /// makes this assertion fail: it is an order check, not a mere
        /// presence/count check.
        #[tokio::test(start_paused = true)]
        async fn channel_lifecycle_events_are_sequenced_across_a_failure_then_recovery() {
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(FlakyOnceChannel {
                call_count: AtomicUsize::new(0),
            }));

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let store = Arc::new(FakeEventStore::default());
            builder.with_event_store(store.clone());
            let registry = builder.build().expect("registry builds");

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry,
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Iteration 1 (happy-path 5s sleep elapses, poll fails, backoff
            // arms) then iteration 2 (backoff delay elapses, poll succeeds,
            // backoff resets) — six lifecycle events total.
            advance_until(&store, 6).await;

            task.abort();

            let sequence = lifecycle_sequence(&store);

            assert_eq!(
                sequence,
                vec![
                    khive_types::EventKind::ChannelPollStarted,
                    khive_types::EventKind::ChannelPollFailed,
                    khive_types::EventKind::ChannelBackoffArmed,
                    khive_types::EventKind::ChannelPollStarted,
                    khive_types::EventKind::ChannelPollSucceeded,
                    khive_types::EventKind::ChannelBackoffReset,
                ],
                "ADR-094 lifecycle events must be sequenced exactly as the poll \
                 loop drives them: started -> failed -> backoff armed -> \
                 started -> succeeded -> backoff reset. Got: {sequence:?}"
            );
        }

        /// Edge case: with no `EventStore` configured, the loop must run the
        /// same fail-then-recover cycle without panicking or blocking on the
        /// (absent) lifecycle-append path.
        #[tokio::test(start_paused = true)]
        async fn channel_lifecycle_events_are_a_no_op_without_an_event_store() {
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(FlakyOnceChannel {
                call_count: AtomicUsize::new(0),
            }));

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");
            assert!(
                registry.event_store().is_none(),
                "no event store was configured for this registry"
            );

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry,
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Step the paused clock through both iterations; with no store
            // configured there is no event count to converge on, so this
            // just needs to comfortably clear the failure backoff delay.
            for _ in 0..48 {
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                tokio::task::yield_now().await;
            }

            task.abort();
        }
    }

    /// Regression tests for issue #449's daemon wiring: the
    /// poll loop must drive `cursor_get` -> `poll_page` -> every
    /// `comm.ingest` -> `cursor_commit`, committing the cursor only when
    /// every envelope in the page durably ingested.
    #[cfg(feature = "channel-email")]
    mod cursor_commit_gating_tests {
        use super::*;
        use async_trait::async_trait;
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine as _;
        use chrono::{DateTime, Utc};
        use khive_channel::{
            Channel, ChannelCheckpoint, ChannelEnvelope, ChannelError, ChannelPollPage,
            ChannelRegistry, StoredChannelCheckpoint,
        };
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        const SOURCE: &str = "imap+tls:h:993:m:INBOX";

        /// First `poll_page` call returns one message that ingests cleanly
        /// and one that permanently fails `comm.ingest` validation (empty
        /// content) -- simulating a partial-page ingest failure. Every
        /// subsequent call returns only the message that already succeeded,
        /// mirroring the daemon's next-poll re-delivery of the whole
        /// unresolved page. Each call's observed checkpoint is recorded
        /// (rather than asserted inline, since a panic inside a
        /// `tokio::spawn`ed task is otherwise silently swallowed by
        /// `task.abort()`) so the test body can assert on it after the loop
        /// task is done.
        struct PartialFailureChannel {
            call_count: AtomicUsize,
            observed_checkpoints: Arc<Mutex<Vec<Option<StoredChannelCheckpoint>>>>,
        }

        #[async_trait]
        impl Channel for PartialFailureChannel {
            fn kind(&self) -> &'static str {
                "mock"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                _since: DateTime<Utc>,
                checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                let call = self.call_count.fetch_add(1, Ordering::SeqCst);
                self.observed_checkpoints
                    .lock()
                    .unwrap()
                    .push(checkpoint.cloned());
                let good = ChannelEnvelope::new(
                    "email:sender@example.com",
                    "email:me@example.com",
                    "good body",
                )
                .with_external_id("imap:h:1:1");

                if call == 0 {
                    let bad = ChannelEnvelope::new(
                        "email:sender@example.com",
                        "email:me@example.com",
                        "",
                    );
                    Ok(ChannelPollPage {
                        envelopes: vec![good, bad],
                        next_checkpoint: Some(ChannelCheckpoint {
                            source: SOURCE.to_string(),
                            generation: 1,
                            high_water: Some(2),
                        }),
                    })
                } else {
                    Ok(ChannelPollPage {
                        envelopes: vec![good],
                        next_checkpoint: Some(ChannelCheckpoint {
                            source: SOURCE.to_string(),
                            generation: 1,
                            high_water: Some(1),
                        }),
                    })
                }
            }
        }

        #[tokio::test(start_paused = true)]
        async fn partial_ingest_failure_does_not_advance_cursor_and_dedup_prevents_double_store() {
            let observed_checkpoints = Arc::new(Mutex::new(Vec::new()));
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(PartialFailureChannel {
                call_count: AtomicUsize::new(0),
                observed_checkpoints: observed_checkpoints.clone(),
            }));

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "local".to_string(),
                "actor:test".to_string(),
            ));

            // Three happy-path 5s ticks: the first drives the partial-failure
            // page, the second drives the retry the fix must produce, and the
            // third observes the checkpoint the retry committed -- proving
            // the retry's `comm.ingest` (and its dedup) actually completed and
            // was durably persisted, not merely that a second `poll_page` call
            // was made while the retry was still in flight. Poll for that
            // directly (rather than blindly running a fixed number of ticks)
            // and bound the wait by a real wall-clock deadline, not a
            // virtual-time/iteration budget -- the loop's `comm.*` dispatches
            // land on `spawn_blocking`'s real OS-thread pool, so how many
            // advance/yield rounds this needs depends on real thread-pool
            // scheduling latency, which a fixed count cannot account for
            // under load.
            let expected_committed_checkpoint = ChannelCheckpoint {
                source: SOURCE.to_string(),
                generation: 1,
                high_water: Some(1),
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if observed_checkpoints
                    .lock()
                    .unwrap()
                    .get(2)
                    .is_some_and(|c| {
                        c.as_ref().map(|stored| &stored.checkpoint)
                            == Some(&expected_committed_checkpoint)
                    })
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the retry must have committed its checkpoint (observable on the \
                     third poll_page call) within 60s of wall-clock time: {:?}",
                    observed_checkpoints.lock().unwrap()
                );
                tokio::time::advance(std::time::Duration::from_secs(5)).await;
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
            }
            task.abort();

            let calls = observed_checkpoints.lock().unwrap().clone();
            assert!(
                calls.len() >= 3,
                "the loop must have retried and then re-polled with the retry's \
                 committed checkpoint: {calls:?}"
            );
            assert!(
                calls[0].is_none(),
                "the first poll must see no persisted checkpoint"
            );
            assert!(
                calls[1].is_none(),
                "the cursor must NOT have advanced past the partially-failed page \
                 -- the retry must still see no committed checkpoint: {calls:?}"
            );
            assert_eq!(
                calls[2].as_ref().map(|stored| &stored.checkpoint),
                Some(&expected_committed_checkpoint),
                "the third poll must observe the checkpoint the retry committed, \
                 proving the retry's comm.ingest (and its dedup) actually completed: \
                 {calls:?}"
            );

            let inbox = registry
                .dispatch(
                    "list",
                    json!({"namespace": "local", "kind": "message", "limit": 50}),
                )
                .await
                .expect("list must succeed");
            let notes = inbox["items"]
                .as_array()
                .expect("list returns an items envelope")
                .clone();
            let matching: Vec<_> = notes
                .iter()
                .filter(|n| {
                    n.get("properties")
                        .and_then(|p| p.get("external_id"))
                        .and_then(|v| v.as_str())
                        == Some("imap:h:1:1")
                })
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "the message that succeeded on the failed page must not be \
                 double-stored once the retry re-delivers the whole page: {notes:?}"
            );
        }

        /// A channel whose every `poll_page` call returns an empty page with
        /// a `next_checkpoint` that `comm.cursor_commit` itself rejects
        /// (`generation: 0` is outside its documented `1..=i64::MAX` range).
        /// Exercises the daemon's `commit_channel_cursor`-`Err` branch
        /// (issue #449): every other test in this module drives
        /// a `cursor_get` failure or a `comm.ingest` failure, never a
        /// rejected commit itself, so that branch was otherwise dead from
        /// this suite's perspective.
        struct CommitRejectedChannel {
            call_count: AtomicUsize,
        }

        #[async_trait]
        impl Channel for CommitRejectedChannel {
            fn kind(&self) -> &'static str {
                "mock_commit_rejected"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                _since: DateTime<Utc>,
                _checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                Ok(ChannelPollPage {
                    envelopes: vec![],
                    next_checkpoint: Some(ChannelCheckpoint {
                        source: SOURCE.to_string(),
                        generation: 0,
                        high_water: Some(1),
                    }),
                })
            }
        }

        #[tokio::test(start_paused = true)]
        async fn rejected_cursor_commit_leaves_no_committed_checkpoint() {
            let channel = Arc::new(CommitRejectedChannel {
                call_count: AtomicUsize::new(0),
            });
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(channel.clone());

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Wait for at least two poll_page calls (deterministic condition
            // on the channel's own call counter, not a fixed tick budget):
            // the second call proves the loop went all the way around after
            // the first call's rejected commit, giving that commit's async
            // dispatch chain every chance to finish before asserting on its
            // durable effect. Bounded on real wall-clock time, not virtual
            // time, since the underlying `comm.*` dispatches land on
            // `spawn_blocking`'s real OS-thread pool.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if channel.call_count.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "poll_page was not called at least twice within 60s of wall-clock time"
                );
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }
            task.abort();

            let restored =
                load_channel_cursor(&registry, "mock_commit_rejected", "mock_commit_rejected")
                    .await
                    .expect("cursor_get must succeed");
            assert!(
                restored.is_none(),
                "a rejected cursor_commit must not leave a committed checkpoint: {restored:?}"
            );
        }

        /// A channel whose one and only `poll_page` call returns a single
        /// quarantine-shaped envelope -- exactly the field shape
        /// `EmailChannel::disposition` produces for a permanently
        /// unparseable UID (see
        /// `khive-channel-email`'s
        /// `poll_page_malformed_uid_produces_a_stable_external_id_and_quarantine_metadata`) --
        /// so this test can drive it through the daemon's real
        /// `comm.ingest` call and query the durably persisted note.
        struct QuarantineOnceChannel {
            envelope: Mutex<Option<ChannelEnvelope>>,
        }

        struct ContentRefusalOnceChannel {
            envelope: Mutex<Option<ChannelEnvelope>>,
        }

        #[async_trait]
        impl Channel for ContentRefusalOnceChannel {
            fn kind(&self) -> &'static str {
                "email"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                _since: DateTime<Utc>,
                _checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                let Some(envelope) = self.envelope.lock().unwrap().take() else {
                    return Ok(ChannelPollPage {
                        envelopes: vec![],
                        next_checkpoint: None,
                    });
                };
                Ok(ChannelPollPage {
                    envelopes: vec![envelope],
                    next_checkpoint: Some(ChannelCheckpoint {
                        source: SOURCE.to_string(),
                        generation: 11,
                        high_water: Some(7),
                    }),
                })
            }
        }

        #[async_trait]
        impl Channel for QuarantineOnceChannel {
            fn kind(&self) -> &'static str {
                "mock_quarantine"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                _since: DateTime<Utc>,
                _checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                let Some(envelope) = self.envelope.lock().unwrap().take() else {
                    return Ok(ChannelPollPage {
                        envelopes: vec![],
                        next_checkpoint: None,
                    });
                };
                Ok(ChannelPollPage {
                    envelopes: vec![envelope],
                    next_checkpoint: Some(ChannelCheckpoint {
                        source: SOURCE.to_string(),
                        generation: 9,
                        high_water: Some(1),
                    }),
                })
            }
        }

        /// khive #449 follow-up: the connector- and
        /// channel-level poison-UID tests prove a malformed message becomes
        /// a quarantine-shaped `ChannelEnvelope`, but neither proves the
        /// daemon actually turns that into a durable, queryable record.
        /// Drives a quarantine envelope through the real `channel_poll_loop`
        /// -> `comm.ingest` path and queries the stored note back out,
        /// asserting its stable external ID and quarantine metadata
        /// persisted exactly -- and that the cursor committed, since a
        /// quarantine envelope must durably ingest like any other message.
        #[tokio::test(start_paused = true)]
        async fn malformed_message_durably_quarantines_with_stable_external_id_and_metadata() {
            let mut envelope = ChannelEnvelope::new(
                "email:quarantine",
                "email:maintainer@example.com",
                "(khive: IMAP message UID 1 could not be parsed and was quarantined)",
            )
            .with_external_id("imap:h:9:1");
            envelope
                .metadata
                .insert("quarantined".to_string(), "true".to_string());
            envelope
                .metadata
                .insert("quarantine_reason".to_string(), "missing-body".to_string());

            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(QuarantineOnceChannel {
                envelope: Mutex::new(Some(envelope)),
            }));

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "local".to_string(),
                "actor:test".to_string(),
            ));

            for _ in 0..2000 {
                let restored = load_channel_cursor(&registry, "mock_quarantine", "mock_quarantine")
                    .await
                    .expect("cursor_get must succeed");
                if restored.is_some() {
                    break;
                }
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }
            task.abort();

            let restored = load_channel_cursor(&registry, "mock_quarantine", "mock_quarantine")
                .await
                .expect("cursor_get must succeed")
                .expect(
                    "the cursor must have committed -- a quarantine envelope must ingest \
                     durably like any other message",
                );
            assert_eq!(restored.checkpoint.high_water, Some(1));

            let inbox = registry
                .dispatch(
                    "list",
                    json!({"namespace": "local", "kind": "message", "limit": 50}),
                )
                .await
                .expect("list must succeed");
            let notes = inbox["items"]
                .as_array()
                .expect("list returns an items envelope")
                .clone();
            let quarantined = notes
                .iter()
                .find(|n| {
                    n.get("properties")
                        .and_then(|p| p.get("external_id"))
                        .and_then(|v| v.as_str())
                        == Some("imap:h:9:1")
                })
                .expect(
                    "the quarantined message must be durably queryable by its stable \
                         external_id, not just held as an intermediate value",
                );

            let props = quarantined
                .get("properties")
                .expect("stored note must carry properties");
            assert_eq!(
                props.get("quarantined").and_then(|v| v.as_str()),
                Some("true"),
                "durable quarantine metadata must survive comm.ingest: {props:?}"
            );
            assert_eq!(
                props.get("quarantine_reason").and_then(|v| v.as_str()),
                Some("missing-body"),
                "the quarantine reason must survive comm.ingest: {props:?}"
            );
            assert_eq!(
                props.get("channel_slug").and_then(|v| v.as_str()),
                Some("mock_quarantine"),
                "the poll loop must persist the exact channel identity used by comm.health"
            );

            let health = registry
                .dispatch("comm.health", json!({}))
                .await
                .expect("health succeeds after a quarantined poll");
            let channel = health["channels"]
                .as_array()
                .expect("channels array")
                .iter()
                .find(|channel| channel["channel_slug"] == "mock_quarantine")
                .expect("mock quarantine heartbeat");
            assert_eq!(channel["consecutive_failures"].as_u64(), Some(0));
            assert_eq!(channel["stalled"].as_bool(), Some(false));
            assert_eq!(channel["quarantined_count"].as_u64(), Some(1));
            assert_eq!(health["quarantined_count"].as_u64(), Some(1));
        }

        #[tokio::test(start_paused = true)]
        async fn content_refusal_stores_exact_replay_and_ingests_body_free_quarantine() {
            const EXTERNAL_ID: &str = "imap:h:11:7";
            const REFUSED_BODY: &str = "AKIAFAKEKEY1234567890"; // gitleaks:allow
            const ORIGINAL_BYTES: &[u8] = b"From: maintainer@example.com\r\n\
                To: mailbox@example.com\r\n\
                Subject: replay fixture\r\n\
                \r\n\
                AKIAFAKEKEY1234567890"; // gitleaks:allow

            let envelope = ChannelEnvelope::new(
                "email:maintainer@example.com",
                "email:mailbox@example.com",
                REFUSED_BODY,
            )
            .with_external_id(EXTERNAL_ID)
            .with_quarantine_replay(ORIGINAL_BYTES.to_vec(), "email:maintainer@example.com");

            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(ContentRefusalOnceChannel {
                envelope: Mutex::new(Some(envelope)),
            }));

            let blob_dir = tempfile::tempdir().expect("blob tempdir");
            let blob_store =
                khive_db::stores::blob::FsBlobStore::new(blob_dir.path().to_path_buf(), 0)
                    .expect("fs blob store");
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            runtime
                .install_blob_store(Arc::new(blob_store))
                .expect("install blob store");
            let mut builder = VerbRegistryBuilder::new();
            builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
            builder.register(
                khive_pack_comm::CommPack::new_with_channel_ingest_capability(
                    runtime.clone(),
                    khive_runtime::ChannelIngestCapability::grant_for_direct_composition(),
                ),
            );
            builder.register(khive_pack_blob::BlobPack::new(runtime.clone()));
            let registry = builder.build().expect("registry builds");
            ensure_channel_quarantine_storage(&registry)
                .await
                .expect("configured replay storage must pass channel startup preflight");

            let refused = registry
                .dispatch(
                    "comm.ingest",
                    json!({
                        "namespace": "test-ns",
                        "from": "email:maintainer@example.com",
                        "to": "email:mailbox@example.com",
                        "content": REFUSED_BODY,
                        "channel_kind": "email",
                        "external_id": EXTERNAL_ID,
                        "default_inbound_actor": "actor:test",
                    }),
                )
                .await
                .expect_err("the fixture must exercise the real content gate");
            assert!(
                matches!(refused, khive_runtime::RuntimeError::SecretDetected(_)),
                "the replay test is invalid unless comm.ingest refuses the original content"
            );

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let restored = load_channel_cursor(&registry, "email", "email")
                    .await
                    .expect("cursor_get must succeed");
                if restored
                    .as_ref()
                    .is_some_and(|stored| stored.checkpoint.high_water == Some(7))
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "a permanently refused message must quarantine and advance the cursor"
                );
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }
            task.abort();

            let inbox = registry
                .dispatch(
                    "list",
                    json!({"namespace": "test-ns", "kind": "message", "limit": 50}),
                )
                .await
                .expect("list must succeed");
            let notes = inbox["items"]
                .as_array()
                .expect("list returns an items envelope");
            assert_eq!(notes.len(), 1, "only the quarantine notification is stored");
            let quarantined = &notes[0];
            assert_eq!(
                quarantined["properties"]["from_actor"],
                "email:quarantine",
                "quarantine sender prefix invariant: `email:` keeps the notification visible to prefix-keyed consumers"
            );
            assert_eq!(quarantined["properties"]["external_id"], EXTERNAL_ID);
            assert_eq!(
                quarantined["properties"]["quarantine_classification"],
                "permanent"
            );
            assert_eq!(
                quarantined["properties"]["quarantine_reason"],
                "SecretDetected"
            );
            assert!(
                !quarantined["content"]
                    .as_str()
                    .expect("note content")
                    .contains(REFUSED_BODY),
                "the notification must never re-present the refused body"
            );

            let content_ref = quarantined["properties"]["quarantine_content_ref"]
                .as_str()
                .expect("quarantine ContentRef");
            let fetched = registry
                .dispatch("blob.get", json!({"content_ref": content_ref}))
                .await
                .expect("quarantine replay blob must be retrievable");
            let replay = BASE64
                .decode(fetched["bytes"].as_str().expect("base64 replay bytes"))
                .expect("valid base64 replay bytes");
            assert_eq!(
                replay, ORIGINAL_BYTES,
                "the replay ContentRef must round-trip the byte-exact original message"
            );
        }

        /// Restart-across-a-checkpoint round-trip (issue #449 part b): once a
        /// page fully ingests and the cursor commits, a fresh call to
        /// `comm.cursor_get` (simulating a daemon restart reading the
        /// persisted row) must return the exact checkpoint that was
        /// committed -- proving the durable path round-trips independent of
        /// any in-process state.
        #[tokio::test]
        async fn committed_cursor_round_trips_across_a_fresh_cursor_get() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            // Simulates a poll that crosses an IMAP UIDVALIDITY/date-window
            // boundary: commit once, then read it back as a brand-new
            // process (a new `comm.cursor_get` call, no shared in-memory
            // cursor) would on restart.
            commit_channel_cursor(
                &registry,
                "mock",
                "mailbox-a",
                &ChannelCheckpoint {
                    source: SOURCE.to_string(),
                    generation: 7,
                    high_water: Some(123),
                },
            )
            .await
            .expect("cursor_commit must succeed");

            let restored = load_channel_cursor(&registry, "mock", "mailbox-a")
                .await
                .expect("cursor_get must succeed")
                .expect("a committed checkpoint must round-trip, not read back as absent");

            assert_eq!(restored.checkpoint.source, SOURCE);
            assert_eq!(restored.checkpoint.generation, 7);
            assert_eq!(restored.checkpoint.high_water, Some(123));
        }
    }

    /// Regression tests for issue #449: a channel's
    /// bootstrap `since` floor (the date used in the IMAP `SINCE` clause
    /// while no UID high-water is committed yet) must only advance once
    /// `cursor_get`, `poll_page`, every `comm.ingest`, and `cursor_commit`
    /// have all succeeded for that channel's cycle. A cursor_get failure or
    /// an ingest failure that blocks the first commit must leave the floor
    /// exactly where it was, so a later successful cycle still searches from
    /// the original floor rather than a newer date -- otherwise, if the
    /// failing cycles spanned a calendar-day boundary, mail from the earlier
    /// day would be permanently skipped.
    #[cfg(feature = "channel-email")]
    mod bootstrap_since_floor_tests {
        use super::*;
        use async_trait::async_trait;
        use chrono::{DateTime, Utc};
        use khive_channel::{
            Channel, ChannelEnvelope, ChannelError, ChannelPollPage, ChannelRegistry,
            StoredChannelCheckpoint,
        };
        use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
        use khive_storage::types::{SqlStatement, SqlValue};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        /// A channel that just records the `since` it is called with on
        /// every `poll_page` call and always reports a clean, empty page --
        /// harmless to call repeatedly, so the test can drive many ticks and
        /// inspect only the recorded `since` history.
        struct RecordingChannel {
            kind: &'static str,
            since_calls: Arc<Mutex<Vec<DateTime<Utc>>>>,
        }

        #[async_trait]
        impl Channel for RecordingChannel {
            fn kind(&self) -> &'static str {
                self.kind
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                since: DateTime<Utc>,
                _checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                self.since_calls.lock().unwrap().push(since);
                Ok(ChannelPollPage {
                    envelopes: vec![],
                    next_checkpoint: None,
                })
            }
        }

        /// A channel whose first `poll_page` call returns one envelope that
        /// permanently fails `comm.ingest` validation (empty `content`),
        /// blocking that cycle's first-ever commit; every later call returns
        /// no envelopes so the cycle cleanly completes. Also records `since`
        /// on every call.
        struct IngestFailsOnceChannel {
            call_count: AtomicUsize,
            since_calls: Arc<Mutex<Vec<DateTime<Utc>>>>,
        }

        #[async_trait]
        impl Channel for IngestFailsOnceChannel {
            fn kind(&self) -> &'static str {
                "mock_ingest_fails_once"
            }

            async fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
                Ok(())
            }

            async fn poll(
                &self,
                _since: DateTime<Utc>,
            ) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("the daemon poll loop must call poll_page, not poll");
            }

            async fn poll_page(
                &self,
                since: DateTime<Utc>,
                _checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                self.since_calls.lock().unwrap().push(since);
                if self.call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    let bad = ChannelEnvelope::new(
                        "email:sender@example.com",
                        "email:me@example.com",
                        "",
                    );
                    Ok(ChannelPollPage {
                        envelopes: vec![bad],
                        next_checkpoint: None,
                    })
                } else {
                    Ok(ChannelPollPage {
                        envelopes: vec![],
                        next_checkpoint: None,
                    })
                }
            }
        }

        /// Drive the paused virtual clock forward in small steps, yielding
        /// after each one, until `calls` has recorded at least `target`
        /// entries (mirrors `cursor_commit_gating_tests`' convergence
        /// pattern: the loop's `comm.*` dispatches need several cooperative
        /// sleep/wake round-trips under a paused clock to settle, so a
        /// single large `advance` can outrun a timer the task has not
        /// re-armed yet).
        async fn advance_until_calls(calls: &Mutex<Vec<DateTime<Utc>>>, target: usize) {
            for _ in 0..2000 {
                if calls.lock().unwrap().len() >= target {
                    return;
                }
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            }
        }

        /// Cross-date regression (issue #449 High, failure shape 1): a
        /// `comm.cursor_get` failure on a channel's first cycle must not
        /// lose that channel's bootstrap floor. This corrupts the durable
        /// cursor row for `mock_broken_cursor_get` directly (an unparseable
        /// `generation` column) so its very first `cursor_get` fails and the
        /// channel is skipped for that tick, then repairs the row before the
        /// next tick. A `mock_control` channel with no corruption is polled
        /// on every tick as a same-run reference for what the *first* tick's
        /// floor actually was -- if the fix works, the broken channel's
        /// first successful `poll_page` call (after recovery) sees the exact
        /// same `since` as the control channel's very first call, proving
        /// the floor survived the cursor_get failure instead of jumping
        /// forward to a later tick's timestamp (which, across a calendar-day
        /// boundary, would silently drop the previous day's mail from the
        /// IMAP `SINCE` search).
        #[tokio::test(start_paused = true)]
        async fn cursor_get_failure_preserves_the_bootstrap_floor() {
            const BROKEN_KIND: &str = "mock_broken_cursor_get";

            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            // Seed a valid row (also bootstraps the pack-owned schema), then
            // corrupt `generation` in place so cursor_get's column-type match
            // falls through to its "malformed" error arm.
            commit_channel_cursor(
                &registry,
                BROKEN_KIND,
                BROKEN_KIND,
                &khive_channel::ChannelCheckpoint {
                    source: "seed".to_string(),
                    generation: 1,
                    high_water: Some(1),
                },
            )
            .await
            .expect("seed cursor_commit must succeed");

            let sql = runtime.sql();
            {
                let mut w = sql.writer().await.expect("writer");
                w.execute(SqlStatement {
                    sql: "UPDATE comm_channel_cursor SET generation = 1.5 \
                          WHERE channel_kind = ?1 AND channel_slug = ?2"
                        .into(),
                    params: vec![
                        SqlValue::Text(BROKEN_KIND.to_string()),
                        SqlValue::Text(BROKEN_KIND.to_string()),
                    ],
                    label: Some("test_corrupt_generation".into()),
                })
                .await
                .expect("corrupting update must succeed");
            }

            let control_calls = Arc::new(Mutex::new(Vec::new()));
            let broken_calls = Arc::new(Mutex::new(Vec::new()));

            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(RecordingChannel {
                kind: "mock_control",
                since_calls: control_calls.clone(),
            }));
            ch_registry.register(Arc::new(RecordingChannel {
                kind: BROKEN_KIND,
                since_calls: broken_calls.clone(),
            }));

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Tick 1: control succeeds (records the tick's floor); the
            // broken channel's cursor_get fails on the corrupted row, so it
            // is skipped and records nothing.
            advance_until_calls(&control_calls, 1).await;
            assert_eq!(
                control_calls.lock().unwrap().len(),
                1,
                "control channel must be polled on the first tick"
            );
            assert_eq!(
                broken_calls.lock().unwrap().len(),
                0,
                "the broken channel must be skipped while cursor_get fails"
            );

            // Repair the row so cursor_get succeeds from the next tick on.
            {
                let mut w = sql.writer().await.expect("writer");
                w.execute(SqlStatement {
                    sql: "UPDATE comm_channel_cursor SET generation = 1 \
                          WHERE channel_kind = ?1 AND channel_slug = ?2"
                        .into(),
                    params: vec![
                        SqlValue::Text(BROKEN_KIND.to_string()),
                        SqlValue::Text(BROKEN_KIND.to_string()),
                    ],
                    label: Some("test_repair_generation".into()),
                })
                .await
                .expect("repairing update must succeed");
            }

            // Tick 2: cursor_get now succeeds and the broken channel is
            // finally polled for the first time.
            advance_until_calls(&broken_calls, 1).await;
            task.abort();

            let control_first = control_calls.lock().unwrap()[0];
            let broken_first = *broken_calls
                .lock()
                .unwrap()
                .first()
                .expect("the broken channel must have been polled after recovery");

            assert_eq!(
                broken_first, control_first,
                "the broken channel's first poll_page call must see the SAME \
                 bootstrap floor as the control channel's very first call \
                 ({control_first:?}), not a later tick's timestamp \
                 ({broken_first:?}) -- the cursor_get failure must not have \
                 lost the earlier floor"
            );
        }

        /// Cross-date regression (issue #449 High, failure shape 2): an
        /// ingest failure that blocks a channel's first-ever `cursor_commit`
        /// must not lose that channel's bootstrap floor either. Uses the
        /// same same-run control-channel comparison as the cursor_get test
        /// above.
        #[tokio::test(start_paused = true)]
        async fn quarantine_ingest_failure_blocking_first_commit_preserves_the_bootstrap_floor() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            let control_calls = Arc::new(Mutex::new(Vec::new()));
            let failing_calls = Arc::new(Mutex::new(Vec::new()));

            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(RecordingChannel {
                kind: "mock_control",
                since_calls: control_calls.clone(),
            }));
            ch_registry.register(Arc::new(IngestFailsOnceChannel {
                call_count: AtomicUsize::new(0),
                since_calls: failing_calls.clone(),
            }));

            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Tick 1: control succeeds; the ingest-failing channel is polled
            // (records `since`) but its one envelope fails comm.ingest, so
            // no checkpoint is committed for it this tick.
            advance_until_calls(&control_calls, 1).await;
            advance_until_calls(&failing_calls, 1).await;
            assert_eq!(control_calls.lock().unwrap().len(), 1);
            assert_eq!(
                failing_calls.lock().unwrap().len(),
                1,
                "poll_page is still called even though ingest will fail"
            );

            // Tick 2: the channel is polled again (its envelope now ingests
            // cleanly with no bad message), and its cycle finally succeeds.
            advance_until_calls(&failing_calls, 2).await;
            task.abort();

            let control_first = control_calls.lock().unwrap()[0];
            let failing_calls = failing_calls.lock().unwrap();
            assert_eq!(
                failing_calls.len(),
                2,
                "the channel must have been polled again on the second tick"
            );

            assert_eq!(
                failing_calls[0], control_first,
                "the first poll_page call's `since` must match the control \
                 channel's first-tick floor"
            );
            assert_eq!(
                failing_calls[1], control_first,
                "the SECOND poll_page call's `since` must still match the \
                 same original floor ({control_first:?}), not a fresh \
                 timestamp from the tick where the ingest failure blocked \
                 the first commit ({:?}) -- otherwise a failure spanning a \
                 calendar-day boundary would permanently skip the earlier \
                 day's uncommitted mail",
                failing_calls[1]
            );
        }

        /// First-tick regression (issue #449): the
        /// very first bootstrap floor a channel ever sees must be seeded
        /// from when the daemon started, not from whenever the loop's
        /// first sleep happens to finish. Runs on a live (unpaused) clock
        /// on purpose -- `tokio::time::pause` only fast-forwards the
        /// virtual timer, not `Utc::now()`, so it cannot observe a
        /// regression where `since` is captured after the sleep instead of
        /// before the loop is entered. If the loop ever goes back to
        /// computing that floor post-sleep, a daemon started just before
        /// UTC midnight whose first tick lands just after it would seed
        /// the new day and permanently skip the previous day's mail.
        #[tokio::test]
        async fn first_tick_uses_startup_time_not_post_sleep_time() {
            let runtime = KhiveRuntime::memory().expect("in-memory runtime");
            let mut builder = VerbRegistryBuilder::new();
            khive_runtime::PackRegistry::register_packs(
                &["kg".to_string(), "comm".to_string()],
                runtime.clone(),
                &mut builder,
            )
            .expect("register kg+comm through the factory path");
            let registry = builder.build().expect("registry builds");

            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut ch_registry = ChannelRegistry::new();
            ch_registry.register(Arc::new(RecordingChannel {
                kind: "mock_startup_clock",
                since_calls: calls.clone(),
            }));

            let startup = Utc::now();
            let task = tokio::spawn(channel_poll_loop(
                Arc::new(ch_registry),
                registry.clone(),
                "test-ns".to_string(),
                "actor:test".to_string(),
            ));

            // Real-time wait for the loop's first (~5s) tick to fire.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if !calls.lock().unwrap().is_empty() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "first poll_page call did not arrive within 15s"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            task.abort();

            let first_since = calls.lock().unwrap()[0];
            let drift_ms = (first_since - startup).num_milliseconds().abs();
            assert!(
                drift_ms < 2_000,
                "the first poll_page call's `since` ({first_since:?}) must \
                 reflect the daemon's startup time ({startup:?}), not a \
                 timestamp captured after the loop's first ~5s sleep -- a \
                 {drift_ms}ms drift means the floor is still seeded \
                 post-sleep, which would drop a full day of mail if that \
                 sleep happened to cross a calendar-day boundary"
            );
        }
    }

    /// The sweep-lifecycle guard funnels every serve-path early return
    /// through the unconditional sweep shutdown: transport-resolution
    /// failure must still complete promptly (the sweep task is shut down
    /// and awaited, not leaked past the error return).
    #[tokio::test]
    #[serial]
    async fn serve_with_session_sweep_completes_shutdown_on_unknown_transport() {
        use clap::Parser;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("khive.db");
        let config_path = write_config(dir.path(), "");
        let args = Args::parse_from([
            "kkernel",
            "--db",
            db_path.to_str().expect("utf8 path"),
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--transport",
            "no-such-transport",
            "--no-embed",
            "--pack",
            "kg",
        ]);
        let (server, _schedule_rt) = build_server(&args).await.expect("build server");

        let registry = TransportRegistry::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            serve_with_session_sweep(server, &args, &registry),
        )
        .await
        .expect("guard must complete promptly, including sweep shutdown")
        .expect_err("unknown transport must fail resolution");
        assert!(
            err.to_string().contains("unknown transport"),
            "unexpected error: {err}"
        );

        // No heartbeat entry may survive the guard's explicit shutdown —
        // the sweep never went over-threshold here, and its clean-shutdown
        // path removes any heartbeat it did write.
        let heartbeat = dir
            .path()
            .join("khive.db.walpin")
            .join(format!("{}.json", std::process::id()));
        assert!(
            !heartbeat.exists(),
            "sweep heartbeat must not survive the serve guard"
        );
    }

    /// Discriminating regression for the sweep-lifecycle guard: the old
    /// code's early `?` return on transport resolution dropped the handle
    /// without awaiting shutdown, so the task's exit raced the caller's
    /// resume. The guard must await `SessionSweepHandle::shutdown` on the
    /// error path — observed here via an injected task whose completion
    /// flag flips only after it receives the shutdown signal, checked
    /// synchronously the moment the guard returns.
    #[tokio::test]
    #[serial]
    async fn serve_guard_awaits_sweep_shutdown_before_returning() {
        use clap::Parser;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("khive.db");
        let config_path = write_config(dir.path(), "");
        let args = Args::parse_from([
            "kkernel",
            "--db",
            db_path.to_str().expect("utf8 path"),
            "--config",
            config_path.to_str().expect("utf8 path"),
            "--transport",
            "no-such-transport",
            "--no-embed",
            "--pack",
            "kg",
        ]);
        let (server, _schedule_rt) = build_server(&args).await.expect("build server");

        let completed = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
        let join = tokio::spawn({
            let completed = Arc::clone(&completed);
            async move {
                let _ = shutdown_rx.changed().await;
                completed.store(true, Ordering::SeqCst);
            }
        });
        let handle = SessionSweepHandle { shutdown_tx, join };

        let registry = TransportRegistry::default();
        let err = serve_holding_sweep(Some(handle), server, &args, &registry)
            .await
            .expect_err("unknown transport must fail resolution");
        assert!(
            err.to_string().contains("unknown transport"),
            "unexpected error: {err}"
        );
        assert!(
            completed.load(Ordering::SeqCst),
            "the guard must await sweep shutdown before returning on the \
             transport-resolution error path — an unawaited (dropped) handle \
             leaves this flag unset at the moment the guard returns"
        );
    }
}
