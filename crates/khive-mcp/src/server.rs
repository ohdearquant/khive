//! KhiveMcpServer — rmcp-based MCP server exposing a single `request` tool.
//!
//! Accepts the function-call DSL or JSON form and dispatches each parsed operation
//! through the [`VerbRegistry`] built from the configured packs.
//!
// FILE SIZE JUSTIFICATION: `run_parsed` is long because it encodes the
// execution-mode contract (Single/Parallel/Chain) as a single match
// expression. Splitting the three branches into separate functions would
// scatter the contract invariants (summary shape, aborted semantics,
// $prev substitution ordering) across files, making them harder to review
// as a unit. The module is the authoritative implementation of request
// dispatch and is intentionally co-located.

use std::{
    collections::HashMap,
    future::Future,
    ops::Range,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use futures::{stream::FuturesUnordered, StreamExt};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use khive_db::ConnectionPool;
use khive_pack_kg::handlers::{search_rank_fields, SearchSubstrate, ValidatedSearchRequest};
use khive_request::{
    parse_request, parse_typed_json_batch, unit_write_key_conflicts, ArgValue, DslError,
    ExecutionMode, ParsedOp, ParsedRequest, PrevFailure, TypedJsonOp,
};
use khive_runtime::daemon::DAEMON_LEXICAL_TIMEOUT_MARKER;
use khive_runtime::presentation::{
    prepare_format_value_with_note_content, present_with_policy_at,
    render_format_with_note_content, NoteContentScope, PresentationNow,
};
use khive_runtime::{
    present_with_policy, render_format, DispatchError, DomainDisposition,
    InterceptedDispatchResult, KhiveRuntime, OutputFormat, PackLoadError, PackRegistry,
    PresentationMode, RuntimeConfig, RuntimeError, VerbPresentationPolicy, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::RefusalReason;

use khive_storage::EdgeRelation;

use crate::coordinator::CoordinatorService;
use crate::tools::request::RequestParams;

const _: () = assert!(
    khive_runtime::daemon::ERROR_DETAIL_NESTING_DEPTH_LIMIT == khive_request::NESTING_DEPTH_LIMIT
);

static BRIDGE_INSTANCE_ID: std::sync::OnceLock<uuid::Uuid> = std::sync::OnceLock::new();

pub(crate) fn bridge_instance_id() -> uuid::Uuid {
    *BRIDGE_INSTANCE_ID.get_or_init(uuid::Uuid::new_v4)
}

mod disk_policy;
mod search_diagnostics;

use disk_policy::disk_guard_policy_fingerprint;

use search_diagnostics::{
    backend_errors_value, op_success_from_registry_result, search_arm_participation_value,
    search_incomplete_error, validated_search_text_mode, OpSuccess, SearchDegradation,
    SearchStatus,
};
#[cfg(test)]
use search_diagnostics::{
    bounded_backend_error_key, bounded_backend_error_message, search_diagnostic_value,
    search_diagnostic_wire_len, search_retry_after_ms, BackendErrorDiagnostic, SearchArmEvidence,
    SearchArmParticipation, SearchArmStatus, MAX_BACKEND_ERROR_ENTRIES,
    MAX_BACKEND_ERROR_KEY_CHARS, MAX_BACKEND_ERROR_MESSAGE_CHARS,
    MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP, MISSING_BACKEND_ERROR_MESSAGE,
};

/// Per-request parallelism stays bounded even when the parser accepts 100 ops; must be nonzero.
const MAX_BATCH_CONCURRENCY: usize = 8;

/// Half the frame remains for the daemon's outer serialization and budget-error entries.
const BATCH_RESPONSE_BUDGET_BYTES: usize = khive_runtime::daemon::MAX_FRAME_BYTES / 2;

struct BatchTask<F> {
    index: usize,
    tool: String,
    future: F,
}

/// One bracketed-batch unit's future (ADR-016 Amendment 2): a linear chain
/// dispatched under [`execute_bounded_units`]'s concurrency cap.
struct UnitTask<F> {
    future: F,
}

/// One unit's flattened leaf entries plus the `parse_content` recomputations
/// its own dispatched leaves produced (mirroring plain chain mode's per-step
/// recompute), reported back to the caller for merging into the shared
/// request-wide `parse_content` vector.
struct UnitOutcome {
    unit_index: usize,
    entries: Vec<Value>,
    content_updates: Vec<(usize, bool)>,
}

/// The ADR-016 Amendment 2 aggregate response budget, shared by every unit of
/// one bracketed batch of chains. Every leaf checks [`Self::is_breached`]
/// before it dispatches and calls [`Self::record`] after it produces its
/// final entry, so the total is spent once across the whole request
/// regardless of how many units or leaves are concurrently in flight.
struct UnitBudget {
    limit: usize,
    state: std::sync::Mutex<(usize, bool)>,
}

impl UnitBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            state: std::sync::Mutex::new((0, false)),
        }
    }

    /// `true` once the aggregate budget has been exhausted. Checked before
    /// every leaf, including a continuation of an already-active unit, so no
    /// new leaf is admitted anywhere in the request past this point.
    fn is_breached(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }

    /// Records `entry`'s serialized size against the shared total. An entry
    /// that itself exhausts the budget is still kept as-is; it already ran
    /// and its disposition is real; only the *next* leaf anywhere in the
    /// request is refused.
    fn record(&self, entry: &Value) {
        let bytes = serde_json::to_vec(entry)
            .expect("serde_json::Value is always serializable")
            .len();
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.0 = guard.0.saturating_add(bytes);
        if guard.0 > self.limit {
            guard.1 = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchOrigin {
    Local,
    Daemon,
}

#[derive(Clone, Copy)]
struct RunParsedContext<'a> {
    enforce_response_budget: bool,
    max_batch_concurrency: usize,
    from_wire: bool,
    identity: Option<&'a khive_runtime::RequestIdentity>,
}

#[derive(Clone, Copy)]
struct ParsedDispatchPolicy {
    strict_refusals: bool,
    max_batch_concurrency: usize,
}

impl ParsedDispatchPolicy {
    const fn bounded_parallel(strict_refusals: bool) -> Self {
        Self {
            strict_refusals,
            max_batch_concurrency: MAX_BATCH_CONCURRENCY,
        }
    }

    const fn serial(strict_refusals: bool) -> Self {
        Self {
            strict_refusals,
            max_batch_concurrency: 1,
        }
    }
}

/// Typed failure crossing the dispatch/envelope seam.
///
/// `error` retains the pre-existing human or structured payload. `reason` is
/// an additive machine classification and is absent for ordinary validation,
/// storage, transport, coordinator, and authorization-gate failures.
#[derive(Debug)]
struct DispatchFailure {
    tool: String,
    error: Value,
    reason: Option<RefusalReason>,
}

impl DispatchFailure {
    fn before_dispatch(tool: impl Into<String>, error: Value) -> Self {
        Self::with_disposition(tool, error, DomainDisposition::NotCommitted)
    }

    fn committed(tool: impl Into<String>, error: Value) -> Self {
        Self::with_disposition(tool, error, DomainDisposition::Committed)
    }

    fn with_disposition(
        tool: impl Into<String>,
        error: Value,
        disposition: DomainDisposition,
    ) -> Self {
        Self {
            tool: tool.into(),
            error: error_with_disposition(error, disposition),
            reason: None,
        }
    }

    fn from_dispatch(tool: &str, error: DispatchError) -> Self {
        let (error, disposition) = error.into_parts();
        let reason = match error.refusal_source() {
            RuntimeError::SecretDetected(_) => Some(RefusalReason::GateRefusal),
            RuntimeError::UnknownVerb(_) => Some(RefusalReason::VerbRefused),
            error if error.is_stream_policy_refusal() => Some(RefusalReason::PolicyRefusal),
            _ => None,
        };
        Self {
            tool: tool.into(),
            error: runtime_error_value(error, disposition),
            reason,
        }
    }

    fn into_entry(self) -> Value {
        let disposition = error_disposition(&self.error);
        let mut entry = failure_entry(self.tool, self.error, disposition);
        if let Some(reason) = self.reason {
            entry["reason"] = json!(reason.as_str());
        }
        entry
    }
}

/// One constructor for per-op failures. Moving values avoids recursively
/// serializing a canonical result before its depth has been checked.
/// The entry-level disposition comes from the same authoritative argument as
/// the nested error, so callers inspecting `ok` also see the domain outcome.
fn failure_entry(tool: impl Into<String>, error: Value, disposition: DomainDisposition) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), Value::Bool(false));
    entry.insert("tool".into(), Value::String(tool.into()));
    entry.insert("domain_disposition".into(), json!(disposition.as_str()));
    entry.insert("error".into(), error_with_disposition(error, disposition));
    Value::Object(entry)
}

fn aborted_entry(tool: impl Into<String>, message: Option<String>) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("ok".into(), Value::Bool(false));
    entry.insert("tool".into(), Value::String(tool.into()));
    entry.insert("aborted".into(), Value::Bool(true));
    entry.insert(
        "domain_disposition".into(),
        json!(DomainDisposition::NotCommitted.as_str()),
    );
    if let Some(message) = message {
        entry.insert("message".into(), Value::String(message));
    }
    Value::Object(entry)
}

/// Missing/foreign disposition is uncertainty, never permission to replay.
fn error_disposition(error: &Value) -> DomainDisposition {
    match error.get("domain_disposition").and_then(Value::as_str) {
        Some("committed") => DomainDisposition::Committed,
        Some("not_committed") => DomainDisposition::NotCommitted,
        _ => DomainDisposition::Unknown,
    }
}

fn error_with_disposition(error: Value, disposition: DomainDisposition) -> Value {
    let mut error = match error {
        Value::Object(map) => map,
        Value::String(message) => serde_json::Map::from_iter([
            ("kind".into(), json!("runtime_error")),
            ("message".into(), Value::String(message)),
        ]),
        other => {
            drop_value_iteratively(other);
            serde_json::Map::from_iter([
                ("kind".into(), json!("runtime_error")),
                ("message".into(), json!("operation failed")),
            ])
        }
    };
    if let Some(result) = error.remove("domain_result") {
        if disposition != DomainDisposition::Committed {
            // A nested operation's result is not proof of the outer result.
            drop_value_iteratively(result);
        } else if !result_within_depth_limit(&result) {
            drop_value_iteratively(result);
            error.insert("code".into(), json!("result_too_deep"));
            error.insert(
                "message".into(),
                json!("committed domain result omitted because it exceeds the nesting depth limit"),
            );
        } else {
            error.insert("domain_result".into(), result);
        }
    }
    error.insert("domain_disposition".into(), json!(disposition.as_str()));
    Value::Object(error)
}

/// Fingerprint the engine-coherence parts of a resolved [`RuntimeConfig`].
///
/// Identical resolved configurations produce the same id. A daemon may also
/// serve a compatible client with a different id when it has a superset of
/// the client's requested extra embedders. Every other field must match:
/// same pack set (order-independent), same storage target and effective access
/// mode, same primary embedder, same backend topology/routing, and same
/// construction-baked fresh-tail, blob-hydration, outbound, caller-enrollment,
/// and git-write policies.
/// Identity fields (`namespace`, `actor_id`, `visible_namespaces`) are carried
/// per request in the daemon frame and must never enter this key. The daemon
/// compares this against each forwarded request's `config_id` and rejects any
/// difference outside the extra-embedder superset rule, so a restricted client
/// cannot execute through a broader runtime with incompatible behavior.
///
/// When `khive_cfg` is supplied and contains a non-empty `[[backends]]`
/// declaration, the backend topology (sorted backend list, explicit read-only
/// modes, effective WAL ceilings, served-substrate declarations, and
/// pack→backend assignments) is
/// folded into the fingerprint so that two configs differing only in routing,
/// access mode, or effective ceiling produce different ids (ADR-049 / B-SHOULD-FIX-4).
/// Delimiter-free topologies retain their legacy field encoding; a topology
/// containing reserved delimiter text uses an injective, escaped v2 encoding
/// so path data can never impersonate access mode.
///
/// When `khive_cfg` is `None` or its `backends` list is empty, the implicit
/// main backend's effective WAL ceiling is folded into the `backend` field.
/// An existing path with no filesystem write bits gains the read-only backend
/// marker before the runtime opens it, so forwarding and server fingerprints
/// converge.
///
/// `config.db_path` and each declared backend path are canonicalized against
/// the process's current working directory before entering the fingerprint. A
/// raw relative string (e.g. `./data/main.db`) would otherwise fingerprint
/// identically for two different projects that happen to declare or override
/// the same relative path, even though they resolve to two different files —
/// letting a warm daemon started for one project accept requests meant for
/// the other's database.
pub fn compute_config_id(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        khive_runtime::ann_fresh_tail_enabled_from_env(),
        configured_storage_read_only(config, khive_cfg),
    )
}

/// Compute the daemon identity with an already-snapshotted ADR-118 policy.
///
/// Test-only compatibility wrapper for exercising one already-snapshotted
/// policy. Runtime-owning call sites pass both captured policies through
/// [`compute_config_id_with_runtime_policies`].
#[cfg(test)]
pub(crate) fn compute_config_id_with_ann_fresh_tail(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    ann_fresh_tail_enabled: bool,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        ann_fresh_tail_enabled,
        configured_storage_read_only(config, khive_cfg),
    )
}

fn configured_storage_read_only(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
) -> bool {
    if let Some(main) = khive_cfg
        .filter(|cfg| !cfg.backends.is_empty())
        .and_then(|cfg| cfg.backends.iter().find(|backend| backend.name == "main"))
    {
        return main.kind == khive_runtime::BackendKind::Sqlite && main.read_only;
    }

    config.db_path.as_ref().is_some_and(|path| {
        std::fs::metadata(khive_runtime::expand_tilde(path))
            .is_ok_and(|metadata| metadata.permissions().readonly())
    })
}

/// Compute the daemon identity with an authoritative effective storage mode.
///
/// A chmod-detected snapshot has the same configured path as its writable
/// source but cannot safely share a warm daemon with it: the writable daemon
/// would omit the audit advisory and could retain a write-capable file handle.
/// Fold the effective main-backend mode into the existing `backend` component
/// so the mismatch remains parseable as a structured backend mismatch.
/// Pre-open callers that have already applied a storage override (for example,
/// multi-backend `--db :memory:`) must use this form rather than re-reading
/// the superseded declaration through [`compute_config_id`].
pub fn compute_config_id_with_storage_mode(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    storage_read_only: bool,
) -> String {
    compute_config_id_with_runtime_policies(
        config,
        khive_cfg,
        khive_runtime::ann_fresh_tail_enabled_from_env(),
        storage_read_only,
    )
}

mod config_id;
pub(crate) use config_id::compute_config_id_with_runtime_policies;

/// Reserved syntax in the legacy topology spelling.
///
/// Keeping the legacy representation when every caller-controlled component
/// excludes these bytes avoids needless changes to topology encoding without
/// retaining its ambiguity. The v2 marker itself contains `|`, so a safe
/// legacy value can never equal a v2 value.
fn legacy_topology_component_is_safe(value: &str) -> bool {
    !value
        .bytes()
        .any(|byte| matches!(byte, b':' | b',' | b'[' | b']' | b'=' | b';' | b'|'))
}

/// Percent-encode a v2 topology field so its payload can contain none of the
/// structural `:`, `,`, or `=` delimiters. `%` itself is always escaped, making
/// the mapping injective over the original UTF-8 bytes.
fn escape_topology_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/') {
            escaped.push(byte as char);
        } else {
            escaped.push('%');
            escaped.push(HEX[(byte >> 4) as usize] as char);
            escaped.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    escaped
}

/// Format a backend's served-kinds fingerprint component (`""` when the
/// backend serves everything, `:serves=<kind>+<kind>` otherwise), shared by
/// both the legacy and escaped topology encodings below so they always agree
/// on the same served-kinds suffix for the same input.
fn format_served_kinds_suffix(served_kinds: Option<&str>) -> String {
    served_kinds
        .map(|kinds| format!(":serves={kinds}"))
        .unwrap_or_default()
}

/// Identity spelling of an effective WAL ceiling. The numeric value is always
/// encoded, zero included: a disabled ceiling is an explicit policy that a
/// daemon must be able to report, so a client with a disabled ceiling must not
/// reuse a daemon built before ceilings existed. That daemon fingerprints
/// differently and the client falls back to local dispatch until it restarts.
fn wal_ceiling_identity_suffix(effective_bytes: u64) -> String {
    format!(":wal_ceiling_bytes={effective_bytes}")
}

/// Only the ceiling enforced by a writable SQLite backend participates in
/// daemon identity. The configured value and its source remain operator
/// diagnostics; a read-only or in-memory backend enforces no writer policy.
fn effective_named_wal_ceiling_bytes(
    config: &RuntimeConfig,
    backend: &khive_runtime::BackendConfig,
) -> u64 {
    if backend.read_only || backend.kind != khive_runtime::BackendKind::Sqlite {
        0
    } else {
        backend
            .wal_ceiling_bytes
            .unwrap_or(config.wal_ceiling_configured_bytes)
    }
}

fn encode_backend_topology(cfg: &khive_runtime::KhiveConfig, config: &RuntimeConfig) -> String {
    let mut legacy_safe = true;
    let mut backend_rows: Vec<(String, String, String, bool, Option<String>, u64)> = cfg
        .backends
        .iter()
        .map(|backend| {
            let kind = format!("{:?}", backend.kind);
            let path = backend
                .path
                .as_deref()
                .map(canonical_fingerprint_path)
                .unwrap_or_else(|| ":memory:".to_string());
            legacy_safe &= legacy_topology_component_is_safe(&backend.name)
                && legacy_topology_component_is_safe(&kind)
                && backend
                    .path
                    .as_ref()
                    .is_none_or(|_| legacy_topology_component_is_safe(&path));
            let served_kinds = backend.served_kinds.as_ref().map(|kinds| {
                kinds
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join("+")
            });
            (
                backend.name.clone(),
                kind,
                path,
                backend.read_only,
                served_kinds,
                effective_named_wal_ceiling_bytes(config, backend),
            )
        })
        .collect();
    backend_rows.sort();

    let mut pack_rows: Vec<(String, String, bool)> = cfg
        .packs
        .iter()
        .map(|(pack, pack_config)| {
            legacy_safe &= legacy_topology_component_is_safe(pack)
                && legacy_topology_component_is_safe(&pack_config.backend);
            (
                pack.clone(),
                pack_config.backend.clone(),
                pack_config.no_embed,
            )
        })
        .collect();
    pack_rows.sort();

    let (backends, pack_backends) = if legacy_safe {
        let backends = backend_rows
            .iter()
            .map(
                |(name, kind, path, is_read_only, served_kinds, wal_ceiling_bytes)| {
                    let read_only = if *is_read_only { ":read_only" } else { "" };
                    let served_kinds = format_served_kinds_suffix(served_kinds.as_deref());
                    let wal_ceiling = wal_ceiling_identity_suffix(*wal_ceiling_bytes);
                    format!("{name}:{kind}:{path}{read_only}{served_kinds}{wal_ceiling}")
                },
            )
            .collect::<Vec<_>>()
            .join(",");
        let pack_backends = pack_rows
            .iter()
            .map(|(pack, backend, no_embed)| {
                // `no_embed` changes runtime behavior (that pack's runtime
                // carries zero embedders), so it must move the fingerprint;
                // emitted only when set so pre-existing configs keep their id.
                let no_embed = if *no_embed { ":no_embed" } else { "" };
                format!("{pack}={backend}{no_embed}")
            })
            .collect::<Vec<_>>()
            .join(",");
        (backends, pack_backends)
    } else {
        let backends = backend_rows
            .iter()
            .map(
                |(name, kind, path, read_only, served_kinds, wal_ceiling_bytes)| {
                    let mode = if *read_only { "r" } else { "w" };
                    let served_kinds = format_served_kinds_suffix(served_kinds.as_deref());
                    let wal_ceiling = wal_ceiling_identity_suffix(*wal_ceiling_bytes);
                    format!(
                        "{}:{}:{}:{mode}{served_kinds}{wal_ceiling}",
                        escape_topology_component(name),
                        escape_topology_component(kind),
                        escape_topology_component(path),
                    )
                },
            )
            .collect::<Vec<_>>()
            .join(",");
        let pack_backends = pack_rows
            .iter()
            .map(|(pack, backend, no_embed)| {
                let no_embed = if *no_embed { ":no_embed" } else { "" };
                format!(
                    "{}={}{no_embed}",
                    escape_topology_component(pack),
                    escape_topology_component(backend),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        (format!("v2|{backends}"), format!("v2|{pack_backends}"))
    };

    format!(";backends=[{backends}];pack_backends=[{pack_backends}]")
}

/// Resolve any path headed into `config_id` fingerprinting — a declared
/// `[[backends]].path` or the resolved `RuntimeConfig.db_path` (itself
/// derived from `--db`/`KHIVE_DB`) — to a stable, cwd-independent string
/// without creating anything on disk.
///
/// Delegates to [`crate::serve::canonical_path_no_side_effects`] — the same
/// no-side-effects canonicalization the `--db` override equivalence check
/// uses — so a relative path resolves against the process's current working
/// directory the same way a real backend open would. Falls back to the raw
/// display string only on a canonicalization error (e.g. an unreadable
/// ancestor directory); this is strictly no worse than the pre-fix behavior,
/// which always used the raw string.
fn canonical_fingerprint_path(path: &std::path::Path) -> String {
    crate::serve::canonical_path_no_side_effects(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Build a sorted, human-readable verb catalog from `(pack_name, verb_name, description)` triples.
///
/// When multiple packs register the same verb name, each pack's description is
/// emitted on its own continuation line with a `[pack]` prefix so the caller can
/// see every contributing pack. A `tracing::warn!` is emitted once per duplicate.
fn build_verb_catalog(verbs: impl IntoIterator<Item = (String, String, String)>) -> String {
    let mut by_verb: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (pack_name, verb_name, description) in verbs {
        by_verb
            .entry(verb_name)
            .or_default()
            .push((pack_name, description));
    }
    let mut out = String::new();
    for (name, pack_descs) in &by_verb {
        if pack_descs.len() > 1 {
            let packs: Vec<&str> = pack_descs.iter().map(|(p, _)| p.as_str()).collect();
            tracing::warn!(
                verb = %name,
                packs = ?packs,
                "verb registered by multiple packs; all descriptions included in catalog"
            );
        }
        out.push_str("  ");
        out.push_str(name);
        out.push_str(" — ");
        if pack_descs.len() == 1 {
            out.push_str(&pack_descs[0].1);
        } else {
            for (i, (pack, desc)) in pack_descs.iter().enumerate() {
                if i > 0 {
                    out.push_str("\n    ");
                }
                out.push('[');
                out.push_str(pack);
                out.push_str("] ");
                out.push_str(desc);
            }
        }
        out.push('\n');
    }
    out
}

/// Runtime-mode admission for transport background work.
///
/// The inbound tasks dispatch only `comm.*` verbs (`comm.ingest`, heartbeat,
/// and cursor operations), and the outbound tasks scan, claim, and mark
/// outbound `message` notes through the runtime's non-wire owner-side APIs —
/// both against the comm pack's actual assigned runtime, since under a
/// `[packs.comm]` backend assignment that is the backend holding comm's
/// rows. Neither loop may run unless that runtime can durably record its
/// writes. The two decisions stay separate so a future topology can admit
/// one direction without the other.
///
/// Inbound polling also publishes each quarantined message's original bytes
/// through `blob.put`, so it needs the blob pack's runtime to accept writes as
/// well. A blob runtime that cannot write would fail that publish on every
/// poll, hold the cursor, and retry the same message forever; admission
/// refuses it up front instead, and `inbound_blocked_by_read_only_blob`
/// records that this was the reason so the refusal can be logged by name.
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelLoopAdmission {
    pub(crate) inbound_poll: bool,
    pub(crate) outbound_delivery: bool,
    pub(crate) inbound_blocked_by_read_only_blob: bool,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
impl ChannelLoopAdmission {
    fn for_single_runtime(runtime: &KhiveRuntime, packs: &[String]) -> Self {
        let comm_loaded = packs.iter().any(|pack| pack == "comm");
        let writable = !runtime.is_read_only();
        Self {
            inbound_poll: comm_loaded && writable,
            outbound_delivery: comm_loaded && writable,
            inbound_blocked_by_read_only_blob: false,
        }
    }

    /// `blob` is `None` when the blob pack is not loaded; the poll task's own
    /// storage readiness check reports that case.
    pub(crate) fn for_pack_runtimes(
        comm: Option<&KhiveRuntime>,
        blob: Option<&KhiveRuntime>,
    ) -> Self {
        let admitted = comm.is_some_and(|runtime| !runtime.is_read_only());
        let blob_read_only = blob.is_some_and(KhiveRuntime::is_read_only);
        Self {
            inbound_poll: admitted && !blob_read_only,
            outbound_delivery: admitted,
            inbound_blocked_by_read_only_blob: admitted && blob_read_only,
        }
    }
}

/// MCP server that dispatches all verbs through a [`VerbRegistry`].
#[derive(Clone)]
pub struct KhiveMcpServer {
    registry: VerbRegistry,
    #[cfg(unix)]
    bridge_executable: Option<Arc<std::sync::Mutex<crate::daemon::executable::BridgeExecutable>>>,
    stdio_bridge: bool,
    /// Namespace this registry was built for. The stdio client passes it to the
    /// daemon; a namespace mismatch triggers local-dispatch fallback.
    default_namespace: String,
    /// Fingerprint of the resolved runtime config (packs, db target, embedders).
    /// The stdio client passes it to the daemon; a config mismatch triggers
    /// local-dispatch fallback so a restricted client never runs through the
    /// broader default daemon.
    config_id: String,
    /// Cross-backend coordinator (ADR-029 Phase 2). Present only in multi-backend
    /// deployments. `None` in single-backend mode — all dispatch goes through the
    /// `VerbRegistry` unchanged (zero-change invariant).
    coordinator: Option<Arc<dyn CoordinatorService>>,
    /// The default-backend `KhiveRuntime` this server was built from, retained
    /// for non-wire background APIs that are genuinely default-backend scoped.
    /// Pack-routed owner operations must use their dedicated runtime handle
    /// below instead of assuming this one owns the row. `None` only for servers built via
    /// [`Self::from_registry`]/[`Self::from_registry_with_meta`] without an
    /// explicit [`Self::with_runtime`] call (test-only construction paths).
    runtime: Option<KhiveRuntime>,
    /// Runtime that owns the outbound `message` notes the delivery loops
    /// scan, claim, and mark — the comm pack's assigned runtime. Every one of
    /// those touches is deliberately non-wire (the generic verbs run on the
    /// kg/main runtime, which under a `[packs.comm]` backend assignment does
    /// not hold comm's rows). In a multi-backend topology this may differ
    /// from `runtime` (the default backend); retaining the exact comm
    /// runtime keeps scan, owner claim, and delivered-at update on one store.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    channel_outbox_runtime: Option<KhiveRuntime>,
    /// Pool arc for the WAL checkpoint background task. `None` for in-memory
    /// or registry-only servers that have no persistent database.
    pool: Option<Arc<ConnectionPool>>,
    /// File-backed backend pools beyond `pool` (ADR-091 Amendment 3
    /// fan-out): every additional backend a multi-backend boot wired, so the
    /// session sweep and the daemon's checkpoint ownership can cover them
    /// too. Always empty for a single-backend server — `pool` alone is that
    /// server's one backend.
    secondary_pools: Vec<Arc<ConnectionPool>>,
    /// Server-level default output format (ADR-078). Resolved from TOML →
    /// `KHIVE_OUTPUT_FORMAT` → builtin `json`. Per-request `format` fields
    /// override this at dispatch time.
    default_output_format: OutputFormat,
    /// Last instant at which this process's daemon schedule loop began a tick.
    /// Zero means this server instance has never observed the loop running.
    /// Shared by server clones but never persisted, so a replacement process
    /// cannot inherit a plausible-looking heartbeat from its predecessor.
    schedule_ticker_last_tick_micros: Arc<AtomicI64>,
    /// Per-verb-runtime write admission for email and Telegram background
    /// tasks. CLI daemon role is necessary but not sufficient: snapshot
    /// runtimes must never poll into a failing ingest path or send externally
    /// when delivery state cannot be durably marked.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    channel_loop_admission: ChannelLoopAdmission,
}

/// Failure reason inside a [`PackRegError`].
pub enum PackRegFailure {
    UnknownPack(String),
    DuplicatePack(String),
    MissingDependency { pack: String, dep: String },
    NoPublicVerbs { pack: String },
    Registry(khive_runtime::RuntimeError),
    Schema(khive_runtime::PackSchemaCollisionError),
}

/// Returned by [`KhiveMcpServer::with_packs`] when pack registration fails.
/// The original runtime is returned so the caller can recover.
pub struct PackRegError {
    pub failure: PackRegFailure,
    pub runtime: KhiveRuntime,
}

impl std::fmt::Debug for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut dbg = f.debug_struct("PackRegError");
        match &self.failure {
            PackRegFailure::UnknownPack(unknown) => dbg.field("unknown", unknown),
            PackRegFailure::DuplicatePack(pack) => dbg.field("duplicate_pack", pack),
            PackRegFailure::MissingDependency { pack, dep } => {
                dbg.field("pack", pack).field("missing_dep", dep)
            }
            PackRegFailure::NoPublicVerbs { pack } => dbg.field("pack", pack),
            PackRegFailure::Registry(source) => dbg.field("source", source),
            PackRegFailure::Schema(source) => dbg.field("schema", source),
        }
        .finish_non_exhaustive()
    }
}

impl std::fmt::Display for PackRegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.failure {
            PackRegFailure::UnknownPack(unknown) => write!(
                f,
                "unknown pack name {:?} — built-in packs: {}",
                unknown,
                builtin_pack_names().join(", ")
            ),
            PackRegFailure::DuplicatePack(pack) => write!(f, "duplicate pack {pack:?}"),
            PackRegFailure::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
            PackRegFailure::NoPublicVerbs { pack } => write!(
                f,
                "declared pack {pack:?} registers no public verbs and is not marked as \
                 intentionally vocabulary- or ontology-only"
            ),
            PackRegFailure::Registry(source) => write!(f, "pack registry build failed: {source}"),
            PackRegFailure::Schema(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for PackRegError {}

/// Built-in pack names known to this binary.
///
/// Sourced from `PackRegistry::discovered_names()` so the list always reflects
/// whatever pack crates are linked into the binary.
pub fn builtin_pack_names() -> Vec<&'static str> {
    PackRegistry::discovered_names()
}

/// Which MCP handshake mode [`KhiveMcpServer::serve_stdio`] should use for
/// this process instance (#714). Unix-only: the resumed-generation self-heal
/// re-exec this decides between requires `crate::daemon`'s Unix-only
/// mismatch-recovery machinery (in turn only ever armed by a Unix-domain-socket
/// daemon-forwarding protocol mismatch); non-Unix `serve_stdio` always takes
/// the plain handshake path (see its `#[cfg(not(unix))]` variant below).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdioServeMode {
    /// Normal MCP `initialize` handshake — the overwhelmingly common case.
    Handshake,
    /// Skip the handshake (`serve_directly`): this process is a resumed
    /// generation of a prior self-heal re-exec (`crate::daemon`, #714 §2.3).
    Resumed,
}

/// Pure decision behind [`StdioServeMode`], factored out so it is
/// unit-testable without driving real stdio I/O. `resumed_generation` is
/// [`crate::daemon::resumed_generation`]'s return value.
#[cfg(unix)]
fn stdio_serve_mode_for(resumed_generation: Option<u32>) -> StdioServeMode {
    match resumed_generation {
        Some(_) => StdioServeMode::Resumed,
        None => StdioServeMode::Handshake,
    }
}

/// Optional idle timeout for a stdio bridge session: when configured, no
/// request for this long and the session closes (see
/// [`crate::transport::CancelOnEofTransport`]), releasing its reader-pool
/// admission and DB connection — a client that comes back simply respawns the
/// bridge, seamlessly from its perspective.
/// This closes only a genuinely idle session: an admitted request with a
/// response still being written — running long, or delivered slowly to a
/// backpressured reader — defers the close rather than being cancelled out
/// from under it, up to the separate response-delivery bound documented on
/// [`stdio_bridge_response_deadline_from_env`].
///
/// **Off unless configured, and that is a deliberate reading of existing
/// repository law rather than caution.** ADR-091 enumerates "kill long-lived
/// reader sessions" among its rejected alternatives, on the ground that
/// long-lived stdio sessions are live Claude Code instances and closing them
/// by age is a worse user experience than bounding what they hold underneath
/// them. Closing a session after an hour of quiet is that rejected policy
/// whatever the mechanism, because this transport has no signal that
/// separates an abandoned pipe from a live client that simply has not been
/// asked anything. Defaulting it on would reverse an accepted decision from
/// inside an unrelated change, so the default is off and turning it on is an
/// operator's explicit act — a supervised deployment, a CI harness, or any
/// context where session churn is cheap and a pinned WAL connection is not.
/// Making it the default requires amending ADR-091, not a different number
/// here.
///
/// Set `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS` to a positive number of seconds to
/// enable it. `0`, absent, and unparsable all leave it disabled; unparsable
/// falls back rather than panicking, matching this codebase's other
/// `_from_env` helpers, and it falls back to *disabled* because a typo must
/// never silently start closing live sessions. 3600 is the suggested value
/// where it is wanted: long enough that ordinary gaps in a live session never
/// trip it.
fn stdio_bridge_idle_timeout_from_env() -> Option<std::time::Duration> {
    let secs = khive_storage::read_env_number::<u64>("KHIVE_BRIDGE_IDLE_TIMEOUT_SECS").unwrap_or(0);
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

/// How long an admitted request whose response has not been written keeps
/// deferring the idle close.
///
/// The idle check must not treat a session with work outstanding as idle, or
/// it cancels a running handler out from under itself. But rmcp spawns each
/// request handler and drops the join handle, so a handler that panics never
/// reaches the response construction that would clear its obligation. Deferring
/// on an outstanding obligation with no bound therefore hands any panicking
/// handler the power to disable idle reaping for the life of the session — the
/// exact unbounded lifetime the idle timeout exists to close, reintroduced
/// through the guard that protects it.
///
/// This bound is separate from the idle window on purpose. Reusing the idle
/// window would mean a handler that runs longer than one quiet window stops
/// protecting its own session, which is the guarantee the obligation exists to
/// provide. It is set far above any real handler and answers a different
/// question: not "has this session been quiet" but "has this request been
/// outstanding so long that its handler must be gone".
///
/// Overridable via `KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS`; `0` disables the
/// bound, restoring the unbounded defer. An unparsable value falls back to the
/// default. Default: 3600s.
fn stdio_bridge_request_obligation_ttl_from_env() -> Option<std::time::Duration> {
    const DEFAULT_SECS: u64 = 3600;
    let secs = khive_storage::read_env_number::<u64>("KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS")
        .unwrap_or(DEFAULT_SECS);
    if secs == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(secs))
    }
}

/// Maximum number of requests a stdio bridge admits to rmcp while their
/// responses are still outstanding. A full session is closed before another
/// handler is spawned, bounding the per-session handler and obligation state.
///
/// Overridable via `KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS`. Values must be
/// positive; `0`, an unparsable value, or a value too large for this platform
/// falls back to the default. Default: 1024, enough for ordinary concurrent
/// MCP traffic while keeping a peer that stops reading from growing the
/// session without limit.
fn stdio_bridge_max_outstanding_requests_from_env() -> usize {
    khive_storage::read_env_number::<usize>("KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS")
        .filter(|&value| value > 0)
        .unwrap_or(crate::transport::DEFAULT_MAX_OUTSTANDING_REQUESTS)
}

/// Response-delivery deadline for a stdio bridge session: the longest a
/// single response write may stay pending before it is abandoned (see
/// [`crate::transport::CancelOnEofTransport::send`]) and this session is
/// closed. Independent of the idle timeout above — it bounds an admitted
/// request's response write directly, rather than the gap between
/// requests. Without this bound, a peer that admits a request and then
/// stops reading its response — while leaving the pipe itself open — keeps
/// that write pending forever, so the idle timeout would defer indefinitely
/// (an in-flight response always defers idle-close) and the session would
/// never be reaped.
///
/// Overridable via `KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS`, accepted range
/// 1..=u64::MAX seconds. Unlike `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS`, this bound
/// cannot be disabled: `0` is a startup error naming the variable, the
/// rejected value, and the accepted range, rather than a silent opt-out — a
/// configuration that restores an unbounded pending write restores the
/// defect this deadline exists to close (see the type doc above). An
/// unparsable value falls back to the default rather than erroring, matching
/// this codebase's other `_from_env` helpers. Default: 300s (5 minutes) —
/// long enough that legitimately slow verbs and ordinary reader backpressure
/// never trip it, short enough that a peer that has genuinely stopped
/// reading does not pin the session's reader-pool admission / DB connection
/// indefinitely.
fn stdio_bridge_response_deadline_from_env() -> anyhow::Result<std::time::Duration> {
    const DEFAULT_SECS: u64 = 300;
    let secs = match std::env::var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => secs,
            Err(_) => DEFAULT_SECS,
        },
        Err(_) => DEFAULT_SECS,
    };
    if secs == 0 {
        anyhow::bail!(
            "KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 is not accepted: the response-delivery \
             deadline cannot be disabled (a disabled deadline lets a peer that stops reading \
             pin the bridge's response write forever). Set it to a positive number of seconds \
             (accepted range: 1..=u64::MAX, default {DEFAULT_SECS})."
        );
    }
    Ok(std::time::Duration::from_secs(secs))
}

impl KhiveMcpServer {
    /// Build a server from `runtime.config().packs`. Errors if any pack is unknown or missing deps.
    ///
    /// This constructor assumes the supplied runtime's database is already at a
    /// complete V21 attachment cutover. It intentionally performs no migration
    /// or blob-evidence verification. Production hosts opening a database should
    /// use the async builders in [`crate::serve`] and reserve this constructor for
    /// already-prepared runtimes and tests.
    // The error variant intentionally carries the runtime so callers can recover.
    #[allow(clippy::result_large_err)]
    pub fn new(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        let packs: Vec<String> = runtime.config().packs.clone();
        // Fail-fast on bad packs so callers can decide recovery.
        // Schema plan application happens inside with_packs.
        Self::with_packs(runtime, &packs)
    }

    /// Build a server with an explicit pack list (strict — fails on unknown names).
    ///
    /// The same already-prepared-runtime precondition as [`Self::new`] applies.
    // The error variant intentionally carries the runtime by value so callers
    // can recover and retry. Boxing would force every recovery path through a
    // deref for no real benefit.
    #[allow(clippy::result_large_err)]
    pub fn with_packs(runtime: KhiveRuntime, packs: &[String]) -> Result<Self, PackRegError> {
        if !runtime.config().mounts.is_empty() {
            return Err(PackRegError {
                failure: PackRegFailure::Registry(RuntimeError::InvalidInput(
                    "configured mounts require the async server constructor".into(),
                )),
                runtime,
            });
        }
        Self::with_mounted_packs(runtime, packs, Vec::new())
    }

    /// Build a prepared runtime's native registry and start its configured sources.
    #[allow(clippy::result_large_err)]
    pub async fn new_with_mounts(runtime: KhiveRuntime) -> Result<Self, PackRegError> {
        let packs = runtime.config().packs.clone();
        let mounted = khive_mounts::start_mounts(&runtime).await;
        Self::with_mounted_packs(runtime, &packs, mounted)
    }

    #[allow(clippy::result_large_err)]
    fn with_mounted_packs(
        runtime: KhiveRuntime,
        packs: &[String],
        mounted: Vec<khive_mounts::MountedPack>,
    ) -> Result<Self, PackRegError> {
        #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
        let channel_loop_admission = ChannelLoopAdmission::for_single_runtime(&runtime, packs);
        let gate = runtime.config().gate.clone();
        let default_namespace = runtime.config().default_namespace.clone();
        let config_id = compute_config_id_with_runtime_policies(
            runtime.config(),
            None,
            runtime.ann_fresh_tail_enabled(),
            runtime.is_read_only(),
        );
        let visible_namespaces = runtime.config().visible_namespaces.clone();
        let actor_id = runtime.config().actor_id.clone();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(gate);
        builder.with_default_namespace(default_namespace.as_str());
        builder.with_visible_namespaces(visible_namespaces);
        builder.with_actor_id(actor_id);
        // A read-only snapshot deliberately retains no EventStore handle; the
        // registry exposes an advisory beside each successful result instead.
        if runtime.is_read_only() {
            builder.with_read_only_audit_store();
        } else {
            // The configured sink must open at build before serving starts.
            builder
                .with_runtime_event_store(&runtime)
                .map_err(|source| PackRegError {
                    failure: PackRegFailure::Registry(source),
                    runtime: runtime.clone(),
                })?;
        }
        if let Err(load_err) = PackRegistry::register_packs(packs, runtime.clone(), &mut builder) {
            let failure = match load_err {
                PackLoadError::UnknownPack(name) => PackRegFailure::UnknownPack(name),
                PackLoadError::DuplicatePack(name) => PackRegFailure::DuplicatePack(name),
                PackLoadError::MissingDependency { pack, dep } => {
                    PackRegFailure::MissingDependency { pack, dep }
                }
                PackLoadError::NoPublicVerbs { pack } => PackRegFailure::NoPublicVerbs { pack },
            };
            return Err(PackRegError { failure, runtime });
        }
        for mount in mounted {
            builder
                .register_mounted(Box::new(mount))
                .map_err(|source| PackRegError {
                    failure: PackRegFailure::Registry(source),
                    runtime: runtime.clone(),
                })?;
        }
        let registry = builder.build().map_err(|source| PackRegError {
            failure: PackRegFailure::Registry(source),
            runtime: runtime.clone(),
        })?;
        // Aggregate pack-declared edge endpoint rules into the runtime
        // so `validate_edge_relation_endpoints` can consult them.
        runtime.install_edge_rules(registry.all_edge_rules());
        // Invoke `PackRuntime::register_embedders` on every pack so custom
        // embedding providers are available before the first verb dispatch.
        // Must happen after the registry is built (packs are ordered)
        // and before any `remember`/`recall` calls that would resolve embedders.
        registry.call_register_embedders(&runtime);
        // Invoke `PackRuntime::register_entity_type_validator` on every pack so
        // entity-type validation is active at the runtime layer for all write
        // paths, including direct `create_many` callers that bypass the handler.
        registry.call_register_entity_type_validators(&runtime);
        // #750: install pack-owned note-mutation hooks (currently
        // only khive-pack-memory's warm-ANN-cache invalidation) so KG's
        // update/delete verbs notify caching packs even though there is no
        // crate-level dependency between them.
        registry.call_register_note_mutation_hooks(&runtime);
        registry.call_register_note_search_ann_providers(&runtime);
        // Note-write identity: the pack-owned kind set drives `update`'s
        // properties refusal and `merge`'s identity preservation; the
        // validator derives owned identity properties at every note-write.
        runtime.install_pack_owned_note_kinds(
            registry
                .pack_owned_note_kinds()
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        runtime.install_note_embedding_policies(&registry.all_note_embedding_policies());
        registry.call_register_note_write_validators(&runtime);
        // #2943: install entity-kind update hooks so the generic entity
        // `update` path can re-run a pack's create-time invariant against
        // the merged properties — same scope/timing as the entity-type
        // validator above.
        runtime.install_entity_kind_hooks(registry.entity_kind_hooks());
        // A required pack schema failure must refuse boot before any handler
        // can run. Use the same validation and error path as multi-backend boot.
        registry
            .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
            .map_err(|source| PackRegError {
                failure: PackRegFailure::Schema(source),
                runtime: runtime.clone(),
            })?;
        // Capture the pool arc for the WAL checkpoint task. Only available for
        // file-backed databases; in-memory backends return None here.
        let pool = if runtime.backend().is_file_backed() && !runtime.is_read_only() {
            Some(runtime.backend().pool_arc())
        } else {
            None
        };
        Ok(Self {
            registry,
            default_namespace: default_namespace.as_str().to_string(),
            config_id,
            coordinator: None,
            pool,
            secondary_pools: Vec::new(),
            default_output_format: OutputFormat::Json,
            schedule_ticker_last_tick_micros: Arc::new(AtomicI64::new(0)),
            #[cfg(unix)]
            bridge_executable: None,
            stdio_bridge: false,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_outbox_runtime: Some(runtime.clone()),
            runtime: Some(runtime),
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_loop_admission,
        })
    }

    /// Build a server directly from a pre-configured registry.
    ///
    /// Intended for tests that need to inject mock packs (e.g. packs that
    /// return `RuntimeError::Khive` to exercise structured error serialization).
    /// Production code should use [`Self::new`] or [`Self::with_packs`].
    #[doc(hidden)]
    pub fn from_registry(registry: VerbRegistry) -> Self {
        // A registry injected directly has no resolved RuntimeConfig; use a
        // sentinel that matches no real daemon so such servers always
        // dispatch locally rather than forward.
        Self::from_registry_with_meta(registry, "local", "registry-only")
    }

    /// Build a server from a pre-built registry with explicit namespace and config_id.
    ///
    /// Used by the multi-backend boot path in `serve.rs` where the registry is
    /// assembled externally before constructing the server.
    pub fn from_registry_with_meta(
        registry: VerbRegistry,
        default_namespace: &str,
        config_id: &str,
    ) -> Self {
        Self {
            registry,
            default_namespace: default_namespace.to_string(),
            config_id: config_id.to_string(),
            coordinator: None,
            pool: None,
            secondary_pools: Vec::new(),
            default_output_format: OutputFormat::Json,
            schedule_ticker_last_tick_micros: Arc::new(AtomicI64::new(0)),
            #[cfg(unix)]
            bridge_executable: None,
            stdio_bridge: false,
            runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_outbox_runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_loop_admission: ChannelLoopAdmission::default(),
        }
    }

    /// Attach the default-backend `KhiveRuntime` (see the `runtime` field docs
    /// on [`KhiveMcpServer`]). Used by the multi-backend boot path to wire in
    /// the same `default_runtime` it already resolved while building the
    /// registry.
    pub fn with_runtime(mut self, runtime: KhiveRuntime) -> Self {
        #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
        if self.channel_outbox_runtime.is_none() {
            self.channel_outbox_runtime = Some(runtime.clone());
        }
        self.runtime = Some(runtime);
        self
    }

    /// Attach the exact comm-routed runtime that owns outbox note properties.
    /// Multi-backend boot overrides the default-runtime fallback installed by
    /// [`Self::with_runtime`]; single-backend construction already points both
    /// handles at the same runtime.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn with_channel_outbox_runtime(mut self, runtime: Option<KhiveRuntime>) -> Self {
        self.channel_outbox_runtime = runtime;
        self
    }

    /// Override the server-level default output format (ADR-078).
    ///
    /// Called after construction to wire in the format resolved from
    /// `KHIVE_OUTPUT_FORMAT` or `[runtime] default_output_format` in
    /// `khive.toml`. Per-request `format` fields override this at dispatch time.
    pub fn with_default_output_format(mut self, fmt: OutputFormat) -> Self {
        self.default_output_format = fmt;
        self
    }

    /// Attach the runtime-derived channel admission computed by the
    /// multi-backend builder after pack routing has been resolved.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn with_channel_loop_admission(mut self, admission: ChannelLoopAdmission) -> Self {
        self.channel_loop_admission = admission;
        self
    }

    /// Return the fixed boot-time admission for channel background tasks.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn channel_loop_admission(&self) -> ChannelLoopAdmission {
        self.channel_loop_admission
    }

    /// Attach a cross-backend coordinator (ADR-029 Phase 2).
    ///
    /// Only multi-backend servers need a coordinator. Single-backend servers
    /// leave `coordinator` as `None` (zero-change invariant: all dispatch goes
    /// through `VerbRegistry` unchanged).
    pub fn with_coordinator(mut self, coordinator: Arc<dyn CoordinatorService>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// Attach a connection pool for the WAL checkpoint background task.
    ///
    /// Used by the multi-backend boot path to wire the main backend's pool into a
    /// server built via `from_registry_with_meta` (which cannot carry a pool itself
    /// because registry-only construction has no access to the backend layer).
    pub fn with_pool(mut self, pool: Arc<ConnectionPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Attach every file-backed backend pool beyond the main one (ADR-091
    /// Amendment 3 fan-out), so the session sweep and the daemon's
    /// checkpoint task can cover the full multi-backend deployment instead
    /// of only `pool`.
    pub fn with_secondary_pools(mut self, pools: Vec<Arc<ConnectionPool>>) -> Self {
        self.secondary_pools = pools;
        self
    }

    pub(crate) fn blob_upload_manager(
        &self,
    ) -> Option<Arc<khive_pack_blob::uploads::UploadManager>> {
        self.registry.pack_host_state("blob")
    }

    /// Clone the verb registry for use by background tasks (e.g. channel polling loops).
    ///
    /// `VerbRegistry` is internally `Arc`-wrapped so this clone is cheap. The returned
    /// registry shares the same packs and dispatch state as the server.
    #[cfg(any(test, feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn verb_registry_clone(&self) -> VerbRegistry {
        self.registry.clone()
    }

    /// Clone the KG-routed `KhiveRuntime` retained for background tasks that
    /// need a non-wire owner API (the email outbox loop's `external_id`
    /// claim). `KhiveRuntime` is internally `Arc`-wrapped so this is cheap.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    pub(crate) fn channel_outbox_runtime_clone(&self) -> Option<KhiveRuntime> {
        self.channel_outbox_runtime.clone()
    }

    /// Route a `link` or `search` verb through the coordinator when in multi-backend mode.
    ///
    /// Returns `Some(result)` when the coordinator handled the op (caller should skip
    /// `registry.dispatch`). Returns `None` (fall-through) when:
    /// - no coordinator is attached (`coordinator == None`)
    /// - the coordinator reports a single backend (`is_single_backend()`)
    /// - the verb is not `link` or `search`
    /// - args cannot be extracted for coordinator dispatch (e.g. non-UUID source/target)
    ///
    /// Result semantics mirror the per-op envelope from the registry:
    /// `Ok(OpSuccess)` → success payload plus any coordinator degradation
    /// metadata (caller wraps it in the per-op envelope).
    /// `Err(DispatchFailure)` → error payload plus any stable refusal reason.
    ///
    /// `identity` mirrors the override [`Self::dispatch_op`] applies to the
    /// registry path (ADR-096 Fork 1): when present, its namespace is used
    /// instead of `self.default_namespace` so a per-request identity can't
    /// diverge between the coordinator intercept and the registry dispatch
    /// it falls through to.
    async fn dispatch_via_coordinator(
        &self,
        tool: &str,
        args_value: &Value,
        identity: Option<&khive_runtime::RequestIdentity>,
    ) -> Option<Result<OpSuccess, DispatchFailure>> {
        let coord = self.coordinator.as_ref()?;
        if coord.is_single_backend() {
            return None;
        }
        dispatch_via_coordinator_inner(coord.as_ref(), &self.registry, tool, args_value, identity)
            .await
    }

    /// Namespace this server's registry was built for.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// Fingerprint of the runtime config this server's registry was built for.
    /// The resolved events-split config of the default-backend runtime, if
    /// any (ADR-170). Daemon supervision derives the events daemon's
    /// db/socket from this exact value so the supervised daemon and the
    /// forwarding clients cannot anchor at diverging paths.
    pub(crate) fn events_split_config(
        &self,
    ) -> Option<&khive_runtime::events_split::EventsSplitConfig> {
        self.runtime
            .as_ref()
            .and_then(|rt| rt.config().events_split.as_ref())
    }

    /// Events storage inherits the resolved main-backend policy, including its
    /// source. The server's checkpoint pool may be overridden independently.
    pub(crate) fn events_wal_ceiling_policy(&self) -> Option<khive_db::WalCeilingPolicy> {
        self.runtime
            .as_ref()
            .map(|rt| rt.core().backend().pool().config().wal_ceiling)
    }

    /// Whether the default-backend runtime is read-only. Daemon supervision
    /// asks this before spawning an events daemon: the supervised daemon
    /// opens the events sidecar writable, which a read-only deployment must
    /// never cause (ADR-170).
    pub(crate) fn default_runtime_is_read_only(&self) -> bool {
        self.runtime.as_ref().is_some_and(|rt| rt.is_read_only())
    }

    pub fn config_id(&self) -> &str {
        &self.config_id
    }

    /// This server's resolved actor identity label, if configured (ADR-057).
    ///
    /// Read when building the daemon request frame (ADR-096 Fork 1) to carry
    /// this server's own identity on the wire, so a warm daemon with a
    /// different baked identity serves the request under this caller's
    /// actor instead of the daemon's.
    pub fn actor_id(&self) -> Option<&str> {
        self.registry.actor_id()
    }

    /// This server's resolved extra read-visibility namespaces (ADR-007
    /// Rev 4 Rule 3b). See [`Self::actor_id`] for why this is exposed
    /// (ADR-096 Fork 1).
    pub fn visible_namespaces(&self) -> &[khive_runtime::Namespace] {
        self.registry.visible_namespaces()
    }

    /// The connection pool to use for background WAL checkpointing, if any.
    ///
    /// Returns `None` for in-memory or registry-only servers.
    pub fn pool(&self) -> Option<Arc<ConnectionPool>> {
        self.pool.clone()
    }

    /// File-backed backend pools beyond [`Self::pool`] (ADR-091 Amendment 3
    /// fan-out). Empty for a single-backend server.
    pub fn secondary_pools(&self) -> Vec<Arc<ConnectionPool>> {
        self.secondary_pools.clone()
    }

    /// This server's configured audit `EventStore`, if any (ADR-094).
    ///
    /// Exposed so the `DaemonDispatch::event_store_for_checkpoint` impl and
    /// the email channel poll loop can append best-effort lifecycle events
    /// to the same sink gate-check audit rows already use, without a second
    /// constructor argument threaded everywhere a registry is built.
    pub fn event_store(&self) -> Option<Arc<dyn khive_storage::EventStore>> {
        self.registry.event_store()
    }

    /// Quiesce the ADR-133 audit-batch supervisor before a short-lived
    /// process drops this server.
    ///
    /// Stops admission and waits for every already-accepted audit row to
    /// reach a terminal state, then joins the supervisor's own task so its
    /// separate clone of the runtime's connection pool is released before
    /// this call returns (a no-op returning `Ok(())` when no audit
    /// `EventStore` is configured, see [`VerbRegistry::shutdown_audit_batch`]).
    ///
    /// Exists so a caller that owns this server for the lifetime of one
    /// process invocation, not a long-running daemon, can make the pool's
    /// own `Drop` run deterministically on return instead of racing a
    /// detached supervisor task that may still hold a pool reference. Every
    /// serving path (the daemon, `serve_stdio`) keeps the registry and its
    /// audit batch alive for the process lifetime and must not call this.
    pub async fn shutdown_audit_batch(
        &self,
    ) -> Result<(), khive_runtime::audit_batch::AuditTerminalReason> {
        self.registry.shutdown_audit_batch().await
    }

    /// The server-level default output format (ADR-078), as resolved at
    /// construction by [`crate::serve::apply_env_output_format`].
    pub fn default_output_format(&self) -> OutputFormat {
        self.default_output_format
    }

    /// Record that this process's daemon schedule loop began a tick.
    ///
    /// The loop calls this before starting its drain pass, including passes
    /// that find no due rows or return an error. A pass that wedges after this
    /// point leaves a frozen timestamp for callers to classify as stale.
    pub(crate) fn record_schedule_ticker_tick(&self) {
        self.schedule_ticker_last_tick_micros
            .store(chrono::Utc::now().timestamp_micros(), Ordering::Release);
    }

    /// Warm every pack's in-memory state. Called by the daemon in a background
    /// task after the socket is bound.
    pub async fn warm_all(&self) {
        self.registry.call_warm_all().await;
    }

    /// Serve over stdio (blocks until the connection closes).
    ///
    /// #714: a resumed generation (produced by `crate::daemon`'s in-place
    /// re-exec self-heal on a stale-protocol mismatch) skips the normal MCP
    /// initialize handshake via `serve_directly` — by construction, its peer
    /// already completed a real handshake with the prior generation over this
    /// same, uninterrupted stdio pipe pair, so waiting for another one would
    /// hang forever (the client has no reason to send a second `initialize`).
    /// A cold start (the overwhelmingly common case) is unaffected: no
    /// `--resumed-generation` marker means the normal `.serve()` handshake
    /// runs exactly as before this change.
    ///
    /// Both branches keep `crate::daemon::SelfHealOnFlushTransport` directly
    /// around the raw stdio transport — the actual happens-after edge that
    /// fires an armed self-heal re-exec (or drain-and-exit) only once a message
    /// has genuinely finished flushing to the client. The outer EOF adapter
    /// shares rmcp's root cancellation token so disconnect cancels every
    /// per-request child before rmcp starts its graceful drain.
    #[cfg(unix)]
    pub async fn serve_stdio(mut self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let _ = bridge_instance_id();
        self.stdio_bridge = true;
        self.bridge_executable = crate::daemon::executable::BridgeExecutable::current()
            .map(|executable| Arc::new(std::sync::Mutex::new(executable)));
        let root = tokio_util::sync::CancellationToken::new();
        let idle_timeout = stdio_bridge_idle_timeout_from_env();
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let max_outstanding_requests = stdio_bridge_max_outstanding_requests_from_env();
        let max_line_bytes = crate::stdio_line_limit::max_line_bytes_from_env()?;
        let build_transport = |root: tokio_util::sync::CancellationToken| {
            let (read, write) = stdio();
            let write =
                crate::transport::DeadlineWriter::new(write, response_deadline, root.clone());
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                crate::daemon::SelfHealOnFlushTransport::new(AsyncRwTransport::new_server(
                    crate::stdio_line_limit::BoundedLineReader::new(read, max_line_bytes),
                    write,
                )),
                root,
                idle_timeout,
                Some(response_deadline),
                stdio_bridge_request_obligation_ttl_from_env(),
                max_outstanding_requests,
            )
        };

        match stdio_serve_mode_for(crate::daemon::resumed_generation()) {
            StdioServeMode::Resumed => {
                let service = rmcp::service::serve_directly_with_ct(
                    self,
                    build_transport(root.clone()),
                    None,
                    root,
                );
                service.waiting().await?;
            }
            StdioServeMode::Handshake => {
                let service = self
                    .serve_with_ct(build_transport(root.clone()), root)
                    .await?;
                service.waiting().await?;
            }
        }
        Ok(())
    }

    /// Non-Unix stdio serving. The #714 self-heal re-exec mechanism
    /// (`crate::daemon`'s `SelfHealOnFlushTransport`/resumed-generation
    /// machinery) requires `exec()` (POSIX-only) and is only ever armed by a
    /// Unix-domain-socket daemon-forwarding protocol mismatch — there is
    /// nothing to self-heal from on this target (`--daemon` mode itself is
    /// Unix-only, see `serve.rs::serve_server`), so this path always runs the
    /// normal MCP `initialize` handshake, with no resumed-generation skip and
    /// no flush-triggered hook. It still shares rmcp's root token with the EOF
    /// adapter so disconnect cancellation is platform-independent.
    #[cfg(not(unix))]
    pub async fn serve_stdio(mut self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let _ = bridge_instance_id();
        self.stdio_bridge = true;
        let root = tokio_util::sync::CancellationToken::new();
        let (read, write) = stdio();
        let max_line_bytes = crate::stdio_line_limit::max_line_bytes_from_env()?;
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let write = crate::transport::DeadlineWriter::new(write, response_deadline, root.clone());
        let transport =
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                AsyncRwTransport::new_server(
                    crate::stdio_line_limit::BoundedLineReader::new(read, max_line_bytes),
                    write,
                ),
                root.clone(),
                stdio_bridge_idle_timeout_from_env(),
                Some(response_deadline),
                stdio_bridge_request_obligation_ttl_from_env(),
                stdio_bridge_max_outstanding_requests_from_env(),
            );
        let service = self.serve_with_ct(transport, root).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Build the textual verb catalog included in the request tool's description.
    ///
    /// The list is rebuilt from the runtime registry so it always reflects which
    /// packs are actually loaded.
    fn verb_catalog(&self) -> String {
        let verbs = self
            .registry
            .all_verbs_with_names()
            .into_iter()
            .map(|(pack, v)| (pack.to_owned(), v.name.to_owned(), v.description.to_owned()));
        let mounted = self
            .registry
            .mounted_verb_snapshot()
            .into_iter()
            .map(|verb| {
                (
                    verb["pack"].as_str().unwrap_or_default().to_owned(),
                    verb["verb"].as_str().unwrap_or_default().to_owned(),
                    verb["description"].as_str().unwrap_or_default().to_owned(),
                )
            });
        build_verb_catalog(verbs.chain(mounted))
    }

    /// Dispatch a single [`ParsedOp`] by resolving its args (potentially
    /// substituting `$prev` references) and calling the [`VerbRegistry`].
    ///
    /// Returns a per-op result object: `{ok, tool, result}` on success or
    /// `{ok: false, tool, error, reason?}` on failure.
    async fn dispatch_op(
        &self,
        op: ParsedOp,
        prev_result: Option<&Value>,
        from_wire: bool,
        identity: Option<&khive_runtime::RequestIdentity>,
    ) -> Result<Value, DispatchFailure> {
        let ParsedOp { tool, args } = op;

        // Resolve args — substitute $prev references when prev_result is Some.
        // Handles flat PrevRef as well as Array/Object containing nested refs.
        let mut resolved: serde_json::Map<String, Value> = serde_json::Map::new();
        for (name, arg_val) in args {
            let needs_prev = !matches!(&arg_val, ArgValue::Value(_));
            let value = if needs_prev {
                // `dispatch_op` only ever runs inside a chain (see `run_parsed`'s
                // `ExecutionMode::Chain` arm); `prev_result` is `None` here
                // exactly when this is the chain's first op, so there is no
                // preceding result to substitute from at all.
                let prev = prev_result.ok_or_else(|| {
                    DispatchFailure::before_dispatch(
                        tool.clone(),
                        json!({
                            "kind": "substitution_error",
                            "reason": "no_preceding_op",
                            "message": format!(
                                "argument {name:?}: $prev has no preceding op to resolve \
                                 against — this is the first operation in the chain. $prev \
                                 always resolves against the immediately preceding op's result \
                                 only; it cannot reach further back or forward. Move this op \
                                 after the one that produces the value, or pass a literal value \
                                 here instead of $prev."
                            )
                        }),
                    )
                })?;
                let resolved_val = arg_val.resolve_all(prev).ok_or_else(|| {
                    DispatchFailure::before_dispatch(
                        tool.clone(),
                        substitution_error_payload(&name, &arg_val, prev),
                    )
                })?;
                // UE4-H1: bare `$prev` (no path) resolving to a map or array
                // will cause a confusing downstream type error. Detect it here and
                // surface a clear substitution error with available field names.
                if matches!(&arg_val, ArgValue::PrevRef { path } if path.is_empty()) {
                    match &resolved_val {
                        Value::Object(map) => {
                            let fields: Vec<&str> = map.keys().map(String::as_str).collect();
                            return Err(DispatchFailure::before_dispatch(
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
                                    "reason": "bare_ref_ambiguous",
                                    "message": format!(
                                        "argument {name:?}: $prev requires a dotted path \
                                         (e.g. $prev.id) when the prior result is a map. \
                                         Available top-level fields: [{}]",
                                        fields.join(", ")
                                    ),
                                }),
                            ));
                        }
                        Value::Array(_) => {
                            return Err(DispatchFailure::before_dispatch(
                                tool.clone(),
                                json!({
                                    "kind": "substitution_error",
                                    "reason": "bare_ref_ambiguous",
                                    "message": format!(
                                        "argument {name:?}: $prev requires a dotted path \
                                         (e.g. $prev.0) when the prior result is an array. \
                                         Use $prev.N to select a specific element."
                                    ),
                                }),
                            ));
                        }
                        _ => {}
                    }
                }
                resolved_val
            } else {
                match arg_val {
                    ArgValue::Value(v) => v,
                    _ => unreachable!(),
                }
            };
            resolved.insert(name, value);
        }

        let args_value = Value::Object(resolved);

        // Subhandler verbs are operator-only — block them at the MCP wire
        // boundary (`from_wire`), never on the operator path (`kkernel exec`,
        // in-process callers). Exception: `help=true` is short-circuited in
        // VerbRegistry::dispatch before reaching the pack, so introspection works.
        let is_help = args_value
            .get("help")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if from_wire && !is_help && self.registry.is_subhandler_verb(&tool) {
            return Err(DispatchFailure::before_dispatch(
                tool.clone(),
                json!(format!(
                    "permission denied for verb {tool:?}: verb '{tool}' is an internal \
                     subhandler and cannot be invoked via the MCP request surface"
                )),
            ));
        }

        // Multi-backend interception: route link/search through the coordinator (ADR-029 D3/D4).
        // Single-backend and non-link/search verbs fall through to the registry unchanged.
        if let Some(coord_result) = self
            .dispatch_via_coordinator(&tool, &args_value, identity)
            .await
        {
            return coord_result.and_then(|result| chain_ok_envelope_or_depth_error(tool, result));
        }

        let search_args = (tool == "search" && !is_help).then(|| args_value.clone());
        match self
            .registry
            .dispatch_with_disposition(&tool, args_value, identity.cloned())
            .await
        {
            Ok(result) => {
                let text_mode = match search_args.as_ref() {
                    Some(args) => match validated_search_text_mode(args, &self.registry) {
                        Ok(mode) => mode,
                        Err(error) => {
                            return Err(DispatchFailure::before_dispatch(
                                &tool,
                                json!(error.to_string()),
                            ));
                        }
                    },
                    None => "all_terms",
                };
                let result = decorate_schedule_agenda_with_ticker_health(
                    &tool,
                    is_help,
                    result,
                    self.schedule_ticker_last_tick_micros.as_ref(),
                );
                let vector_selected = self
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.vector_arm_selected());
                let success = op_success_from_registry_result(
                    &tool,
                    is_help,
                    result,
                    vector_selected,
                    text_mode,
                );
                chain_ok_envelope_or_depth_error(tool, success)
            }
            Err(error) => Err(DispatchFailure::from_dispatch(&tool, error)),
        }
    }

    /// Execute a parsed request, dispatching according to its [`ExecutionMode`].
    ///
    /// - `Single` / `Parallel`: at most [`MAX_BATCH_CONCURRENCY`] ops run at
    ///   once; per-op failure does not abort siblings. `aborted` count is 0.
    /// - `Chain`: ops run sequentially; `$prev` from each op's result is
    ///   substituted into the next op's args. If any op fails (or a `$prev`
    ///   substitution fails), remaining ops appear as `aborted: true`.
    ///
    /// Presentation transforms are applied per-op AFTER dispatch,
    /// using `mode_for_op` to determine the mode per position. Chain `$prev`
    /// substitution uses canonical (verbose) handler output; the transform runs
    /// only at the final response-envelope boundary.
    ///
    /// Aggregate `status` describes failed or aborted operations — distinct
    /// from a `search` op's own per-operation `status` field ("complete" /
    /// "partial", ADR-130 §1), which lives inside that op's `results` entry,
    /// never at this top level. A successful but incomplete coordinator
    /// search remains a success (`status="partial"` on that entry) and
    /// carries bounded `missing_backends` and `backend_errors` diagnostics
    /// plus the deprecated `partial` alias; a search where no hit survives a
    /// backend failure is instead a failed op (`ok: false`,
    /// `error.kind: "search_incomplete"`).
    ///
    /// Response envelope:
    /// ```json
    /// {
    ///   "results": [...],
    ///   "summary": { "total": N, "succeeded": K, "failed": M, "aborted": A },
    ///   "status": "success" | "partial"
    /// }
    /// ```
    ///
    /// `status` is a structural signal for a partially-failed batch (#1220):
    /// per-op `results` entries and `summary.failed`/`summary.aborted` counts
    /// already carry this information, but a caller that checks only for the
    /// absence of a top-level RPC error has nothing to branch on. `"partial"`
    /// means at least one op in this response failed or was aborted;
    /// `"success"` means every op in `results` reports `ok: true`.
    async fn run_parsed(
        &self,
        ops: Vec<ParsedOp>,
        mode: ExecutionMode,
        ranges: Vec<Range<usize>>,
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
        context: RunParsedContext<'_>,
    ) -> (Value, Vec<NoteContentScope>) {
        let RunParsedContext {
            enforce_response_budget,
            max_batch_concurrency,
            from_wire,
            identity,
        } = context;
        debug_assert!(max_batch_concurrency > 0);
        let response_budget = if mode == ExecutionMode::Parallel && enforce_response_budget {
            BATCH_RESPONSE_BUDGET_BYTES
        } else {
            usize::MAX
        };
        let now_unix = PresentationNow::from(chrono::Utc::now());

        // Resolve per-op presentation mode: per-op entry overrides batch default.
        let mode_for_op = |i: usize| -> PresentationMode {
            presentation_per_op
                .as_ref()
                .and_then(|v| v.get(i))
                .and_then(|o| *o)
                .unwrap_or(presentation)
        };

        let mut parse_content: Vec<bool> = ops
            .iter()
            .map(|op| parse_content_requested(op, None))
            .collect();
        let response = match mode {
            // ADR-016 Amendment 2: a bracketed batch whose ranges partition
            // `ops` into units longer than one leaf is a parallel batch of
            // linear chains, not an ordinary flat batch; every ordinary
            // batch keeps `ranges.len() == ops.len()` (one leaf per range)
            // and falls through to the unchanged arm below.
            ExecutionMode::Parallel if ranges.iter().any(|range| range.len() > 1) => {
                let (resp, content_updates) = self
                    .run_parallel_units(
                        ops,
                        ranges,
                        presentation,
                        presentation_per_op.clone(),
                        context,
                        now_unix,
                    )
                    .await;
                for (index, requested) in content_updates {
                    parse_content[index] = requested;
                }
                resp
            }
            ExecutionMode::Single | ExecutionMode::Parallel => {
                // Write-key conflict preflight.
                //
                // Detect ops that target the same write key in the same parallel/single
                // batch. Conflicting ops receive per-op error entries; non-conflicting ops
                // execute normally. `results.length == summary.total` is preserved.
                let conflict_indices: std::collections::HashSet<usize> = {
                    let mut seen: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    let mut bad: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    for (i, op) in ops.iter().enumerate() {
                        for key in khive_request::write_keys_for_op_pub(op) {
                            if let Some(&prior) = seen.get(&key) {
                                bad.insert(prior);
                                bad.insert(i);
                            } else {
                                seen.insert(key, i);
                            }
                        }
                    }
                    bad
                };

                // Clone coordinator and namespace for use in the per-op closures (ADR-029 D3/D4).
                let coordinator: Option<Arc<dyn CoordinatorService>> = self.coordinator.clone();
                let schedule_ticker_last_tick_micros =
                    self.schedule_ticker_last_tick_micros.clone();
                let vector_selected = self
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.vector_arm_selected());
                // ADR-096 Fork 1: a per-request identity overrides the default
                // namespace for both the coordinator intercept and the registry
                // dispatch below, so the two can't drift out of sync per op.
                let identity_owned: Option<khive_runtime::RequestIdentity> = identity.cloned();

                // Independent dispatch — bounded concurrency, results restored to input order.
                let futures = ops.into_iter().enumerate().map(|(i, op)| {
                    let conflict_with: Option<String> = if conflict_indices.contains(&i) {
                        Some(format!(
                            "conflict: writes overlap with another op in this batch (op #{})",
                            i
                        ))
                    } else {
                        None
                    };

                    let registry = self.registry.clone();
                    let coord = coordinator.clone();
                    let schedule_ticker_last_tick_micros =
                        schedule_ticker_last_tick_micros.clone();
                    let op_identity = identity_owned.clone();
                    let op_vector_selected = vector_selected;
                    let op_mode = mode_for_op(i);
                    let task_tool = op.tool.clone();
                    let parse_content = parse_content[i];
                    BatchTask {
                        index: i,
                        tool: task_tool,
                        future: async move {
                        // ADR-103 Amendment 2: one dispatch-accounting context
                        // per op; the entry is stamped with the frozen usage
                        // snapshot after dispatch resolves.
                        let usage_ctx = khive_runtime::usage::UsageContext::new();
                        let operation = khive_types::OperationAttribution {
                            op_index: u32::try_from(i).expect("parser bounds operation count"),
                            ref_resolution: khive_types::RefResolution::Literal,
                        };
                        let mut entry = khive_storage::operation_context::scope_operation_attribution(
                            operation,
                            khive_runtime::usage::scope(usage_ctx.clone(), async {
                        let tool = op.tool.clone();
                        // Conflicting ops get a per-op error; skip dispatch.
                        if let Some(msg) = conflict_with {
                            return failure_entry(tool, json!(msg), DomainDisposition::NotCommitted);
                        }
                        // AlwaysVerbose verbs override the caller's presentation mode.
                        let presentation_policy = registry.presentation_policy_for(&tool);
                        let effective_mode =
                            if presentation_policy == VerbPresentationPolicy::AlwaysVerbose
                            {
                                PresentationMode::Verbose
                            } else {
                                op_mode
                            };
                        // No $prev in parallel/single mode — PrevRef, Array(PrevRef),
                        // and Object(PrevRef) are all errors here.
                        let mut resolved: serde_json::Map<String, Value> =
                            serde_json::Map::new();
                        let mut prev_error: Option<Value> = None;
                        for (name, arg_val) in &op.args {
                            if matches!(arg_val, ArgValue::Value(_)) {
                                if let ArgValue::Value(v) = arg_val {
                                    resolved.insert(name.clone(), v.clone());
                                }
                            } else {
                                prev_error = Some(failure_entry(&tool, json!(format!(
                                    "argument {name:?}: $prev reference is only valid in chain (|) mode"
                                )), DomainDisposition::NotCommitted));
                                break;
                            }
                        }
                        if let Some(err) = prev_error {
                            return err;
                        }
                        let args_value = Value::Object(resolved);

                        // Block subhandler verbs at the MCP wire boundary
                        // (`from_wire`) only — operator paths pass through.
                        // Exception: help=true is short-circuited in
                        // VerbRegistry::dispatch before the pack, so
                        // introspection passes through.
                        let is_help = args_value
                            .get("help")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if from_wire && !is_help && registry.is_subhandler_verb(&tool) {
                            return failure_entry(tool.clone(), json!(format!(
                                "permission denied for verb {tool:?}: verb '{tool}' is an \
                                 internal subhandler and cannot be invoked via the MCP request surface"
                            )), DomainDisposition::NotCommitted);
                        }

                        // Multi-backend interception: route link/search through the coordinator
                        // (ADR-029 D3/D4). Falls through to registry for single-backend and
                        // non-link/search verbs.
                        if let Some(active_coord) = coord.as_ref() {
                            if !active_coord.is_single_backend() {
                                if let Some(coord_result) = dispatch_via_coordinator_inner(
                                    active_coord.as_ref(),
                                    &registry,
                                    &tool,
                                    &args_value,
                                    op_identity.as_ref(),
                                )
                                .await
                                {
                                    return match coord_result {
                                        Ok(result) => present_ok_envelope_or_depth_error(
                                            tool,
                                            result,
                                            effective_mode,
                                            now_unix,
                                            presentation_policy,
                                            NoteContentScope::None,
                                        ),
                                        Err(failure) => failure.into_entry(),
                                    };
                                }
                            }
                        }

                        let search_args =
                            (tool == "search" && !is_help).then(|| args_value.clone());
                        match registry
                            .dispatch_with_disposition(&tool, args_value, op_identity)
                            .await
                        {
                            Ok(result) => {
                                let text_mode = match search_args.as_ref() {
                                    Some(args) => {
                                        match validated_search_text_mode(args, &registry) {
                                            Ok(mode) => mode,
                                            Err(error) => {
                                                return DispatchFailure::before_dispatch(
                                                    &tool,
                                                    json!(error.to_string()),
                                                )
                                                .into_entry();
                                            }
                                        }
                                    }
                                    None => "all_terms",
                                };
                                let result = decorate_schedule_agenda_with_ticker_health(
                                    &tool,
                                    is_help,
                                    result,
                                    schedule_ticker_last_tick_micros.as_ref(),
                                );
                                let success =
                                    op_success_from_registry_result(
                                        &tool,
                                        is_help,
                                        result,
                                        op_vector_selected,
                                        text_mode,
                                    );
                                let content_scope = note_content_scope(
                                    parse_content && !is_help, &tool, &success.result, &registry,
                                );
                                present_ok_envelope_or_depth_error(
                                    tool,
                                    success,
                                    effective_mode,
                                    now_unix,
                                    presentation_policy,
                                    content_scope,
                                )
                            }
                            Err(error) => {
                                DispatchFailure::from_dispatch(&tool, error).into_entry()
                            }
                        }
                        }))
                        .await;
                        stamp_usage(&mut entry, &usage_ctx);
                        entry
                        },
                    }
                });
                let results =
                    execute_bounded_batch(futures, response_budget, max_batch_concurrency).await;
                parallel_batch_envelope(results)
            }
            ExecutionMode::Chain => {
                // Sequential execution with $prev substitution and abort-on-failure.
                // $prev uses canonical (verbose) handler output — presentation runs
                // only at the final response-envelope boundary.
                let total = ops.len();
                let mut results: Vec<Value> = Vec::with_capacity(total);
                // prev_result holds the CANONICAL result (pre-presentation) for $prev.
                let mut prev_result: Option<Value> = None;
                let mut aborted_from: Option<usize> = None;

                for (i, op) in ops.into_iter().enumerate() {
                    if let Some(failed_at) = aborted_from {
                        // A prior op failed — mark remaining as aborted, and say so
                        // plainly: this op was never dispatched (its own $prev, if any,
                        // was never attempted), so the failure to debug lives at the
                        // earlier op, not here.
                        let failed_index = failed_at - 1;
                        let failed_tool = results
                            .get(failed_index)
                            .and_then(|r| r.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>");
                        results.push(aborted_entry(op.tool, Some(format!(
                            "not executed: op #{failed_index} ({failed_tool:?}) failed earlier in this chain, \
                             so the chain aborted before reaching this op. Fix op #{failed_index} — this \
                             op's own arguments, including any $prev reference, were never evaluated."
                        ))));
                        continue;
                    }
                    parse_content[i] = parse_content_requested(&op, prev_result.as_ref());
                    let op_mode = mode_for_op(i);
                    // AlwaysVerbose verbs override the caller's presentation mode.
                    let presentation_policy = self.registry.presentation_policy_for(&op.tool);
                    let effective_mode =
                        if presentation_policy == VerbPresentationPolicy::AlwaysVerbose {
                            PresentationMode::Verbose
                        } else {
                            op_mode
                        };
                    let usage_ctx = khive_runtime::usage::UsageContext::new();
                    let operation = khive_types::OperationAttribution {
                        op_index: u32::try_from(i).expect("parser bounds operation count"),
                        ref_resolution: if op
                            .args
                            .values()
                            .any(|arg| !matches!(arg, ArgValue::Value(_)))
                        {
                            khive_types::RefResolution::Resolved
                        } else {
                            khive_types::RefResolution::Literal
                        },
                    };
                    match khive_storage::operation_context::scope_operation_attribution(
                        operation,
                        khive_runtime::usage::scope(
                            usage_ctx.clone(),
                            self.dispatch_op(op, prev_result.as_ref(), from_wire, identity),
                        ),
                    )
                    .await
                    {
                        Ok(mut result_obj) => {
                            stamp_usage(&mut result_obj, &usage_ctx);
                            // Guard against a pathologically deep handler result
                            // (e.g. `traverse`/`context`) before it is ever cloned
                            // into `$prev` context or handed to presentation/
                            // serialization, both of which recurse natively over
                            // `Value` and would otherwise be exposed to the same
                            // unbounded-nesting stack-overflow risk (CWE-674) the
                            // DSL parser guard already closes for syntax input.
                            match chain_aggregation_depth_reject(result_obj) {
                                Err(error_entry) => {
                                    results.push(error_entry);
                                    prev_result = None;
                                    aborted_from = Some(i + 1);
                                    continue;
                                }
                                Ok(result_obj) => {
                                    // Extract canonical result for $prev (pre-presentation).
                                    prev_result = result_obj.get("result").cloned();
                                    // Apply presentation to the result field only,
                                    // using the effective mode (AlwaysVerbose override honored).
                                    let content_scope = note_content_scope(
                                        parse_content[i],
                                        result_obj["tool"].as_str().unwrap_or_default(),
                                        &result_obj["result"],
                                        &self.registry,
                                    );
                                    let presented_obj = apply_presentation_to_result(
                                        result_obj,
                                        effective_mode,
                                        now_unix,
                                        presentation_policy,
                                        content_scope,
                                    );
                                    results.push(presented_obj);
                                }
                            }
                        }
                        Err(failure) => {
                            let mut entry = failure.into_entry();
                            stamp_usage(&mut entry, &usage_ctx);
                            results.push(entry);
                            aborted_from = Some(i + 1);
                        }
                    }
                }

                let succeeded = results
                    .iter()
                    .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
                    .count();
                let aborted = results
                    .iter()
                    .filter(|r| r.get("aborted").and_then(Value::as_bool) == Some(true))
                    .count();
                let failed = total - succeeded - aborted;
                json!({
                    "results": results,
                    "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": aborted },
                    "status": batch_status(failed, aborted),
                })
            }
        };
        let content_scopes = response["results"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, entry)| {
                note_content_scope(
                    parse_content[i] && entry["ok"] == true,
                    entry["tool"].as_str().unwrap_or_default(),
                    &entry["result"],
                    &self.registry,
                )
            })
            .collect();
        (response, content_scopes)
    }

    /// ADR-016 Amendment 2 executor for a bracketed batch of linear chains.
    ///
    /// Each `ranges` entry is a unit: its own leaves dispatch sequentially
    /// with their own `$prev`, exactly like plain chain mode (only a unit's
    /// first leaf is barred from `$prev`). Units themselves run
    /// concurrently, bounded by `context.max_batch_concurrency`, and share
    /// one aggregate response budget; once it is exhausted, no leaf
    /// anywhere in the request starts; an already-admitted leaf keeps its
    /// real disposition, and the next never-started leaf of every affected
    /// unit is refused with `response_budget_exceeded`.
    ///
    /// A static write-key conflict between two different units refuses both
    /// units before either dispatches a single leaf; a key repeated inside
    /// one unit is left alone, since that unit's own leaves are already
    /// ordered.
    ///
    /// Returns the response envelope plus the `(global leaf index,
    /// parse_content)` pairs for every leaf this call actually dispatched,
    /// so the caller can fold them into the request-wide `parse_content`
    /// vector before its own trailing `content_scopes` pass.
    async fn run_parallel_units(
        &self,
        ops: Vec<ParsedOp>,
        ranges: Vec<Range<usize>>,
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
        context: RunParsedContext<'_>,
        now_unix: PresentationNow,
    ) -> (Value, Vec<(usize, bool)>) {
        let RunParsedContext {
            enforce_response_budget,
            max_batch_concurrency,
            from_wire,
            identity,
        } = context;
        let total = ops.len();
        let response_budget = if enforce_response_budget {
            BATCH_RESPONSE_BUDGET_BYTES
        } else {
            usize::MAX
        };
        let budget = Arc::new(UnitBudget::new(response_budget));
        let unit_conflicts = unit_write_key_conflicts(&ops, &ranges);
        let presentation_per_op: Arc<Vec<Option<PresentationMode>>> =
            Arc::new(presentation_per_op.unwrap_or_default());

        let mut leaf_slots: Vec<Option<ParsedOp>> = ops.into_iter().map(Some).collect();
        let unit_tasks: Vec<UnitTask<_>> = ranges
            .iter()
            .cloned()
            .enumerate()
            .map(|(unit_index, range)| {
                let leaves: Vec<ParsedOp> = range
                    .clone()
                    .map(|i| {
                        leaf_slots[i]
                            .take()
                            .expect("each leaf belongs to exactly one range")
                    })
                    .collect();
                let conflicts = unit_conflicts.get(&unit_index).cloned();
                let budget = budget.clone();
                let presentation_per_op = presentation_per_op.clone();
                UnitTask {
                    future: async move {
                        let mut entries: Vec<Value> = Vec::with_capacity(leaves.len());
                        let mut content_updates: Vec<(usize, bool)> =
                            Vec::with_capacity(leaves.len());

                        if let Some(conflicts) = conflicts {
                            let positions: Vec<String> = conflicts
                                .iter()
                                .map(|c| format!("op #{} in unit #{}", c.other_leaf, c.other_unit))
                                .collect();
                            let message = format!(
                                "write-key conflict: this unit shares a write key with another \
                                 unit in the same request ({}). Both units are refused before \
                                 dispatch; split them into separate requests.",
                                positions.join(", ")
                            );
                            let mut leaves = leaves.into_iter();
                            let first = leaves.next().expect("a unit always has at least one leaf");
                            entries.push(failure_entry(
                                first.tool,
                                json!(message),
                                DomainDisposition::NotCommitted,
                            ));
                            for leaf in leaves {
                                entries.push(aborted_entry(
                                    leaf.tool,
                                    Some(
                                        "not executed: this unit was refused before dispatch \
                                         because it shares a write key with another unit in the \
                                         same request."
                                            .to_string(),
                                    ),
                                ));
                            }
                            return UnitOutcome {
                                unit_index,
                                entries,
                                content_updates,
                            };
                        }

                        let mut prev_result: Option<Value> = None;
                        let mut aborted_from: Option<usize> = None;
                        for (step_index, op) in leaves.into_iter().enumerate() {
                            let global_index = range.start + step_index;
                            if let Some(failed_at) = aborted_from {
                                let failed_step = failed_at - 1;
                                let failed_tool = entries
                                    .get(failed_step)
                                    .and_then(|r| r.get("tool"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("<unknown>");
                                entries.push(aborted_entry(
                                    op.tool,
                                    Some(format!(
                                        "not executed: leaf #{failed_step} of this unit \
                                         ({failed_tool:?}) failed earlier in the same unit, so \
                                         this unit's tail aborted before reaching this leaf."
                                    )),
                                ));
                                continue;
                            }
                            if budget.is_breached() {
                                entries.push(batch_budget_error(&op.tool, response_budget));
                                aborted_from = Some(step_index + 1);
                                continue;
                            }

                            let content_requested =
                                parse_content_requested(&op, prev_result.as_ref());
                            content_updates.push((global_index, content_requested));
                            let op_mode = presentation_per_op
                                .get(global_index)
                                .and_then(|mode| *mode)
                                .unwrap_or(presentation);
                            let presentation_policy =
                                self.registry.presentation_policy_for(&op.tool);
                            let effective_mode =
                                if presentation_policy == VerbPresentationPolicy::AlwaysVerbose {
                                    PresentationMode::Verbose
                                } else {
                                    op_mode
                                };
                            let usage_ctx = khive_runtime::usage::UsageContext::new();
                            let operation = khive_types::OperationAttribution {
                                op_index: u32::try_from(global_index)
                                    .expect("parser bounds operation count"),
                                ref_resolution: if op
                                    .args
                                    .values()
                                    .any(|arg| !matches!(arg, ArgValue::Value(_)))
                                {
                                    khive_types::RefResolution::Resolved
                                } else {
                                    khive_types::RefResolution::Literal
                                },
                            };
                            match khive_storage::operation_context::scope_operation_attribution(
                                operation,
                                khive_runtime::usage::scope(
                                    usage_ctx.clone(),
                                    self.dispatch_op(op, prev_result.as_ref(), from_wire, identity),
                                ),
                            )
                            .await
                            {
                                Ok(mut result_obj) => {
                                    stamp_usage(&mut result_obj, &usage_ctx);
                                    match chain_aggregation_depth_reject(result_obj) {
                                        Err(error_entry) => {
                                            budget.record(&error_entry);
                                            entries.push(error_entry);
                                            prev_result = None;
                                            aborted_from = Some(step_index + 1);
                                        }
                                        Ok(result_obj) => {
                                            prev_result = result_obj.get("result").cloned();
                                            let content_scope = note_content_scope(
                                                content_requested,
                                                result_obj["tool"].as_str().unwrap_or_default(),
                                                &result_obj["result"],
                                                &self.registry,
                                            );
                                            let presented = apply_presentation_to_result(
                                                result_obj,
                                                effective_mode,
                                                now_unix,
                                                presentation_policy,
                                                content_scope,
                                            );
                                            budget.record(&presented);
                                            entries.push(presented);
                                        }
                                    }
                                }
                                Err(failure) => {
                                    let mut entry = failure.into_entry();
                                    stamp_usage(&mut entry, &usage_ctx);
                                    budget.record(&entry);
                                    entries.push(entry);
                                    aborted_from = Some(step_index + 1);
                                }
                            }
                        }

                        UnitOutcome {
                            unit_index,
                            entries,
                            content_updates,
                        }
                    },
                }
            })
            .collect();

        let outcomes = execute_bounded_units(unit_tasks, max_batch_concurrency).await;

        let mut results: Vec<Option<Value>> = (0..total).map(|_| None).collect();
        let mut content_updates: Vec<(usize, bool)> = Vec::new();
        for outcome in outcomes {
            let range = ranges[outcome.unit_index].clone();
            for (offset, mut entry) in outcome.entries.into_iter().enumerate() {
                // Additive on this bracketed shape only: an ordinary single,
                // flat batch, or top-level chain response carries none of
                // these fields, unchanged by this amendment.
                if let Some(object) = entry.as_object_mut() {
                    object.insert("op_index".to_string(), json!(range.start + offset));
                    object.insert("unit_index".to_string(), json!(outcome.unit_index));
                    object.insert("step_index".to_string(), json!(offset));
                }
                results[range.start + offset] = Some(entry);
            }
            content_updates.extend(outcome.content_updates);
        }
        let results: Vec<Value> = results
            .into_iter()
            .map(|entry| entry.expect("every leaf in every unit produces exactly one entry"))
            .collect();

        let succeeded = results
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(true))
            .count();
        let aborted = results
            .iter()
            .filter(|r| r.get("aborted").and_then(Value::as_bool) == Some(true))
            .count();
        let failed = total - succeeded - aborted;
        let response = json!({
            "results": results,
            "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": aborted },
            "status": batch_status(failed, aborted),
        });
        (response, content_updates)
    }
}

/// Request-derived policy stays outside the public response envelope and is
/// limited to actual note results. Caller content never supplies a policy marker.
fn parse_content_requested(op: &ParsedOp, prev_result: Option<&Value>) -> bool {
    matches!(op.tool.as_str(), "get" | "list")
        && op.args.get("parse_content").and_then(|arg| match arg {
            ArgValue::Value(value) => Some(value.clone()),
            _ => prev_result.and_then(|prev| arg.resolve_all(prev)),
        }) == Some(Value::Bool(true))
}

fn note_content_scope(
    enabled: bool,
    tool: &str,
    result: &Value,
    registry: &VerbRegistry,
) -> NoteContentScope {
    if !enabled {
        return NoteContentScope::None;
    }
    let note_kinds = registry.all_note_kinds();
    let is_note = |record: &Value| {
        // By-ID get and broad note lists can read a persisted kind whose pack
        // is no longer loaded. Its versioned Note shape is still a note; do not
        // let that one row disable opacity for an otherwise ordinary note page.
        let versioned_note = record.get("version").and_then(Value::as_u64).is_some()
            && record.get("id").is_some_and(Value::is_string)
            && record.get("created_at").is_some_and(Value::is_string)
            && record.get("updated_at").is_some_and(Value::is_string);
        record
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| note_kinds.contains(&kind) || versioned_note)
            && record.get("content").is_some()
    };
    if tool == "get" && is_note(result) {
        return NoteContentScope::Record;
    }
    if tool == "list" {
        for (key, scope) in [
            ("items", NoteContentScope::Items),
            ("notes", NoteContentScope::Notes),
        ] {
            if result
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|records| !records.is_empty() && records.iter().all(is_note))
            {
                return scope;
            }
        }
    }
    NoteContentScope::None
}

/// Route a `link` or `search` verb through `coord` when in multi-backend mode.
/// Shared logic behind both dispatch sites (`dispatch_op` chain mode and the
/// parallel/single closure in `run_parsed`). Returns `Some(Ok(OpSuccess))` when
/// the coordinator handled the op, `Some(Err(DispatchFailure))` on a
/// coordinator error (including fail-closed namespace rejection), `None` to
/// fall through to the registry. Must apply the exact same fail-closed
/// namespace rule as `VerbRegistry::dispatch` (RUNTIME-AUD-002, #433) — see
/// `crates/khive-mcp/docs/api/coordinator.md`.
async fn dispatch_via_coordinator_inner(
    coord: &dyn CoordinatorService,
    registry: &VerbRegistry,
    tool: &str,
    args_value: &Value,
    identity: Option<&khive_runtime::RequestIdentity>,
) -> Option<Result<OpSuccess, DispatchFailure>> {
    // Only link/search are ever intercepted here.
    if !matches!(tool, "link" | "search") {
        return None;
    }

    match tool {
        "link" => {
            // Only intercept single-link form (not bulk `links` array).
            // Bulk link falls through to the registry for now.
            if args_value.get("links").is_some() {
                return None;
            }
            // Normalize the `source`/`target`/`kind` aliases as the handler does, so both
            // spellings take this path; a call the handler would refuse falls through to it.
            let mut link_args = args_value.clone();
            khive_pack_kg::handlers::normalize_link_params(&mut link_args).ok()?;
            let source_str = link_args.get("source_id")?.as_str()?;
            let target_str = link_args.get("target_id")?.as_str()?;
            let relation_str = link_args.get("relation")?.as_str()?;

            // Only intercept when both endpoints are parseable UUIDs.
            // Name/prefix resolution requires single-backend context — fall through.
            let source_id: uuid::Uuid = source_str.parse().ok()?;
            let target_id: uuid::Uuid = target_str.parse().ok()?;
            let relation: EdgeRelation = relation_str.parse().ok()?;
            let weight = link_args
                .get("weight")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let metadata = link_args.get("metadata").cloned();
            let dependency_kind = link_args.get("dependency_kind").cloned();
            let resurrect = link_args
                .get("resurrect")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let result = registry
                .dispatch_intercepted_with_metadata_and_disposition(
                    tool,
                    args_value,
                    identity,
                    |namespace| async move {
                        // The coordinator receives the same metadata as the KG
                        // handler: a top-level dependency_kind fills the key only
                        // when metadata does not already contain it.
                        let dependency_kind = match dependency_kind {
                            None | Some(Value::Null) => None,
                            Some(Value::String(value)) => Some(value),
                            Some(_) => {
                                return Err(RuntimeError::InvalidInput(
                                    "dependency_kind must be a string".into(),
                                ));
                            }
                        };
                        let metadata =
                            khive_runtime::merge_entry_metadata(metadata, dependency_kind)?;
                        let coord_result = coord
                            .link(
                                &namespace, source_id, target_id, relation, weight, metadata,
                                resurrect,
                            )
                            .await
                            .map_err(RuntimeError::from)?;
                        let mut raw = serde_json::to_value(&coord_result.edge)
                            .unwrap_or_else(|e| json!({"error": format!("serialize edge: {e}")}));
                        if relation.is_symmetric() {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert("source_id".to_string(), json!(source_id.to_string()));
                                obj.insert("target_id".to_string(), json!(target_id.to_string()));
                            }
                        }
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("mutation".to_string(), json!(coord_result.mutation.name()));
                        }
                        Ok(InterceptedDispatchResult::new(raw, ()))
                    },
                )
                .await;
            Some(
                result
                    .map(|outcome| OpSuccess::complete(outcome.result))
                    .map_err(|error| DispatchFailure::from_dispatch(tool, error)),
            )
        }
        "search" => {
            if args_value.get("help").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let mut handler_args = args_value.clone();
            if let Some(fields) = handler_args.as_object_mut() {
                fields.remove("namespace");
            }
            // Preserve the coordinated read scope and the actor resolved by
            // the gate in one sealed token through backend fan-out.
            let extra_visible = coordinator_search_visibility(registry, args_value, identity);
            let result = registry
                .dispatch_intercepted_with_token_and_disposition(
                    tool,
                    args_value,
                    identity,
                    |token| async move {
                        // Match normal registry dispatch ordering: the gate has
                        // already authorized this namespace before handler-level
                        // search validation runs inside the intercepted closure.
                        let request = ValidatedSearchRequest::from_value(handler_args, registry)?;
                        let coord_result = coord
                            .fan_out_search_scoped(&request, &token, args_value, &extra_visible)
                            .await?;
                        khive_storage::ensure_request_read_active("search")?;
                        // Preserve the coordinator search response's compatibility
                        // fields, and add the KG single-backend handler's canonical
                        // row fields for shape parity (MIN-1): `kind` (duplicates
                        // entity_kind/note_kind), `name`, `created_at`, `updated_at`,
                        // and `version`. Shared KG rank conversion also adds
                        // `score`, `rank_score`, `rank_score_kind`, and `signals`.
                        let result_val = if request.substrate() == SearchSubstrate::Note {
                            let items: Vec<Value> = coord_result
                                .note_hits
                                .iter()
                                .filter(|h| h.score >= request.min_rank_score())
                                .filter_map(|h| {
                                    let version = coord_result.note_versions.get(&h.note_id)?;
                                    let note_kind = coord_result.note_kinds.get(&h.note_id);
                                    let name =
                                        coord_result.note_names.get(&h.note_id).cloned().flatten();
                                    let created_at = coord_result
                                        .note_created_at
                                        .get(&h.note_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let updated_at = coord_result
                                        .note_updated_at
                                        .get(&h.note_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let ranking =
                                        search_rank_fields(h.score, h.rank_score_kind, h.signals);
                                    let mut row = json!({
                                        "id": h.note_id.to_string(),
                                        "kind": note_kind,
                                        "note_kind": note_kind,
                                        "name": name,
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                        "updated_at": updated_at,
                                        "version": version,
                                    });
                                    row.as_object_mut().expect("search row object").extend(
                                        ranking.as_object().expect("ranking fields object").clone(),
                                    );
                                    Some(row)
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        } else {
                            let items: Vec<Value> = coord_result
                                .entity_hits
                                .iter()
                                .filter(|h| h.score >= request.min_rank_score())
                                .map(|h| {
                                    let entity_kind = coord_result.entity_kinds.get(&h.entity_id);
                                    let created_at = coord_result
                                        .entity_created_at
                                        .get(&h.entity_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let updated_at = coord_result
                                        .entity_updated_at
                                        .get(&h.entity_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let ranking =
                                        search_rank_fields(h.score, h.rank_score_kind, h.signals);
                                    let version = coord_result.entity_versions.get(&h.entity_id);
                                    let mut row = json!({
                                        "id": h.entity_id.to_string(),
                                        "kind": entity_kind,
                                        "entity_kind": entity_kind,
                                        "name": h.title,
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                        "updated_at": updated_at,
                                        "version": version,
                                    });
                                    row.as_object_mut().expect("search row object").extend(
                                        ranking.as_object().expect("ranking fields object").clone(),
                                    );
                                    row
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        };
                        let degradation = SearchDegradation::from_result(
                            &coord_result,
                            &result_val,
                            request.text_mode_name(),
                        );

                        Ok(InterceptedDispatchResult::new(result_val, degradation))
                    },
                )
                .await;
            Some(match result {
                Ok(outcome) => {
                    let is_empty = outcome
                        .result
                        .as_array()
                        .map(|items| items.is_empty())
                        .unwrap_or(true);
                    // ADR-130 §1: a backend failure with zero surviving hits
                    // (post server-side filtering, min_rank_score included) is a
                    // failed operation, not a successful empty result — the
                    // "no match" reading is not established when the answer
                    // may be sitting on the backend that never responded.
                    if outcome.metadata.is_partial() && is_empty {
                        let mut error = search_incomplete_error(outcome.metadata);
                        error
                            .as_object_mut()
                            .expect("structured search error")
                            .insert("domain_result".into(), outcome.result);
                        Err(DispatchFailure::committed(tool, error))
                    } else {
                        Ok(OpSuccess {
                            result: outcome.result,
                            degradation: outcome.metadata,
                        })
                    }
                }
                Err(error) => Err(DispatchFailure::from_dispatch(tool, error)),
            })
        }
        _ => None,
    }
}

/// Resolve the coordinator search boundary's extra read-visibility set
/// (MAJ-3 fix), mirroring the normal registry dispatch path's default-case
/// widening to `['local'] ∪ visible_namespaces`
/// (`khive_runtime::pack::VerbRegistry::dispatch_with_identity`,
/// `crates/khive-runtime/src/pack.rs`). Without this, `fan_out_search`
/// authorizes each backend token against the resolved primary namespace
/// alone, so a namespace visible only through `visible_namespaces` silently
/// drops out of coordinator search results even though the same caller's
/// non-coordinator (single-backend) search would see it.
///
/// An explicit `namespace=` request parameter intentionally narrows
/// visibility to that one namespace — this returns an empty set in that
/// case, unwidened, exactly like the registry path's `explicit_namespace`
/// branch.
fn coordinator_search_visibility(
    registry: &VerbRegistry,
    args_value: &Value,
    identity: Option<&khive_runtime::RequestIdentity>,
) -> Vec<khive_runtime::Namespace> {
    let explicit_namespace = args_value.get("namespace").is_some();
    if explicit_namespace {
        return Vec::new();
    }
    let mut extra_visible: Vec<khive_runtime::Namespace> = match identity {
        Some(id) => id
            .visible_namespaces
            .iter()
            .filter_map(|s| match khive_runtime::Namespace::parse(s) {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    tracing::warn!(
                        namespace = %s,
                        error = %e,
                        "coordinator_search_visibility: skipping invalid visible_namespace \
                         entry from per-request identity"
                    );
                    None
                }
            })
            .collect(),
        None => registry.visible_namespaces().to_vec(),
    };
    extra_visible.push(khive_runtime::Namespace::local());
    extra_visible
}

/// Every runtime variant is explicitly covered. Dispatch provenance, not the
/// variant, determines whether the domain handler ran successfully, except for
/// the named write outcomes, which carry their own domain proof.
/// Apply MCP transport limits after the shared typed projection. The boundary's
/// original disposition decides whether an obligation result may be retained;
/// named error overrides remain exactly as emitted by the shared projection.
fn runtime_error_value(error: RuntimeError, disposition: DomainDisposition) -> Value {
    let projected = khive_runtime::runtime_error_value(error, disposition);
    let projected_disposition = error_disposition(&projected);
    let mut value = error_with_disposition(projected, disposition);
    value["domain_disposition"] = json!(projected_disposition.as_str());
    value
}

/// Returns `true` when a raw handler `result` value's container nesting is
/// within [`khive_request::NESTING_DEPTH_LIMIT`]. Callers MUST call this on
/// the raw value straight out of coordinator/registry dispatch, before any
/// recursive `Value` operation (clone, serialize, presentation transform)
/// touches it — see `crates/khive-mcp/docs/design.md` (Result depth guard).
fn result_within_depth_limit(result: &Value) -> bool {
    khive_request::value_nesting_within_limit(result, khive_request::NESTING_DEPTH_LIMIT)
}

/// Per-op error payload for a handler result that failed
/// [`result_within_depth_limit`]. Carries only the configured depth limit,
/// never the oversized value itself.
fn depth_error_payload(context: &str) -> Value {
    json!({
        "kind": "result_too_deep",
        "code": "result_too_deep",
        "message": format!(
            "op result nesting depth exceeds max {}{context}",
            khive_request::NESTING_DEPTH_LIMIT
        ),
    })
}

/// Build the `{ok: true, tool, result}` envelope for a successful op,
/// without re-serializing an already-owned `Value` through `json!` (which
/// would call `serde_json::to_value` and recurse over the whole tree
/// again). The depth check must already have passed before this is called.
fn ok_envelope(tool: String, success: OpSuccess) -> Value {
    let OpSuccess {
        result,
        degradation,
    } = success;
    let SearchDegradation {
        status,
        retryable: _,
        arm_participation,
        retry_after_ms: _,
        missing_backends,
        backend_errors,
        backend_errors_omitted,
    } = degradation;
    let is_partial = status == Some(SearchStatus::Partial);
    let extra_fields = usize::from(status.is_some())
        + usize::from(arm_participation.is_some())
        + if is_partial {
            3 + usize::from(backend_errors_omitted > 0) * 2
        } else {
            0
        };
    let mut map = serde_json::Map::with_capacity(3 + extra_fields);
    map.insert("ok".to_string(), Value::Bool(true));
    map.insert("tool".to_string(), Value::String(tool));
    map.insert("result".to_string(), result);
    // ADR-130 §1: `status` is present on every successful search envelope
    // (complete or partial); absent for every other verb.
    if let Some(status) = status {
        map.insert(
            "status".to_string(),
            Value::String(status.as_str().to_string()),
        );
    }
    if let Some(participation) = arm_participation {
        map.insert(
            "arm_participation".to_string(),
            search_arm_participation_value(participation),
        );
    }
    // Legacy `partial`/`missing_backends` alias, compatibility-release only
    // (ADR-130 §Compatibility) — omitted for `status="complete"`.
    if is_partial {
        map.insert("partial".to_string(), Value::Bool(true));
        map.insert(
            "missing_backends".to_string(),
            Value::Array(missing_backends.into_iter().map(Value::String).collect()),
        );
        map.insert(
            "backend_errors".to_string(),
            backend_errors_value(&backend_errors),
        );
        if backend_errors_omitted > 0 {
            map.insert("backend_errors_truncated".to_string(), Value::Bool(true));
            map.insert(
                "backend_errors_omitted".to_string(),
                json!(backend_errors_omitted),
            );
        }
    }
    Value::Object(map)
}

/// Discard a rejected over-limit `Value` without native recursion.
///
/// `Value`'s derived `Drop` walks nested containers the same way `Clone`
/// and `Serialize` do, so simply letting a pathologically deep `result`
/// fall out of scope after the depth guard rejects it would trade a stack
/// overflow during serialization for one during drop. Draining containers
/// onto an explicit heap-allocated worklist keeps each removal O(1) on the
/// call stack regardless of nesting depth.
fn drop_value_iteratively(value: Value) {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.into_values()),
            _ => {}
        }
    }
}

/// Builds a `substitution_error` payload for a `$prev` argument that failed
/// to resolve (`resolve_all` returned `None`). Uses [`ArgValue::find_prev_failure`]
/// to identify exactly which lookup failed and why — a missing field/index, a
/// path segment applied to the wrong JSON type, or unsupported bracket syntax
/// — each worded differently so the caller isn't left with one generic
/// "not found" for three different mistakes. Falls back to a generic message
/// only if `find_prev_failure` cannot explain a miss `resolve_all` reported
/// (defensive; the two are expected to always agree).
fn substitution_error_payload(name: &str, arg_val: &ArgValue, prev: &Value) -> Value {
    let Some(failure) = arg_val.find_prev_failure(prev) else {
        let fields_hint = if let Value::Object(map) = prev {
            let mut fields: Vec<&str> = map.keys().map(String::as_str).collect();
            fields.sort_unstable();
            format!(" Available top-level fields: [{}]", fields.join(", "))
        } else {
            String::new()
        };
        return json!({
            "kind": "substitution_error",
            "reason": "path_not_found",
            "message": format!(
                "argument {name:?}: one or more $prev paths not found in prior result.{fields_hint}"
            ),
        });
    };
    let reason = match &failure {
        PrevFailure::NotFound { .. } => "path_not_found",
        PrevFailure::WrongType { .. } => "path_wrong_type",
        PrevFailure::Unsupported { .. } => "path_unsupported",
    };
    json!({
        "kind": "substitution_error",
        "reason": reason,
        "message": format!(
            "argument {name:?}: {failure}. $prev resolves only against the immediately \
             preceding op's result — a non-adjacent dependency cannot be expressed inside \
             one chain; split into separate calls and carry the value across yourself."
        ),
    })
}

/// ADR-103 Amendment 2: stamp the per-op envelope entry with the dispatch's
/// frozen usage snapshot when its counters remain complete. A marked context
/// omits the key even after freeze. Best-effort — never alters ok/error status.
fn stamp_usage(entry: &mut Value, ctx: &khive_runtime::usage::UsageContext) {
    if let Value::Object(map) = entry {
        match ctx.shipping_snapshot() {
            Some(snapshot) => {
                map.insert("usage".to_string(), snapshot);
            }
            None => {
                map.remove("usage");
            }
        }
    }
}

/// Add host-owned ticker liveness to the schedule pack's canonical agenda
/// payload. The pack owns scheduled intent; the MCP host owns the daemon loop,
/// so this decoration stays at their dispatch boundary instead of persisting a
/// process heartbeat in schedule data.
fn decorate_schedule_agenda_with_ticker_health(
    tool: &str,
    is_help: bool,
    mut result: Value,
    last_tick_micros: &AtomicI64,
) -> Value {
    if tool != "schedule.agenda" || is_help {
        return result;
    }
    let last_tick = last_tick_micros.load(Ordering::Acquire);
    let last_tick_at = (last_tick > 0).then(|| khive_runtime::micros_to_iso(last_tick));
    if let Some(result) = result.as_object_mut() {
        result.insert(
            "ticker".to_string(),
            json!({ "last_tick_at": last_tick_at }),
        );
    }
    result
}

/// Chain-mode (`dispatch_op`) success path: check the raw handler `result`
/// against the depth guard before it is ever cloned into `$prev` context or
/// wrapped in the response envelope. On violation returns a `result_too_deep`
/// error that does not embed the oversized value, and discards the rejected
/// value iteratively so its own drop can't overflow the stack either.
fn chain_ok_envelope_or_depth_error(
    tool: String,
    success: OpSuccess,
) -> Result<Value, DispatchFailure> {
    if !result_within_depth_limit(&success.result) {
        drop_value_iteratively(success.result);
        return Err(DispatchFailure::committed(
            tool,
            depth_error_payload("; cannot be used as $prev chain context"),
        ));
    }
    Ok(ok_envelope(tool, success))
}

/// Parallel/single-mode success path: check the raw handler `result` against
/// the depth guard *before* it is handed to `present` (which recurses
/// natively over `Value` in agent mode) or wrapped in the response envelope.
/// On violation returns a `result_too_deep` per-op error entry that does not
/// embed the oversized value, and discards the rejected value iteratively
/// (see [`drop_value_iteratively`]).
fn present_ok_envelope_or_depth_error(
    tool: String,
    mut success: OpSuccess,
    mode: PresentationMode,
    now_unix: impl Into<PresentationNow>,
    policy: VerbPresentationPolicy,
    content_scope: NoteContentScope,
) -> Value {
    if !result_within_depth_limit(&success.result) {
        drop_value_iteratively(success.result);
        return failure_entry(tool, depth_error_payload(""), DomainDisposition::Committed);
    }
    success.result = content_scope.protect(success.result, |value| {
        present_with_policy_at(value, mode, now_unix.into(), policy)
    });
    ok_envelope(tool, success)
}

/// Returns `true` if a dispatched op's canonical `result` field nests
/// container values (`[`/`{`) deeper than [`khive_request::NESTING_DEPTH_LIMIT`].
///
/// This is a second, defense-in-depth check retained on the chain-mode
/// aggregation path in [`KhiveMcpServer::run_parsed`]: by the time it runs,
/// [`chain_ok_envelope_or_depth_error`] has already screened the same
/// `result` field inside `dispatch_op`, so this should never trip in
/// practice. It stays cheap (iterative, not recursive) so keeping it costs
/// nothing and catches a future refactor that bypasses the earlier guard.
fn result_exceeds_depth_limit(result_obj: &Value) -> bool {
    result_obj
        .get("result")
        .is_some_and(|v| !result_within_depth_limit(v))
}

/// Chain-mode aggregation-loop seam in [`KhiveMcpServer::run_parsed`]:
/// defense-in-depth depth check on a dispatched op's full `result_obj`
/// envelope (should never trip — `dispatch_op` already screened `result`).
/// Returns the unchanged envelope on success, or an already-built error
/// entry on rejection. See `crates/khive-mcp/docs/design.md` (Result depth
/// guard) for why the rejected envelope is drained iteratively.
fn chain_aggregation_depth_reject(result_obj: Value) -> Result<Value, Value> {
    if result_exceeds_depth_limit(&result_obj) {
        let tool_name = result_obj
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let error_entry = failure_entry(
            tool_name,
            depth_error_payload("; cannot be used as $prev chain context"),
            DomainDisposition::Committed,
        );
        drop_value_iteratively(result_obj);
        return Err(error_entry);
    }
    Ok(result_obj)
}

/// Apply the presentation transform to the `result` field of a successful
/// per-op envelope, leaving error envelopes unchanged.
///
/// Error envelopes are never transformed — only successful `result` fields.
fn apply_presentation_to_result(
    mut result_obj: Value,
    mode: PresentationMode,
    now_unix: impl Into<PresentationNow>,
    policy: VerbPresentationPolicy,
    content_scope: NoteContentScope,
) -> Value {
    if result_obj.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(result_field) = result_obj.get("result").cloned() {
            let presented = content_scope.protect(result_field, |value| {
                present_with_policy_at(value, mode, now_unix.into(), policy)
            });
            if let Some(obj) = result_obj.as_object_mut() {
                obj.insert("result".to_string(), presented);
            }
        }
    }
    result_obj
}

// ── single MCP tool ─────────────────────────────────────────────────────────

fn request_read_timeout() -> std::time::Duration {
    static TIMEOUT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        // The storage resolver owns the accepted range and warns on a corrected value.
        let timeout = khive_storage::request_read_timeout_from_env();
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_REQUEST_READ_TIMEOUT_SECS",
            timeout.as_secs().to_string(),
        );
        timeout
    })
}

/// Ensure every MCP request attempt has a daemon/audit correlation id.
///
/// A caller-supplied id is an explicit retry/correlation key and therefore
/// wins unchanged. Otherwise the bridge mints an opaque nonzero 64-bit id from
/// UUID entropy before daemon forwarding or local fallback. The same value is
/// then echoed by the daemon frame and stamped into every operation's audit
/// resource, making a handler that disappears after admission observable.
fn ensure_bridge_request_id(params: &mut RequestParams) -> u64 {
    if let Some(request_id) = params.request_id {
        return request_id;
    }

    let bytes = uuid::Uuid::new_v4().into_bytes();
    let high = u64::from_be_bytes(
        bytes[..8]
            .try_into()
            .expect("UUID high half is eight bytes"),
    );
    let low = u64::from_be_bytes(bytes[8..].try_into().expect("UUID low half is eight bytes"));
    let request_id = (high ^ low).max(1);
    params.request_id = Some(request_id);
    request_id
}

#[cfg(test)]
async fn scope_mcp_request_read_cancellation<F>(
    cancellation: tokio_util::sync::CancellationToken,
    future: F,
) -> F::Output
where
    F: Future,
{
    scope_mcp_request_read_cancellation_with_timeout(cancellation, request_read_timeout(), future)
        .await
}

async fn scope_mcp_request_read_cancellation_with_timeout<F>(
    cancellation: tokio_util::sync::CancellationToken,
    timeout: std::time::Duration,
    future: F,
) -> F::Output
where
    F: Future,
{
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    // Seed synchronously: a pre-cancelled rmcp context must be visible even
    // when the wrapped request future is ready before the bridge task's first
    // poll.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(cancellation.is_cancelled());
    let _bridge = AbortOnDrop(tokio::spawn(async move {
        cancellation.cancelled().await;
        let _ = cancel_tx.send(true);
    }));
    khive_storage::scope_request_read_cancellation(
        cancel_rx,
        khive_storage::scope_request_read_deadline(timeout, future),
    )
    .await
}

#[tool_router]
impl KhiveMcpServer {
    #[tool(description = r#"Run one or more khive verbs in a single MCP call.

Set plan=true with ops alone to check syntax without execution. The result has
parsed, mode, stage_count, stages (verb, pack, known, args, prev_refs), and parser
limits. A syntax error returns parsed=false and error, with no stages. Planning
does not check permission or resolve references. presentation, presentation_per_op,
format, format_per_op, save_to, and request_id cannot accompany plan=true.

ops syntax:

  Single op   : verb(name=value, name=value)
  Batch       : [verb(...), verb(...)]                 — parallel, max 100
  Chain       : verb1(...) | verb2(id=$prev.id)        — sequential, $prev
  JSON form   : [{"tool":"verb","args":{...}}, ...]    — INDEPENDENT ops only

Argument values are JSON literals: strings (double-quoted), numbers, booleans,
null, arrays, objects. Strings may contain commas / parens; escape with \".

Chain-only: $prev resolves to the prior op's result. Path extraction syntax:
  $prev               — full result
  $prev.field         — nested object field
  $prev.items[0].id   — array index
  $prev[2]            — top-level array index
Quoted strings that contain $prev are promoted to substitutions (e.g. id="$prev.id"
is the same as id=$prev.id). To pass a literal "$prev", escape with backslash:
\"\\$prev\". JSON form is for independent ops only — any $prev string in JSON
form is rejected.

Response shape:

  {
    "results": [ {"ok": true, "tool": "verb", "result": {...}}, ... ],
    "summary": { "total": N, "succeeded": N, "failed": N, "aborted": N },
    "status": "success" | "partial"
  }

Parallel: a failed op does NOT abort siblings. Chain: failure aborts remaining
ops (reported as {"ok": false, "aborted": true}). Committed ops are not rolled back.
`status` is "partial" whenever summary.failed or summary.aborted is non-zero.
The MCP tool result sets isError=true only when no op succeeded and at least
one failed or aborted. For mixed batches inspect each result and the summary.

A parallel write-heavy batch is best-effort, not atomic: `results` ordering is
not a commit prefix (an earlier entry succeeding implies nothing about a later
one, or vice versa), and one entry's safe-retry failure (e.g. `retryable:
true`, `code: "writer_pool_checkout_timeout"`, `"writer_queue_saturated"`,
or `"writer_task_begin_busy"`)
never rolls back a sibling that already committed. Inspect each result
entry's own `ok` field rather than assuming batch-level atomicity.

`comm.read` and `comm.mark_read` mutate delivery state. In a parallel batch,
either acknowledgement does not wait for or depend on comm.send/comm.reply, so
a read mark can commit even when the sibling send fails. When the mark must
depend on a send, use a chain so a failed send aborts the mark. For the common
reply-and-read flow, prefer `comm.reply`: comm.reply delivers first, then attempts
the original message's best-effort read mark.

`search` carries its own per-op `status` ("complete" | "partial") inside that
op's `result` entry, separate from the top-level batch `status` above. A
degraded-but-answered search stays ok:true with status="partial" plus a
missing_backends list, bounded backend_errors causes, and the deprecated
partial:true alias. Truncation is explicit through backend_errors_truncated and
backend_errors_omitted. When a backend failure leaves no hit standing after
filtering, the op instead fails outright with ok:false and
error.kind="search_incomplete" while retaining the same diagnostics — that case
must not be read as "no results found." Per-backend causes use kind="timeout"
for typed deadline failures and kind="backend_error" otherwise. The incomplete
error is retryable only when every failed backend leg timed out. Every such
error names retry_after_ms: 2000ms plus 250ms per additional failed backend,
capped at 10000ms and computed from the full pre-truncation failure set.
Conforming clients use at most three total attempts per logical request,
exponential backoff with nonnegative jitter (never shorter than the named
pace), and a 30s circuit breaker after three consecutive all-timeout outcomes
for the same backend set; the breaker suppresses first attempts too and admits
one half-open probe after the open interval.

Verb discovery: install the `kg` / `gtd` plugins for usage skills. The verbs
currently registered on this server (pack-derived) are listed below. Argument
schemas live in each pack's docs and SKILL.md files.

Bridge control: `bridge.diagnostics()` reads this stdio bridge's in-memory
fallback counters without contacting a daemon or store. It must be the only
operation in the request; `bridge.diagnostics(help=true)` describes its schema.

Tip: for one-shot calls, the single-op form is the densest. Use batch when
several independent ops can run together; use chain when each op needs the prior
result (e.g. create then link with the new entity's id)."#)]
    async fn request(
        &self,
        Parameters(p): Parameters<RequestParams>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<String, McpError> {
        let timeout = crate::request_policy::read_timeout(&p.ops, request_read_timeout());
        scope_mcp_request_read_cancellation_with_timeout(
            cancellation,
            timeout,
            self.request_with_cancellation(p),
        )
        .await
    }
}

/// Owned boxed future returned by the daemon-forwarding seam.
///
/// Ownership is deliberate: once daemon forwarding is admitted the exchange
/// runs in its own task, so dropping the outer MCP handler cannot drop the
/// socket future after the daemon may already have committed work.
#[cfg(unix)]
type ForwardFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<Result<String, McpError>>> + Send + 'static>,
>;

/// Function pointer type for the daemon-forwarding seam, parameterized so
/// tests can inject a spy in place of the real `forward_or_spawn_with_config_and_packs`
/// call — the real call spawns/contacts an actual daemon process, which
/// tests must not do. `packs` mirrors `forward_or_spawn_with_config_and_packs`'s own
/// optional `packs` argument exactly: the `Some`/`None` decision is made by
/// the shared call site in `request_with_forward`, not inside the adapter,
/// so a spy standing in for this seam observes the same optional argument
/// the real function would receive. `config`/`db` are always `None` at this
/// call site.
#[cfg(unix)]
type ForwardFnPtr =
    fn(khive_runtime::DaemonRequestFrame, Option<Vec<String>>, bool) -> ForwardFuture;

/// Adapts the real `forward_or_spawn_with_replay_policy` to the `ForwardFnPtr`
/// signature. A pure pass-through — the `Some`/`None` decision already
/// happened at the call site — so this boundary carries no logic a test
/// spy could fail to observe.
#[cfg(unix)]
fn forward_or_spawn_boxed(
    frame: khive_runtime::DaemonRequestFrame,
    packs: Option<Vec<String>>,
    replay_read_only: bool,
) -> ForwardFuture {
    Box::pin(async move {
        crate::daemon::forward_or_spawn_with_replay_policy(
            &frame,
            None,
            None,
            packs.as_deref(),
            replay_read_only,
        )
        .await
    })
}

impl KhiveMcpServer {
    pub(crate) fn plan_ops(&self, ops: &str) -> String {
        let catalog = self
            .registry
            .all_verbs_with_names()
            .into_iter()
            .map(|(pack, handler)| (handler.name.to_string(), pack.to_string()))
            .chain(
                self.registry
                    .mounted_verb_snapshot()
                    .into_iter()
                    .map(|verb| {
                        (
                            verb["verb"].as_str().unwrap_or_default().to_owned(),
                            verb["pack"].as_str().unwrap_or_default().to_owned(),
                        )
                    }),
            )
            .collect();
        khive_request::plan_request(ops, &catalog).to_string()
    }

    fn plan_bridge_ops(&self, ops: &str) -> String {
        let mut plan: Value =
            serde_json::from_str(&self.plan_ops(ops)).expect("plan output is always JSON");
        if let Some(stages) = plan.get_mut("stages").and_then(Value::as_array_mut) {
            for stage in stages {
                if stage["verb"] == "bridge.diagnostics" {
                    stage["known"] = Value::Bool(true);
                    stage["pack"] = json!("bridge-control");
                }
            }
        }
        plan.to_string()
    }

    fn bridge_diagnostics_response(
        &self,
        p: &RequestParams,
        parsed: &ParsedRequest,
    ) -> Result<String, McpError> {
        if !self.stdio_bridge {
            return Err(invalid_request_error(
                "bridge.diagnostics is available only on the stdio bridge".into(),
            ));
        }
        if parsed.mode != ExecutionMode::Single || parsed.ops.len() != 1 {
            return Err(invalid_request_error(
                "bridge.diagnostics must be the only operation in an unbatched request".into(),
            ));
        }
        if p.save_to.is_some() {
            return Err(invalid_request_error(
                "bridge.diagnostics does not accept save_to".into(),
            ));
        }
        let op = &parsed.ops[0];
        let is_help = op.args.len() == 1
            && matches!(
                op.args.get("help"),
                Some(ArgValue::Value(Value::Bool(true)))
            );
        if !op.args.is_empty() && !is_help {
            return Err(invalid_request_error(
                "bridge.diagnostics accepts only help=true".into(),
            ));
        }
        validate_request_overrides(p, 1)?;
        if khive_storage::request_read_is_cancelled() {
            return Err(McpError::internal_error(
                "request cancelled before bridge diagnostics read",
                Some(error_with_disposition(
                    json!({"kind":"cancelled", "message":"request cancelled before bridge diagnostics read"}),
                    DomainDisposition::NotCommitted,
                )),
            ));
        }

        let result = if is_help {
            bridge_diagnostics_help()
        } else {
            #[cfg(unix)]
            {
                bridge_diagnostics_value().ok_or_else(|| {
                    McpError::internal_error(
                        "bridge fallback total overflow",
                        Some(error_with_disposition(
                            json!({"kind":"counter_overflow", "message":"bridge fallback total overflow"}),
                            DomainDisposition::NotCommitted,
                        )),
                    )
                })?
            }
            #[cfg(not(unix))]
            {
                bridge_diagnostics_value()
            }
        };
        let presentation =
            parse_presentation_mode(p.presentation.as_deref()).map_err(invalid_request_error)?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            p.presentation_per_op.as_ref().map(|modes| {
                modes
                    .iter()
                    .map(|mode| {
                        mode.as_deref().map(|mode| {
                            parse_presentation_mode(Some(mode))
                                .expect("presentation override was validated")
                        })
                    })
                    .collect()
            });
        let effective_presentation = presentation_per_op
            .as_ref()
            .and_then(|modes| modes.first())
            .and_then(|mode| *mode)
            .unwrap_or(presentation);
        let result = present_with_policy(
            result,
            effective_presentation,
            chrono::Utc::now().timestamp(),
            VerbPresentationPolicy::Standard,
        );
        let batch_format = parse_output_format(p.format.as_deref())
            .map_err(invalid_request_error)?
            .unwrap_or(self.default_output_format);
        let format_per_op: Option<Vec<Option<OutputFormat>>> =
            p.format_per_op.as_ref().map(|formats| {
                formats
                    .iter()
                    .map(|format| {
                        parse_output_format(format.as_deref())
                            .expect("format override was validated")
                    })
                    .collect()
            });
        let effective_format = format_per_op
            .as_ref()
            .and_then(|formats| formats.first())
            .and_then(|format| *format)
            .unwrap_or(batch_format);
        let formatted = if effective_format == OutputFormat::Json {
            khive_runtime::presentation::prepare_format_value(
                result,
                effective_format,
                effective_presentation,
            )
        } else {
            Value::String(render_format(
                result,
                effective_format,
                effective_presentation,
            ))
        };
        Ok(serialize_response_value(&json!({
            "results": [{"ok": true, "tool": "bridge.diagnostics", "result": formatted}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
            "status": "success",
        })))
    }

    fn plan_response(&self, p: &RequestParams) -> Result<Option<String>, McpError> {
        if p.plan != Some(true) {
            return Ok(None);
        }
        p.validate_plan_envelope()?;
        Ok(Some(self.plan_ops(&p.ops)))
    }

    async fn request_with_cancellation(&self, p: RequestParams) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops);
        if let Ok(ref parsed) = parsed {
            if parsed_contains_bridge_diagnostics(parsed) {
                if p.plan == Some(true) && self.stdio_bridge {
                    p.validate_plan_envelope()?;
                    return Ok(self.plan_bridge_ops(&p.ops));
                }
                if p.plan != Some(true) {
                    return self.bridge_diagnostics_response(&p, parsed);
                }
            }
        }
        #[cfg(unix)]
        if let Some(executable) = &self.bridge_executable {
            executable
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .check()?;
        }
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        parsed.map_err(dsl_err_to_mcp)?;
        let mut p = p;
        let request_id = ensure_bridge_request_id(&mut p);
        tracing::debug!(
            request_id,
            "MCP request admitted with bridge correlation id"
        );
        #[cfg(unix)]
        return self.request_with_forward(p, forward_or_spawn_boxed).await;
        #[cfg(not(unix))]
        return self.request_with_forward(p).await;
    }

    /// Inner implementation of `request_with_cancellation`, parameterized
    /// over the daemon-forwarding seam so tests can drive the real dispatch
    /// path (registry resolution included) while asserting on what would
    /// have reached `forward_or_spawn_with_config_and_packs`, without spawning or
    /// contacting a real daemon.
    async fn request_with_forward(
        &self,
        p: RequestParams,
        #[cfg(unix)] forward_fn: ForwardFnPtr,
    ) -> Result<String, McpError> {
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        // Parse before the daemon decision. The daemon protocol's historical
        // error channel is string-only, so forwarding malformed DSL would turn
        // `invalid_params` plus its structured `parse-error` reason into an
        // untyped `internal_error` on the bridge back to MCP. This cheap,
        // side-effect-free preflight keeps the local and warm-daemon surfaces on
        // the same RPC contract; valid requests are still parsed authoritatively
        // inside `dispatch_request_inner` at the dispatch seam.
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
        if parsed_contains_bridge_diagnostics(&parsed) {
            return self.bridge_diagnostics_response(&p, &parsed);
        }
        validate_request_overrides(&p, parsed.ops.len())?;
        #[cfg(unix)]
        let replay_read_only = !parsed.ops.is_empty()
            && parsed
                .ops
                .iter()
                .all(|op| self.registry.is_read_replay_safe(&op.tool));
        #[cfg(not(unix))]
        let _ = parsed;

        // Forward to the warm daemon when reachable, auto-spawning it
        // on first use. An ordinary no-socket condition, a namespace
        // mismatch, or KHIVE_NO_DAEMON falls through to local dispatch.
        // A confirmed respawn failure (spawn error, or the child exits
        // before binding the socket) instead returns a caller-visible
        // `respawn_failed` error without local dispatch, per ADR-049
        // Amendment 2.
        //
        // MCP-AUD-002: the daemon wire frame has no `save_to` field, so
        // daemon-forwarded requests silently drop the sink and return the
        // inline result instead. Bypass daemon forwarding whenever `save_to`
        // is set so the local path's manifest/file behavior always applies,
        // matching the existing `kkernel exec --save-file` precedent.
        // A request cancelled before daemon admission must not start new
        // work — this must hold regardless of whether the request would go
        // through daemon forwarding or straight to local dispatch via the
        // `save_to` bypass below, so the check runs before either branch is
        // chosen. After admission, however, cancellation cannot safely drop
        // the forward: the daemon may already have committed one or more
        // operations, so the bridge must preserve its real response.
        #[cfg(unix)]
        if khive_storage::request_read_is_cancelled() {
            return Err(McpError::internal_error(
                "request cancelled before daemon dispatch",
                Some(error_with_disposition(
                    json!({"kind":"cancelled", "message":"request cancelled before daemon dispatch"}),
                    DomainDisposition::NotCommitted,
                )),
            ));
        }
        // In-memory state belongs to this runtime. Even a daemon with the same
        // configuration would own a separate store, so keep these requests local.
        #[cfg(unix)]
        let in_memory = self
            .runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.backend().is_file_backed());
        #[cfg(unix)]
        if p.save_to.is_none() && !in_memory {
            let frame = self.wire_daemon_frame(&p);
            let request_id = frame.request_id;
            // Forward this server's own resolved pack list so a daemon this
            // call spawns serves the SAME packs this process registered —
            // `pack_names()` reflects the actual loaded registry regardless
            // of whether that selection came from `--pack`, `KHIVE_PACKS`,
            // or a discovered `[runtime].packs` config entry, none of which
            // otherwise reach a freshly spawned child (khive-oss#1941).
            let resolved_packs: Vec<String> = self
                .registry
                .pack_names()
                .into_iter()
                .map(str::to_string)
                .collect();
            // Captured at the moment the forward is admitted so the
            // post-cancellation wait below can bound itself by the time
            // actually left on the REQUEST's own deadline, not by a fresh
            // full ceiling starting from whenever cancellation happens to
            // arrive. Also propagated into the spawned task itself
            // (`inherit_request_read_context`) so the forwarding path's
            // socket-exchange deadline is this exact same instant, instead
            // of a second, independently-ticking relative timer.
            let post_cancellation_deadline = khive_storage::capture_request_read_context()
                .deadline()
                .map(khive_storage::RequestReadDeadline::async_at);
            let mut forward_task =
                tokio::spawn(khive_storage::inherit_request_read_context(async move {
                    let outcome = forward_fn(frame, Some(resolved_packs), replay_read_only).await;
                    tracing::debug!(
                        request_id,
                        daemon_outcome_present = outcome.is_some(),
                        "admitted daemon forward reached a terminal outcome"
                    );
                    outcome
                }));
            let mut cancelled_during_forward = false;
            let forwarded = tokio::select! {
                result = &mut forward_task => result,
                _ = khive_storage::wait_for_request_read_cancellation() => {
                    cancelled_during_forward = true;
                    tracing::warn!(
                        request_id,
                        "request cancellation arrived after daemon admission; shielding the \
                         forward until its per-operation outcome is known"
                    );
                    // Shielding the forward must still be bounded: a stalled
                    // daemon or a silent same-UID socket peer must not keep
                    // this handler (and the task awaiting it) alive forever.
                    // Wait only for whatever time remains on the request's
                    // own absolute deadline (captured above, before this
                    // select ever ran) rather than starting a fresh
                    // `request_read_timeout()` ceiling from the moment
                    // cancellation happens to arrive — a request cancelled a
                    // moment before its own deadline must not then hold this
                    // handler for almost another full ceiling. When no
                    // deadline was installed (a direct `request_with_forward`
                    // call outside `scope_mcp_request_read_cancellation`, as
                    // some tests do), fall back to `request_read_timeout()`.
                    // If the deadline had already passed by the time
                    // cancellation arrived, `timeout_at` elapses on its very
                    // next poll — an immediate unknown-outcome return, which
                    // is the only consistent reading: there is no time left
                    // to shield the forward with.
                    let wait_result = match post_cancellation_deadline {
                        Some(deadline) => {
                            tokio::time::timeout_at(deadline, &mut forward_task).await
                        }
                        None => {
                            tokio::time::timeout(request_read_timeout(), &mut forward_task).await
                        }
                    };
                    match wait_result
                    {
                        Ok(result) => result,
                        Err(_elapsed) => {
                            // Never abort the task: the daemon may already
                            // have committed. Just stop waiting on it here —
                            // dropping the `JoinHandle` detaches without
                            // cancelling, so the forward keeps running to
                            // completion on its own.
                            tracing::warn!(
                                request_id,
                                "admitted daemon forward did not resolve within the \
                                 post-cancellation wait bound; reporting an unknown \
                                 outcome and leaving the forward to finish on its own"
                            );
                            return Err(cancelled_forward_error(request_id));
                        }
                    }
                }
            };
            let forwarded = forwarded.map_err(|error| {
                McpError::internal_error(
                    format!(
                        "daemon forwarding task failed after admission ({error}); outcome is \
                         unknown and the request must not be retried blindly"
                    ),
                    Some(json!({
                        "outcome": "unknown",
                        "kind": "transport",
                        "message": format!("daemon forwarding task failed after admission ({error}); outcome is unknown and the request must not be retried blindly"),
                                    "domain_disposition": "unknown",
                        "retryable": false,
                        "request_id": request_id,
                    })),
                )
            })?;
            if let Some(res) = forwarded {
                return match res {
                    Ok(s) => Ok(s),
                    // #947/#898: a strict-mode pre-dispatch rejection is
                    // tagged with
                    // `daemon::STRICT_FALLBACK_MARKER` so it can be reshaped
                    // into the normal per-op envelope instead of surfacing as
                    // an RPC-level error. Every other daemon-forward error
                    // (non-strict respawn failure, protocol mismatch,
                    // oversized frame, ambiguous post-write outcome) is
                    // untagged and passes through unchanged.
                    Err(e) => match strict_fallback_reason(&e) {
                        Some(reason) => strict_fallback_envelope_response(&p, reason),
                        None => Err(e),
                    },
                };
            }
            // The forward produced no daemon-side outcome (e.g. no socket,
            // `KHIVE_NO_DAEMON`). `tokio::select!` is unbiased, so the
            // task-result arm can win with `None` in the same instant
            // cancellation becomes visible — `cancelled_during_forward` alone
            // would miss that race. Re-check the flag directly so local
            // dispatch never runs after an admitted cancellation regardless
            // of which arm happened to win.
            if cancelled_during_forward || khive_storage::request_read_is_cancelled() {
                return Err(McpError::internal_error(
                    "request cancelled before local fallback dispatch",
                    Some(json!({
                        "outcome": "not_dispatched",
                        "kind": "cancelled",
                        "message": "request cancelled before local fallback dispatch",
                        "domain_disposition": "not_committed",
                        "retryable": true,
                        "request_id": request_id,
                    })),
                ));
            }
        }
        self.dispatch_request_wire(p).await
    }
}

#[cfg(unix)]
fn bridge_diagnostics_value() -> Option<Value> {
    crate::daemon::bridge_diagnostics_snapshot()
        .map(|snapshot| serde_json::to_value(snapshot).expect("bridge snapshot is serializable"))
}

#[cfg(not(unix))]
fn bridge_diagnostics_value() -> Value {
    json!({
        "bridge_instance_id": bridge_instance_id(),
        "pid": std::process::id(),
        "fallback_reasons": {
            "config_mismatch": 0,
            "namespace_mismatch": 0,
            "no_socket": 0,
            "parse_failure": 0,
            "protocol_mismatch": 0,
        },
        "fallback_total": 0,
        "strict_violations": 0,
    })
}

fn parsed_contains_bridge_diagnostics(parsed: &ParsedRequest) -> bool {
    parsed.ops.iter().any(|op| op.tool == "bridge.diagnostics")
}

fn bridge_diagnostics_help() -> Value {
    let counter = json!({"type":"integer", "minimum":0});
    json!({
        "verb": "bridge.diagnostics",
        "pack": "bridge-control",
        "description": "Read this stdio bridge process image's fallback counters without daemon or store access.",
        "category": "Read",
        "params": [{
            "name": "help",
            "type": "boolean",
            "required": false,
            "description": "Set to true to return this local schema. No other operation argument is accepted."
        }],
        "input_schema": {
            "type": "object",
            "properties": {"help": {"const": true}},
            "additionalProperties": false
        },
        "result_schema": {
            "type": "object",
            "required": ["bridge_instance_id", "pid", "fallback_reasons", "fallback_total", "strict_violations"],
            "properties": {
                "bridge_instance_id": {"type":"string", "format":"uuid", "description":"Full 36-character generation UUID; changes after an in-place re-exec."},
                "pid": {"type":"integer", "minimum":0},
                "fallback_reasons": {
                    "type":"object",
                    "required": ["config_mismatch", "namespace_mismatch", "no_socket", "parse_failure", "protocol_mismatch"],
                    "properties": {
                        "config_mismatch": counter,
                        "namespace_mismatch": counter,
                        "no_socket": counter,
                        "parse_failure": counter,
                        "protocol_mismatch": counter
                    },
                    "additionalProperties": false
                },
                "fallback_total": counter,
                "strict_violations": counter
            },
            "additionalProperties": false
        },
        "identifier_resolution": khive_runtime::pack::identifier_resolution_help(),
    })
}

/// Response-envelope `status` for a batch of `failed`/`aborted` counts
/// (#1220): `"partial"` when either is non-zero, `"success"` otherwise. A
/// caller that only checks for the absence of a top-level RPC error has
/// nothing else to branch on for a batch where some ops failed or were
/// skipped after a chain abort.
fn batch_status(failed: usize, aborted: usize) -> &'static str {
    if failed == 0 && aborted == 0 {
        "success"
    } else {
        "partial"
    }
}

/// Attach the CLI aggregate `strict-op-failure` reason before any save sink
/// serializes and hashes the canonical result rows.
///
/// This function deliberately does not emit stderr: the operator boundary in
/// `kkernel` owns emission. A dispatch-owned specific reason always wins, and
/// an unfamiliar future sibling reason is preserved rather than overwritten.
fn attach_strict_refusal_reasons(result: &mut Value) {
    let failed = result["summary"]["failed"].as_u64().unwrap_or(0);
    let aborted = result["summary"]["aborted"].as_u64().unwrap_or(0);
    if failed == 0 && aborted == 0 {
        return;
    }

    let Some(entries) = result["results"].as_array_mut() else {
        return;
    };
    for entry in entries {
        if entry["ok"].as_bool() == Some(true) || entry.get("reason").is_some() {
            continue;
        }
        if let Some(object) = entry.as_object_mut() {
            object.insert(
                "reason".to_string(),
                json!(RefusalReason::StrictOpFailure.as_str()),
            );
        }
    }
}

fn batch_budget_error(tool: &str, response_budget: usize) -> Value {
    failure_entry(
        tool,
        json!({"kind":"response_budget_exceeded", "code":"response_budget_exceeded",
        "message":format!("batch response budget of {response_budget} serialized bytes exceeded")}),
        DomainDisposition::NotCommitted,
    )
}

async fn execute_bounded_batch<I, F>(
    tasks: I,
    response_budget: usize,
    max_concurrency: usize,
) -> Vec<Value>
where
    I: IntoIterator<Item = BatchTask<F>>,
    F: Future<Output = Value>,
{
    assert!(max_concurrency > 0, "batch concurrency must be nonzero");
    let mut queued: std::collections::VecDeque<_> = tasks.into_iter().collect();
    let total = queued.len();
    let mut in_flight = FuturesUnordered::new();
    let start = |task: BatchTask<F>| async move {
        let entry = task.future.await;
        (task.index, task.tool, entry)
    };
    for _ in 0..max_concurrency {
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    let mut results: Vec<Option<Value>> = (0..total).map(|_| None).collect();
    let mut accumulated_bytes = 0usize;
    let mut budget_breached = false;

    while let Some((index, _tool, entry)) = in_flight.next().await {
        if budget_breached {
            results[index] = Some(entry);
            continue;
        }
        let serialized_bytes = serde_json::to_vec(&entry)
            .expect("serde_json::Value is always serializable")
            .len();
        if serialized_bytes > response_budget.saturating_sub(accumulated_bytes) {
            results[index] = Some(entry);
            budget_breached = true;
            continue;
        }

        accumulated_bytes += serialized_bytes;
        results[index] = Some(entry);
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    for task in queued {
        results[task.index] = Some(batch_budget_error(&task.tool, response_budget));
    }

    results
        .into_iter()
        .map(|entry| entry.expect("every started or queued batch task has a result"))
        .collect()
}

/// Bounded-concurrency driver for one bracketed batch of chains (ADR-016
/// Amendment 2): at most `max_concurrency` units are admitted and polled at
/// once, refilled only as units complete. Every admitted unit dispatches its
/// own leaves sequentially (never more than one leaf in flight per unit), so
/// this bounds total in-flight leaves at `max_concurrency` too. Unlike
/// [`execute_bounded_batch`], response-budget accounting lives inside each
/// unit's own leaf loop (via the shared [`UnitBudget`]) rather than in this
/// driver: a unit admitted after the budget is already exhausted finds that
/// out at the top of its own first leaf, before it dispatches anything.
async fn execute_bounded_units<I, F>(tasks: I, max_concurrency: usize) -> Vec<UnitOutcome>
where
    I: IntoIterator<Item = UnitTask<F>>,
    F: Future<Output = UnitOutcome>,
{
    assert!(max_concurrency > 0, "unit concurrency must be nonzero");
    let mut queued: std::collections::VecDeque<_> = tasks.into_iter().collect();
    let mut in_flight = FuturesUnordered::new();
    let start = |task: UnitTask<F>| task.future;
    for _ in 0..max_concurrency {
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    let mut outcomes = Vec::new();
    while let Some(outcome) = in_flight.next().await {
        outcomes.push(outcome);
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }
    outcomes
}

fn parallel_batch_envelope(results: Vec<Value>) -> Value {
    let total = results.len();
    let succeeded = results
        .iter()
        .filter(|result| result.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let failed = total - succeeded;
    json!({
        "results": results,
        "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": 0 },
        "status": batch_status(failed, 0),
    })
}

/// Extract the fallback-reason string from a strict-mode rejection's
/// [`McpError`] (#947), or `None` if `e` is not tagged with
/// [`crate::daemon::STRICT_FALLBACK_MARKER`] — i.e. some other daemon-forward
/// error that must stay an RPC-level error.
#[cfg(unix)]
fn strict_fallback_reason(e: &McpError) -> Option<String> {
    let data = e.data.as_ref()?;
    if data.get(crate::daemon::STRICT_FALLBACK_MARKER)?.as_bool() != Some(true) {
        return None;
    }
    data.get("reason")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Build the wire-contract failed-op envelope for a strict-mode daemon
/// fallback rejection (#947 Medium finding).
///
/// The request was never attempted — locally or on the daemon — but the wire
/// response must still be a normal per-op envelope
/// (`{"results": [...], "summary": {...}}`) reporting the fallback reason as
/// each op's `error`, not an RPC-level `McpError`. Chain mode aborts after the
/// first op, matching `run_parsed`'s `Chain` arm and the wire contract's
/// documented abort-on-failure behavior for `|`-chained ops.
#[cfg(unix)]
fn strict_fallback_envelope_response(
    p: &RequestParams,
    reason: String,
) -> Result<String, McpError> {
    let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
    let total = parsed.ops.len();
    let error_msg = format!(
        "daemon fallback rejected under KHIVE_DAEMON_STRICT=1: reason={reason}; \
         refusing to complete the request via local dispatch; \
         rebuild with `make local` and retry"
    );

    let results: Vec<Value> = match parsed.mode {
        ExecutionMode::Chain => parsed
            .ops
            .iter()
            .enumerate()
            .map(|(i, op)| {
                if i == 0 {
                    failure_entry(
                        op.tool.clone(),
                        json!(error_msg),
                        DomainDisposition::NotCommitted,
                    )
                } else {
                    aborted_entry(op.tool.clone(), None)
                }
            })
            .collect(),
        ExecutionMode::Single | ExecutionMode::Parallel => parsed
            .ops
            .iter()
            .map(|op| {
                failure_entry(
                    op.tool.clone(),
                    json!(error_msg),
                    DomainDisposition::NotCommitted,
                )
            })
            .collect(),
    };

    let aborted = if parsed.mode == ExecutionMode::Chain {
        total.saturating_sub(1)
    } else {
        0
    };
    let failed = total - aborted;
    Ok(serde_json::to_string(&json!({
        "results": results,
        "summary": { "total": total, "succeeded": 0, "failed": failed, "aborted": aborted },
        "status": batch_status(failed, aborted),
    }))
    .expect("envelope of string/bool JSON values always serializes"))
}

impl KhiveMcpServer {
    /// Build the daemon forward-frame for an agent-facing `request` tool call.
    ///
    /// `from_wire` is unconditionally `true`: this is the agent wire surface, so
    /// `Visibility::Subhandler` verbs must be rejected whether the request runs
    /// on the warm daemon or via the local fallback. Keeping the bit in one
    /// named, unit-tested place stops the daemon-forward path from silently
    /// diverging from `dispatch_request_wire`.
    #[cfg(unix)]
    pub(crate) fn wire_daemon_frame(&self, p: &RequestParams) -> khive_runtime::DaemonRequestFrame {
        khive_runtime::DaemonRequestFrame {
            plan: false,
            ops: p.ops.clone(),
            presentation: p.presentation.clone(),
            presentation_per_op: p.presentation_per_op.clone(),
            namespace: self.default_namespace.clone(),
            // ADR-096 Fork 1: carry this server's OWN resolved actor/visibility
            // identity on the frame so a warm daemon with a *different* baked
            // identity serves the request under this caller's identity instead
            // of rejecting it or silently stamping writes under its own actor.
            actor_id: self.actor_id().map(str::to_string),
            process_ref: khive_runtime::process_ref_from_env(),
            visible_namespaces: self
                .visible_namespaces()
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            config_id: self.config_id.clone(),
            protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: p.format.clone(),
            format_per_op: p.format_per_op.clone(),
            from_wire: true,
            // khive#948: forwarded unchanged from the tool caller's params.
            // `None` when the caller supplied no id (pre-#948 client).
            request_id: p.request_id,
        }
    }

    /// Parse and dispatch a request against this server's own registry.
    ///
    /// This is the canonical **operator** dispatch path: subhandler verbs are
    /// allowed. `kkernel exec`, in-process callers, and tests use this. The
    /// agent-facing MCP wire surface goes through `dispatch_request_wire`
    /// (or sets `from_wire` on the daemon frame), which enforces verb visibility.
    ///
    /// Pure local dispatch: no [`khive_runtime::RequestIdentity`] override is
    /// applied by this caller (ADR-096 Fork 1) — this server's own
    /// construction-baked namespace/actor/visibility is used, unchanged from
    /// before per-request identity existed. `dispatch_request_inner` (khive#948)
    /// may still synthesize an identity carrying those same baked scalars if
    /// `p.request_id` is set, purely so the audit row is correlatable.
    pub async fn dispatch_request_local(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, false, None, DispatchOrigin::Local)
            .await
    }

    /// Operator dispatch used by `kkernel exec`.
    ///
    /// When `strict_refusals` is true, otherwise-unclassified failed/aborted
    /// rows receive `strict-op-failure` before `save_to` writes and checksums
    /// JSONL. Stderr emission remains at the CLI boundary.
    pub async fn dispatch_request_local_for_exec(
        &self,
        p: RequestParams,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_request_inner_with_strict_refusals(
            p,
            false,
            None,
            DispatchOrigin::Local,
            strict_refusals,
        )
        .await
    }

    /// Dispatch a bounded, already-decoded JSON batch for `kkernel exec --ops-file`.
    ///
    /// The ops-file reader owns its 96 MiB line, 512 MiB file, 32 MiB chunk,
    /// and 100-op limits. This seam preserves JSON-form validation but avoids
    /// serializing those typed values back into the public raw-DSL parser,
    /// whose independent 1 MiB limit remains unchanged for MCP, HTTP, daemon,
    /// inline exec, and every other string request surface.
    pub async fn dispatch_typed_json_batch_local_for_exec(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_typed_json_batch_local_for_exec_with_policy(
            ops,
            presentation,
            format,
            ParsedDispatchPolicy::bounded_parallel(strict_refusals),
        )
        .await
    }

    /// Dispatch one full typed ops-file chunk with exactly one handler in
    /// flight while retaining ordinary parallel-batch semantics.
    ///
    /// Parsing, write-key conflict detection, aggregate response budgeting,
    /// result ordering, presentation, audit, and strict-refusal handling remain
    /// shared with [`Self::dispatch_typed_json_batch_local_for_exec`]. Only the
    /// trusted local scheduler's concurrency cap changes.
    pub async fn dispatch_typed_json_batch_serial_local_for_exec(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        self.dispatch_typed_json_batch_local_for_exec_with_policy(
            ops,
            presentation,
            format,
            ParsedDispatchPolicy::serial(strict_refusals),
        )
        .await
    }

    async fn dispatch_typed_json_batch_local_for_exec_with_policy(
        &self,
        ops: Vec<TypedJsonOp>,
        presentation: Option<String>,
        format: Option<String>,
        policy: ParsedDispatchPolicy,
    ) -> Result<String, McpError> {
        debug_assert!(policy.max_batch_concurrency > 0);
        let parsed = parse_typed_json_batch(ops).map_err(dsl_err_to_mcp)?;
        let p = RequestParams {
            plan: None,
            ops: String::new(),
            presentation,
            presentation_per_op: None,
            save_to: None,
            format,
            format_per_op: None,
            request_id: None,
        };
        let timeout = crate::request_policy::parsed_read_timeout(&parsed, request_read_timeout());
        let dispatch = Box::pin(self.dispatch_parsed_request_inner_scoped(
            p,
            parsed,
            false,
            None,
            DispatchOrigin::Local,
            policy,
        ));
        khive_storage::scope_request_read_deadline(timeout, dispatch).await
    }

    /// Replay one stored public-surface request under a host-verified actor.
    ///
    /// An attributed actor must come from an out-of-band provenance check,
    /// never from a field inside the stored request. `None` is reserved for a
    /// provenance-verified anonymous/local creator, preserving that actor kind.
    /// Replay deliberately sets `from_wire=true`: scheduling delays a public
    /// request; it does not upgrade that request into the operator-only local
    /// surface where [`khive_runtime::Visibility::Subhandler`] verbs are callable.
    pub(crate) async fn dispatch_request_replay_as(
        &self,
        p: RequestParams,
        namespace: &str,
        verified_actor: Option<khive_runtime::VerifiedActor>,
    ) -> Result<String, McpError> {
        let identity = khive_runtime::RequestIdentity {
            namespace: namespace.to_string(),
            // `None` is the provenance-verified anonymous/local identity;
            // spelling that identity as `Some("local")` would incorrectly
            // reconstruct it as the distinct authenticated `actor:local`.
            actor_id: verified_actor.map(|actor| actor.as_str().to_string()),
            process_ref: khive_runtime::process_ref_from_env(),
            // A scheduled action is scoped exactly to its event namespace;
            // it never inherits the daemon's broader read visibility.
            visible_namespaces: Vec::new(),
            request_id: None,
        };
        self.dispatch_request_inner(p, true, Some(identity), DispatchOrigin::Local)
            .await
    }

    /// Wire-surface dispatch: same as [`Self::dispatch_request_local`] but
    /// enforces verb visibility (`Visibility::Subhandler` verbs are rejected).
    /// Used by the stdio `request` tool's local-fallback path.
    pub(crate) async fn dispatch_request_wire(&self, p: RequestParams) -> Result<String, McpError> {
        self.dispatch_request_inner(p, true, None, DispatchOrigin::Local)
            .await
    }

    /// Shared body for both dispatch surfaces. `from_wire` decides whether the
    /// subhandler-visibility gate fires (see [`run_parsed`](Self::run_parsed)).
    ///
    /// `identity` is the per-request identity context threaded from a daemon
    /// frame (ADR-096 Fork 1, see `crate::daemon`'s `DaemonDispatch` impl).
    /// `None` for every local (non-daemon-served) call — this server's own
    /// baked identity applies, exactly as before this parameter existed.
    /// `origin` independently controls daemon-frame response fitting; wire
    /// visibility does not imply that the response travels through the daemon.
    ///
    /// khive#948: when `identity` is `None` (every local-dispatch call —
    /// `KHIVE_NO_DAEMON`/soft daemon-fallback and the `save_to` bypass both
    /// route here via `dispatch_request_wire`) and the caller supplied a
    /// `request_id`, a `RequestIdentity` is synthesized so the audit row
    /// stamped by this dispatch is still correlatable. The synthesized
    /// identity mirrors this server's own baked `default_namespace` /
    /// `actor_id` / `visible_namespaces` exactly — it changes no dispatch
    /// semantics, only adds the correlation id — so a request with no
    /// `request_id` still dispatches through the untouched `identity = None`
    /// path.
    pub(crate) async fn dispatch_request_inner(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
    ) -> Result<String, McpError> {
        self.dispatch_request_inner_with_strict_refusals(p, from_wire, identity, origin, false)
            .await
    }

    async fn dispatch_request_inner_with_strict_refusals(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        if let Some(plan) = self.plan_response(&p)? {
            return Ok(plan);
        }
        // `dispatch_request_inner_scoped` is the complete parse/dispatch/render
        // pipeline. Keep that large generator behind one pointer before handing
        // it to the generic task-local scope: otherwise the scope embeds the
        // pipeline in every MCP, local-exec, and replay request future. LLVM
        // coverage instrumentation amplifies the resulting poll stack enough to
        // overflow Tokio's normal worker stack even for unrelated small verbs.
        let timeout = crate::request_policy::read_timeout(&p.ops, request_read_timeout());
        let dispatch = Box::pin(self.dispatch_request_inner_scoped(
            p,
            from_wire,
            identity,
            origin,
            strict_refusals,
        ));
        khive_storage::scope_request_read_deadline(timeout, dispatch).await
    }

    async fn dispatch_request_inner_scoped(
        &self,
        p: RequestParams,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        strict_refusals: bool,
    ) -> Result<String, McpError> {
        let parsed = parse_request(&p.ops).map_err(dsl_err_to_mcp)?;
        self.dispatch_parsed_request_inner_scoped(
            p,
            parsed,
            from_wire,
            identity,
            origin,
            ParsedDispatchPolicy::bounded_parallel(strict_refusals),
        )
        .await
    }

    async fn dispatch_parsed_request_inner_scoped(
        &self,
        p: RequestParams,
        parsed: ParsedRequest,
        from_wire: bool,
        identity: Option<khive_runtime::RequestIdentity>,
        origin: DispatchOrigin,
        policy: ParsedDispatchPolicy,
    ) -> Result<String, McpError> {
        if parsed_contains_bridge_diagnostics(&parsed) {
            return Err(invalid_request_error(
                "bridge.diagnostics is available only on the stdio bridge".into(),
            ));
        }
        validate_request_overrides(&p, parsed.ops.len())?;
        let ParsedDispatchPolicy {
            strict_refusals,
            max_batch_concurrency,
        } = policy;
        let save_to = p.save_to.clone();
        let identity = identity.or_else(|| {
            p.request_id
                .map(|request_id| khive_runtime::RequestIdentity {
                    namespace: self.default_namespace.clone(),
                    actor_id: self.actor_id().map(str::to_string),
                    process_ref: khive_runtime::process_ref_from_env(),
                    visible_namespaces: self
                        .visible_namespaces()
                        .iter()
                        .map(|ns| ns.as_str().to_string())
                        .collect(),
                    request_id: Some(request_id),
                })
        });

        // Parse presentation strings → PresentationMode.
        let presentation =
            parse_presentation_mode(p.presentation.as_deref()).map_err(invalid_request_error)?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            if let Some(per_op_strs) = p.presentation_per_op {
                let mut modes = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let mode = match s.as_deref() {
                        None => None,
                        Some(v) => {
                            Some(parse_presentation_mode(Some(v)).map_err(invalid_request_error)?)
                        }
                    };
                    modes.push(mode);
                }
                Some(modes)
            } else {
                None
            };

        // Resolve the output format for this request (ADR-078 §2 precedence):
        // per-request `format` field → server default (already resolved from
        // env + toml + builtin by `serve.rs`).
        let batch_format = parse_output_format(p.format.as_deref())
            .map_err(invalid_request_error)?
            .unwrap_or(self.default_output_format);

        // Per-op format overrides (ADR-078 §8.4).
        let format_per_op: Option<Vec<Option<OutputFormat>>> =
            if let Some(per_op_strs) = p.format_per_op {
                let mut fmts = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let fmt = match s.as_deref() {
                        None => None,
                        Some(v) => Some(
                            parse_output_format(Some(v))
                                .map_err(invalid_request_error)?
                                .unwrap_or(batch_format),
                        ),
                    };
                    fmts.push(fmt);
                }
                Some(fmts)
            } else {
                None
            };

        // Reserve and validate the destination before any operation can run.
        // Wire requests are restricted to the export root; the trusted CLI
        // keeps its documented unrestricted destination policy.
        let save_sink = save_to
            .as_deref()
            .map(|path| {
                crate::save_sink::JsonlSaveSink::new(std::path::Path::new(path), from_wire)
                    .map_err(|error| invalid_request_error(format!("save_to: {error}")))
            })
            .transpose()?;

        let (mut result, content_scopes) = self
            .run_parsed(
                parsed.ops,
                parsed.mode,
                parsed.ranges,
                presentation,
                presentation_per_op.clone(),
                RunParsedContext {
                    enforce_response_budget: save_to.is_none(),
                    max_batch_concurrency,
                    from_wire,
                    identity: identity.as_ref(),
                },
            )
            .await;

        attach_audit_persistence_advisories(&mut result, &self.registry);

        if strict_refusals {
            attach_strict_refusal_reasons(&mut result);
        }

        if let Some(sink) = save_sink {
            let manifest = sink
                .write_envelope(&result)
                .map_err(|error| save_to_write_error(format!("save_to: {error}"), &result))?;
            // Manifests are always compact JSON regardless of format (lossless metadata).
            return serde_json::to_string(&manifest)
                .map_err(|e| request_internal_error(format!("serialize manifest: {e}")));
        }

        if origin == DispatchOrigin::Daemon {
            mark_daemon_lexical_timeout(&mut result);
        }

        // Apply per-op format rendering (ADR-078 §8.4 and §9).
        Ok(render_result(
            result,
            batch_format,
            &format_per_op,
            presentation,
            &presentation_per_op,
            &RenderContext {
                registry: &self.registry,
                content_scopes: &content_scopes,
            },
            (origin == DispatchOrigin::Daemon).then_some(self.config_id.as_str()),
        ))
    }
}

/// Attach a registry-level audit advisory to successful operation entries
/// without changing their canonical `result` values.
///
/// Help introspection is excluded because it short-circuits before the
/// gate/audit lifecycle and therefore would not append an audit row.
fn attach_audit_persistence_advisories(response: &mut Value, registry: &VerbRegistry) {
    let Some(advisory) = registry.audit_persistence_advisory() else {
        return;
    };
    let Some(results) = response.get_mut("results").and_then(Value::as_array_mut) else {
        return;
    };

    for entry in results {
        if entry.get("ok").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let tool = entry.get("tool").and_then(Value::as_str);
        let is_help = entry.get("result").is_some_and(|result| {
            let identifiers = result.get("identifier_resolution");
            result.get("verb").and_then(Value::as_str) == tool
                && result.get("pack").is_some_and(Value::is_string)
                && result.get("description").is_some_and(Value::is_string)
                && result.get("category").is_some_and(Value::is_string)
                && identifiers
                    .and_then(|value| value.get("full_uuid"))
                    .is_some_and(Value::is_string)
                && identifiers
                    .and_then(|value| value.get("short_prefix"))
                    .is_some_and(Value::is_string)
                && identifiers
                    .and_then(|value| value.get("parameter_rule"))
                    .is_some_and(Value::is_string)
        });
        if is_help {
            continue;
        }

        if let Some(map) = entry.as_object_mut() {
            if let Some(existing) = map.get_mut("advisories") {
                if let Some(advisories) = existing.as_array_mut() {
                    let code = advisory.get("code");
                    if !advisories.iter().any(|item| item.get("code") == code) {
                        advisories.push(advisory.clone());
                    }
                }
            } else {
                map.insert(
                    "advisories".to_string(),
                    Value::Array(vec![advisory.clone()]),
                );
            }
        }
    }
}

fn request_error(
    message: String,
    code: rmcp::model::ErrorCode,
    kind: &str,
    disposition: DomainDisposition,
) -> McpError {
    McpError::new(
        code,
        message.clone(),
        Some(error_with_disposition(
            json!({"kind": kind, "message": message}),
            disposition,
        )),
    )
}

fn invalid_request_error(message: String) -> McpError {
    request_error(
        message,
        rmcp::model::ErrorCode::INVALID_PARAMS,
        "invalid_input",
        DomainDisposition::NotCommitted,
    )
}

#[cfg(unix)]
fn cancelled_forward_error(request_id: Option<u64>) -> McpError {
    McpError::internal_error(
        "daemon forward outcome unknown after cancellation",
        Some(error_with_disposition(
            json!({
                "outcome": "unknown",
                "kind": "transport",
                "message": "daemon forward outcome unknown after cancellation",
                "retryable": false,
                "request_id": request_id,
            }),
            DomainDisposition::Unknown,
        )),
    )
}

fn request_internal_error(message: String) -> McpError {
    // Rendering/saving can follow a mixture of per-op outcomes.
    request_error(
        message,
        rmcp::model::ErrorCode::INTERNAL_ERROR,
        "internal",
        DomainDisposition::Unknown,
    )
}

/// A sink can still fail after domain dispatch. Preserve the known per-op
/// receipts while bounding error details for results too large to return inline.
fn save_to_write_error(message: String, result: &Value) -> McpError {
    const MAX_INLINE_OUTCOME_BYTES: usize = 8 * 1024;

    let outcomes = result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, row)| {
            let ok = row.get("ok").and_then(Value::as_bool);
            let disposition = if ok == Some(true) {
                DomainDisposition::Committed.as_str()
            } else {
                row.get("domain_disposition")
                    .and_then(Value::as_str)
                    .unwrap_or(DomainDisposition::Unknown.as_str())
            };
            let mut outcome = json!({
                "op_index": row.get("op_index").and_then(Value::as_u64).unwrap_or(index as u64),
                "tool": row.get("tool").and_then(Value::as_str).unwrap_or("?"),
                "ok": ok,
                "domain_disposition": disposition,
            });
            for field in ["result", "error"] {
                if let Some(value) = row.get(field) {
                    if serialized_response_len(value) <= MAX_INLINE_OUTCOME_BYTES {
                        outcome[field] = value.clone();
                    } else {
                        outcome
                            .as_object_mut()
                            .expect("outcome is an object")
                            .insert(format!("{field}_omitted"), Value::Bool(true));
                    }
                }
            }
            for field in ["reason", "aborted"] {
                if let Some(value) = row.get(field) {
                    outcome[field] = value.clone();
                }
            }
            outcome
        })
        .collect::<Vec<_>>();
    let mut detail = json!({
        "kind": "internal",
        "message": message.clone(),
        "summary": result.get("summary"),
        "results": outcomes,
    });
    if let Some(atomic) = result.get("atomic") {
        detail["atomic"] = atomic.clone();
    }
    McpError::internal_error(
        message,
        Some(error_with_disposition(detail, DomainDisposition::Unknown)),
    )
}

fn dsl_err_to_mcp(e: DslError) -> McpError {
    McpError::invalid_params(
        e.to_string(),
        Some(error_with_disposition(
            json!({ "reason": RefusalReason::ParseError.as_str(),
            "kind":"parse_error", "message":e.to_string() }),
            DomainDisposition::NotCommitted,
        )),
    )
}

/// Parse an optional presentation mode string from the request envelope.
///
/// `None` → default (`Agent`). Known values: `"agent"`, `"verbose"`, `"human"`.
fn parse_presentation_mode(s: Option<&str>) -> Result<PresentationMode, String> {
    match s {
        None | Some("agent") => Ok(PresentationMode::Agent),
        Some("verbose") => Ok(PresentationMode::Verbose),
        Some("human") => Ok(PresentationMode::Human),
        Some(other) => Err(format!(
            "unknown presentation mode {other:?}; valid values: \"agent\", \"verbose\", \"human\""
        )),
    }
}

/// Parse an optional output format string from the request envelope (ADR-078).
///
/// `None` → `None` (caller uses server default). Known values: `"json"`, `"auto"`, `"table"`.
fn parse_output_format(s: Option<&str>) -> Result<Option<OutputFormat>, String> {
    match s {
        None => Ok(None),
        Some("json") => Ok(Some(OutputFormat::Json)),
        Some("auto") => Ok(Some(OutputFormat::Auto)),
        Some("table") => Ok(Some(OutputFormat::Table)),
        Some(other) => Err(format!(
            "unknown output format {other:?}; valid values: \"json\", \"auto\", \"table\""
        )),
    }
}

/// Validate envelope presentation/format overrides before daemon framing and
/// before local dispatch. A surplus per-op array is never meaningful and can
/// otherwise make a valid request exceed the daemon's frame budget.
fn validate_request_overrides(p: &RequestParams, op_count: usize) -> Result<(), McpError> {
    parse_presentation_mode(p.presentation.as_deref()).map_err(invalid_request_error)?;
    parse_output_format(p.format.as_deref()).map_err(invalid_request_error)?;
    if let Some(entries) = &p.presentation_per_op {
        if entries.len() > op_count {
            return Err(invalid_request_error(format!(
                "presentation_per_op has {} entries for {op_count} operations",
                entries.len()
            )));
        }
        for entry in entries {
            if let Some(value) = entry.as_deref() {
                parse_presentation_mode(Some(value)).map_err(invalid_request_error)?;
            }
        }
    }
    if let Some(entries) = &p.format_per_op {
        if entries.len() > op_count {
            return Err(invalid_request_error(format!(
                "format_per_op has {} entries for {op_count} operations",
                entries.len()
            )));
        }
        for entry in entries {
            if let Some(value) = entry.as_deref() {
                parse_output_format(Some(value)).map_err(invalid_request_error)?;
            }
        }
    }
    Ok(())
}

/// Registered policies and per-operation content scopes used during rendering.
struct RenderContext<'a> {
    registry: &'a VerbRegistry,
    content_scopes: &'a [NoteContentScope],
}

/// Preserve a daemon-only signal before auto/table rendering turns a result
/// into display text. The daemon moves it into response-frame metadata.
fn mark_daemon_lexical_timeout(response: &mut Value) {
    let timed_out = response
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|entry| {
                entry.get("ok").and_then(Value::as_bool) == Some(true)
                    && matches!(
                        entry.get("tool").and_then(Value::as_str),
                        Some("knowledge.search" | "knowledge.suggest")
                    )
                    && entry
                        .get("result")
                        .and_then(|result| result.get("degraded"))
                        .and_then(|degraded| degraded.get("lexical_timeout"))
                        .and_then(Value::as_bool)
                        == Some(true)
            })
        });
    if timed_out {
        response[DAEMON_LEXICAL_TIMEOUT_MARKER] = Value::Bool(true);
    }
}

/// Render the `run_parsed` result envelope using per-op format dispatch (ADR-078 §8.4).
///
/// For each op entry in `results`:
/// - If `ok=false` (error entry): always compact JSON, never reformatted (§8.2).
/// - If `ok=true`: resolve per-op format (per_op_formats[i] → batch_format) and
///   per-op presentation (presentation_per_op[i] → batch presentation, then the
///   verb's AlwaysVerbose policy forces Verbose), apply the effective presentation
///   to the `result` payload so that both `presentation_per_op=["verbose"]` and
///   AlwaysVerbose verbs (including strict feedback, delivery-correlation
///   acknowledgements, and durable receipt responses) correctly skip the
///   redundancy-drop pre-pass (ADR-078 §7 + §8.4; mirrors `run_parsed`). JSON
///   output stays a JSON value — `prepare_format_value` applies the pre-pass
///   in place — so a compounded response never round-trips `result` through a
///   string; every other format calls `render_format` and stores the rendered
///   string instead (ADR-078 Amendment 3).
///
/// The outer envelope (`{results:[...], summary:{...}}`) is always compact JSON (§8.4).
/// Daemon-served responses are rendered before fitting. If the rendered envelope
/// exceeds the frame allowance, entries fall back to compact JSON before payload
/// details are omitted. Local dispatch has no daemon-frame allowance and returns
/// the requested representation without fitting. Every daemon fit decision is
/// computed from per-entry serialized lengths (`fit_rendered_batch_envelope`),
/// not by re-serializing the response-frame shape on each candidate; the
/// arithmetic accounts for JSON string escaping exactly (see
/// `json_escaped_len`), so the fit decision matches what serializing the
/// actual frame would have measured.
fn render_result(
    value: serde_json::Value,
    batch_format: OutputFormat,
    format_per_op: &Option<Vec<Option<OutputFormat>>>,
    presentation: PresentationMode,
    presentation_per_op: &Option<Vec<Option<PresentationMode>>>,
    context: &RenderContext<'_>,
    daemon_frame_config_id: Option<&str>,
) -> String {
    // Try to detect the compound batch envelope shape: { results: [...], summary: {...} }
    if let serde_json::Value::Object(ref map) = value {
        if let Some(serde_json::Value::Array(results)) = map.get("results") {
            let out_results = results
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    render_batch_entry(
                        index,
                        entry,
                        batch_format,
                        format_per_op,
                        presentation,
                        presentation_per_op,
                        context,
                    )
                })
                .collect();
            let out_map = match daemon_frame_config_id {
                Some(config_id) => fit_rendered_batch_envelope(
                    map,
                    results,
                    out_results,
                    config_id,
                    context.registry,
                ),
                None => {
                    let mut out_map = map.clone();
                    out_map.insert("results".to_string(), Value::Array(out_results));
                    out_map
                }
            };
            return serialize_response_value(&serde_json::Value::Object(out_map));
        }
    }

    let rendered = render_format(value.clone(), batch_format, presentation);
    let Some(config_id) = daemon_frame_config_id else {
        return rendered;
    };
    if rendered_response_fits_daemon_frame(&rendered, config_id) {
        return rendered;
    }
    let compact = serialize_response_value(&value);
    if rendered_response_fits_daemon_frame(&compact, config_id) {
        return compact;
    }
    serde_json::to_string(&failure_entry(
        "request",
        frame_budget_error(
            "response payload omitted because it exceeds the daemon frame budget",
            DomainDisposition::Unknown,
        ),
        DomainDisposition::Unknown,
    ))
    .expect("static frame-budget error is serializable")
}

fn render_batch_entry(
    index: usize,
    entry: &Value,
    batch_format: OutputFormat,
    format_per_op: &Option<Vec<Option<OutputFormat>>>,
    presentation: PresentationMode,
    presentation_per_op: &Option<Vec<Option<PresentationMode>>>,
    context: &RenderContext<'_>,
) -> Value {
    let per_op_format = format_per_op
        .as_ref()
        .and_then(|formats| formats.get(index))
        .and_then(|format| *format)
        .unwrap_or(batch_format);
    let is_ok = entry.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !is_ok {
        return entry.clone();
    }

    let base_presentation = presentation_per_op
        .as_ref()
        .and_then(|modes| modes.get(index))
        .and_then(|mode| *mode)
        .unwrap_or(presentation);
    let effective_presentation = match entry.get("tool").and_then(Value::as_str) {
        Some(tool)
            if context.registry.presentation_policy_for(tool)
                == VerbPresentationPolicy::AlwaysVerbose =>
        {
            PresentationMode::Verbose
        }
        _ => base_presentation,
    };
    if entry.get("result").is_none() {
        return entry.clone();
    }
    let mut rendered_entry = entry.clone();
    let Value::Object(ref mut fields) = rendered_entry else {
        return rendered_entry;
    };
    // `result` was already duplicated by `entry.clone()` above; take it back
    // out of the clone instead of cloning it a second time off `entry`. Both
    // `prepare_format_value` and `render_format` take the value by ownership,
    // so a no-op reduction (Verbose/Human JSON) no longer pays for a clone it
    // throws away.
    let Some(result) = fields.remove("result") else {
        return rendered_entry;
    };
    let content_scope = context
        .content_scopes
        .get(index)
        .copied()
        .unwrap_or_default();
    let formatted = if per_op_format == OutputFormat::Json {
        prepare_format_value_with_note_content(
            result,
            per_op_format,
            effective_presentation,
            content_scope,
        )
    } else {
        Value::String(render_format_with_note_content(
            result,
            per_op_format,
            effective_presentation,
            content_scope,
        ))
    };
    fields.insert("result".to_string(), formatted);
    rendered_entry
}

/// Fit a rendered batch envelope inside the daemon frame budget.
///
/// The envelope's compact-JSON length is additive: for a fixed set of
/// non-`results` keys, `serialize({..metadata, results: X})` only ever
/// changes in the byte range spanned by `results`'s own array literal, so
/// its length is exactly `envelope_metadata_escaped_len(metadata) +
/// sum(entry_escaped_len) + separators` (`envelope_escaped_len` below), and
/// the daemon-frame length is that same number plus the frame's own fixed
/// overhead (`empty_rendered_daemon_frame_len`). That turns "does this
/// candidate fit" from a full-batch clone-and-reserialize into arithmetic
/// over lengths computed once per entry, so fitting no longer costs
/// O(entries × batch bytes).
fn fit_rendered_batch_envelope(
    map: &serde_json::Map<String, Value>,
    compact_results: &[Value],
    out_results: Vec<Value>,
    served_config_id: &str,
    registry: &VerbRegistry,
) -> serde_json::Map<String, Value> {
    let frame_base = empty_rendered_daemon_frame_len(served_config_id);
    let mut metadata = envelope_metadata(map);
    let mut metadata_len = envelope_metadata_escaped_len(&metadata);
    let mut out_results = out_results;
    let mut entry_lens: Vec<usize> = out_results.iter().map(entry_escaped_len).collect();
    let fits = |entry_lens: &[usize], metadata_len: usize| {
        frame_base + envelope_escaped_len(entry_lens, metadata_len)
            <= khive_runtime::daemon::MAX_FRAME_BYTES
    };

    if fits(&entry_lens, metadata_len) {
        metadata.insert("results".to_string(), Value::Array(out_results));
        return metadata;
    }

    // Pass 1: canonical (compact) fallback wherever it actually saves bytes,
    // largest saving first. Agent JSON reduction can make a canonical form
    // LARGER than its rendered form (redundant fields already dropped), so a
    // fallback there would move the envelope the wrong way and is skipped.
    let mut compact_fallbacks: Vec<(usize, usize, usize)> = compact_results
        .iter()
        .zip(&out_results)
        .enumerate()
        .filter_map(|(index, (compact, rendered))| {
            if compact == rendered {
                return None;
            }
            let compact_len = entry_escaped_len(compact);
            // `then_some` evaluates its argument eagerly regardless of the
            // guard, so an inline `rendered_len - compact_len` here would
            // underflow-panic exactly when compact is larger (the case this
            // guard exists to exclude) — `then` defers it to a closure.
            (compact_len < entry_lens[index])
                .then(|| (index, entry_lens[index] - compact_len, compact_len))
        })
        .collect();
    compact_fallbacks.sort_unstable_by_key(|&(_, saved_bytes, _)| std::cmp::Reverse(saved_bytes));
    for (index, _, compact_len) in compact_fallbacks {
        out_results[index] = compact_results[index].clone();
        entry_lens[index] = compact_len;
        if fits(&entry_lens, metadata_len) {
            metadata.insert("results".to_string(), Value::Array(out_results));
            return metadata;
        }
    }

    // Pass 2: omission, largest current entry first.
    let mut by_size: Vec<(usize, usize)> = out_results
        .iter()
        .enumerate()
        .map(|(index, entry)| (index, serialized_response_len(entry)))
        .collect();
    by_size.sort_unstable_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
    for (index, _) in by_size {
        let omitted = frame_budget_omission(&compact_results[index], registry);
        entry_lens[index] = entry_escaped_len(&omitted);
        out_results[index] = omitted;
        refresh_frame_budget_outcome(&mut metadata, &out_results);
        metadata_len = envelope_metadata_escaped_len(&metadata);
        if fits(&entry_lens, metadata_len) {
            break;
        }
    }
    metadata.insert("results".to_string(), Value::Array(out_results));
    metadata
}

/// All envelope fields except `results`, copied without ever touching the
/// (potentially large) `results` array — `map.clone()` would deep-clone it
/// just to have `results` overwritten a moment later.
fn envelope_metadata(map: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    map.iter()
        .filter(|(key, _)| key.as_str() != "results")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The escaped byte length of `{..metadata, "results": []}`. Since
/// `metadata` excludes `results`, this costs nothing proportional to batch
/// size — it changes only when `refresh_frame_budget_outcome` rewrites
/// `summary`/`status`, both tiny fixed-shape objects.
fn envelope_metadata_escaped_len(metadata: &serde_json::Map<String, Value>) -> usize {
    let mut probe = metadata.clone();
    probe.insert("results".to_string(), Value::Array(Vec::new()));
    json_escaped_len(&serde_json::to_vec(&Value::Object(probe)).expect("value is serializable"))
}

/// The escaped byte length `entry` contributes once embedded in the
/// envelope's `results` array (i.e. as it will be re-escaped a second time
/// when the whole envelope is wrapped in a daemon response frame).
fn entry_escaped_len(entry: &Value) -> usize {
    json_escaped_len(&serde_json::to_vec(entry).expect("value is serializable"))
}

/// `envelope_metadata_escaped_len(metadata)` plus the array literal built
/// from `entry_escaped_lens` — the incremental form of
/// `envelope_metadata_escaped_len` with `results` populated instead of
/// empty. `metadata_escaped_len` already counts the two bracket bytes of
/// `results`'s empty `[]`, and those brackets stay in place either way, so
/// only the entries and the commas between them are additional.
fn envelope_escaped_len(entry_escaped_lens: &[usize], metadata_escaped_len: usize) -> usize {
    let separators = entry_escaped_lens.len().saturating_sub(1);
    metadata_escaped_len + entry_escaped_lens.iter().sum::<usize>() + separators
}

/// The number of bytes `bytes` would occupy once JSON-string-escaped
/// (excluding the surrounding quotes), matching serde_json's default
/// formatter (`ESCAPE` table in `serde_json::ser`) byte for byte. Escaping
/// has no cross-byte state, so this is additive over concatenation — the
/// property `fit_rendered_batch_envelope` relies on to compute the frame
/// length from per-entry lengths instead of re-serializing the envelope.
fn json_escaped_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&byte| match byte {
            b'"' | b'\\' => 2,
            0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
            0x00..=0x1F => 6,
            _ => 1,
        })
        .sum()
}

/// The daemon-frame byte length for an empty rendered payload — the fixed
/// overhead every non-empty payload's escaped length is added on top of.
fn empty_rendered_daemon_frame_len(served_config_id: &str) -> usize {
    rendered_response_daemon_frame_len("", served_config_id)
}

fn frame_budget_error(message: &str, disposition: DomainDisposition) -> Value {
    error_with_disposition(
        json!({"kind":"response_frame_budget_exceeded", "code":"response_frame_budget_exceeded",
        "message":message, "retryable":false,
        "max_frame_bytes":khive_runtime::daemon::MAX_FRAME_BYTES}),
        disposition,
    )
}

fn frame_budget_omission(entry: &Value, registry: &VerbRegistry) -> Value {
    let ok = entry.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if entry.get("aborted").and_then(Value::as_bool) == Some(true) {
        let mut aborted = aborted_entry(
            entry
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            None,
        );
        for key in ["usage", "reason", "advisories"] {
            if let Some(value) = entry.get(key) {
                aborted[key] = value.clone();
            }
        }
        return aborted;
    }
    let disposition = if ok {
        DomainDisposition::Committed
    } else {
        error_disposition(&entry["error"])
    };
    let mut omitted = failure_entry(entry.get("tool").and_then(Value::as_str).unwrap_or_default(),
        frame_budget_error("operation failed; error details omitted because the response frame budget was exceeded", disposition),
        disposition).as_object().expect("error envelope").clone();
    // `reason` is stable machine metadata, not payload detail. It is tiny and
    // must survive even when a large result/error body is omitted to fit the
    // daemon frame.
    for key in ["ok", "tool", "usage", "aborted", "reason", "advisories"] {
        if let Some(value) = entry.get(key) {
            omitted.insert(key.to_string(), value.clone());
        }
    }
    if ok {
        // Once the result is discarded, the operation is not a usable
        // success. In particular, a pager must not interpret the missing
        // payload as an empty terminal page. Surface a small, typed error
        // and tell the caller what to actually do about it: reissue with a
        // narrower request, or read the already-committed outcome back.
        //
        // This decision runs after `run_parsed` has already dispatched the
        // operation (and, in a chain, every operation after it) — the frame
        // budget is checked at render time, once the full envelope is known.
        // A frame-budget overflow is never a transient, pace-and-retry
        // condition: reissuing the identical request exceeds the identical
        // budget identically. `retryable` therefore always stays `false`
        // here — ADR-130 §2/§4 tie a `true` value to a published
        // `retry_after_ms`/backoff/breaker contract this failure class does
        // not have — and `recoverable` carries the actual guidance instead.
        // A `Directive`/`Commissive`/`Declaration` verb (or an unregistered/
        // unknown tool name, which cannot be proven side-effect-free)
        // already committed its effect before the transport discovered the
        // response was too large, so it fails closed to `read_outcome`; an
        // `Assertive` verb with nothing to duplicate is told
        // `reduce_result_size` instead. `is_retry_safe_after_frame_omission`
        // (`khive-runtime`) additionally excludes a short, audited list of
        // `Assertive` verbs that schedule a persisted write on every
        // dispatch (`memory.recall`'s serve ledger, `search`'s
        // `SearchExecuted` telemetry) — see its doc comment and
        // `VerbCategory`'s doc comment in `khive-types`.
        omitted.insert("ok".to_string(), Value::Bool(false));
        let tool = entry.get("tool").and_then(Value::as_str);
        let retry_safe = tool.is_some_and(|verb| registry.is_retry_safe_after_frame_omission(verb));
        let (message, recoverable) = if retry_safe {
            (
                "operation result exceeded the daemon response frame budget; reduce limit \
                 or result size and reissue the request",
                "reduce_result_size",
            )
        } else {
            omitted.insert("executed".to_string(), Value::Bool(true));
            (
                "operation completed but its result exceeded the daemon response frame \
                 budget; read the outcome back instead of reissuing the operation",
                "read_outcome",
            )
        };
        let mut error = serde_json::Map::from_iter([
            ("kind".to_string(), json!("response_frame_budget_exceeded")),
            ("code".to_string(), json!("response_frame_budget_exceeded")),
            ("message".to_string(), json!(message)),
            ("retryable".to_string(), json!(false)),
            ("recoverable".to_string(), json!(recoverable)),
            (
                "max_frame_bytes".to_string(),
                json!(khive_runtime::daemon::MAX_FRAME_BYTES),
            ),
        ]);
        // ADR-130 defines `status`/`arm_participation`/`partial`/`missing_backends`/
        // `backend_errors*` only on a successful search entry. Once `ok` flips to
        // false here they no longer belong at the top level; fold any that were
        // present into `error.search` instead of dropping the diagnostic outright.
        let search_fields: serde_json::Map<String, Value> = [
            "status",
            "arm_participation",
            "partial",
            "missing_backends",
            "backend_errors",
            "backend_errors_truncated",
            "backend_errors_omitted",
        ]
        .into_iter()
        .filter_map(|key| entry.get(key).map(|value| (key.to_string(), value.clone())))
        .collect();
        if !search_fields.is_empty() {
            error.insert("search".to_string(), Value::Object(search_fields));
        }
        omitted.insert(
            "error".to_string(),
            error_with_disposition(Value::Object(error), disposition),
        );
    } else {
        // ADR-130 §Compatibility (MCP envelope builder): `search_incomplete`
        // is small and typed — it must survive omission untransformed rather
        // than collapse to the generic omitted-error string every other
        // (potentially large) error payload gets.
        let is_search_incomplete = entry
            .get("error")
            .and_then(|error| error.get("kind"))
            .and_then(Value::as_str)
            == Some("search_incomplete");
        if is_search_incomplete {
            if let Some(error) = entry.get("error") {
                omitted.insert("error".to_string(), error.clone());
            }
        }
    }
    Value::Object(omitted)
}

/// Rebuild aggregate outcome fields after the transport layer turns one or
/// more oversized successes into explicit failures.
fn refresh_frame_budget_outcome(map: &mut serde_json::Map<String, Value>, results: &[Value]) {
    let total = results.len();
    let succeeded = results
        .iter()
        .filter(|entry| entry.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let aborted = results
        .iter()
        .filter(|entry| {
            entry.get("ok").and_then(Value::as_bool) == Some(false)
                && entry.get("aborted").and_then(Value::as_bool) == Some(true)
        })
        .count();
    let failed = total.saturating_sub(succeeded + aborted);
    map.insert(
        "summary".to_string(),
        json!({
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
            "aborted": aborted,
        }),
    );
    map.insert("status".to_string(), json!(batch_status(failed, aborted)));
}

fn serialized_response_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("serde_json::Value is always serializable")
        .len()
}

fn serialize_response_value(value: &Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is always serializable")
}

fn rendered_response_fits_daemon_frame(rendered: &str, served_config_id: &str) -> bool {
    rendered_response_daemon_frame_len(rendered, served_config_id)
        <= khive_runtime::daemon::MAX_FRAME_BYTES
}

fn rendered_response_daemon_frame_len(rendered: &str, served_config_id: &str) -> usize {
    let frame = khive_runtime::DaemonResponseFrame {
        ok: true,
        result: Some(rendered.to_string()),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(served_config_id.to_string()),
        version_mismatch: false,
        daemon_protocol_version: khive_runtime::PROTOCOL_VERSION,
        metrics: None,
        request_id: Some(u64::MAX),
    };
    serde_json::to_vec(&frame)
        .expect("daemon response frame is always serializable")
        .len()
}

/// Build the `initialize` instructions string from the verb catalog, the
/// packs this server actually loaded, and the packs that are linked into the
/// binary but were not selected. Extracted from [`ServerHandler::get_info`] so
/// the docs-pointer section (#594) is unit-testable without standing up a
/// full server.
///
/// The two pack lists answer different questions and the instructions must not
/// merge them (#2913). `loaded` is what the caller can call right now: the
/// selection resolved at startup, plus any configured mounts. `unloaded` is
/// what `KHIVE_PACKS` or `--pack` could additionally select from this binary.
/// Naming the linked set alone told callers that packs contributing nothing to
/// the catalog below were available, and every verb of theirs is refused.
fn build_instructions(catalog: &str, loaded: &str, unloaded: &str) -> String {
    let selectable = if unloaded.is_empty() {
        String::new()
    } else {
        format!(
            " Also linked into this binary but not loaded here, so their verbs are \
             absent from the catalog below until selected: {unloaded}."
        )
    };
    format!(
        "khive — request-only MCP surface. One tool, `request`, \
         dispatches verbs through the loaded pack registry. Configure packs via \
         KHIVE_PACKS or --pack. Loaded on this server: {loaded}.{selectable} The kg pack's verbs are \
         unprefixed (create, get, list, search, link, neighbors, ...); every other pack's \
         verbs are written pack.verb. Read verbs return their record or hits directly \
         unless the verb's help says it wraps them in an envelope. Verbs registered on this \
         server:\n{catalog}\nFor detailed usage of each verb, see the corresponding \
         plugin's SKILL.md files.\n\
         Docs: https://ohdearquant.github.io/khive/ (hosted) or docs/*.md in the repo \
         checkout. Treat the live verb catalog above and help=true as authoritative over \
         cached/training knowledge. Config/backend issues: docs/configuration.md. Usage \
         patterns: docs/guide/tips-and-tricks.md."
    )
}

/// Preserve the request envelope verbatim while making its all-failed state
/// visible to MCP clients that inspect `isError` instead of parsing text.
fn mark_all_failed_request_result(result: &mut rmcp::model::CallToolResult) {
    let Some(text) = result.content.iter().find_map(|content| content.as_text()) else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<Value>(&text.text) else {
        return;
    };
    let Some(summary) = envelope.get("summary") else {
        return;
    };
    let (Some(succeeded), Some(failed), Some(aborted)) = (
        summary.get("succeeded").and_then(Value::as_u64),
        summary.get("failed").and_then(Value::as_u64),
        summary.get("aborted").and_then(Value::as_u64),
    ) else {
        return;
    };
    if succeeded == 0 && failed.saturating_add(aborted) > 0 {
        result.is_error = Some(true);
    }
}

#[cfg(test)]
#[path = "server/request_result_error_tests.rs"]
mod request_result_error_tests;

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let is_request_tool = request.name == "request";
        // The router turns parameter-deserialization failures into tool errors.
        // Plan isolation requires the JSON-RPC invalid_params response instead.
        if request.name == "request" {
            if let Some(args) = request.arguments.as_ref() {
                if args.get("plan") == Some(&Value::Bool(true)) {
                    for field in crate::tools::request::PLAN_COMPANIONS {
                        if args.contains_key(field) {
                            return Err(invalid_request_error(format!(
                                "plan=true cannot be combined with {field}"
                            )));
                        }
                    }
                }
            }
        }
        let context = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let mut result = Self::tool_router().call(context).await?;
        if is_request_tool {
            mark_all_failed_request_result(&mut result);
        }
        Ok(result)
    }

    fn get_info(&self) -> ServerInfo {
        let catalog = self.verb_catalog();
        let loaded_names = self.registry.pack_names();
        let loaded = loaded_names.join(", ");
        // Linked minus loaded: the packs `--pack`/`KHIVE_PACKS` could still
        // select. `builtin_pack_names` is the link-time inventory, so it never
        // lists a configured mount; a mount is only ever in `loaded_names`.
        let unloaded = builtin_pack_names()
            .into_iter()
            .filter(|name| !loaded_names.contains(name))
            .collect::<Vec<_>>()
            .join(", ");
        let instructions = build_instructions(&catalog, &loaded, &unloaded);
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    /// Override the macro-generated `list_tools` so the `request` tool's
    /// description carries the dynamic verb catalog built from the loaded
    /// pack registry. Many MCP clients only surface `tools/list` descriptions
    /// (not server instructions) — discovery must work via tool listing.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let mut tools = Self::tool_router().list_all();
        self.registry.mounted_verb_catalog().await.map_err(|_| {
            McpError::internal_error(
                "mounted catalog unavailable",
                Some(error_with_disposition(
                    json!({
                        "kind": "unavailable",
                        "message": "mounted catalog unavailable",
                        "details": {"class": "tool_error", "reason": "catalog_drift"},
                    }),
                    DomainDisposition::NotCommitted,
                )),
            )
        })?;
        let catalog = self.verb_catalog();
        for t in &mut tools {
            if t.name == "request" {
                let base = t.description.as_deref().unwrap_or("");
                t.description = Some(std::borrow::Cow::Owned(format!(
                    "{base}\n\nVerbs registered on this server:\n{catalog}"
                )));
            }
        }
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }
}

#[cfg(test)]
#[path = "server/events_wal_policy_tests.rs"]
mod events_wal_policy_tests;

#[cfg(test)]
mod request_read_cancellation_tests;
#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

#[cfg(test)]
mod bridge_diagnostics_tests;

#[cfg(test)]
mod disposition_tests;

#[cfg(test)]
mod refusal_event_tests;

#[cfg(test)]
mod issue_2537_tests;

#[cfg(test)]
#[path = "server_operation_attribution_tests.rs"]
mod operation_attribution_tests;

#[cfg(test)]
#[path = "event_row_usage_tests.rs"]
mod event_row_usage_tests;

#[cfg(test)]
#[path = "server/numeric_env_tests.rs"]
mod numeric_env_tests;
