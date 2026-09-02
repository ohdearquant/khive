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
    collections::BTreeMap,
    future::Future,
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
use khive_pack_kg::handlers::{SearchSubstrate, ValidatedSearchRequest};
use khive_request::{
    parse_request, parse_typed_json_batch, ArgValue, DslError, ExecutionMode, ParsedOp,
    ParsedRequest, PrevFailure, TypedJsonOp,
};
use khive_runtime::{
    present, render_format, InterceptedDispatchResult, KhiveRuntime, OutputFormat, PackLoadError,
    PackRegistry, PresentationMode, RuntimeConfig, RuntimeError, VerbPresentationPolicy,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_types::RefusalReason;

use khive_storage::{EdgeRelation, StorageCapability};

use crate::coordinator::{CoordSearchResult, CoordinatorService};
use crate::tools::request::RequestParams;

const MAX_BACKEND_ERROR_ENTRIES: usize = 16;
const MAX_BACKEND_ERROR_KEY_CHARS: usize = 256;
const MAX_BACKEND_ERROR_MESSAGE_CHARS: usize = 1_024;
const MAX_BACKEND_ERROR_INPUT_CHARS: usize = MAX_BACKEND_ERROR_MESSAGE_CHARS * 4;
/// A legal request carries at most `MAX_OPS` operation envelopes. Reserving a
/// quarter of the daemon frame for their mandatory diagnostic metadata leaves
/// another quarter for JSON escaping and half for fixed envelope fields.
const MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP: usize =
    khive_runtime::daemon::MAX_FRAME_BYTES / khive_request::MAX_OPS / 4;
// A JSON scalar needs at most six bytes (`\uXXXX`). The key occurs in both
// `missing_backends` and `backend_errors`; 1 KiB covers the typed object,
// integer metadata, punctuation, and the optional ellipsis.
const MAX_SINGLE_BACKEND_SEARCH_DIAGNOSTIC_BYTES: usize =
    MAX_BACKEND_ERROR_KEY_CHARS * 6 * 2 + MAX_BACKEND_ERROR_MESSAGE_CHARS * 6 + 1_024;
const _: () = assert!(
    MAX_SINGLE_BACKEND_SEARCH_DIAGNOSTIC_BYTES <= MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP,
    "the search diagnostic budget must retain at least one backend cause"
);
const MISSING_BACKEND_ERROR_MESSAGE: &str = "backend search failed without diagnostic detail";

/// Per-operation completeness discriminator for the `search` verb (ADR-130
/// §1). `SearchDegradation::status == None` means "not a search op" — no
/// `status` field is emitted on that operation's envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchStatus {
    Complete,
    Partial,
}

impl SearchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BackendErrorDiagnostic {
    message: String,
    backend_id_masked: bool,
    backend_id_truncated: bool,
    backend_id_chars: usize,
}

#[derive(Debug, Default)]
struct SearchDegradation {
    status: Option<SearchStatus>,
    missing_backends: Vec<String>,
    backend_errors: BTreeMap<String, BackendErrorDiagnostic>,
    backend_errors_omitted: usize,
}

impl SearchDegradation {
    /// A search op that ran to completion: single-backend/no-coordinator
    /// registry dispatch, or (in principle) a coordinator fan-out where
    /// every selected backend succeeded — `from_result` is used for the
    /// latter instead, since it also has to compute `missing_backends`.
    fn complete() -> Self {
        Self {
            status: Some(SearchStatus::Complete),
            missing_backends: Vec::new(),
            backend_errors: BTreeMap::new(),
            backend_errors_omitted: 0,
        }
    }

    fn from_result(result: &CoordSearchResult) -> Self {
        let failed_backend_count = result
            .per_backend
            .iter()
            .filter(|backend| backend.error.is_some())
            .count();
        let mut candidates = BTreeMap::new();
        for (backend, error) in result
            .per_backend
            .iter()
            .filter_map(|backend| backend.error.as_deref().map(|error| (backend, error)))
        {
            let (key, backend_id_masked, backend_id_truncated, backend_id_chars) =
                bounded_backend_error_key(backend.backend_id.as_str());
            candidates.insert(
                key,
                BackendErrorDiagnostic {
                    message: bounded_backend_error_message(error),
                    backend_id_masked,
                    backend_id_truncated,
                    backend_id_chars,
                },
            );
            if candidates.len() > MAX_BACKEND_ERROR_ENTRIES {
                let _ = candidates.pop_last();
            }
        }

        let mut backend_errors = BTreeMap::new();
        for (backend, diagnostic) in candidates {
            let mut candidate = backend_errors.clone();
            candidate.insert(backend, diagnostic);
            let candidate_degradation = Self {
                status: Some(SearchStatus::Partial),
                missing_backends: candidate.keys().cloned().collect(),
                backend_errors_omitted: failed_backend_count.saturating_sub(candidate.len()),
                backend_errors: candidate.clone(),
            };
            if search_diagnostic_wire_len(&candidate_degradation)
                <= MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP
            {
                backend_errors = candidate;
            }
        }

        let missing_backends: Vec<String> = backend_errors.keys().cloned().collect();
        let backend_errors_omitted = failed_backend_count.saturating_sub(backend_errors.len());
        let is_partial = failed_backend_count > 0;
        debug_assert!(!is_partial || !backend_errors.is_empty());
        debug_assert_eq!(
            missing_backends,
            backend_errors.keys().cloned().collect::<Vec<_>>()
        );
        debug_assert_eq!(result.partial, is_partial);
        let status = if is_partial {
            SearchStatus::Partial
        } else {
            SearchStatus::Complete
        };
        for (backend, diagnostic) in &backend_errors {
            tracing::warn!(
                backend,
                error = %diagnostic.message,
                "fan-out search backend failed"
            );
        }
        if backend_errors_omitted > 0 {
            tracing::warn!(
                failed_backend_count,
                retained_backend_errors = backend_errors.len(),
                backend_errors_omitted,
                "additional fan-out search backend diagnostics omitted by bounds"
            );
        }
        Self {
            status: Some(status),
            missing_backends,
            backend_errors,
            backend_errors_omitted,
        }
    }

    fn is_partial(&self) -> bool {
        self.status == Some(SearchStatus::Partial)
    }
}

fn bounded_backend_error_message(message: &str) -> String {
    let bounded_input: String = message
        .chars()
        .take(MAX_BACKEND_ERROR_INPUT_CHARS)
        .collect();
    let masked = khive_runtime::secret_gate::mask_secrets(&bounded_input);
    if masked.trim().is_empty() {
        return MISSING_BACKEND_ERROR_MESSAGE.to_string();
    }
    let mut chars = masked.chars();
    let mut bounded: String = chars
        .by_ref()
        .take(MAX_BACKEND_ERROR_MESSAGE_CHARS)
        .collect();
    if chars.next().is_some() || message.chars().nth(MAX_BACKEND_ERROR_INPUT_CHARS).is_some() {
        bounded.push('…');
    }
    bounded
}

fn bounded_backend_error_key(backend_id: &str) -> (String, bool, bool, usize) {
    let backend_id_chars = backend_id.chars().count();
    let bounded_input: String = backend_id
        .chars()
        .take(MAX_BACKEND_ERROR_INPUT_CHARS)
        .collect();
    let masked = khive_runtime::secret_gate::mask_secrets(&bounded_input);
    let backend_id_masked = masked.as_ref() != bounded_input || masked.trim().is_empty();
    let sanitized = if masked.trim().is_empty() {
        "masked-backend"
    } else {
        masked.as_ref()
    };
    if !backend_id_masked && backend_id_chars <= MAX_BACKEND_ERROR_KEY_CHARS {
        return (sanitized.to_string(), false, false, backend_id_chars);
    }

    let fingerprint = format!("{:x}", Sha256::digest(backend_id.as_bytes()));
    let suffix = format!("…#{fingerprint}");
    let prefix_chars = MAX_BACKEND_ERROR_KEY_CHARS - suffix.chars().count();
    let prefix: String = sanitized.chars().take(prefix_chars).collect();
    (
        format!("{prefix}{suffix}"),
        backend_id_masked,
        backend_id_chars > MAX_BACKEND_ERROR_KEY_CHARS,
        backend_id_chars,
    )
}

fn backend_errors_value(errors: &BTreeMap<String, BackendErrorDiagnostic>) -> Value {
    Value::Object(
        errors
            .iter()
            .map(|(backend, diagnostic)| {
                let mut value = json!({
                    "kind": "backend_error",
                    "message": diagnostic.message,
                });
                if diagnostic.backend_id_masked {
                    value["backend_id_masked"] = Value::Bool(true);
                }
                if diagnostic.backend_id_truncated {
                    value["backend_id_truncated"] = Value::Bool(true);
                    value["backend_id_chars"] = json!(diagnostic.backend_id_chars);
                }
                (backend.clone(), value)
            })
            .collect(),
    )
}

fn search_diagnostic_value(degradation: &SearchDegradation) -> Value {
    let mut value = json!({
        "kind": "search_incomplete",
        "message": "no-match was not established because selected backends failed",
        "retryable": false,
        "missing_backends": degradation.missing_backends,
        "backend_errors": backend_errors_value(&degradation.backend_errors),
    });
    if degradation.backend_errors_omitted > 0 {
        value["backend_errors_truncated"] = Value::Bool(true);
        value["backend_errors_omitted"] = json!(degradation.backend_errors_omitted);
    }
    value
}

fn search_diagnostic_wire_len(degradation: &SearchDegradation) -> usize {
    serde_json::to_vec(&search_diagnostic_value(degradation))
        .expect("search diagnostic metadata is always serializable")
        .len()
}

struct OpSuccess {
    result: Value,
    degradation: SearchDegradation,
}

impl OpSuccess {
    fn complete(result: Value) -> Self {
        Self {
            result,
            degradation: SearchDegradation::default(),
        }
    }
}

/// `OpSuccess` for an op dispatched through the plain registry path
/// (single-backend deployment, or no coordinator attached). A `search` op
/// (excluding `help=true`, which returns a schema rather than a result
/// array) carries `status="complete"` (ADR-130 §1); every other verb keeps
/// the untagged `OpSuccess::complete` — no `status` field on its envelope.
fn op_success_from_registry_result(tool: &str, is_help: bool, result: Value) -> OpSuccess {
    if tool == "search" && !is_help {
        OpSuccess {
            result,
            degradation: SearchDegradation::complete(),
        }
    } else {
        OpSuccess::complete(result)
    }
}

/// Structured error for a search whose selected backends failed such that no
/// hit survived server-side filtering (ADR-130 §1 `search_incomplete`).
///
/// Distinguishes a degraded read from a genuine no-match: a genuine no-match
/// keeps `ok=true` with an empty `result`; this is `ok=false` — a caller
/// doing `if response.ok && response.result.is_empty()` sees the two cases
/// differently, instead of concluding "no match" in both.
fn search_incomplete_error(degradation: SearchDegradation) -> Value {
    search_diagnostic_value(&degradation)
}

/// Per-request parallelism stays bounded even when the parser accepts 100 ops; must be nonzero.
const MAX_BATCH_CONCURRENCY: usize = 8;

/// Half the frame remains for the daemon's outer serialization and budget-error entries.
const BATCH_RESPONSE_BUDGET_BYTES: usize = khive_runtime::daemon::MAX_FRAME_BYTES / 2;

struct BatchTask<F> {
    index: usize,
    tool: String,
    future: F,
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
    fn unclassified(tool: impl Into<String>, error: Value) -> Self {
        Self {
            tool: tool.into(),
            error,
            reason: None,
        }
    }

    fn from_runtime(tool: &str, error: RuntimeError) -> Self {
        let reason = match &error {
            // `gate-refusal` is deliberately limited to the write-time secret
            // gate. Authorization denials and gate infrastructure errors keep
            // their established, unclassified shapes.
            RuntimeError::SecretDetected(_) => Some(RefusalReason::GateRefusal),
            RuntimeError::UnknownVerb(_) => Some(RefusalReason::VerbRefused),
            _ => None,
        };
        Self {
            tool: tool.to_string(),
            error: runtime_error_value(error),
            reason,
        }
    }

    fn into_entry(self) -> Value {
        let mut entry = json!({ "ok": false, "tool": self.tool, "error": self.error });
        if let Some(reason) = self.reason {
            entry["reason"] = json!(reason.as_str());
        }
        entry
    }
}

/// Fingerprint the engine-coherence parts of a resolved [`RuntimeConfig`].
///
/// Two servers produce the same id iff they can safely share one warm engine:
/// same pack set (order-independent), same storage target and effective access
/// mode, same embedders, same backend topology/routing, and same
/// construction-baked fresh-tail, blob-hydration, outbound, and git-write
/// policies.
/// Identity fields (`namespace`, `actor_id`, `visible_namespaces`) are carried
/// per request in the daemon frame and must never enter this key. The daemon
/// compares this against each forwarded request's `config_id` and rejects
/// mismatches so a restricted client (e.g. `--pack kg`, `--db :memory:`) cannot
/// execute through the broader default daemon.
///
/// When `khive_cfg` is supplied and contains a non-empty `[[backends]]`
/// declaration, the backend topology (sorted backend list, explicit read-only
/// modes, and pack→backend assignments) is folded into the fingerprint so that
/// two configs differing only in routing or access mode produce different ids
/// (ADR-049 / B-SHOULD-FIX-4). Delimiter-free topologies retain their legacy
/// spelling; a topology containing reserved delimiter text uses an injective,
/// escaped v2 encoding so path data can never impersonate access mode.
///
/// When `khive_cfg` is `None` or its `backends` list is empty, a writable
/// target remains byte-identical to what it would have been before this
/// parameter was added. An existing path with no filesystem write bits gains
/// the read-only backend marker before the runtime opens it, so forwarding and
/// server fingerprints converge.
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
/// so the mismatch remains parseable as a structured backend mismatch without
/// changing the legacy fingerprint for writable runtimes. Pre-open callers
/// that have already applied a storage override (for example, multi-backend
/// `--db :memory:`) must use this form rather than re-reading the superseded
/// declaration through [`compute_config_id`].
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

/// Compute daemon identity from construction-captured runtime policies.
///
/// `storage_read_only` is authoritative. Re-probing filesystem permissions
/// here could relabel a runtime that already retained a write-capable SQLite
/// handle after a later chmod; only the pre-open wrappers above may probe.
pub(crate) fn compute_config_id_with_runtime_policies(
    config: &RuntimeConfig,
    khive_cfg: Option<&khive_runtime::KhiveConfig>,
    ann_fresh_tail_enabled: bool,
    storage_read_only: bool,
) -> String {
    let mut packs = config.packs.clone();
    packs.sort();
    let db = config
        .db_path
        .as_deref()
        .map(canonical_fingerprint_path)
        .unwrap_or_else(|| ":memory:".to_string());
    let primary = config
        .embedding_model
        .as_ref()
        .map(|m| format!("{m:?}"))
        .unwrap_or_else(|| "none".to_string());
    let mut extra: Vec<String> = config
        .additional_embedding_models
        .iter()
        .map(|m| format!("{m:?}"))
        .collect();
    extra.sort();
    let mut outbound: Vec<String> = config
        .allowed_outbound_namespaces
        .iter()
        .map(|ns| ns.as_str().to_owned())
        .collect();
    outbound.sort();
    outbound.dedup();
    let mut git_write_hasher = Sha256::new();
    git_write_hasher.update(b"khive.git-write-policy.v1");
    git_write_hasher.update((config.git_write.allowed.len() as u64).to_be_bytes());
    for entry in &config.git_write.allowed {
        git_write_hasher.update((entry.repo.len() as u64).to_be_bytes());
        git_write_hasher.update(entry.repo.as_bytes());
        git_write_hasher.update((entry.branches.len() as u64).to_be_bytes());
        for branch in &entry.branches {
            git_write_hasher.update((branch.len() as u64).to_be_bytes());
            git_write_hasher.update(branch.as_bytes());
        }
    }
    let git_write = format!("{:x}", git_write_hasher.finalize());

    let backend = if storage_read_only {
        format!("{:?}:read_only", config.backend_id)
    } else {
        format!("{:?}", config.backend_id)
    };
    // `display_timezone` is part of daemon identity, not merely of rendering
    // (ADR-169). `gtd.assign` anchors a date-only `due` through
    // `config.display_timezone` and PERSISTS the resulting instant, so two
    // runtimes differing only in this field are not interchangeable: a warm
    // daemon reused across them writes an instant that is wrong by the offset
    // between the zones, silently and durably.
    //
    // Included unconditionally rather than only when non-default. The default
    // is the HOST's zone, not UTC, so "differs from the default" is itself a
    // host-dependent predicate and would make identity depend on where the
    // fingerprint was computed.
    //
    // The cost, stated as it actually happens: a daemon already warm when this
    // lands keeps the identity it computed at startup, so a client built from
    // this code sends an ID that daemon does not recognise. The daemon answers
    // `config_mismatch` and the client falls back to LOCAL dispatch. It does
    // not respawn — `FallbackReason::ConfigMismatch` is classified
    // `FallbackSeverity::Illegitimate`, and the kill-and-respawn path
    // (#644/#539) governs the protocol/parse reasons, not this one. So until
    // that daemon is restarted, every request pays a failed forwarding round
    // trip and loses the daemon's warm indexes and embedders, and each one
    // increments a counter documented as never expected on a correctly
    // configured fleet.
    //
    // Spelled out because "the daemon takes a new identity" invites the reading
    // that it restarts itself. It does not, and nothing here makes it: this is
    // a one-time operational cost that ends when the daemon is restarted, by
    // whoever restarts it.
    let base = format!(
        "packs=[{}];db={};embed={};extra=[{}];fresh_tail={};blob_hydration_bytes={};backend={};outbound=[{}];git_write={};display_tz={}",
        packs.join(","),
        db,
        primary,
        extra.join(","),
        ann_fresh_tail_enabled,
        config.blob_hydration_bytes,
        backend,
        outbound.join(","),
        git_write,
        config.display_timezone.name(),
    );

    // Fold backend topology when non-empty so two configs differing only in
    // pack→backend routing produce different config_ids (ADR-049).
    // When backends is empty this branch is skipped, preserving byte-identity
    // with the pre-change fingerprint.
    let topology = khive_cfg
        .filter(|cfg| !cfg.backends.is_empty())
        .map(encode_backend_topology)
        .unwrap_or_default();

    format!("{base}{topology}")
}

/// Reserved syntax in the legacy topology spelling.
///
/// Keeping the legacy representation when every caller-controlled component
/// excludes these bytes preserves existing warm-daemon identities without
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

fn encode_backend_topology(cfg: &khive_runtime::KhiveConfig) -> String {
    let mut legacy_safe = true;
    let mut backend_rows: Vec<(String, String, String, bool)> = cfg
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
            (backend.name.clone(), kind, path, backend.read_only)
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
            .map(|(name, kind, path, is_read_only)| {
                let read_only = if *is_read_only { ":read_only" } else { "" };
                format!("{name}:{kind}:{path}{read_only}")
            })
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
            .map(|(name, kind, path, read_only)| {
                let mode = if *read_only { "r" } else { "w" };
                format!(
                    "{}:{}:{}:{mode}",
                    escape_topology_component(name),
                    escape_topology_component(kind),
                    escape_topology_component(path),
                )
            })
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
#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChannelLoopAdmission {
    pub(crate) inbound_poll: bool,
    pub(crate) outbound_delivery: bool,
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
impl ChannelLoopAdmission {
    fn for_single_runtime(runtime: &KhiveRuntime, packs: &[String]) -> Self {
        let comm_loaded = packs.iter().any(|pack| pack == "comm");
        let writable = !runtime.is_read_only();
        Self {
            inbound_poll: comm_loaded && writable,
            outbound_delivery: comm_loaded && writable,
        }
    }

    pub(crate) fn for_pack_runtimes(comm: Option<&KhiveRuntime>) -> Self {
        let admitted = comm.is_some_and(|runtime| !runtime.is_read_only());
        Self {
            inbound_poll: admitted,
            outbound_delivery: admitted,
        }
    }
}

/// MCP server that dispatches all verbs through a [`VerbRegistry`].
#[derive(Clone)]
pub struct KhiveMcpServer {
    registry: VerbRegistry,
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
    MissingDependency { pack: String, dep: String },
    NoPublicVerbs { pack: String },
    Registry(khive_runtime::RuntimeError),
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
            PackRegFailure::MissingDependency { pack, dep } => {
                dbg.field("pack", pack).field("missing_dep", dep)
            }
            PackRegFailure::NoPublicVerbs { pack } => dbg.field("pack", pack),
            PackRegFailure::Registry(source) => dbg.field("source", source),
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
    let secs = std::env::var("KHIVE_BRIDGE_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
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
    let secs = std::env::var("KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
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
    std::env::var("KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
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
        } else if let Ok(tok) = runtime.authorize(khive_runtime::Namespace::local()) {
            if let Ok(event_store) = runtime.events(&tok) {
                builder.with_event_store(event_store);
            }
        }
        if let Err(load_err) = PackRegistry::register_packs(packs, runtime.clone(), &mut builder) {
            let failure = match load_err {
                PackLoadError::UnknownPack(name) => PackRegFailure::UnknownPack(name),
                PackLoadError::MissingDependency { pack, dep } => {
                    PackRegFailure::MissingDependency { pack, dep }
                }
                PackLoadError::NoPublicVerbs { pack } => PackRegFailure::NoPublicVerbs { pack },
            };
            return Err(PackRegError { failure, runtime });
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
        registry.call_register_note_write_validators(&runtime);
        // Apply pack-auxiliary schema plans at startup so pack tables are
        // present before any handler runs. Errors are logged but not propagated
        // so a single pack's schema failure cannot abort startup.
        registry.apply_schema_plans(runtime.backend());
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
        Self {
            registry,
            default_namespace: "local".to_string(),
            // A registry injected directly has no resolved RuntimeConfig; use a
            // sentinel that matches no real daemon so such servers always
            // dispatch locally rather than forward.
            config_id: "registry-only".to_string(),
            coordinator: None,
            pool: None,
            secondary_pools: Vec::new(),
            default_output_format: OutputFormat::Json,
            schedule_ticker_last_tick_micros: Arc::new(AtomicI64::new(0)),
            runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_outbox_runtime: None,
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            channel_loop_admission: ChannelLoopAdmission::default(),
        }
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

    /// Clone the verb registry for use by background tasks (e.g. channel polling loops).
    ///
    /// `VerbRegistry` is internally `Arc`-wrapped so this clone is cheap. The returned
    /// registry shares the same packs and dispatch state as the server.
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
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
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let root = tokio_util::sync::CancellationToken::new();
        let idle_timeout = stdio_bridge_idle_timeout_from_env();
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let max_outstanding_requests = stdio_bridge_max_outstanding_requests_from_env();
        let build_transport = |root: tokio_util::sync::CancellationToken| {
            let (read, write) = stdio();
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                crate::daemon::SelfHealOnFlushTransport::new(AsyncRwTransport::new_server(
                    read, write,
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
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        use rmcp::transport::{async_rw::AsyncRwTransport, stdio};

        let root = tokio_util::sync::CancellationToken::new();
        let (read, write) = stdio();
        let response_deadline = stdio_bridge_response_deadline_from_env()?;
        let transport =
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                AsyncRwTransport::new_server(read, write),
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
        build_verb_catalog(verbs)
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
                    DispatchFailure::unclassified(
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
                    DispatchFailure::unclassified(
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
                            return Err(DispatchFailure::unclassified(
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
                            return Err(DispatchFailure::unclassified(
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
            return Err(DispatchFailure::unclassified(
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

        match self
            .registry
            .dispatch_with_identity(&tool, args_value, identity.cloned())
            .await
        {
            Ok(result) => {
                let result = decorate_schedule_agenda_with_ticker_health(
                    &tool,
                    is_help,
                    result,
                    self.schedule_ticker_last_tick_micros.as_ref(),
                );
                let success = op_success_from_registry_result(&tool, is_help, result);
                chain_ok_envelope_or_depth_error(tool, success)
            }
            Err(error) => Err(DispatchFailure::from_runtime(&tool, error)),
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
        presentation: PresentationMode,
        presentation_per_op: Option<Vec<Option<PresentationMode>>>,
        context: RunParsedContext<'_>,
    ) -> Value {
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
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0);

        // Resolve per-op presentation mode: per-op entry overrides batch default.
        let mode_for_op = |i: usize| -> PresentationMode {
            presentation_per_op
                .as_ref()
                .and_then(|v| v.get(i))
                .and_then(|o| *o)
                .unwrap_or(presentation)
        };

        match mode {
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
                    let op_mode = mode_for_op(i);
                    let task_tool = op.tool.clone();
                    BatchTask {
                        index: i,
                        tool: task_tool,
                        future: async move {
                        // ADR-103 Amendment 2: one dispatch-accounting context
                        // per op; the entry is stamped with the frozen usage
                        // snapshot after dispatch resolves.
                        let usage_ctx = khive_runtime::usage::UsageContext::new();
                        let mut entry = khive_runtime::usage::scope(usage_ctx.clone(), async {
                        let tool = op.tool.clone();
                        // Conflicting ops get a per-op error; skip dispatch.
                        if let Some(msg) = conflict_with {
                            return json!({ "ok": false, "tool": tool, "error": msg });
                        }
                        // AlwaysVerbose verbs override the caller's presentation mode.
                        let effective_mode =
                            if registry.presentation_policy_for(&tool)
                                == VerbPresentationPolicy::AlwaysVerbose
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
                                prev_error = Some(json!({
                                    "ok": false,
                                    "tool": tool,
                                    "error": format!(
                                        "argument {name:?}: $prev reference is only valid in chain (|) mode"
                                    )
                                }));
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
                            return json!({
                                "ok": false,
                                "tool": tool,
                                "error": format!(
                                    "permission denied for verb {tool:?}: verb '{tool}' is an \
                                     internal subhandler and cannot be invoked via the MCP \
                                     request surface"
                                )
                            });
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
                                        ),
                                        Err(failure) => failure.into_entry(),
                                    };
                                }
                            }
                        }

                        match registry
                            .dispatch_with_identity(&tool, args_value, op_identity)
                            .await
                        {
                            Ok(result) => {
                                let result = decorate_schedule_agenda_with_ticker_health(
                                    &tool,
                                    is_help,
                                    result,
                                    schedule_ticker_last_tick_micros.as_ref(),
                                );
                                let success =
                                    op_success_from_registry_result(&tool, is_help, result);
                                present_ok_envelope_or_depth_error(
                                    tool,
                                    success,
                                    effective_mode,
                                    now_unix,
                                )
                            }
                            Err(error) => {
                                DispatchFailure::from_runtime(&tool, error).into_entry()
                            }
                        }
                        })
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
                        results.push(json!({
                            "ok": false,
                            "tool": op.tool,
                            "aborted": true,
                            "message": format!(
                                "not executed: op #{failed_index} ({failed_tool:?}) failed \
                                 earlier in this chain, so the chain aborted before reaching \
                                 this op. Fix op #{failed_index} — this op's own arguments, \
                                 including any $prev reference, were never evaluated."
                            ),
                        }));
                        continue;
                    }
                    let op_mode = mode_for_op(i);
                    // AlwaysVerbose verbs override the caller's presentation mode.
                    let effective_mode = if self.registry.presentation_policy_for(&op.tool)
                        == VerbPresentationPolicy::AlwaysVerbose
                    {
                        PresentationMode::Verbose
                    } else {
                        op_mode
                    };
                    let usage_ctx = khive_runtime::usage::UsageContext::new();
                    match khive_runtime::usage::scope(
                        usage_ctx.clone(),
                        self.dispatch_op(op, prev_result.as_ref(), from_wire, identity),
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
                                    let presented_obj = apply_presentation_to_result(
                                        result_obj,
                                        effective_mode,
                                        now_unix,
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
        }
    }
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
            let source_str = args_value.get("source_id")?.as_str()?;
            let target_str = args_value.get("target_id")?.as_str()?;
            let relation_str = args_value.get("relation")?.as_str()?;

            // Only intercept when both endpoints are parseable UUIDs.
            // Name/prefix resolution requires single-backend context — fall through.
            let source_id: uuid::Uuid = source_str.parse().ok()?;
            let target_id: uuid::Uuid = target_str.parse().ok()?;
            let relation: EdgeRelation = relation_str.parse().ok()?;
            let weight = args_value
                .get("weight")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let metadata = args_value.get("metadata").cloned();

            let result = registry
                .dispatch_intercepted_with_identity(
                    tool,
                    args_value,
                    identity,
                    |namespace| async move {
                        let coord_result = coord
                            .link(&namespace, source_id, target_id, relation, weight, metadata)
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
                        Ok(raw)
                    },
                )
                .await;
            Some(
                result
                    .map(OpSuccess::complete)
                    .map_err(|error| DispatchFailure::from_runtime(tool, error)),
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
            // MAJ-3: widen the fan-out's read-visibility scope to match the
            // normal registry dispatch path — see `coordinator_search_visibility`.
            let extra_visible = coordinator_search_visibility(registry, args_value, identity);
            let result = registry
                .dispatch_intercepted_with_metadata_with_identity(
                    tool,
                    args_value,
                    identity,
                    |namespace| async move {
                        // Match normal registry dispatch ordering: the gate has
                        // already authorized this namespace before handler-level
                        // search validation runs inside the intercepted closure.
                        let request = ValidatedSearchRequest::from_value(handler_args, registry)?;
                        let coord_result = coord
                            .fan_out_search(&request, &namespace, &extra_visible)
                            .await;
                        khive_storage::ensure_request_read_active("search")?;
                        let degradation = SearchDegradation::from_result(&coord_result);

                        // Preserve the coordinator search response's compatibility
                        // fields, and add the KG single-backend handler's canonical
                        // row fields for shape parity (MIN-1): `kind` (duplicates
                        // entity_kind/note_kind), `name`, and `created_at`.
                        // Entity hits: [{id, kind, entity_kind, name, score, source, title, snippet, created_at}]
                        // Note hits:   [{id, kind, note_kind, name, score, source, title, snippet, created_at}]
                        let result_val = if request.substrate() == SearchSubstrate::Note {
                            let items: Vec<Value> = coord_result
                                .note_hits
                                .iter()
                                .filter(|h| h.score.to_f64() >= request.min_score())
                                .map(|h| {
                                    let note_kind = coord_result.note_kinds.get(&h.note_id);
                                    let name =
                                        coord_result.note_names.get(&h.note_id).cloned().flatten();
                                    let created_at = coord_result
                                        .note_created_at
                                        .get(&h.note_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    json!({
                                        "id": h.note_id.to_string(),
                                        "kind": note_kind,
                                        "note_kind": note_kind,
                                        "name": name,
                                        "score": h.score.to_f64(),
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                    })
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        } else {
                            let items: Vec<Value> = coord_result
                                .entity_hits
                                .iter()
                                .filter(|h| h.score.to_f64() >= request.min_score())
                                .map(|h| {
                                    let entity_kind = coord_result.entity_kinds.get(&h.entity_id);
                                    let created_at = coord_result
                                        .entity_created_at
                                        .get(&h.entity_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    json!({
                                        "id": h.entity_id.to_string(),
                                        "kind": entity_kind,
                                        "entity_kind": entity_kind,
                                        "name": h.title,
                                        "score": h.score.to_f64(),
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                    })
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        };

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
                    // (post server-side filtering, min_score included) is a
                    // failed operation, not a successful empty result — the
                    // "no match" reading is not established when the answer
                    // may be sitting on the backend that never responded.
                    if outcome.metadata.is_partial() && is_empty {
                        Err(DispatchFailure::unclassified(
                            tool,
                            search_incomplete_error(outcome.metadata),
                        ))
                    } else {
                        Ok(OpSuccess {
                            result: outcome.result,
                            degradation: outcome.metadata,
                        })
                    }
                }
                Err(error) => Err(DispatchFailure::from_runtime(tool, error)),
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

/// Preserve the established flat-string payload for ordinary runtime errors,
/// while carrying typed write admission and writer-request finality through
/// every MCP execution mode. Finality is independent of retryability: a proven
/// rollback makes duplicate effects impossible but retains the source error's
/// transient policy, while an unverified rollback remains terminal and
/// ambiguous.
fn runtime_error_value(error: RuntimeError) -> Value {
    match error {
        RuntimeError::Khive(k) => serde_json::to_value(&k)
            .unwrap_or_else(|_| json!({"kind": "internal", "message": k.to_string()})),
        other => {
            if let Some(context) = other.writer_task_failure_context() {
                return json!({
                    "kind": "storage",
                    "code": context.stage,
                    "stage": context.stage,
                    "message": other.to_string(),
                    "retryable": context.retryable,
                    "request_state": context.request_state.to_string(),
                    "task_terminated": context.task_terminated,
                });
            }
            let Some(context) = other.retryable_failure_context() else {
                return json!(other.to_string());
            };
            let timeout_ms = u64::try_from(context.timeout.as_millis()).unwrap_or(u64::MAX);
            let capability = context.capability.map(storage_capability_wire_name);
            json!({
                "kind": "unavailable",
                "code": context.stage,
                "stage": context.stage,
                "message": other.to_string(),
                "retryable": true,
                "timeout_ms": timeout_ms,
                "capability": capability,
                "operation": context.operation,
                "scope": context.scope,
                "retry_after_ms": context.retry_after_ms,
            })
        }
    }
}

fn storage_capability_wire_name(capability: StorageCapability) -> &'static str {
    match capability {
        StorageCapability::Sql => "sql",
        StorageCapability::Notes => "notes",
        StorageCapability::Entities => "entities",
        StorageCapability::Graph => "graph",
        StorageCapability::Events => "events",
        StorageCapability::Vectors => "vectors",
        StorageCapability::Sparse => "sparse",
        StorageCapability::Text => "text",
        StorageCapability::Blob => "blob",
        StorageCapability::Attachments => "attachments",
    }
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
        missing_backends,
        backend_errors,
        backend_errors_omitted,
    } = degradation;
    let is_partial = status == Some(SearchStatus::Partial);
    let extra_fields = usize::from(status.is_some())
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
/// frozen usage snapshot. All-or-nothing: an empty snapshot (nothing measured
/// counted, but the context WAS armed) still stamps `{}`; the key is absent
/// only when no context existed. Best-effort — never alters ok/error status.
fn stamp_usage(entry: &mut Value, ctx: &khive_runtime::usage::UsageContext) {
    if let Value::Object(map) = entry {
        map.insert("usage".to_string(), ctx.frozen_or_snapshot());
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
        return Err(DispatchFailure::unclassified(
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
    now_unix: i64,
) -> Value {
    if !result_within_depth_limit(&success.result) {
        drop_value_iteratively(success.result);
        return json!({ "ok": false, "tool": tool, "error": depth_error_payload("") });
    }
    success.result = present(success.result, mode, now_unix);
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
        let error_entry = json!({
            "ok": false,
            "tool": tool_name,
            "error": {
                "kind": "result_too_deep",
                "message": format!(
                    "op result nesting depth exceeds max {}; \
                     cannot be used as $prev chain context",
                    khive_request::NESTING_DEPTH_LIMIT
                ),
            },
        });
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
    now_unix: i64,
) -> Value {
    if result_obj.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(result_field) = result_obj.get("result").cloned() {
            let presented = present(result_field, mode, now_unix);
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
        let configured = std::env::var("KHIVE_REQUEST_READ_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let timeout = configured
            .filter(|seconds| (1..=3_600).contains(seconds))
            .map(std::time::Duration::from_secs)
            .unwrap_or_else(|| {
                if let Some(invalid) = configured {
                    tracing::warn!(
                        invalid,
                        default = khive_storage::DEFAULT_REQUEST_READ_TIMEOUT_SECS,
                        "KHIVE_REQUEST_READ_TIMEOUT_SECS must be in [1, 3600]"
                    );
                }
                khive_storage::request_read_timeout_from_env()
            });
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_REQUEST_READ_TIMEOUT_SECS",
            timeout.as_secs().to_string(),
        );
        timeout
    })
}

async fn scope_mcp_request_read_cancellation<F>(
    cancellation: tokio_util::sync::CancellationToken,
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
        khive_storage::scope_request_read_deadline(request_read_timeout(), future),
    )
    .await
}

#[tool_router]
impl KhiveMcpServer {
    #[tool(description = r#"Run one or more khive verbs in a single MCP call.

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
`status` is "partial" whenever summary.failed or summary.aborted is non-zero — check
it (or summary) rather than relying on the absence of a top-level error.

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
must not be read as "no results found."

Verb discovery: install the `kg` / `gtd` plugins for usage skills. The verbs
currently registered on this server (pack-derived) are listed below. Argument
schemas live in each pack's docs and SKILL.md files.

Tip: for one-shot calls, the single-op form is the densest. Use batch when
several independent ops can run together; use chain when each op needs the prior
result (e.g. create then link with the new entity's id)."#)]
    async fn request(
        &self,
        Parameters(p): Parameters<RequestParams>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<String, McpError> {
        scope_mcp_request_read_cancellation(cancellation, self.request_with_cancellation(p)).await
    }
}

/// Boxed future returned by the daemon-forwarding seam.
#[cfg(unix)]
type ForwardFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<Result<String, McpError>>> + Send + 'a>,
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
    for<'a> fn(&'a khive_runtime::DaemonRequestFrame, Option<Vec<String>>) -> ForwardFuture<'a>;

/// Adapts the real `forward_or_spawn_with_config_and_packs` to the `ForwardFnPtr`
/// signature. A pure pass-through — the `Some`/`None` decision already
/// happened at the call site — so this boundary carries no logic a test
/// spy could fail to observe.
#[cfg(unix)]
fn forward_or_spawn_boxed(
    frame: &khive_runtime::DaemonRequestFrame,
    packs: Option<Vec<String>>,
) -> ForwardFuture<'_> {
    Box::pin(async move {
        crate::daemon::forward_or_spawn_with_config_and_packs(frame, None, None, packs.as_deref())
            .await
    })
}

impl KhiveMcpServer {
    async fn request_with_cancellation(&self, p: RequestParams) -> Result<String, McpError> {
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
        // Parse before the daemon decision. The daemon protocol's historical
        // error channel is string-only, so forwarding malformed DSL would turn
        // `invalid_params` plus its structured `parse-error` reason into an
        // untyped `internal_error` on the bridge back to MCP. This cheap,
        // side-effect-free preflight keeps the local and warm-daemon surfaces on
        // the same RPC contract; valid requests are still parsed authoritatively
        // inside `dispatch_request_inner` at the dispatch seam.
        if let Err(error) = parse_request(&p.ops) {
            return Err(dsl_err_to_mcp(error));
        }

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
        #[cfg(unix)]
        if p.save_to.is_none() {
            let frame = self.wire_daemon_frame(&p);
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
            let forwarded = forward_fn(&frame, Some(resolved_packs));
            tokio::pin!(forwarded);
            let forwarded = tokio::select! {
                result = &mut forwarded => result,
                _ = khive_storage::wait_for_request_read_cancellation() => {
                    return Err(McpError::internal_error("request cancelled", None));
                }
            };
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
        }
        self.dispatch_request_wire(p).await
    }
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
    json!({
        "ok": false,
        "tool": tool,
        "error": format!(
            "batch response budget of {response_budget} serialized bytes exceeded"
        ),
    })
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
                    json!({ "ok": false, "tool": op.tool, "error": error_msg })
                } else {
                    json!({ "ok": false, "tool": op.tool, "aborted": true })
                }
            })
            .collect(),
        ExecutionMode::Single | ExecutionMode::Parallel => parsed
            .ops
            .iter()
            .map(|op| json!({ "ok": false, "tool": op.tool, "error": error_msg }))
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
            ops: String::new(),
            presentation,
            presentation_per_op: None,
            save_to: None,
            format,
            format_per_op: None,
            request_id: None,
        };
        let dispatch = Box::pin(self.dispatch_parsed_request_inner_scoped(
            p,
            parsed,
            false,
            None,
            DispatchOrigin::Local,
            policy,
        ));
        khive_storage::scope_request_read_deadline(request_read_timeout(), dispatch).await
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
        // `dispatch_request_inner_scoped` is the complete parse/dispatch/render
        // pipeline. Keep that large generator behind one pointer before handing
        // it to the generic task-local scope: otherwise the scope embeds the
        // pipeline in every MCP, local-exec, and replay request future. LLVM
        // coverage instrumentation amplifies the resulting poll stack enough to
        // overflow Tokio's normal worker stack even for unrelated small verbs.
        let dispatch = Box::pin(self.dispatch_request_inner_scoped(
            p,
            from_wire,
            identity,
            origin,
            strict_refusals,
        ));
        khive_storage::scope_request_read_deadline(request_read_timeout(), dispatch).await
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
        let presentation = parse_presentation_mode(p.presentation.as_deref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let presentation_per_op: Option<Vec<Option<PresentationMode>>> =
            if let Some(per_op_strs) = p.presentation_per_op {
                let mut modes = Vec::with_capacity(per_op_strs.len());
                for s in per_op_strs {
                    let mode = match s.as_deref() {
                        None => None,
                        Some(v) => Some(
                            parse_presentation_mode(Some(v))
                                .map_err(|e| McpError::invalid_params(e, None))?,
                        ),
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
            .map_err(|e| McpError::invalid_params(e, None))?
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
                                .map_err(|e| McpError::invalid_params(e, None))?
                                .unwrap_or(batch_format),
                        ),
                    };
                    fmts.push(fmt);
                }
                Some(fmts)
            } else {
                None
            };

        let mut result = self
            .run_parsed(
                parsed.ops,
                parsed.mode,
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

        if let Some(path_str) = save_to {
            let path = std::path::Path::new(&path_str);
            // `from_wire` gates the destination policy: the agent-facing MCP
            // `request` tool (`from_wire = true`) restricts `save_to` to the
            // allowed export root; the trusted operator CLI path
            // (`kkernel exec --save-file`, `from_wire = false`) is unrestricted,
            // matching its documented "write anywhere" behavior.
            let manifest = crate::save_sink::write_and_manifest(&result, path, from_wire)
                .map_err(|e| McpError::internal_error(format!("save_to: {e}"), None))?;
            // Manifests are always compact JSON regardless of format (lossless metadata).
            return serde_json::to_string(&manifest)
                .map_err(|e| McpError::internal_error(format!("serialize manifest: {e}"), None));
        }

        // Apply per-op format rendering (ADR-078 §8.4 and §9).
        Ok(render_result(
            result,
            batch_format,
            &format_per_op,
            presentation,
            &presentation_per_op,
            &self.registry,
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

fn dsl_err_to_mcp(e: DslError) -> McpError {
    McpError::invalid_params(
        e.to_string(),
        Some(json!({ "reason": RefusalReason::ParseError.as_str() })),
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

/// Render the `run_parsed` result envelope using per-op format dispatch (ADR-078 §8.4).
///
/// For each op entry in `results`:
/// - If `ok=false` (error entry): always compact JSON, never reformatted (§8.2).
/// - If `ok=true`: resolve per-op format (per_op_formats[i] → batch_format) and
///   per-op presentation (presentation_per_op[i] → batch presentation, then the
///   verb's AlwaysVerbose policy forces Verbose), apply `render_format` to the
///   `result` payload with the effective presentation so that both
///   `presentation_per_op=["verbose"]` and AlwaysVerbose verbs (including
///   strict feedback, delivery-correlation acknowledgements, and durable receipt
///   responses) correctly skip the redundancy-drop
///   pre-pass (ADR-078 §7 + §8.4; mirrors `run_parsed`).
///
/// The outer envelope (`{results:[...], summary:{...}}`) is always compact JSON (§8.4).
/// Daemon-served responses are rendered before fitting. If the rendered envelope
/// exceeds the frame allowance, entries fall back to compact JSON before payload
/// details are omitted. Local dispatch has no daemon-frame allowance and returns
/// the requested representation without fitting. Every daemon fit decision
/// serializes the actual response-frame shape so JSON string escaping is included.
fn render_result(
    value: serde_json::Value,
    batch_format: OutputFormat,
    format_per_op: &Option<Vec<Option<OutputFormat>>>,
    presentation: PresentationMode,
    presentation_per_op: &Option<Vec<Option<PresentationMode>>>,
    registry: &VerbRegistry,
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
                        registry,
                    )
                })
                .collect();
            let out_map = match daemon_frame_config_id {
                Some(config_id) => {
                    fit_rendered_batch_envelope(map, results, out_results, config_id)
                }
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
    serde_json::to_string(&json!({
        "ok": false,
        "error": "response payload omitted because it exceeds the daemon frame budget",
    }))
    .expect("static frame-budget error is serializable")
}

fn render_batch_entry(
    index: usize,
    entry: &Value,
    batch_format: OutputFormat,
    format_per_op: &Option<Vec<Option<OutputFormat>>>,
    presentation: PresentationMode,
    presentation_per_op: &Option<Vec<Option<PresentationMode>>>,
    registry: &VerbRegistry,
) -> Value {
    let per_op_format = format_per_op
        .as_ref()
        .and_then(|formats| formats.get(index))
        .and_then(|format| *format)
        .unwrap_or(batch_format);
    let is_ok = entry.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !is_ok || per_op_format == OutputFormat::Json {
        return entry.clone();
    }

    let base_presentation = presentation_per_op
        .as_ref()
        .and_then(|modes| modes.get(index))
        .and_then(|mode| *mode)
        .unwrap_or(presentation);
    let effective_presentation = match entry.get("tool").and_then(Value::as_str) {
        Some(tool)
            if registry.presentation_policy_for(tool) == VerbPresentationPolicy::AlwaysVerbose =>
        {
            PresentationMode::Verbose
        }
        _ => base_presentation,
    };
    let Some(result) = entry.get("result") else {
        return entry.clone();
    };
    let mut rendered_entry = entry.clone();
    if let Value::Object(ref mut fields) = rendered_entry {
        fields.insert(
            "result".to_string(),
            Value::String(render_format(
                result.clone(),
                per_op_format,
                effective_presentation,
            )),
        );
    }
    rendered_entry
}

fn fit_rendered_batch_envelope(
    map: &serde_json::Map<String, Value>,
    compact_results: &[Value],
    mut out_results: Vec<Value>,
    served_config_id: &str,
) -> serde_json::Map<String, Value> {
    let mut out_map = map.clone();
    out_map.insert(
        "results".to_string(),
        serde_json::Value::Array(out_results.clone()),
    );
    if response_value_fits_daemon_frame(
        &serde_json::Value::Object(out_map.clone()),
        served_config_id,
    ) {
        return out_map;
    }

    let rendered_frame_bytes = response_value_daemon_frame_len(
        &serde_json::Value::Object(out_map.clone()),
        served_config_id,
    );
    let mut compact_fallbacks: Vec<(usize, usize)> = compact_results
        .iter()
        .zip(&out_results)
        .enumerate()
        .filter_map(|(index, (compact, rendered))| {
            if compact == rendered {
                return None;
            }
            let mut candidate_results = out_results.clone();
            candidate_results[index] = compact.clone();
            let mut candidate_map = out_map.clone();
            candidate_map.insert("results".to_string(), Value::Array(candidate_results));
            let compact_frame_bytes =
                response_value_daemon_frame_len(&Value::Object(candidate_map), served_config_id);
            (compact_frame_bytes < rendered_frame_bytes)
                .then_some((index, rendered_frame_bytes - compact_frame_bytes))
        })
        .collect();
    compact_fallbacks.sort_unstable_by_key(|&(_, saved_bytes)| std::cmp::Reverse(saved_bytes));
    for (index, _) in compact_fallbacks {
        out_results[index] = compact_results[index].clone();
        out_map.insert("results".to_string(), Value::Array(out_results.clone()));
        if response_value_fits_daemon_frame(&Value::Object(out_map.clone()), served_config_id) {
            return out_map;
        }
    }

    let mut by_size: Vec<(usize, usize)> = out_results
        .iter()
        .enumerate()
        .map(|(index, entry)| (index, serialized_response_len(entry)))
        .collect();
    by_size.sort_unstable_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
    for (index, _) in by_size {
        out_results[index] = frame_budget_omission(&compact_results[index]);
        out_map.insert(
            "results".to_string(),
            serde_json::Value::Array(out_results.clone()),
        );
        if response_value_fits_daemon_frame(
            &serde_json::Value::Object(out_map.clone()),
            served_config_id,
        ) {
            break;
        }
    }
    out_map
}

fn frame_budget_omission(entry: &Value) -> Value {
    let ok = entry.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let mut omitted = serde_json::Map::new();
    // `reason` is stable machine metadata, not payload detail. It is tiny and
    // must survive even when a large result/error body is omitted to fit the
    // daemon frame.
    for key in [
        "ok",
        "tool",
        "usage",
        "aborted",
        "reason",
        "status",
        "partial",
        "missing_backends",
        "backend_errors",
        "backend_errors_truncated",
        "backend_errors_omitted",
        "advisories",
    ] {
        if let Some(value) = entry.get(key) {
            omitted.insert(key.to_string(), value.clone());
        }
    }
    if ok {
        omitted.insert(
            "result_omitted".to_string(),
            json!("operation succeeded; result omitted because the response frame budget was exceeded"),
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
        } else {
            omitted.insert(
                "error".to_string(),
                json!("operation failed; error details omitted because the response frame budget was exceeded"),
            );
        }
    }
    Value::Object(omitted)
}

fn serialized_response_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("serde_json::Value is always serializable")
        .len()
}

fn serialize_response_value(value: &Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is always serializable")
}

fn response_value_fits_daemon_frame(value: &Value, served_config_id: &str) -> bool {
    response_value_daemon_frame_len(value, served_config_id)
        <= khive_runtime::daemon::MAX_FRAME_BYTES
}

fn response_value_daemon_frame_len(value: &Value, served_config_id: &str) -> usize {
    rendered_response_daemon_frame_len(&serialize_response_value(value), served_config_id)
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

/// Build the `initialize` instructions string from the verb catalog and the
/// loaded builtin pack names. Extracted from [`ServerHandler::get_info`] so
/// the docs-pointer section (#594) is unit-testable without standing up a
/// full server.
fn build_instructions(catalog: &str, builtins: &str) -> String {
    format!(
        "khive — request-only MCP surface. One tool, `request`, \
         dispatches verbs through the loaded pack registry. Configure packs via \
         KHIVE_PACKS or --pack (built-ins: {builtins}). Verbs registered on this \
         server:\n{catalog}\nFor detailed usage of each verb, see the corresponding \
         plugin's SKILL.md files.\n\
         Docs: https://ohdearquant.github.io/khive/ (hosted) or docs/*.md in the repo \
         checkout. Treat the live verb catalog above and help=true as authoritative over \
         cached/training knowledge. Config/backend issues: docs/configuration.md. Usage \
         patterns: docs/guide/tips-and-tricks.md."
    )
}

#[tool_handler]
impl ServerHandler for KhiveMcpServer {
    fn get_info(&self) -> ServerInfo {
        let catalog = self.verb_catalog();
        let builtins = builtin_pack_names().join(", ");
        let instructions = build_instructions(&catalog, &builtins);
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
mod tests {
    use super::*;
    use khive_runtime::Namespace;
    use khive_storage::{EventFilter, PageRequest};
    use serial_test::serial;

    #[derive(Clone, Default)]
    struct SearchCapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SearchCapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured search log mutex poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SearchCapturedLog {
        type Writer = SearchCapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl SearchCapturedLog {
        fn contents(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("captured search log mutex poisoned")
                    .clone(),
            )
            .expect("captured search logs are UTF-8")
        }
    }

    #[cfg(unix)]
    use khive_storage::test_support::freeze_snapshot_sidecars;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn dsl_parse_errors_carry_stable_reason_data() {
        let parse_error = parse_request("stats(").expect_err("input must be malformed");
        let error = dsl_err_to_mcp(parse_error);
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(
            error.data.as_ref().and_then(|data| data["reason"].as_str()),
            Some("parse-error")
        );
    }

    #[tokio::test]
    async fn wire_dispatch_retains_raw_one_mib_input_limit() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
        let params = RequestParams {
            ops: json!({
                "tool": "stats",
                "args": {"payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1)},
            })
            .to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        };

        let error = server
            .dispatch_request_wire(params)
            .await
            .expect_err("the public wire path must reject raw ops above 1 MiB");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("ops input is"), "{error}");
        assert_eq!(
            error.data.as_ref().and_then(|data| data["reason"].as_str()),
            Some("parse-error")
        );
    }

    /// khive-oss#1941 regression seam: `request_with_forward` must pass
    /// THIS server's own resolved registry pack list to the daemon-forwarding
    /// seam as `Some(...)`, not `None` and not some other list. The spy's
    /// signature is `Option<Vec<String>>` — the exact shape
    /// `forward_or_spawn_with_config_and_packs` itself receives — because the
    /// `Some`/`None` decision is made at the shared call site in
    /// `request_with_forward` before `forward_fn` is invoked, not inside the
    /// production adapter (`forward_or_spawn_boxed`) that this spy replaces.
    /// That adapter is now a pure pass-through with no logic of its own, so
    /// this spy observes precisely what the real
    /// `forward_or_spawn_with_config_and_packs` call would receive. Two independent
    /// mutations must both redden this test:
    /// - swapping the production `forward_fn(&frame, Some(resolved_packs))`
    ///   call for `forward_fn(&frame, Some(Vec::new()))` — the restricted
    ///   two-pack registry built here (`kg`, `gtd`) no longer matches what
    ///   the spy records;
    /// - swapping that same call for `forward_fn(&frame, None)` — the spy
    ///   records `None` instead of `Some(vec!["kg", "gtd"])`.
    #[cfg(unix)]
    #[tokio::test]
    async fn restricted_registry_pack_list_reaches_forward_seam() {
        thread_local! {
            static SPY_CAPTURED_PACKS: std::cell::RefCell<Option<Option<Vec<String>>>> =
                const { std::cell::RefCell::new(None) };
        }

        fn spy_forward(
            _frame: &khive_runtime::DaemonRequestFrame,
            packs: Option<Vec<String>>,
        ) -> ForwardFuture<'_> {
            SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
            Box::pin(async {
                Some(Ok(json!({
                    "results": [{"ok": true, "tool": "stats", "result": {}}],
                    "summary": {"total": 1, "succeeded": 1, "failed": 0},
                })
                .to_string()))
            })
        }

        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "gtd".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime restricted to kg + gtd");
        let server =
            KhiveMcpServer::new(runtime).expect("server builds with restricted kg+gtd registry");
        SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

        let params = RequestParams {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        };

        let result = server.request_with_forward(params, spy_forward).await;

        assert!(
            result.is_ok(),
            "the spy-forwarded dispatch must succeed: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
            Some(Some(vec!["kg".to_string(), "gtd".to_string()])),
            "the server's own resolved registry pack set must reach the daemon-forwarding \
             seam as Some(...) so a daemon this call spawns serves the same packs this \
             process registered"
        );
    }

    /// Adapter-boundary regression: `restricted_registry_pack_list_reaches_forward_seam`
    /// above proves the derivation site (`Some(resolved_packs)` in
    /// `request_with_forward`) folds the right pack list, but its `spy_forward`
    /// stands in for `forward_or_spawn_boxed` itself, so it never executes the
    /// adapter's own `packs.as_deref()` conversion at
    /// `crate::daemon::forward_or_spawn_with_config_and_packs`'s call site. This test
    /// instead drives `request_with_cancellation` — the real production entry
    /// point, which always calls the real `forward_or_spawn_boxed` — and
    /// observes the argument via a one-shot capture hook armed at the entry of
    /// `forward_or_spawn_with_config_and_packs` itself (`crate::daemon::test_forward_seam`),
    /// past both the derivation site AND the adapter conversion. Changing the
    /// adapter's `packs.as_deref()` argument to `None` reddens this test.
    #[cfg(unix)]
    #[tokio::test]
    async fn restricted_registry_pack_list_reaches_real_adapter_boundary() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "gtd".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime restricted to kg + gtd");
        let server =
            KhiveMcpServer::new(runtime).expect("server builds with restricted kg+gtd registry");

        crate::daemon::test_forward_seam::arm();

        let params = RequestParams {
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        };

        let result = server.request_with_cancellation(params).await;

        assert!(
            result.is_ok(),
            "the intercepted dispatch through the real production entry point must \
             succeed: {result:?}"
        );
        assert_eq!(
            crate::daemon::test_forward_seam::take_captured(),
            Some(Some(vec!["kg".to_string(), "gtd".to_string()])),
            "the real forward_or_spawn_boxed adapter must convert the server's resolved \
             registry pack set into Some(&packs) at the forward_or_spawn_with_config_and_packs \
             call boundary"
        );
    }

    /// ADR-118's serving toggle is baked into a runtime. Opposite policies
    /// must never share one warm daemon even when every `RuntimeConfig` field
    /// is otherwise identical.
    #[test]
    fn config_id_differs_when_ann_fresh_tail_policy_differs() {
        let config = RuntimeConfig::no_embeddings();

        assert_ne!(
            compute_config_id_with_ann_fresh_tail(&config, None, true),
            compute_config_id_with_ann_fresh_tail(&config, None, false),
            "opposite fresh-tail policies must not share one warm daemon"
        );
    }

    /// `gtd.assign` anchors a date-only `due` through `display_timezone` and
    /// PERSISTS the resulting instant, so a warm daemon reused across two
    /// runtimes differing only in that field writes an instant wrong by the
    /// offset between the zones. Identity must separate them.
    #[test]
    fn config_id_differs_when_display_timezone_differs() {
        // One base, cloned, for the reason spelled out on the test below — and
        // it matters MORE here. This assertion is `assert_ne!`, so the shared
        // environment racing between two constructor calls would make it pass
        // by producing two different `db_path`s, which is a pass that would
        // survive deleting the fix this test exists to hold.
        let base = RuntimeConfig::no_embeddings();
        let utc = RuntimeConfig {
            display_timezone: "UTC".parse().expect("UTC is a known IANA zone"),
            events_split: None,
            ..base.clone()
        };
        let new_york = RuntimeConfig {
            display_timezone: "America/New_York".parse().expect("known IANA zone"),
            events_split: None,
            ..base
        };

        assert_ne!(
            compute_config_id_with_runtime_policies(&utc, None, true, false),
            compute_config_id_with_runtime_policies(&new_york, None, true, false),
            "runtimes differing only in display_timezone must not share one warm daemon: \
             a reused daemon would anchor date-only due values in the wrong zone and \
             persist the wrong instant"
        );
    }

    /// The other direction, so the assertion above cannot pass for an
    /// incidental reason: identical zones must still collapse to one identity.
    #[test]
    fn config_id_matches_when_display_timezone_matches() {
        // ONE base, cloned — not two constructor calls. `RuntimeConfig::default`
        // reads `HOME` to build `db_path`, and `db_path` is folded into the id,
        // so two calls read that variable at two different instants. Other
        // tests in this binary set and restore `HOME` around their own work
        // (`config_id_matches_for_tilde_and_equivalent_absolute_db_override` is
        // one, and it matches the same `config_id` filter), and tests run in
        // parallel threads against one process-global environment. A mutation
        // landing between the two calls gave the two configs different paths
        // and reddened this test for a reason that has nothing to do with
        // timezones. Cloning one base removes the window: whatever `HOME` is,
        // both sides read the same one.
        let base = RuntimeConfig::no_embeddings();
        let a = RuntimeConfig {
            display_timezone: "America/New_York".parse().expect("known IANA zone"),
            events_split: None,
            ..base.clone()
        };
        let b = RuntimeConfig {
            display_timezone: "America/New_York".parse().expect("known IANA zone"),
            events_split: None,
            ..base
        };

        assert_eq!(
            compute_config_id_with_runtime_policies(&a, None, true, false),
            compute_config_id_with_runtime_policies(&b, None, true, false),
            "identical runtimes must share one warm daemon"
        );
    }

    #[test]
    fn config_id_treats_absent_and_explicit_default_blob_hydration_budget_as_equivalent() {
        use khive_runtime::engine_config::RuntimeSectionConfig;
        use khive_runtime::{runtime_config_from_khive_config, KhiveConfig};

        let base = RuntimeConfig::no_embeddings();
        let absent = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
        let explicit = runtime_config_from_khive_config(
            &KhiveConfig {
                runtime: RuntimeSectionConfig {
                    blob_hydration_bytes: Some(base.blob_hydration_bytes),
                    ..RuntimeSectionConfig::default()
                },
                ..KhiveConfig::default()
            },
            base,
        );

        assert_eq!(
            compute_config_id_with_ann_fresh_tail(&absent, None, true),
            compute_config_id_with_ann_fresh_tail(&explicit, None, true)
        );
    }

    #[test]
    fn config_id_differs_when_resolved_blob_hydration_budget_differs() {
        let config = RuntimeConfig::no_embeddings();
        let mut changed = config.clone();
        changed.blob_hydration_bytes /= 2;

        assert_ne!(
            compute_config_id_with_ann_fresh_tail(&config, None, true),
            compute_config_id_with_ann_fresh_tail(&changed, None, true),
            "different resident-blob admission budgets must not share one warm daemon"
        );
    }

    async fn observed_batch_entry(
        _index: usize,
        delay_ms: u64,
        entry: Value,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    ) -> Value {
        let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max_in_flight.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
        entry
    }

    fn batch_task<F>(index: usize, future: F) -> BatchTask<F>
    where
        F: Future<Output = Value>,
    {
        BatchTask {
            index,
            tool: "probe".to_string(),
            future,
        }
    }

    struct LargeResultPack;

    impl khive_types::Pack for LargeResultPack {
        const NAME: &'static str = "large-result-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
            name: "large_result",
            description: "returns a caller-sized test result",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for LargeResultPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            if let Some(bytes) = params
                .get("table_bytes")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
            {
                let payload = "x".repeat(bytes);
                return Ok(json!([
                    {"payload": payload},
                    {"payload": payload},
                ]));
            }
            let bytes = params
                .get("bytes")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .expect("test supplies a valid byte count");
            Ok(json!("x".repeat(bytes)))
        }
    }

    struct SlowSqlReadPack {
        bridge: khive_db::SqlBridge,
    }

    impl khive_types::Pack for SlowSqlReadPack {
        const NAME: &'static str = "slow-sql-read-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
            khive_runtime::HandlerDef {
                name: "slow_sql_read",
                description: "runs SQLite work until the request read deadline interrupts it",
                visibility: khive_runtime::Visibility::Verb,
                category: khive_runtime::VerbCategory::Assertive,
                params: &[],
            },
            khive_runtime::HandlerDef {
                name: "pending_read_phase",
                description: "waits until the request read deadline interrupts it",
                visibility: khive_runtime::Visibility::Verb,
                category: khive_runtime::VerbCategory::Assertive,
                params: &[],
            },
        ];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for SlowSqlReadPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            use khive_storage::{SqlAccess, SqlStatement};

            if verb == "pending_read_phase" {
                khive_storage::await_request_read_phase(
                    "outer-deadline-probe",
                    std::future::pending::<()>(),
                )
                .await?;
                return Ok(Value::Null);
            }

            let mut reader = self.bridge.reader().await?;
            let rows = reader
                .query_all(SqlStatement {
                    sql: "WITH RECURSIVE numbers(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                          ) SELECT SUM(a.value * b.value * c.value) \
                          FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
                        .into(),
                    params: vec![],
                    label: Some("canonical-dispatch-deadline-probe".into()),
                })
                .await?;
            Ok(json!({ "rows": rows.len() }))
        }
    }

    fn slow_sql_read_test_server() -> KhiveMcpServer {
        let pool = Arc::new(
            khive_db::ConnectionPool::new(khive_db::PoolConfig::default())
                .expect("in-memory SQLite pool"),
        );
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SlowSqlReadPack {
            bridge: khive_db::SqlBridge::new(pool, false),
        });
        KhiveMcpServer::from_registry(builder.build().expect("slow-SQL test registry"))
    }

    #[test]
    fn canonical_request_deadline_wrapper_does_not_embed_dispatch_pipeline() {
        // Construct the generators on an explicitly roomy stack so this
        // regression reports their footprint instead of reproducing the LLVM
        // coverage stack abort it is meant to prevent. Nothing is polled, so an
        // empty registry is sufficient and the test performs no storage work.
        let (pipeline_bytes, wrapper_bytes) = std::thread::Builder::new()
            .name("request-dispatch-future-footprint".to_string())
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let server = KhiveMcpServer::from_registry(
                    VerbRegistryBuilder::new()
                        .build()
                        .expect("empty test registry"),
                );
                let pipeline_bytes = std::mem::size_of_val(&server.dispatch_request_inner_scoped(
                    RequestParams {
                        ops: "stats()".to_string(),
                        ..Default::default()
                    },
                    false,
                    None,
                    DispatchOrigin::Local,
                    false,
                ));
                let wrapper_bytes =
                    std::mem::size_of_val(&server.dispatch_request_inner_with_strict_refusals(
                        RequestParams {
                            ops: "stats()".to_string(),
                            ..Default::default()
                        },
                        false,
                        None,
                        DispatchOrigin::Local,
                        false,
                    ));
                (pipeline_bytes, wrapper_bytes)
            })
            .expect("spawn request future footprint measurement")
            .join()
            .expect("request future footprint measurement panicked");

        assert!(
            wrapper_bytes.saturating_mul(2) < pipeline_bytes,
            "the canonical deadline wrapper must keep the dispatch pipeline behind a pointer: \
             wrapper={wrapper_bytes}B pipeline={pipeline_bytes}B"
        );
    }

    fn assert_request_read_timed_out(response: &str) {
        let envelope: Value = serde_json::from_str(response).expect("JSON response envelope");
        assert_eq!(envelope["summary"]["failed"], 1, "{envelope}");
        let error = envelope["results"][0]["error"].to_string().to_lowercase();
        assert!(
            error.contains("timeout") || error.contains("timed out"),
            "request read must fail as a timeout: {envelope}"
        );
    }

    // These dispatch-layer regressions compose with khive-db's
    // `request_deadline_interrupts_statement_without_outer_timeout`, which
    // separately proves that the same SQL bridge deadline sees SQLite VM
    // progress and stops it before returning. Do not assert that paused Tokio
    // time reaches the full default here: the SQLite wall-clock/progress
    // backstop may win first under instrumentation. The typed timeout proves
    // the canonical scope reached the database; the test below independently
    // pins absolute Tokio-deadline ordering.
    #[tokio::test(start_paused = true)]
    async fn local_exec_dispatch_installs_the_default_request_read_deadline() {
        let server = slow_sql_read_test_server();
        let expected = request_read_timeout();
        let response = tokio::time::timeout(
            expected
                .saturating_add(khive_db::sqlite_interrupt_grace_from_env())
                .saturating_add(Duration::from_secs(1)),
            server.dispatch_request_local_for_exec(
                RequestParams {
                    ops: "slow_sql_read()".to_string(),
                    ..Default::default()
                },
                false,
            ),
        )
        .await
        .expect("canonical local request-read deadline was never installed")
        .expect("deadline is a per-op failure, not an RPC failure");

        assert_request_read_timed_out(&response);
    }

    #[tokio::test(start_paused = true)]
    async fn replay_dispatch_installs_the_default_request_read_deadline() {
        let server = slow_sql_read_test_server();
        let expected = request_read_timeout();
        let response = tokio::time::timeout(
            expected
                .saturating_add(khive_db::sqlite_interrupt_grace_from_env())
                .saturating_add(Duration::from_secs(1)),
            server.dispatch_request_replay_as(
                RequestParams {
                    ops: "slow_sql_read()".to_string(),
                    ..Default::default()
                },
                "local",
                None,
            ),
        )
        .await
        .expect("canonical replay request-read deadline was never installed")
        .expect("deadline is a per-op failure, not an RPC failure");

        assert_request_read_timed_out(&response);
    }

    #[tokio::test(start_paused = true)]
    async fn canonical_dispatch_preserves_an_earlier_outer_deadline() {
        let server = slow_sql_read_test_server();
        let outer = Duration::from_millis(50);
        let default = request_read_timeout();
        let started = tokio::time::Instant::now();
        let response = khive_storage::scope_request_read_deadline(
            outer,
            server.dispatch_request_local(RequestParams {
                ops: "pending_read_phase()".to_string(),
                ..Default::default()
            }),
        )
        .await
        .expect("deadline is a per-op failure, not an RPC failure");
        let elapsed = tokio::time::Instant::now().duration_since(started);

        assert!(
            elapsed >= outer,
            "outer deadline fired too early: {elapsed:?}"
        );
        assert!(
            elapsed < default,
            "canonical dispatch renewed an earlier outer deadline: {elapsed:?}"
        );
        assert_request_read_timed_out(&response);
    }

    /// Receipt-only stand-in for the Git pack. Two operations can return
    /// distinguishable reports for the same project without touching a git
    /// repository, which lets the request-layer test prove that `request_id`
    /// groups receipts but does not uniquely identify one operation.
    struct RequestGroupDigestPack {
        project_id: uuid::Uuid,
    }

    impl khive_types::Pack for RequestGroupDigestPack {
        const NAME: &'static str = "request-group-digest-test";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
            name: "git.digest",
            description: "return a request-group receipt fixture",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Commissive,
            params: &[khive_runtime::ParamDef {
                name: "marker",
                param_type: "string",
                required: true,
                description: "distinguishes reports in one request group",
                resolution_mode: khive_types::IdResolutionMode::NotApplicable,
            }],
        }];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for RequestGroupDigestPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            <Self as khive_types::Pack>::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(json!({
                "project_id": self.project_id,
                "marker": params.get("marker").cloned().unwrap_or(Value::Null),
                "done": true,
            }))
        }
    }

    fn request_group_digest_test_server() -> (
        KhiveMcpServer,
        Arc<dyn khive_storage::EventStore>,
        uuid::Uuid,
    ) {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");
        let store = runtime.events(&token).expect("event store");
        let project_id = uuid::Uuid::new_v4();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(RequestGroupDigestPack { project_id });
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("request-group registry");
        (KhiveMcpServer::from_registry(registry), store, project_id)
    }

    async fn assert_request_group_receipts(ops: &str) {
        let (server, store, project_id) = request_group_digest_test_server();
        let response = server
            .dispatch_request_local(RequestParams {
                ops: ops.to_string(),
                request_id: Some(16_470),
                ..Default::default()
            })
            .await
            .expect("grouped digest request succeeds");
        let envelope: Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
        let mut returned_by_marker = std::collections::HashMap::new();
        for entry in envelope["results"]
            .as_array()
            .expect("batch/chain response results")
        {
            let result = entry.get("result").expect("successful result");
            let marker = result["marker"].as_str().expect("returned marker");
            let receipt_id = result["receipt_id"]
                .as_str()
                .expect("returned receipt_id remains a string");
            uuid::Uuid::parse_str(receipt_id)
                .expect("default MCP presentation must retain the full receipt UUID");
            assert!(
                returned_by_marker
                    .insert(marker.to_string(), result.clone())
                    .is_none(),
                "markers are unique"
            );
        }

        let page = store
            .query_events(
                EventFilter {
                    verbs: vec!["git.digest".to_string()],
                    ..EventFilter::default()
                },
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query grouped receipts");
        assert_eq!(page.items.len(), 2, "one receipt per successful operation");
        assert!(page.items.iter().all(|event| {
            event.target_id == Some(project_id)
                && event.payload["resource"]["request_id"] == json!(16_470)
        }));

        let mut markers: Vec<&str> = page
            .items
            .iter()
            .map(|event| {
                assert_eq!(
                    event.payload["result"]["receipt_id"],
                    json!(event.id),
                    "receipt_id is the operation-unique selector"
                );
                let marker = event.payload["result"]["marker"].as_str().expect("marker");
                let returned = returned_by_marker
                    .remove(marker)
                    .expect("every stored marker was returned");
                assert_eq!(
                    returned, event.payload["result"],
                    "default MCP output must exactly equal the durable receipt result"
                );
                marker
            })
            .collect();
        markers.sort_unstable();
        assert_eq!(markers, vec!["first", "second"]);
        assert!(returned_by_marker.is_empty());
        assert_ne!(page.items[0].id, page.items[1].id);
    }

    #[tokio::test]
    async fn duplicate_digest_batch_and_chain_share_request_group_but_keep_distinct_receipts() {
        assert_request_group_receipts(
            r#"[git.digest(marker="first"), git.digest(marker="second")]"#,
        )
        .await;
        assert_request_group_receipts(
            r#"git.digest(marker="first") | git.digest(marker="second")"#,
        )
        .await;
    }

    async fn dispatch_large_result_through_daemon(
        server: &KhiveMcpServer,
        ops: String,
        format: Option<String>,
    ) -> String {
        khive_runtime::daemon::DaemonDispatch::dispatch(
            server, ops, None, None, format, None, false, None,
        )
        .await
        .expect("daemon dispatch")
    }

    fn large_result_test_server() -> KhiveMcpServer {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LargeResultPack);
        KhiveMcpServer::from_registry(builder.build().expect("test registry"))
    }

    fn typed_test_op(tool: &str, args: Value) -> TypedJsonOp {
        let Value::Object(args) = args else {
            panic!("typed test args must be an object")
        };
        TypedJsonOp {
            tool: tool.to_string(),
            args,
        }
    }

    #[tokio::test]
    async fn typed_serial_dispatch_retains_full_batch_write_conflict_preflight() {
        let ops = vec![
            typed_test_op("update", json!({"id": "same-id", "name": "new"})),
            typed_test_op("delete", json!({"id": "same-id"})),
        ];
        let server = large_result_test_server();
        let parallel: Value = serde_json::from_str(
            &server
                .dispatch_typed_json_batch_local_for_exec(
                    ops.clone(),
                    Some("verbose".to_string()),
                    Some("json".to_string()),
                    false,
                )
                .await
                .expect("parallel typed dispatch"),
        )
        .expect("parallel response JSON");
        let serial: Value = serde_json::from_str(
            &server
                .dispatch_typed_json_batch_serial_local_for_exec(
                    ops,
                    Some("verbose".to_string()),
                    Some("json".to_string()),
                    false,
                )
                .await
                .expect("serial typed dispatch"),
        )
        .expect("serial response JSON");

        assert_eq!(serial["summary"], parallel["summary"]);
        assert_eq!(serial["status"], parallel["status"]);
        for (serial_row, parallel_row) in serial["results"]
            .as_array()
            .expect("serial rows")
            .iter()
            .zip(parallel["results"].as_array().expect("parallel rows"))
        {
            assert_eq!(serial_row["ok"], false);
            assert_eq!(serial_row["tool"], parallel_row["tool"]);
            assert_eq!(serial_row["error"], parallel_row["error"]);
            assert!(serial_row["error"]
                .as_str()
                .expect("conflict error")
                .contains("writes overlap"));
        }
    }

    #[tokio::test]
    async fn typed_serial_dispatch_retains_one_aggregate_response_budget() {
        let result_bytes = BATCH_RESPONSE_BUDGET_BYTES / 3 - 4096;
        let ops: Vec<TypedJsonOp> = (0..12)
            .map(|_| typed_test_op("large_result", json!({"bytes": result_bytes})))
            .collect();
        let server = large_result_test_server();
        let parallel: Value = serde_json::from_str(
            &server
                .dispatch_typed_json_batch_local_for_exec(
                    ops.clone(),
                    Some("verbose".to_string()),
                    Some("json".to_string()),
                    false,
                )
                .await
                .expect("parallel typed dispatch"),
        )
        .expect("parallel response JSON");
        let serial: Value = serde_json::from_str(
            &server
                .dispatch_typed_json_batch_serial_local_for_exec(
                    ops,
                    Some("verbose".to_string()),
                    Some("json".to_string()),
                    false,
                )
                .await
                .expect("serial typed dispatch"),
        )
        .expect("serial response JSON");

        let serial_rows = serial["results"].as_array().expect("serial rows");
        let first_serial_budget_error = serial_rows
            .iter()
            .position(|row| {
                row["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("batch response budget"))
            })
            .expect("serial tail must contain canonical budget errors");
        assert!(
            first_serial_budget_error >= 3 && first_serial_budget_error < serial_rows.len(),
            "serial dispatch must spend one aggregate budget, not reset it per op"
        );
        assert!(serial_rows[..first_serial_budget_error]
            .iter()
            .all(|row| row["ok"] == json!(true)));
        assert!(serial_rows[first_serial_budget_error..].iter().all(|row| {
            row["ok"] == json!(false)
                && row["error"]
                    .as_str()
                    .is_some_and(|error| error.contains(&BATCH_RESPONSE_BUDGET_BYTES.to_string()))
        }));

        let parallel_budget_error = parallel["results"]
            .as_array()
            .expect("parallel rows")
            .iter()
            .find_map(|row| row["error"].as_str())
            .expect("parallel undispatched tail must use the same budget error");
        assert_eq!(
            serial_rows[first_serial_budget_error]["error"],
            json!(parallel_budget_error),
            "serial and default typed scheduling must single-source the budget/error contract"
        );
    }

    #[tokio::test]
    async fn bounded_batch_preserves_input_order() {
        let count = MAX_BATCH_CONCURRENCY + 3;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let futures = (0..count).map(|index| {
            batch_task(
                index,
                observed_batch_entry(
                    index,
                    (count - index) as u64,
                    json!({"ok": true, "tool": "probe", "result": {"index": index}}),
                    in_flight.clone(),
                    max_in_flight.clone(),
                ),
            )
        });

        let results = execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await;

        let indices: Vec<u64> = results
            .iter()
            .map(|entry| entry["result"]["index"].as_u64().expect("result index"))
            .collect();
        assert_eq!(indices, (0..count as u64).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn bounded_batch_enforces_aggregate_response_budget() {
        assert_eq!(
            BATCH_RESPONSE_BUDGET_BYTES,
            khive_runtime::daemon::MAX_FRAME_BYTES / 2
        );
        let count = MAX_BATCH_CONCURRENCY * 2;
        let budget = BATCH_RESPONSE_BUDGET_BYTES;
        let small = json!({
            "ok": true,
            "tool": "probe",
            "result": "x".repeat(budget / 4 - 128),
        });
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let futures = (0..count).map(|index| {
            let (delay_ms, entry) = if index < 2 {
                (index as u64, small.clone())
            } else if index == 2 {
                (
                    30,
                    json!({"ok": true, "tool": "probe", "result": "x".repeat(budget)}),
                )
            } else {
                (
                    60,
                    json!({"ok": true, "tool": "probe", "result": {"index": index}}),
                )
            };
            batch_task(
                index,
                observed_batch_entry(
                    index,
                    delay_ms,
                    entry,
                    in_flight.clone(),
                    max_in_flight.clone(),
                ),
            )
        });

        let results = tokio::time::timeout(
            Duration::from_secs(1),
            execute_bounded_batch(futures, budget, MAX_BATCH_CONCURRENCY),
        )
        .await
        .expect("started operations must settle promptly after a budget breach");
        let response = parallel_batch_envelope(results);

        assert_eq!(
            response["summary"],
            json!({"total": count, "succeeded": 10, "failed": count - 10, "aborted": 0})
        );
        assert_eq!(response["results"][0]["ok"], true);
        assert_eq!(response["results"][1]["ok"], true);
        assert_eq!(
            response["results"][2]["result"]
                .as_str()
                .expect("breaching result remains truthful")
                .len(),
            budget
        );
        for index in 3..10 {
            assert_eq!(response["results"][index]["ok"], true);
            assert_eq!(response["results"][index]["result"]["index"], index);
        }
        for entry in response["results"]
            .as_array()
            .expect("results")
            .iter()
            .skip(10)
        {
            assert_eq!(entry["ok"], false);
            let error = entry["error"].as_str().expect("budget error string");
            assert!(error.contains("batch response budget"));
            assert!(error.contains(&budget.to_string()));
        }
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        serde_json::to_vec(&response).expect("budgeted response must serialize");
    }

    #[tokio::test]
    async fn save_to_writes_full_results_without_inline_response_budgeting() {
        let server = large_result_test_server();
        let dir = tempfile::tempdir().expect("tempdir");
        let sink_path = dir.path().join("full-results.jsonl");
        let result_bytes = BATCH_RESPONSE_BUDGET_BYTES * 3 / 4;
        let response = server
            .dispatch_request_inner(
                RequestParams {
                    ops: format!(
                        "[large_result(bytes={result_bytes}), large_result(bytes={result_bytes})]"
                    ),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: Some(sink_path.to_string_lossy().into_owned()),
                    format: None,
                    format_per_op: None,
                    request_id: None,
                },
                false,
                None,
                DispatchOrigin::Local,
            )
            .await
            .expect("save_to dispatch");

        let manifest: Value = serde_json::from_str(&response).expect("manifest JSON");
        assert_eq!(manifest["rows"], 2);
        assert_eq!(manifest["summary"]["succeeded"], 2);
        let rows: Vec<Value> = std::fs::read_to_string(&sink_path)
            .expect("read JSONL")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
            .collect();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row["ok"], true);
            assert_eq!(
                row["result"].as_str().expect("full result string").len(),
                result_bytes
            );
        }
    }

    #[tokio::test]
    async fn local_dispatch_returns_result_larger_than_daemon_frame() {
        let server = large_result_test_server();
        let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
        let response = server
            .dispatch_request_local(RequestParams {
                ops: format!("large_result(bytes={result_bytes})"),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("local dispatch");

        let envelope: Value = serde_json::from_str(&response).expect("response envelope");
        assert_eq!(envelope["results"][0]["ok"], true);
        assert_eq!(
            envelope["results"][0]["result"]
                .as_str()
                .expect("full local result")
                .len(),
            result_bytes
        );
        assert!(envelope["results"][0].get("result_omitted").is_none());
    }

    #[tokio::test]
    async fn daemon_dispatch_degrades_result_larger_than_frame() {
        let server = large_result_test_server();
        let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
        let response = dispatch_large_result_through_daemon(
            &server,
            format!("large_result(bytes={result_bytes})"),
            None,
        )
        .await;

        let envelope: Value = serde_json::from_str(&response).expect("response envelope");
        assert_eq!(envelope["results"][0]["ok"], true);
        assert!(envelope["results"][0].get("result").is_none());
        assert!(envelope["results"][0].get("result_omitted").is_some());
        assert!(rendered_response_fits_daemon_frame(
            &response,
            &server.config_id
        ));
    }

    #[test]
    fn read_only_audit_advisory_decorates_success_but_not_help_or_error() {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_read_only_audit_store();
        let registry = builder.build().expect("registry builds");
        let mut response = json!({
            "results": [
                {"ok": true, "tool": "stats", "result": {"entities": 0}},
                {"ok": true, "tool": "list", "result": {"items": []}},
                {"ok": true, "tool": "stats", "result": {
                    "verb": "stats", "pack": "kg", "description": "help", "category": "assertive",
                    "identifier_resolution": {
                        "full_uuid": "canonical", "short_prefix": "prefix",
                        "parameter_rule": "strict"
                    }
                }},
                {"ok": false, "tool": "create", "error": "read-only"}
            ],
            "summary": {"total": 4, "succeeded": 3, "failed": 1, "aborted": 0},
            "status": "partial"
        });

        attach_audit_persistence_advisories(&mut response, &registry);

        assert_eq!(
            response["results"][0]["advisories"][0]["code"],
            khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY
        );
        assert_eq!(
            response["results"][1]["advisories"][0]["code"],
            khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY
        );
        assert!(response["results"][1]["result"]["items"].is_array());
        assert!(response["results"][2].get("advisories").is_none());
        assert!(response["results"][3].get("advisories").is_none());

        let omitted = frame_budget_omission(&response["results"][0]);
        assert!(
            omitted.get("advisories").is_some(),
            "frame-budget degradation must preserve the warning"
        );
    }

    #[test]
    fn frame_budget_omission_preserves_search_degradation_advisory() {
        let omitted = frame_budget_omission(&json!({
            "ok": true,
            "tool": "search",
            "result": "oversized",
            "status": "partial",
            "partial": true,
            "missing_backends": ["archive"],
            "backend_errors": {
                "archive": {
                    "kind": "backend_error",
                    "message": "storage unavailable"
                }
            },
        }));

        assert_eq!(omitted["ok"], json!(true));
        assert_eq!(omitted["status"], json!("partial"));
        assert_eq!(omitted["partial"], json!(true));
        assert_eq!(omitted["missing_backends"], json!(["archive"]));
        assert_eq!(
            omitted["backend_errors"]["archive"]["message"],
            json!("storage unavailable")
        );
        assert!(omitted.get("result").is_none());
        assert!(omitted.get("result_omitted").is_some());
    }

    #[test]
    fn backend_error_evidence_is_masked_and_bounded_before_preservation() {
        let secret = format!("storage auth token sk_live_{} failed", "a".repeat(32));
        let masked = bounded_backend_error_message(&secret);
        assert!(masked.contains("***MASKED***"));
        assert!(!masked.contains("sk_live_"));

        let backend_secret = format!("archive auth token sk_live_{}", "b".repeat(32));
        let (key, key_masked, key_truncated, key_chars) =
            bounded_backend_error_key(&backend_secret);
        assert!(key_masked);
        assert!(!key_truncated);
        assert_eq!(key_chars, backend_secret.chars().count());
        assert!(key.contains("***MASKED***"));
        assert!(!key.contains("sk_live_"));
        assert!(key.chars().count() <= MAX_BACKEND_ERROR_KEY_CHARS);

        let oversized = "x".repeat(MAX_BACKEND_ERROR_MESSAGE_CHARS + 100);
        let bounded = bounded_backend_error_message(&oversized);
        assert_eq!(bounded.chars().count(), MAX_BACKEND_ERROR_MESSAGE_CHARS + 1);
        assert!(bounded.ends_with('…'));
        assert_eq!(
            bounded_backend_error_message(" \t\n"),
            MISSING_BACKEND_ERROR_MESSAGE
        );
    }

    #[test]
    fn backend_error_evidence_has_aggregate_budget_and_exact_key_parity() {
        fn degraded_result(reverse: bool) -> CoordSearchResult {
            let mut per_backend: Vec<crate::coordinator::BackendSearchResult> = (0
                ..MAX_BACKEND_ERROR_ENTRIES + 9)
                .map(|index| crate::coordinator::BackendSearchResult {
                    backend_id: khive_runtime::BackendId::new(format!(
                        "backend-{index:03}-{}",
                        "x".repeat(MAX_BACKEND_ERROR_KEY_CHARS)
                    )),
                    entity_hits: Vec::new(),
                    note_hits: Vec::new(),
                    error: Some(format!(
                        "backend failure {index}: {}",
                        "\0\"\\".repeat(MAX_BACKEND_ERROR_MESSAGE_CHARS)
                    )),
                })
                .collect();
            if reverse {
                per_backend.reverse();
            }
            CoordSearchResult {
                entity_hits: Vec::new(),
                note_hits: Vec::new(),
                per_backend,
                partial: true,
                entity_kinds: std::collections::HashMap::new(),
                note_kinds: std::collections::HashMap::new(),
                entity_created_at: std::collections::HashMap::new(),
                note_created_at: std::collections::HashMap::new(),
                note_names: std::collections::HashMap::new(),
            }
        }

        let forward = SearchDegradation::from_result(&degraded_result(false));
        let reversed = SearchDegradation::from_result(&degraded_result(true));

        assert!(!forward.backend_errors.is_empty());
        assert!(forward.backend_errors.len() <= MAX_BACKEND_ERROR_ENTRIES);
        assert!(forward.backend_errors.iter().all(|(backend, diagnostic)| {
            backend.chars().count() <= MAX_BACKEND_ERROR_KEY_CHARS
                && diagnostic.backend_id_truncated
                && diagnostic.message.chars().count() <= MAX_BACKEND_ERROR_MESSAGE_CHARS + 1
        }));
        assert_eq!(
            forward.missing_backends,
            forward.backend_errors.keys().cloned().collect::<Vec<_>>()
        );
        assert_eq!(
            forward.backend_errors_omitted,
            MAX_BACKEND_ERROR_ENTRIES + 9 - forward.backend_errors.len()
        );
        assert_eq!(forward.missing_backends, reversed.missing_backends);
        assert_eq!(
            backend_errors_value(&forward.backend_errors),
            backend_errors_value(&reversed.backend_errors)
        );
        assert!(search_diagnostic_wire_len(&forward) <= MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP);

        let envelope = ok_envelope(
            "search".to_string(),
            OpSuccess {
                result: json!([{"id": "11111111-1111-1111-1111-111111111111"}]),
                degradation: forward,
            },
        );
        assert_eq!(envelope["backend_errors_truncated"], true);
        assert!(envelope["backend_errors_omitted"].as_u64().unwrap() > 0);
        assert_eq!(
            envelope["missing_backends"].as_array().unwrap(),
            &envelope["backend_errors"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn backend_id_credentials_are_absent_from_wire_and_warning() {
        let secret = format!("archive auth token sk_live_{}", "c".repeat(32));
        let result = CoordSearchResult {
            entity_hits: Vec::new(),
            note_hits: Vec::new(),
            per_backend: vec![crate::coordinator::BackendSearchResult {
                backend_id: khive_runtime::BackendId::new(secret.clone()),
                entity_hits: Vec::new(),
                note_hits: Vec::new(),
                error: Some("storage unavailable".to_string()),
            }],
            partial: true,
            entity_kinds: std::collections::HashMap::new(),
            note_kinds: std::collections::HashMap::new(),
            entity_created_at: std::collections::HashMap::new(),
            note_created_at: std::collections::HashMap::new(),
            note_names: std::collections::HashMap::new(),
        };
        let captured = SearchCapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let degradation = tracing::subscriber::with_default(subscriber, || {
            SearchDegradation::from_result(&result)
        });
        let wire = search_diagnostic_value(&degradation).to_string();
        let logs = captured.contents();

        assert!(!wire.contains("sk_live_"), "wire leaked backend credential");
        assert!(
            !logs.contains("sk_live_"),
            "warning leaked backend credential"
        );
        let backend = degradation
            .missing_backends
            .first()
            .expect("failed backend diagnostic retained");
        assert!(backend.contains("***MASKED***"));
        assert!(degradation.backend_errors[backend].backend_id_masked);
    }

    #[test]
    fn frame_budget_omission_preserves_complete_search_status() {
        let omitted = frame_budget_omission(&json!({
            "ok": true,
            "tool": "search",
            "result": "oversized",
            "status": "complete",
        }));

        assert_eq!(omitted["ok"], json!(true));
        assert_eq!(omitted["status"], json!("complete"));
        assert!(omitted.get("partial").is_none());
        assert!(omitted.get("result").is_none());
    }

    /// ADR-130 §Compatibility: the `search_incomplete` error is small and
    /// typed — it must survive frame-budget omission untransformed, not
    /// collapse to the generic omitted-error string.
    #[test]
    fn frame_budget_omission_preserves_search_incomplete_error_untransformed() {
        let error = json!({
            "kind": "search_incomplete",
            "message": "no-match was not established because selected backends failed",
            "retryable": false,
            "missing_backends": ["archive"],
            "backend_errors": {
                "archive": {
                    "kind": "backend_error",
                    "message": "storage unavailable"
                }
            },
            "backend_errors_truncated": true,
            "backend_errors_omitted": 2,
        });
        let omitted = frame_budget_omission(&json!({
            "ok": false,
            "tool": "search",
            "error": error,
        }));

        assert_eq!(omitted["ok"], json!(false));
        assert_eq!(omitted["error"], error);
    }

    #[test]
    fn frame_budget_omission_still_collapses_other_large_errors() {
        let omitted = frame_budget_omission(&json!({
            "ok": false,
            "tool": "create",
            "error": { "kind": "invalid_input", "message": "x".repeat(10_000) },
        }));

        assert_eq!(omitted["ok"], json!(false));
        assert_eq!(
            omitted["error"],
            json!(
                "operation failed; error details omitted because the response frame budget was exceeded"
            )
        );
    }

    #[tokio::test]
    async fn daemon_batch_keeps_rendered_result_when_compact_result_exceeds_frame() {
        let server = large_result_test_server();
        let row_bytes = khive_runtime::daemon::MAX_FRAME_BYTES / 2;
        let response = dispatch_large_result_through_daemon(
            &server,
            format!("large_result(table_bytes={row_bytes})"),
            Some("auto".to_string()),
        )
        .await;

        let envelope: Value = serde_json::from_str(&response).expect("response envelope");
        let entry = &envelope["results"][0];
        assert_eq!(entry["ok"], true);
        assert!(entry.get("result_omitted").is_none());
        let rendered = entry["result"].as_str().expect("rendered table result");
        assert!(rendered.starts_with("| payload |"));
        assert!(rendered.len() < row_bytes);
        assert!(rendered_response_fits_daemon_frame(
            &response,
            &server.config_id
        ));
    }

    #[test]
    fn daemon_frame_fitting_preserves_reason_when_error_body_is_omitted() {
        let entry = json!({
            "ok": false,
            "tool": "create",
            "error": "x".repeat(khive_runtime::daemon::MAX_FRAME_BYTES + 1_024),
            "reason": "gate-refusal",
        });
        let envelope = parallel_batch_envelope(vec![entry.clone()]);
        let fitted = fit_rendered_batch_envelope(
            envelope.as_object().expect("batch envelope object"),
            std::slice::from_ref(&entry),
            vec![entry.clone()],
            "test-config",
        );
        let fitted = Value::Object(fitted);

        assert_eq!(fitted["results"][0]["ok"], false);
        assert_eq!(fitted["results"][0]["reason"], "gate-refusal");
        assert!(fitted["results"][0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("frame budget was exceeded")));
        assert!(response_value_fits_daemon_frame(&fitted, "test-config"));
    }

    #[test]
    fn auto_rendered_batch_stays_within_daemon_frame_cap() {
        // Auto renders a single record as compact JSON, so a lone object can
        // no longer balloon past its compact form (the kv-block renderer is
        // gone). The remaining inflation mode is a sparse record array: the
        // table materializes the full column set for every row, so records
        // with disjoint key sets inflate quadratically. The fixture sits
        // where the compact envelope fits the budget but the rendered table
        // exceeds the daemon frame, which is exactly the fallback under test.
        // Sized to the smallest sparse array whose render exceeds the frame
        // with margin: 400 rows × 8000 columns ≈ 3.2M cells at ~3 bytes of
        // separator each ≈ 9.6MB rendered vs the 8MB frame. Larger fixtures
        // (600×20 keys/row was 7.2M cells) only slow the suite.
        let records: Vec<Value> = (0..400)
            .map(|record_index| {
                let mut record = serde_json::Map::new();
                for key_index in 0..20 {
                    record.insert(format!("r{record_index}k{key_index}"), json!(1));
                }
                Value::Object(record)
            })
            .collect();
        let result = json!({ "items": records });
        let envelope = parallel_batch_envelope(vec![json!({
            "ok": true,
            "tool": "probe",
            "result": result.clone(),
        })]);
        let compact_bytes = serde_json::to_vec(&envelope)
            .expect("compact envelope")
            .len();
        assert!(compact_bytes < BATCH_RESPONSE_BUDGET_BYTES);
        // Precondition: the rendered form alone must exceed the daemon frame,
        // otherwise this fixture no longer exercises the fallback.
        let rendered_probe_bytes =
            render_format(result, OutputFormat::Auto, PresentationMode::Agent).len();
        assert!(
            rendered_probe_bytes > khive_runtime::daemon::MAX_FRAME_BYTES,
            "fixture drifted: rendered entry ({rendered_probe_bytes} bytes) \
             no longer exceeds the daemon frame"
        );

        let rendered = render_result(
            envelope,
            OutputFormat::Auto,
            &None,
            PresentationMode::Agent,
            &None,
            &large_result_test_server().registry,
            Some("test"),
        );
        let rendered_value: Value = serde_json::from_str(&rendered).expect("response envelope");
        assert_eq!(rendered_value["status"], "success");
        assert_eq!(rendered_value["results"][0]["ok"], true);
        assert!(
            rendered_value["results"][0]["result"].is_object(),
            "oversized auto output must fall back to the truthful compact result"
        );
        let frame = khive_runtime::DaemonResponseFrame {
            ok: true,
            result: Some(rendered),
            error: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some("test".to_string()),
            version_mismatch: false,
            daemon_protocol_version: khive_runtime::PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        let frame_bytes = serde_json::to_vec(&frame).expect("daemon frame").len();
        assert!(
            frame_bytes <= khive_runtime::daemon::MAX_FRAME_BYTES,
            "rendered daemon frame was {frame_bytes} bytes"
        );
    }

    #[tokio::test]
    async fn bounded_batch_op_error_does_not_abort_siblings() {
        let count = 5;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let futures = (0..count).map(|index| {
            let entry = if index == 2 {
                json!({"ok": false, "tool": "probe", "error": "expected failure"})
            } else {
                json!({"ok": true, "tool": "probe", "result": {"index": index}})
            };
            batch_task(
                index,
                observed_batch_entry(
                    index,
                    (count - index) as u64,
                    entry,
                    in_flight.clone(),
                    max_in_flight.clone(),
                ),
            )
        });

        let response = parallel_batch_envelope(
            execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await,
        );

        assert_eq!(
            response["summary"],
            json!({"total": 5, "succeeded": 4, "failed": 1, "aborted": 0})
        );
        assert_eq!(response["results"][2]["error"], "expected failure");
        assert!(response["results"][3]["ok"].as_bool().unwrap_or(false));
        assert!(response["results"][4]["ok"].as_bool().unwrap_or(false));
    }

    #[tokio::test]
    async fn bounded_batch_never_exceeds_concurrency_limit() {
        let count = MAX_BATCH_CONCURRENCY * 3;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let futures = (0..count).map(|index| {
            batch_task(
                index,
                observed_batch_entry(
                    index,
                    10,
                    json!({"ok": true, "tool": "probe", "result": index}),
                    in_flight.clone(),
                    max_in_flight.clone(),
                ),
            )
        });

        let results = execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await;

        assert_eq!(results.len(), count);
        assert_eq!(max_in_flight.load(Ordering::SeqCst), MAX_BATCH_CONCURRENCY);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    fn t(pack: &str, verb: &str, desc: &str) -> (String, String, String) {
        (pack.to_owned(), verb.to_owned(), desc.to_owned())
    }

    // ── serve_stdio handshake-mode decision (#714) ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn stdio_serve_mode_cold_start_uses_handshake() {
        assert_eq!(stdio_serve_mode_for(None), StdioServeMode::Handshake);
    }

    #[cfg(unix)]
    #[test]
    fn stdio_serve_mode_resumed_generation_skips_handshake() {
        assert_eq!(stdio_serve_mode_for(Some(1)), StdioServeMode::Resumed);
    }

    #[test]
    fn single_pack_verbs_unchanged() {
        let catalog = build_verb_catalog([
            t("kg", "create", "Create an entity or note."),
            t("kg", "list", "List entities."),
        ]);
        assert_eq!(
            catalog,
            "  create — Create an entity or note.\n  list — List entities.\n"
        );
    }

    #[test]
    fn duplicate_verb_concatenates_descriptions_with_pack_attribution() {
        let catalog = build_verb_catalog([
            t("kg", "create", "Create an entity or note."),
            t("gtd", "create", "Create a task."),
        ]);
        // Both pack descriptions must appear with attribution.
        assert!(catalog.contains("[kg] Create an entity or note."));
        assert!(catalog.contains("[gtd] Create a task."));
        // The verb name must appear exactly once in the catalog header.
        assert_eq!(catalog.matches("  create — ").count(), 1);
    }

    #[test]
    fn instructions_carry_docs_address_and_guidance_pointers() {
        let instructions = build_instructions("  create — Create an entity or note.\n", "kg, gtd");
        assert!(instructions.contains("https://ohdearquant.github.io/khive/"));
        assert!(instructions.contains("docs/configuration.md"));
        assert!(instructions.contains("docs/guide/tips-and-tricks.md"));
        // help=true / live-catalog-over-training-knowledge guidance present.
        assert!(instructions.contains("help=true"));
    }

    #[test]
    fn catalog_is_sorted_alphabetically() {
        let catalog = build_verb_catalog([
            t("kg", "search", "Search."),
            t("kg", "assign", "Assign."),
            t("kg", "list", "List."),
        ]);
        let names: Vec<&str> = catalog
            .lines()
            .filter(|l| l.starts_with("  "))
            .map(|l| l.trim_start().split(' ').next().unwrap())
            .collect();
        assert_eq!(names, vec!["assign", "list", "search"]);
    }

    // ── #658 regression: brain dispatch hook wired into production builder ──

    /// The hook (registered via `PackInstall::dispatch_hook`) and the pack
    /// runtime the registry dispatches `brain.*` verbs to must be the same
    /// `BrainPack` instance — otherwise the hook's posterior updates would be
    /// invisible to `brain.state` reads. `brain.state` loads the default
    /// namespace into the shared active slot as a side effect, so a
    /// subsequent non-brain dispatch in the same namespace lands on
    /// `ApplyTarget::ActiveSlot` and is immediately observable.
    ///
    /// Uses the `local` namespace (rather than an arbitrary one) because
    /// ADR-007 Rule 3b always pins the implicit write token to `local`
    /// regardless of the registry's configured default namespace; using
    /// `local` for both keeps the dispatched event's namespace and the
    /// token's namespace identical, so the signal lands on the active slot
    /// instead of the cold-namespace queue.
    #[tokio::test]
    async fn brain_dispatch_hook_updates_state_visible_through_same_instance() {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "brain".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::with_packs(runtime, &["kg".to_string(), "brain".to_string()])
            .expect("server builds with kg + brain");

        server
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state loads the default namespace into the active slot");

        server
            .registry
            .dispatch("stats", serde_json::json!({}))
            .await
            .expect("kg.stats dispatch succeeds");

        let state = server
            .registry
            .dispatch("brain.state", serde_json::Value::Null)
            .await
            .expect("brain.state dispatch");
        let total_events = state["balanced_recall"]["total_events"]
            .as_u64()
            .unwrap_or(0);
        assert!(
            total_events > 0,
            "dispatch hook must update the same BrainPack instance the registry \
             dispatches brain.* verbs to; got snapshot {state:?}"
        );
    }

    /// ADR-124 boot-occupancy regression: `has_note_write_validator` exists
    /// specifically so a transport's own tests can assert, per boot path,
    /// that the documented startup sequence actually filled the slot — but
    /// nothing called it. Every prior ADR-124 test built its registry by
    /// hand (`registry.call_register_note_write_validators(&rt)` in
    /// `khive-pack-comm`'s integration tests), which proves the validator
    /// works but proves nothing about whether `with_packs` — the single-
    /// backend production boot path — installs it. Asserts occupancy
    /// directly through the real builder, then proves the slot is not just
    /// occupied but functioning: a generic `create` naming a forged
    /// `from_actor` on a `message` note must come back derived to the
    /// dispatching token's actor. Sensitivity verified by temporarily
    /// commenting out the `registry.call_register_note_write_validators(&runtime);`
    /// line in `KhiveMcpServer::with_packs` and re-running: both assertions
    /// fail without it (occupancy false; forged value survives) and pass
    /// with it restored.
    #[tokio::test]
    async fn single_runtime_boot_installs_note_write_validator() {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "comm".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let runtime_probe = runtime.clone();
        let server = KhiveMcpServer::with_packs(runtime, &["kg".to_string(), "comm".to_string()])
            .expect("server builds with kg + comm");

        assert!(
            runtime_probe.has_note_write_validator(),
            "with_packs (single-backend boot) must install the note-write \
             validator on the runtime it serves writes through"
        );

        let identity = khive_runtime::RequestIdentity {
            namespace: "local".to_string(),
            actor_id: Some("lambda:probe".to_string()),
            ..Default::default()
        };
        let created = server
            .registry
            .dispatch_with_identity(
                "create",
                serde_json::json!({
                    "kind": "message",
                    "content": "single-runtime boot occupancy probe",
                    "properties": {"from_actor": "forged-actor"},
                }),
                Some(identity),
            )
            .await
            .expect("create must succeed");
        assert_eq!(
            created["properties"]["from_actor"], "lambda:probe",
            "a forged from_actor on a generic create must come back derived to \
             the dispatching token's actor, proving the installed validator is \
             not just present but wired into the write path; got {created:?}"
        );
    }

    // ── relative backend paths must not collide across projects ────────────

    /// RAII guard: temporarily chdirs into `dir`, restoring the original cwd
    /// on drop (even on panic/unwind). Process cwd is global state, so every
    /// test using this guard is `#[serial]`.
    struct CwdGuard {
        original: std::path::PathBuf,
    }

    impl CwdGuard {
        fn enter(dir: &std::path::Path) -> Self {
            let original = std::env::current_dir().expect("read cwd");
            std::env::set_current_dir(dir).expect("chdir into test project root");
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    /// The security finding this guards: `compute_config_id`'s backend
    /// topology fold used to embed the RAW relative path string declared in
    /// `khive.toml`. Two different projects that happen to declare the same
    /// relative string (e.g. `./data/main.db`) but resolve it against two
    /// different working directories produced the SAME `config_id` despite
    /// opening two different physical databases — a warm daemon started for
    /// one project could then accept forwarded requests meant for the other,
    /// serving or writing the wrong project's data.
    #[test]
    #[serial]
    fn config_id_does_not_collide_across_projects_with_same_relative_backend_path() {
        use khive_runtime::{BackendId, BackendKind, KhiveConfig, Namespace};

        let project_a = tempfile::tempdir().expect("project a tempdir");
        let project_b = tempfile::tempdir().expect("project b tempdir");

        let base_rt = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        };

        let relative_backend_cfg = || KhiveConfig {
            backends: vec![khive_runtime::BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(std::path::PathBuf::from("./data/main.db")),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };

        let id_a = {
            let _cwd = CwdGuard::enter(project_a.path());
            compute_config_id(&base_rt, Some(&relative_backend_cfg()))
        };
        let id_b = {
            let _cwd = CwdGuard::enter(project_b.path());
            compute_config_id(&base_rt, Some(&relative_backend_cfg()))
        };

        assert_ne!(
            id_a, id_b,
            "two projects declaring the same relative backend path string from \
             different working directories must not share a config_id; both \
             produced: {id_a}"
        );
    }

    /// A backend path is caller-controlled data, so the legacy `:read_only`
    /// suffix must never be confusable with literal path text. Before this
    /// regression, these two distinct archive backends produced the same
    /// topology string and could therefore share the wrong warm daemon:
    ///
    /// - read-only `/.../archive.db`
    /// - writable `/.../archive.db:read_only`
    #[test]
    fn config_id_does_not_confuse_read_only_mode_with_a_path_suffix() {
        use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

        let dir = tempfile::tempdir().expect("topology collision tempdir");
        let main_path = dir.path().join("main.db");
        let archive_path = dir.path().join("archive.db");
        let literal_suffix_path = dir.path().join("archive.db:read_only");
        let runtime = RuntimeConfig {
            db_path: Some(main_path.clone()),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::no_embeddings()
        };

        let topology = |path, read_only| KhiveConfig {
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
                    name: "archive".to_string(),
                    kind: BackendKind::Sqlite,
                    path: Some(path),
                    cache_mb: None,
                    journal_mode: None,
                    read_only,
                },
            ],
            packs: std::collections::HashMap::from([(
                "knowledge".to_string(),
                PackConfig {
                    backend: "archive".to_string(),
                    no_embed: false,
                },
            )]),
            ..KhiveConfig::default()
        };

        let read_only_archive = topology(archive_path, true);
        let writable_literal_suffix = topology(literal_suffix_path, false);

        assert_ne!(
            compute_config_id(&runtime, Some(&read_only_archive)),
            compute_config_id(&runtime, Some(&writable_literal_suffix)),
            "backend mode must be encoded as a field, not an ambiguous path suffix"
        );
    }

    /// The collision fix is deliberately conditional: ordinary topology
    /// components that contain no reserved syntax keep their existing daemon
    /// identity, avoiding an unnecessary one-time fallback/restart for the
    /// overwhelmingly common configuration shape.
    #[test]
    fn config_id_preserves_legacy_topology_spelling_when_delimiter_free() {
        use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

        let dir = tempfile::tempdir().expect("legacy topology tempdir");
        let main_path = dir.path().join("main.db");
        let runtime = RuntimeConfig {
            db_path: Some(main_path.clone()),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::no_embeddings()
        };
        let topology = KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: std::collections::HashMap::from([(
                "kg".to_string(),
                PackConfig {
                    backend: "main".to_string(),
                    no_embed: false,
                },
            )]),
            ..KhiveConfig::default()
        };

        let expected_suffix = format!(
            ";backends=[main:Sqlite:{}];pack_backends=[kg=main]",
            canonical_fingerprint_path(&main_path)
        );
        let config_id = compute_config_id(&runtime, Some(&topology));
        assert!(
            config_id.ends_with(&expected_suffix),
            "delimiter-free topologies must retain their legacy fingerprint spelling; got {config_id}"
        );
    }

    /// `no_embed` changes runtime behavior (that pack's runtime carries zero
    /// embedders), so two configs differing only in it must not share a
    /// `config_id` — a shared id would let a daemon serve a client whose
    /// embedding policy it does not implement. Absent/false keeps the
    /// pre-existing spelling so already-deployed configs keep their id.
    #[test]
    fn config_id_differs_when_pack_no_embed_differs() {
        use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

        let dir = tempfile::tempdir().expect("no_embed topology tempdir");
        let main_path = dir.path().join("main.db");
        let runtime = RuntimeConfig {
            db_path: Some(main_path.clone()),
            packs: vec!["comm".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::no_embeddings()
        };
        let topology_for = |no_embed: bool| KhiveConfig {
            backends: vec![BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            packs: std::collections::HashMap::from([(
                "comm".to_string(),
                PackConfig {
                    backend: "main".to_string(),
                    no_embed,
                },
            )]),
            ..KhiveConfig::default()
        };

        let with_flag = compute_config_id(&runtime, Some(&topology_for(true)));
        let without_flag = compute_config_id(&runtime, Some(&topology_for(false)));
        assert_ne!(
            with_flag, without_flag,
            "configs differing only in no_embed must not share a config_id"
        );
        assert!(
            with_flag.contains("comm=main:no_embed"),
            "no_embed must appear in the pack fingerprint; got {with_flag}"
        );
        assert!(
            without_flag.contains("comm=main]"),
            "absent no_embed keeps the legacy pack spelling; got {without_flag}"
        );
    }

    #[test]
    fn config_id_separates_effective_read_only_storage_modes() {
        use khive_runtime::{BackendId, BackendKind, KhiveConfig, Namespace};

        let dir = tempfile::tempdir().expect("config-mode tempdir");
        let runtime = RuntimeConfig {
            db_path: Some(dir.path().join("khive-config-mode.db")),
            default_namespace: Namespace::local(),
            embedding_model: None,
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        };

        let writable = compute_config_id(&runtime, None);
        assert_eq!(
            writable,
            compute_config_id_with_storage_mode(&runtime, None, false),
            "the writable fingerprint must remain byte-identical"
        );
        let detected_read_only = compute_config_id_with_storage_mode(&runtime, None, true);
        assert_ne!(
            writable, detected_read_only,
            "a chmod-detected snapshot must not reuse a write-capable warm daemon"
        );
        assert!(detected_read_only.contains(&format!("backend={:?}:read_only", runtime.backend_id)));

        let writable_topology = KhiveConfig {
            backends: vec![khive_runtime::BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: runtime.db_path.clone(),
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        };
        let mut read_only_topology = writable_topology.clone();
        read_only_topology.backends[0].read_only = true;
        assert_ne!(
            compute_config_id(&runtime, Some(&writable_topology)),
            compute_config_id(&runtime, Some(&read_only_topology)),
            "declared multi-backend read_only mode is part of backend topology"
        );
        assert_eq!(
            compute_config_id(&runtime, Some(&read_only_topology)),
            compute_config_id_with_storage_mode(&runtime, Some(&read_only_topology), true),
            "the pre-open client and opened read-only server must fingerprint identically"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_id_auto_detects_chmod_read_only_single_backend() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("config-mode tempdir");
        let path = dir.path().join("chmod-snapshot.db");
        std::fs::write(&path, b"snapshot identity fixture").expect("create fixture");
        let runtime = RuntimeConfig {
            db_path: Some(path.clone()),
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let writable = compute_config_id(&runtime, None);

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&path, permissions).unwrap();
        freeze_snapshot_sidecars(&path);

        let detected = compute_config_id(&runtime, None);
        assert_ne!(writable, detected);
        assert!(
            detected.contains(&format!("backend={:?}:read_only", runtime.backend_id)),
            "{detected}"
        );
        assert_eq!(
            detected,
            compute_config_id_with_storage_mode(&runtime, None, true),
            "pre-open forwarding and opened-server identities must converge"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_owned_config_id_keeps_captured_writable_mode_after_post_open_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("runtime-mode tempdir");
        let path = dir.path().join("post-open-chmod.db");
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            embedding_model: None,
            packs: Vec::new(),
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("open writable runtime");
        assert!(
            !runtime.is_read_only(),
            "runtime must capture writable mode"
        );
        let captured_writable_id = compute_config_id_with_runtime_policies(
            runtime.config(),
            None,
            runtime.ann_fresh_tail_enabled(),
            runtime.is_read_only(),
        );

        let original_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&path, permissions).unwrap();
        freeze_snapshot_sidecars(&path);

        let pre_open_read_only_id = compute_config_id(runtime.config(), None);
        assert!(
            pre_open_read_only_id.contains(&format!(
                "backend={:?}:read_only",
                runtime.config().backend_id
            )),
            "a new runtime must detect the chmod-read-only snapshot: {pre_open_read_only_id}"
        );

        let server = KhiveMcpServer::new(runtime).expect("build server from opened runtime");
        assert_eq!(
            server.config_id(),
            captured_writable_id,
            "runtime-owned identity must trust the access mode captured when its SQLite pool opened"
        );
        assert_ne!(
            server.config_id(),
            pre_open_read_only_id,
            "an already-open writable engine must not advertise the pre-open read-only identity"
        );

        drop(server);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(original_mode);
        std::fs::set_permissions(&path, permissions).unwrap();
    }

    /// The same collision, one layer up the resolution chain: `--db`/`KHIVE_DB`
    /// resolves to a raw relative `PathBuf` (`resolve_db_anchor`) that lands in
    /// `RuntimeConfig.db_path` unchanged. Before this fix, `compute_config_id`
    /// fingerprinted that raw string directly, so two different projects both
    /// running `KHIVE_DB=./data/main.db` produced the SAME `config_id` while
    /// opening two different SQLite files — the single-backend route
    /// (`KhiveMcpServer::with_packs`) would let a warm daemon started for one
    /// project serve requests meant for the other's database.
    #[test]
    #[serial]
    fn config_id_does_not_collide_across_projects_with_same_relative_db_override() {
        use khive_runtime::Namespace;

        let project_a = tempfile::tempdir().expect("project a tempdir");
        let project_b = tempfile::tempdir().expect("project b tempdir");

        let rt_with_db = |db_path: Option<std::path::PathBuf>| RuntimeConfig {
            db_path,
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };

        let relative_db = std::path::PathBuf::from("./data/main.db");

        let id_a = {
            let _cwd = CwdGuard::enter(project_a.path());
            compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
        };
        let id_b = {
            let _cwd = CwdGuard::enter(project_b.path());
            compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
        };

        assert_ne!(
            id_a, id_b,
            "two projects overriding KHIVE_DB with the same relative path from \
             different working directories must not share a config_id; both \
             produced: {id_a}"
        );

        let id_a_again = {
            let _cwd = CwdGuard::enter(project_a.path());
            compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
        };
        assert_eq!(
            id_a, id_a_again,
            "resolving the same project's KHIVE_DB override twice must produce \
             the same config_id"
        );
    }

    // ── #823: runtime `$prev` result depth guard ────────────────────────────

    /// Iteratively (no native recursion) wrap `leaf` in `depth` nested
    /// single-key objects, a synthetic stand-in for a pathologically deep
    /// handler result (e.g. from `traverse`/`context`) that would otherwise
    /// overflow the stack when cloned into `$prev` chain context.
    ///
    /// Builds each level via a direct `Map` insert rather than `json!` — the
    /// `json!` object-literal arm calls `serde_json::to_value(&v)` on the
    /// accumulated value, which would walk the whole tree built so far on
    /// every iteration (recursing to the current depth each time) and
    /// overflow the stack itself well before reaching `depth` large enough
    /// to exercise the guard under test.
    fn nest_object(depth: usize, leaf: Value) -> Value {
        let mut v = leaf;
        for _ in 0..depth {
            let mut map = serde_json::Map::with_capacity(1);
            map.insert("nested".to_string(), v);
            v = Value::Object(map);
        }
        v
    }

    #[test]
    fn deep_nested_result_over_limit_is_flagged() {
        let deep = nest_object(
            khive_request::NESTING_DEPTH_LIMIT + 5,
            json!({"leaf": true}),
        );
        let result_obj = json!({ "ok": true, "tool": "traverse", "result": deep });
        assert!(
            result_exceeds_depth_limit(&result_obj),
            "result nested past NESTING_DEPTH_LIMIT must be flagged"
        );
    }

    #[test]
    fn result_at_exactly_the_depth_limit_is_not_flagged() {
        // A scalar leaf (not a container) so the wrapping objects alone land
        // exactly at NESTING_DEPTH_LIMIT containers deep.
        let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!(true));
        let result_obj = json!({ "ok": true, "tool": "traverse", "result": at_limit });
        assert!(
            !result_exceeds_depth_limit(&result_obj),
            "result nested exactly at the limit must still be usable as $prev context"
        );
    }

    #[test]
    fn shallow_result_is_not_flagged() {
        let shallow = json!({"a": {"b": {"c": 1}}});
        let result_obj = json!({ "ok": true, "tool": "get", "result": shallow });
        assert!(!result_exceeds_depth_limit(&result_obj));
    }

    #[test]
    fn result_missing_field_is_not_flagged() {
        let result_obj = json!({ "ok": false, "tool": "get", "error": "not found" });
        assert!(!result_exceeds_depth_limit(&result_obj));
    }

    #[test]
    fn chain_aggregation_seam_rejects_over_limit_result_via_iterative_drop() {
        // Directly exercises the post-hoc aggregation-loop guard in
        // `run_parsed`'s `Chain` arm (isolated as
        // `chain_aggregation_depth_reject`) with a value nested well past
        // NESTING_DEPTH_LIMIT. If this branch let the rejected `result_obj`
        // fall out of scope instead of routing it through
        // `drop_value_iteratively`, `Value`'s derived recursive `Drop` would
        // overflow the stack on a value this deep — so this test failing to
        // complete (rather than merely asserting wrong) is itself the
        // regression signal for #823's post-hoc-rejection finding.
        let deep = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        // Built via direct `Map` inserts, not `json!({..., "result": deep})`:
        // the object-literal macro arm calls `serde_json::to_value(&deep)` on
        // the already-deep value, which would recurse over the whole tree
        // and overflow the stack while constructing the fixture itself,
        // before the guard under test ever runs (see `nest_object` above).
        let mut envelope = serde_json::Map::with_capacity(3);
        envelope.insert("ok".to_string(), Value::Bool(true));
        envelope.insert("tool".to_string(), Value::String("traverse".to_string()));
        envelope.insert("result".to_string(), deep);
        let result_obj = Value::Object(envelope);

        let err = chain_aggregation_depth_reject(result_obj)
            .expect_err("result nested past NESTING_DEPTH_LIMIT must be rejected");

        assert_eq!(err["ok"], json!(false));
        assert_eq!(err["tool"], json!("traverse"));
        assert_eq!(err["error"]["kind"], json!("result_too_deep"));
        // The error entry must never embed the oversized value itself.
        assert!(err.get("result").is_none());
    }

    #[test]
    fn chain_aggregation_seam_accepts_result_within_limit_unchanged() {
        let shallow = json!({ "ok": true, "tool": "get", "result": {"a": {"b": 1}} });
        let accepted = chain_aggregation_depth_reject(shallow.clone())
            .expect("result within the limit must be passed through unchanged");
        assert_eq!(accepted, shallow);
    }

    // ── earliest-seam guard: raw handler `Value` before json!/present/clone ──
    //
    // These exercise `chain_ok_envelope_or_depth_error` and
    // `present_ok_envelope_or_depth_error` directly with a synthetic
    // over-limit `Value` — no DSL parsing involved, standing in for a mock
    // handler whose result is pathologically deep regardless of how shallow
    // the caller's own op args were. This is the earliest point in
    // `dispatch_op` / `run_parsed`'s parallel closure where the raw value is
    // available, strictly before it is ever cloned, presented, or passed
    // through `json!`/`serde_json::to_value`.

    #[test]
    fn chain_seam_rejects_over_limit_result_before_envelope_build() {
        // Deep enough that native recursion (json!/to_value/present) over
        // this value would be a real stack risk; the guard must reject it
        // via the iterative checker without ever attempting that recursion.
        let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        let err = chain_ok_envelope_or_depth_error(
            "traverse".to_string(),
            OpSuccess::complete(pathological),
        )
        .expect_err("over-limit result must be rejected, not enveloped");
        assert_eq!(err.tool, "traverse");
        assert_eq!(err.error["kind"], json!("result_too_deep"));
        // The error payload must never embed the oversized value itself.
        assert!(err.error.get("result").is_none());
        assert!(err.error.get("nested").is_none());
    }

    #[test]
    fn chain_seam_accepts_at_limit_result_and_moves_value_without_reserializing() {
        let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!("leaf"));
        let envelope = chain_ok_envelope_or_depth_error(
            "get".to_string(),
            OpSuccess::complete(at_limit.clone()),
        )
        .expect("result at exactly the limit must be accepted");
        assert_eq!(envelope["ok"], json!(true));
        assert_eq!(envelope["tool"], json!("get"));
        assert_eq!(envelope["result"], at_limit);
    }

    #[test]
    fn parallel_seam_rejects_over_limit_result_before_present() {
        let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
        let envelope = present_ok_envelope_or_depth_error(
            "context".to_string(),
            OpSuccess::complete(pathological),
            PresentationMode::Agent,
            0,
        );
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["tool"], json!("context"));
        assert_eq!(envelope["error"]["kind"], json!("result_too_deep"));
        assert!(envelope["error"].get("result").is_none());
    }

    #[test]
    fn parallel_seam_accepts_shallow_result_and_applies_presentation() {
        let shallow = json!({"id": "11111111-1111-1111-1111-111111111111"});
        let envelope = present_ok_envelope_or_depth_error(
            "get".to_string(),
            OpSuccess::complete(shallow),
            PresentationMode::Verbose,
            0,
        );
        assert_eq!(envelope["ok"], json!(true));
        assert_eq!(
            envelope["result"]["id"],
            json!("11111111-1111-1111-1111-111111111111")
        );
    }

    #[test]
    fn success_envelope_requires_typed_degradation_and_preserves_it_through_presentation() {
        let success = OpSuccess {
            result: json!([{"id": "11111111-1111-1111-1111-111111111111"}]),
            degradation: SearchDegradation {
                status: Some(SearchStatus::Partial),
                missing_backends: vec!["archive".to_string()],
                backend_errors: BTreeMap::from([(
                    "archive".to_string(),
                    BackendErrorDiagnostic {
                        message: "storage unavailable".to_string(),
                        backend_id_masked: false,
                        backend_id_truncated: false,
                        backend_id_chars: "archive".chars().count(),
                    },
                )]),
                backend_errors_omitted: 0,
            },
        };
        let envelope = present_ok_envelope_or_depth_error(
            "search".to_string(),
            success,
            PresentationMode::Agent,
            0,
        );

        assert_eq!(envelope["ok"], json!(true));
        assert_eq!(envelope["status"], json!("partial"));
        assert_eq!(envelope["partial"], json!(true));
        assert_eq!(envelope["missing_backends"], json!(["archive"]));
        assert_eq!(
            envelope["backend_errors"]["archive"]["message"],
            json!("storage unavailable")
        );
        assert!(envelope.get("result").is_some());
    }

    #[tokio::test]
    async fn chain_with_deep_accumulated_prev_result_errors_cleanly() {
        // Real end-to-end reproduction: chain N `create` ops where each step's
        // `properties.inner` embeds the previous op's full `properties` via
        // `$prev.properties`. Each op's own DSL args stay shallow (well under
        // NESTING_DEPTH_LIMIT), but the accumulated *runtime result* nests one
        // level deeper per chain step, the exact CWE-674 shape the parser's
        // syntax-tree guard cannot see. Past the limit this must surface a
        // clean per-op `result_too_deep` error and abort the remaining chain,
        // never attempting to clone/serialize the unbounded value.
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

        let steps = khive_request::NESTING_DEPTH_LIMIT + 6;
        let mut dsl = String::from(
            r#"create(kind="entity", entity_kind="concept", name="d0", properties={"n": 0})"#,
        );
        for i in 1..steps {
            dsl.push_str(&format!(
                r#" | create(kind="entity", entity_kind="concept", name="d{i}", properties={{"inner": $prev.properties}})"#
            ));
        }

        let parsed = parse_request(&dsl).expect("each op's own args stay shallow; DSL must parse");
        assert_eq!(parsed.mode, ExecutionMode::Chain);

        let response = server
            .run_parsed(
                parsed.ops,
                parsed.mode,
                PresentationMode::Verbose,
                None,
                RunParsedContext {
                    enforce_response_budget: true,
                    max_batch_concurrency: MAX_BATCH_CONCURRENCY,
                    from_wire: false,
                    identity: None,
                },
            )
            .await;

        let results = response["results"]
            .as_array()
            .expect("results must be an array");
        assert_eq!(results.len(), steps);

        let failure_idx = results
            .iter()
            .position(|r| r["ok"] == json!(false))
            .expect("accumulated nesting must trip the depth guard before the chain completes");
        assert_eq!(
            results[failure_idx]["error"]["kind"],
            json!("result_too_deep"),
            "unexpected failure shape at index {failure_idx}: {:?}",
            results[failure_idx]
        );

        // Every op after the failing one is marked aborted, not attempted,
        // proving the process kept running instead of crashing.
        for r in &results[failure_idx + 1..] {
            assert_eq!(
                r["aborted"],
                json!(true),
                "expected abort after the depth guard trips: {r:?}"
            );
        }
    }

    // ── request-boundary regression: raw controls survive wire decoding ─────

    #[tokio::test]
    async fn request_boundary_raw_control_bytes_reach_handler() {
        // Simulates the actual MCP wire: a JSON-RPC client sends the tool's
        // `ops` argument as a JSON string using the standard JSON `\n`
        // escape. Deserializing `RequestParams` decodes that escape into an
        // actual raw LF byte inside the DSL source — the exact shape
        // `normalize_quoted_string` (crates/khive-request/src/parser/scan.rs)
        // exists to accept. This confirms the decoded raw newline survives
        // parsing and dispatch all the way to the pack handler's result.
        let wire = "{\"ops\":\"create(kind=\\\"entity\\\", entity_kind=\\\"concept\\\", name=\\\"line1\\nline2\\\")\"}";
        let params: RequestParams = serde_json::from_str(wire).expect("wire JSON deserializes");
        assert!(
            params.ops.contains('\n'),
            "deserialized ops must carry a raw LF, not the two-char escape: {:?}",
            params.ops
        );

        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

        let parsed = parse_request(&params.ops).expect("literal newline inside quotes must parse");
        let response = server
            .run_parsed(
                parsed.ops,
                parsed.mode,
                PresentationMode::Verbose,
                None,
                RunParsedContext {
                    enforce_response_budget: true,
                    max_batch_concurrency: MAX_BATCH_CONCURRENCY,
                    from_wire: false,
                    identity: None,
                },
            )
            .await;

        let results = response["results"]
            .as_array()
            .expect("results must be an array");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]["ok"],
            json!(true),
            "unexpected result: {response:?}"
        );
        assert_eq!(results[0]["result"]["name"], json!("line1\nline2"));
    }

    // ── MCP-AUD-002 regression: save_to must bypass daemon forwarding ────────

    fn make_daemon_save_to_test_server() -> KhiveMcpServer {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        KhiveMcpServer::new(runtime).expect("server builds with kg")
    }

    fn clear_daemon_env() {
        std::env::remove_var("KHIVE_SOCKET");
        std::env::remove_var("KHIVE_PID");
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::remove_var("KHIVE_LOCK");
        std::env::remove_var("KHIVE_PROCESS_REF");
    }

    fn stats_without_request_local_usage(raw: &str) -> Value {
        let mut envelope: Value = serde_json::from_str(raw).expect("stats response JSON");
        for entry in envelope["results"]
            .as_array_mut()
            .expect("stats results array")
        {
            entry
                .as_object_mut()
                .expect("stats result object")
                .remove("usage");
        }
        envelope
    }

    /// khive#948: `wire_daemon_frame` forwards `RequestParams::request_id`
    /// onto the `DaemonRequestFrame` unchanged, and defaults to `None` when
    /// the caller supplied none.
    #[cfg(unix)]
    #[test]
    fn wire_daemon_frame_forwards_request_id() {
        let server = make_daemon_save_to_test_server();

        let with_id = RequestParams {
            ops: "stats()".to_string(),
            request_id: Some(123),
            ..Default::default()
        };
        let frame = server.wire_daemon_frame(&with_id);
        assert_eq!(frame.request_id, Some(123));

        let without_id = RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        };
        let frame = server.wire_daemon_frame(&without_id);
        assert_eq!(frame.request_id, None);
    }

    /// Query every persisted audit event and find the one whose
    /// `resource.request_id` matches `id`, if any.
    async fn find_audit_event_with_request_id(
        store: &Arc<dyn khive_storage::EventStore>,
        id: u64,
    ) -> Option<khive_storage::Event> {
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query_events must succeed");
        page.items
            .into_iter()
            .find(|ev| ev.payload["resource"]["request_id"] == json!(id))
    }

    /// khive#948: `request_id` was previously dropped on the
    /// `KHIVE_NO_DAEMON`/soft-fallback local dispatch path because
    /// `dispatch_request_wire` always passed `identity = None`. This drives
    /// `request()` end-to-end under `KHIVE_NO_DAEMON=1` and inspects the
    /// persisted audit event, proving the id now survives to
    /// `resource.request_id` on the local-dispatch path too, not just the
    /// daemon-forward path.
    #[tokio::test]
    #[serial]
    async fn request_no_daemon_fallback_preserves_request_id_in_audit_event() {
        clear_daemon_env();
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let server = make_daemon_save_to_test_server();
        server
            .request(
                Parameters(RequestParams {
                    // Explicit `namespace="local"` so the write lands in the
                    // same namespace the server's audit `EventStore` handle is
                    // scoped to at construction (`Namespace::local()`), matching
                    // `find_audit_event_with_request_id`'s read scope.
                    ops: "stats(namespace=\"local\")".to_string(),
                    request_id: Some(9001),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("request() must succeed via local dispatch under KHIVE_NO_DAEMON");

        let store = server
            .event_store()
            .expect("in-memory runtime must configure an EventStore");
        let matched = find_audit_event_with_request_id(&store, 9001).await;
        assert!(
            matched.is_some(),
            "KHIVE_NO_DAEMON local dispatch must stamp request_id onto the persisted \
             audit event"
        );

        clear_daemon_env();
    }

    /// khive#948: the `save_to` bypass (MCP-AUD-002) also routes through
    /// `dispatch_request_wire`'s local dispatch — this proves the id
    /// survives that path too.
    #[tokio::test]
    #[serial]
    async fn request_save_to_bypass_preserves_request_id_in_audit_event() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

        let server = make_daemon_save_to_test_server();
        let sink_path = dir.path().join("out.jsonl");
        server
            .request(
                Parameters(RequestParams {
                    ops: "stats(namespace=\"local\")".to_string(),
                    save_to: Some(sink_path.to_string_lossy().to_string()),
                    request_id: Some(9002),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("request() with save_to must succeed");

        let store = server
            .event_store()
            .expect("in-memory runtime must configure an EventStore");
        let matched = find_audit_event_with_request_id(&store, 9002).await;
        assert!(
            matched.is_some(),
            "save_to local-dispatch bypass must stamp request_id onto the persisted \
             audit event"
        );

        clear_daemon_env();
        std::env::remove_var("KHIVE_SAVE_TO_ROOT");
    }

    #[cfg(unix)]
    async fn connect_when_daemon_ready(sock: &std::path::Path) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if tokio::net::UnixStream::connect(sock).await.is_ok() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never bound {sock:?} within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Regression for MCP-AUD-002 / #440: `request()` must NOT forward a
    /// `save_to`-bearing call to a warm daemon (whose wire frame has no
    /// `save_to` field and would silently return the inline result instead of
    /// writing the sink). With a real daemon reachable at `KHIVE_SOCKET`, a
    /// `save_to` request must still take the local path and return the
    /// manifest with the file actually written — proving the daemon was
    /// bypassed rather than silently dropping the sink.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_save_to_bypasses_daemon_forwarding_and_writes_manifest() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");
        // save_to destinations must resolve inside the allowed export root
        // (crate::save_sink); scope it to this test's tempdir.
        std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

        let server = make_daemon_save_to_test_server();
        let daemon_server = server.clone();
        let handle = tokio::spawn(async move {
            let _ = khive_runtime::daemon::run_daemon(daemon_server).await;
        });
        connect_when_daemon_ready(&sock).await;

        let sink_path = dir.path().join("out.jsonl");
        let resp = server
            .request(
                Parameters(RequestParams {
                    ops: "stats()".to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: Some(sink_path.to_string_lossy().to_string()),
                    format: None,
                    format_per_op: None,
                    request_id: None,
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("request with save_to must succeed even with a warm daemon reachable");

        let manifest: serde_json::Value =
            serde_json::from_str(&resp).expect("response must be the save_to manifest JSON");
        assert!(
            manifest.get("rows").is_some() && manifest.get("path").is_some(),
            "response must be the save_to manifest, not an inline daemon result; got: {resp}"
        );
        assert!(
            sink_path.exists(),
            "save_to file must be written even when a daemon is reachable"
        );
        let contents = std::fs::read_to_string(&sink_path).expect("read sink file");
        assert!(
            !contents.trim().is_empty(),
            "sink file must contain JSONL content"
        );

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
        std::env::remove_var("KHIVE_SAVE_TO_ROOT");
    }

    /// A malformed MCP request must be rejected with the same typed RPC error
    /// even when a matching warm daemon is available. The bridge protocol has a
    /// string-only error channel, so this specifically fences the parse-before-
    /// forward preflight in `request()`.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_parse_error_stays_typed_with_warm_daemon_available() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

        let server = make_daemon_save_to_test_server();
        let daemon_server = server.clone();
        let handle = tokio::spawn(async move {
            let _ = khive_runtime::daemon::run_daemon(daemon_server).await;
        });
        connect_when_daemon_ready(&sock).await;

        let error = server
            .request(
                Parameters(RequestParams {
                    ops: "stats(".to_string(),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect_err("malformed DSL must be rejected before forwarding");
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(
            error.data.as_ref().and_then(|data| data["reason"].as_str()),
            Some("parse-error")
        );

        // Prove this was the normal warm-daemon environment, not a no-daemon
        // fallback that happened to retain the local error shape.
        server
            .request(
                Parameters(RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("valid follow-up must dispatch through the warm daemon");

        handle.abort();
        let _ = handle.await;
        clear_daemon_env();
    }

    // ── #644 regression: ambiguous post-write outcome must not double-dispatch ──
    //
    // `request()`'s daemon-forward call site (`if let Some(res) = forward_or_spawn(...)
    // .await { return res; }`) must return BOTH `Some(Ok(_))` and `Some(Err(_))`
    // directly, never falling through to `dispatch_request_wire` on the `Err`
    // arm. If a future edit narrowed that match to only short-circuit on
    // success (e.g. `if let Some(Ok(res)) = ...`), a mutating op whose real
    // frame was already written to a now-dead daemon would ALSO run through
    // local dispatch — a duplicate execution of the exact case #644 exists to
    // prevent. This forces that ambiguous outcome (a fake socket that reads
    // the request then closes without responding, exactly as a daemon crash
    // mid-dispatch would) and proves both that the caller sees the
    // ambiguous-forward error verbatim AND that the mutating op never actually
    // ran locally.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_returns_ambiguous_forward_error_without_local_double_dispatch() {
        clear_daemon_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("khived.sock");
        let pid = dir.path().join("khived.pid");
        std::env::set_var("KHIVE_SOCKET", &sock);
        std::env::set_var("KHIVE_PID", &pid);
        std::env::remove_var("KHIVE_NO_DAEMON");

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

        // Fake "crashed daemon": accept exactly one connection, read the
        // request frame (the real write #644 cares about), then drop the
        // stream without writing a response.
        let listener =
            tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
        let fake_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = khive_runtime::daemon::read_frame(&mut stream).await;
            }
        });

        let baseline = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("baseline stats() must succeed");

        let resp = server
            .request(
                Parameters(RequestParams {
                    ops: "comm.send(to=\"bob\", content=\"double-forward-probe\")".to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;

        match resp {
            Err(McpError { message, .. }) => {
                assert!(
                    message.contains(
                        "not retrying or locally dispatching to avoid duplicate execution"
                    ),
                    "request() must surface forward_or_spawn's ambiguous-forward error \
                     verbatim, not a local dispatch result; got: {message}"
                );
            }
            Ok(v) => panic!(
                "request() must return the ambiguous-forward error directly, not fall \
                 through to local dispatch; got Ok({v})"
            ),
        }

        let after = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("post-request stats() must succeed");

        assert_eq!(
            stats_without_request_local_usage(&after),
            stats_without_request_local_usage(&baseline),
            "the comm.send op must NEVER have run locally after the ambiguous \
             forward outcome — a double-dispatch would mutate local state here"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;
        clear_daemon_env();
    }

    // ── #947 Medium regression: strict fallback lands as a per-op envelope ──
    //
    // Before this fix, `request()` returned `forward_or_spawn`'s strict-mode
    // rejection as a raw `Err(McpError)`, bypassing the per-op `{ok, tool,
    // result/error}` / `summary` wire contract every other failure mode goes
    // through. This drives `request()` end to end with a genuinely
    // unreachable daemon under `KHIVE_DAEMON_STRICT=1` and asserts: (1) the
    // response is `Ok(envelope_json)`, never an RPC error; (2) each shape
    // (single op, parallel batch, chain) reports the fallback reason as a
    // normal failed-op `error`, with chain aborting the remaining ops exactly
    // like a real op failure would (`run_parsed`'s `Chain` arm); (3) summary
    // counts match `results`; and (4) none of the ops ever ran locally (a
    // `stats()` snapshot taken via the trusted `dispatch_request_local` path
    // is unchanged after all three calls).
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn request_strict_fallback_lands_as_failed_op_envelope_not_rpc_error() {
        clear_daemon_env();
        crate::daemon::reset_fallback_counters();
        let dir = tempfile::tempdir().expect("tempdir");
        // Never bound by anything in this test. The spawned test harness exits
        // immediately on `mcp --daemon`, so #898 classifies this as a confirmed
        // respawn failure rather than the older generic `no_socket` fallback.
        std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
        std::env::remove_var("KHIVE_NO_DAEMON");
        std::env::set_var("KHIVE_DAEMON_STRICT", "1");

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

        let baseline = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("baseline stats() must succeed");

        fn assert_fallback_error(entry: &Value, tool: &str) {
            assert_eq!(entry["ok"], json!(false), "entry: {entry}");
            assert_eq!(entry["tool"], json!(tool), "entry: {entry}");
            let msg = entry["error"].as_str().expect("error must be a string");
            assert!(
                msg.contains("KHIVE_DAEMON_STRICT"),
                "error must name the strict mode that rejected the fallback: {msg}"
            );
            assert!(
                msg.contains("respawn_failed"),
                "error must name the confirmed respawn failure: {msg}"
            );
            assert!(
                msg.contains("make local"),
                "error must include the safe respawn remediation: {msg}"
            );
        }

        // ── single op ──────────────────────────────────────────────────────
        let single_resp = server
            .request(
                Parameters(RequestParams {
                    ops: "comm.send(to=\"bob\", content=\"strict-single-probe\")".to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let single: Value =
            serde_json::from_str(&single_resp).expect("response must be the request envelope");
        assert_eq!(
            single["results"].as_array().expect("results array").len(),
            1
        );
        assert_fallback_error(&single["results"][0], "comm.send");
        assert_eq!(
            single["summary"],
            json!({ "total": 1, "succeeded": 0, "failed": 1, "aborted": 0 })
        );

        // ── parallel batch ─────────────────────────────────────────────────
        let batch_resp = server
            .request(
                Parameters(RequestParams {
                    ops: "[comm.send(to=\"bob\", content=\"strict-batch-1\"), \
                       comm.send(to=\"bob\", content=\"strict-batch-2\")]"
                        .to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let batch: Value =
            serde_json::from_str(&batch_resp).expect("response must be the request envelope");
        let batch_results = batch["results"].as_array().expect("results array");
        assert_eq!(batch_results.len(), 2);
        for entry in batch_results {
            assert_fallback_error(entry, "comm.send");
        }
        assert_eq!(
            batch["summary"],
            json!({ "total": 2, "succeeded": 0, "failed": 2, "aborted": 0 })
        );

        // ── chain (must abort remaining ops per the wire contract) ─────────
        let chain_resp = server
            .request(
                Parameters(RequestParams {
                    ops: "comm.send(to=\"bob\", content=\"strict-chain-1\") | \
                      comm.send(to=\"bob\", content=\"strict-chain-2\")"
                        .to_string(),
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
        let chain: Value =
            serde_json::from_str(&chain_resp).expect("response must be the request envelope");
        let chain_results = chain["results"].as_array().expect("results array");
        assert_eq!(chain_results.len(), 2);
        assert_fallback_error(&chain_results[0], "comm.send");
        assert_eq!(
            chain_results[1],
            json!({ "ok": false, "tool": "comm.send", "aborted": true })
        );
        assert_eq!(
            chain["summary"],
            json!({ "total": 2, "succeeded": 0, "failed": 1, "aborted": 1 })
        );

        // ── no local dispatch ever happened for any of the three calls ─────
        let after = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("post-request stats() must succeed");
        assert_eq!(
            stats_without_request_local_usage(&after),
            stats_without_request_local_usage(&baseline),
            "no comm.send op must ever have run locally under strict-mode fallback \
             rejection — a local dispatch would mutate local state here"
        );

        crate::daemon::reset_fallback_counters();
        clear_daemon_env();
    }

    // ── #1220: top-level `status` distinguishes a partially-failed batch ──────

    fn in_memory_kg_server() -> KhiveMcpServer {
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
        KhiveMcpServer::new(runtime).expect("server builds with kg")
    }

    /// ADR-130 §1: the KG single-backend (no coordinator) envelope must also
    /// carry `status="complete"` on every successful `search` — both for a
    /// genuine no-match and a populated result — with no possible "partial"
    /// state for a lone backend. Other verbs must not gain a `status` field.
    #[tokio::test]
    async fn single_backend_search_reports_status_complete() {
        let server = in_memory_kg_server();

        let resp = server
            .dispatch_request_local(RequestParams {
                ops: r#"search(kind="entity", query="nothing here")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("search dispatch must succeed");
        let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
        let search = &parsed["results"][0];
        assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
        assert_eq!(search["status"], json!("complete"));
        assert_eq!(search["result"], json!([]));
        assert!(search.get("partial").is_none());

        // Chain (`|`), not a parallel batch: `search` must observe the
        // preceding `create`, which an independent-ops batch does not
        // guarantee (bounded-concurrency ops have no relative ordering).
        let resp = server
            .dispatch_request_local(RequestParams {
                ops: "create(kind=\"entity\", entity_kind=\"concept\", name=\"kg-search-status\") \
                       | search(kind=\"entity\", query=\"kg-search-status\")"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("chain dispatch must succeed");
        let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
        let create = &parsed["results"][0];
        let search = &parsed["results"][1];
        assert!(
            create.get("status").is_none(),
            "non-search verbs must not gain a status field: {create}"
        );
        assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
        assert_eq!(search["status"], json!("complete"));
        assert!(
            search["result"]
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false),
            "unexpected response: {search}"
        );
    }

    // ── MAJ-3: explicit-namespace narrowing arm of `coordinator_search_visibility` ──

    fn registry_with_visible_namespaces(ns: Vec<khive_runtime::Namespace>) -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.with_visible_namespaces(ns);
        builder.build().expect("build registry with no packs")
    }

    fn request_identity_with_visible_namespaces(ns: Vec<&str>) -> khive_runtime::RequestIdentity {
        khive_runtime::RequestIdentity {
            namespace: "local".to_string(),
            actor_id: None,
            visible_namespaces: ns.into_iter().map(str::to_string).collect(),
            process_ref: None,
            request_id: None,
        }
    }

    /// No per-request identity: falls back to the registry's operator-baked
    /// `visible_namespaces`, widened with `local` — mirrors the normal
    /// registry dispatch path's default-case widening.
    #[test]
    fn coordinator_search_visibility_widens_to_registry_defaults_when_no_identity() {
        let registry =
            registry_with_visible_namespaces(vec![
                khive_runtime::Namespace::parse("tenant-a").unwrap()
            ]);
        let extra = coordinator_search_visibility(&registry, &json!({}), None);
        assert!(
            extra.contains(&khive_runtime::Namespace::parse("tenant-a").unwrap()),
            "must widen to the registry's baked visible_namespaces: {extra:?}"
        );
        assert!(
            extra.contains(&khive_runtime::Namespace::local()),
            "must always include local: {extra:?}"
        );
    }

    /// A per-request identity's `visible_namespaces` overrides the registry's
    /// baked defaults entirely (ADR-096 Fork 1) — the registry's "tenant-a"
    /// must NOT leak into a request identity scoped to "tenant-b" only.
    #[test]
    fn coordinator_search_visibility_widens_to_identity_visible_namespaces() {
        let registry =
            registry_with_visible_namespaces(vec![
                khive_runtime::Namespace::parse("tenant-a").unwrap()
            ]);
        let identity = request_identity_with_visible_namespaces(vec!["tenant-b"]);
        let extra = coordinator_search_visibility(&registry, &json!({}), Some(&identity));
        assert!(
            extra.contains(&khive_runtime::Namespace::parse("tenant-b").unwrap()),
            "must widen to the per-request identity's visible_namespaces: {extra:?}"
        );
        assert!(
            !extra.contains(&khive_runtime::Namespace::parse("tenant-a").unwrap()),
            "must NOT fall back to the registry's baked defaults when an identity is \
             present: {extra:?}"
        );
        assert!(
            extra.contains(&khive_runtime::Namespace::local()),
            "must always include local: {extra:?}"
        );
    }

    /// An explicit `namespace=` request argument intentionally narrows
    /// visibility to that one namespace — the coordinator boundary must
    /// return an unwidened empty extra-visibility set in that case, exactly
    /// like the normal registry dispatch path's `explicit_namespace` branch.
    ///
    /// RED before the fix: an explicit namespace still widened visibility to
    /// the caller's full `visible_namespaces` set, silently overriding the
    /// caller's intended narrowing.
    #[test]
    fn coordinator_search_visibility_narrows_to_empty_when_namespace_explicit() {
        let registry =
            registry_with_visible_namespaces(vec![
                khive_runtime::Namespace::parse("tenant-a").unwrap()
            ]);
        let identity = request_identity_with_visible_namespaces(vec!["tenant-b"]);
        let extra = coordinator_search_visibility(
            &registry,
            &json!({"namespace": "tenant-c"}),
            Some(&identity),
        );
        assert!(
            extra.is_empty(),
            "an explicit namespace= argument must narrow to an empty extra-visible \
             set, not widen: {extra:?}"
        );
    }

    #[tokio::test]
    async fn unknown_verb_with_invalid_namespace_is_not_classified_as_verb_refused() {
        let server = in_memory_kg_server();
        let response = server
            .dispatch_request_local(RequestParams {
                ops: "not_loaded(namespace=5)".to_string(),
                ..Default::default()
            })
            .await
            .expect("dispatch failures remain in the per-operation envelope");
        let response: Value = serde_json::from_str(&response).expect("response envelope");
        assert!(
            response["results"][0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("invalid namespace")),
            "unexpected error: {response}"
        );
        assert!(
            response["results"][0].get("reason").is_none(),
            "namespace validation is not an unknown-verb refusal: {response}"
        );
    }

    #[tokio::test]
    async fn request_status_is_success_when_every_op_in_batch_succeeds() {
        let server = in_memory_kg_server();
        let resp = server
            .dispatch_request_local(RequestParams {
                ops: "[create(kind=\"entity\", entity_kind=\"concept\", name=\"status-ok-1\"), \
                       create(kind=\"entity\", entity_kind=\"concept\", name=\"status-ok-2\")]"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("batch dispatch must succeed");
        let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
        assert_eq!(parsed["summary"]["failed"], 0);
        assert_eq!(
            parsed["status"], "success",
            "an all-succeeding batch must report status=success; got {parsed}"
        );
    }

    #[tokio::test]
    async fn request_status_is_partial_when_a_batch_op_fails() {
        let server = in_memory_kg_server();
        // The second op targets an unknown kind and fails; the first succeeds.
        let resp = server
            .dispatch_request_local(RequestParams {
                ops:
                    "[create(kind=\"entity\", entity_kind=\"concept\", name=\"status-partial-1\"), \
                       search(kind=\"not_a_real_kind\", query=\"x\")]"
                        .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("batch dispatch must succeed at the RPC level even with a failed op");
        let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
        assert!(
            parsed["summary"]["failed"].as_u64().unwrap_or(0) > 0,
            "expected at least one failed op; got {parsed}"
        );
        assert_eq!(
            parsed["status"], "partial",
            "a batch with a failed op must report status=partial; got {parsed}"
        );
    }

    #[tokio::test]
    async fn request_status_is_partial_when_a_chain_op_is_aborted() {
        let server = in_memory_kg_server();
        let resp = server
            .dispatch_request_local(RequestParams {
                ops: "search(kind=\"not_a_real_kind\", query=\"x\") | \
                      create(kind=\"entity\", entity_kind=\"concept\", name=\"status-chain-aborted\")"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("chain dispatch must succeed at the RPC level even with an aborted op");
        let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
        assert!(
            parsed["summary"]["aborted"].as_u64().unwrap_or(0) > 0,
            "expected the second chain op to be aborted; got {parsed}"
        );
        assert_eq!(
            parsed["status"], "partial",
            "a chain with an aborted op must report status=partial; got {parsed}"
        );
    }
}
#[cfg(test)]
mod request_read_cancellation_tests {
    use std::time::Duration;

    use super::*;

    #[derive(Clone)]
    struct EofProbeServer {
        started: Arc<tokio::sync::Notify>,
        cancelled: Arc<tokio::sync::Notify>,
    }

    impl rmcp::ServerHandler for EofProbeServer {
        fn call_tool(
            &self,
            _request: rmcp::model::CallToolRequestParams,
            context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_
        {
            let started = self.started.clone();
            let cancelled = self.cancelled.clone();
            async move {
                started.notify_one();
                scope_mcp_request_read_cancellation(context.ct, async {
                    khive_storage::wait_for_request_read_cancellation().await;
                })
                .await;
                cancelled.notify_one();
                Ok(rmcp::model::CallToolResult::success(Vec::new()))
            }
        }
    }

    #[tokio::test]
    async fn stdio_eof_cancels_root_and_request_read_before_rmcp_drain() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use tokio::io::AsyncWriteExt;

        let started = Arc::new(tokio::sync::Notify::new());
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let probe = EofProbeServer {
            started: started.clone(),
            cancelled: cancelled.clone(),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let (read, write) = tokio::io::split(server_io);
        let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(read, write),
            root.clone(),
            None,
            None,
            Some(Duration::from_secs(3600)),
        );
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

        client_io
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"probe\",\"arguments\":{}}}\n",
            )
            .await
            .expect("write one real JSON-RPC tool request");
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("rmcp never admitted the request handler");

        drop(client_io);

        tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
            .await
            .expect("stdio EOF did not cancel the request read scope promptly");
        assert!(
            root.is_cancelled(),
            "EOF must cancel the exact root token passed into rmcp"
        );
        let reason = tokio::time::timeout(Duration::from_secs(2), running.waiting())
            .await
            .expect("rmcp remained in its five-second EOF drain")
            .expect("rmcp service task panicked");
        assert!(
            matches!(
                reason,
                rmcp::service::QuitReason::Closed | rmcp::service::QuitReason::Cancelled
            ),
            "unexpected rmcp quit reason after EOF: {reason:?}"
        );
    }

    /// An idle stdio bridge — pipe still open, no request sent — must
    /// be reaped the same way a real EOF is, not held open indefinitely.
    #[tokio::test]
    async fn stdio_idle_timeout_cancels_root_without_eof() {
        use rmcp::transport::async_rw::AsyncRwTransport;

        let started = Arc::new(tokio::sync::Notify::new());
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let probe = EofProbeServer {
            started: started.clone(),
            cancelled: cancelled.clone(),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, client_io) = tokio::io::duplex(16 * 1024);
        let (read, write) = tokio::io::split(server_io);
        let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(read, write),
            root.clone(),
            Some(Duration::from_millis(50)),
            None,
            Some(Duration::from_secs(3600)),
        );
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

        // Deliberately never write anything and never drop `client_io`: the
        // pipe stays open exactly like an abandoned bridge's client — this
        // must still be reaped once the idle timeout elapses.
        // Observe the cancellation BEFORE consuming the service, and it has to
        // be in this order. `RunningService` holds a `dg: DropGuard`
        // (rmcp 1.8.0, `src/service.rs:712`) and `waiting(mut self)` consumes
        // `self`, so the guard cancels this very token as `waiting` returns
        // whatever the transport did. Asserting `root.is_cancelled()` after
        // that await therefore passes even with the adapter's cancel deleted:
        // it measures rmcp's drop guard, not the idle path. Awaiting
        // `root.cancelled()` while the service is still alive is the only
        // ordering that can tell the two apart.
        tokio::time::timeout(Duration::from_secs(2), root.cancelled())
            .await
            .expect("idle timeout must cancel the exact root token passed into rmcp");

        let reason = tokio::time::timeout(Duration::from_secs(2), running.waiting())
            .await
            .expect("idle timeout never closed the session")
            .expect("rmcp service task panicked");
        assert!(
            matches!(
                reason,
                rmcp::service::QuitReason::Closed | rmcp::service::QuitReason::Cancelled
            ),
            "unexpected rmcp quit reason after idle timeout: {reason:?}"
        );
        drop(client_io);
    }

    #[derive(Clone)]
    struct SlowProbeServer {
        started: Arc<tokio::sync::Notify>,
        delay: Duration,
    }

    impl rmcp::ServerHandler for SlowProbeServer {
        fn call_tool(
            &self,
            _request: rmcp::model::CallToolRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_
        {
            let started = self.started.clone();
            let delay = self.delay;
            async move {
                started.notify_one();
                tokio::time::sleep(delay).await;
                Ok(rmcp::model::CallToolResult::success(Vec::new()))
            }
        }
    }

    /// Regression: two outstanding obligations under one request id are
    /// refused, not resolved by guessing.
    ///
    /// Retirement matches on request id, so two live entries sharing an id
    /// make it ambiguous which one a completing response discharges — and both
    /// resolutions are wrong in opposite directions. Removing the first match
    /// can leave a *completed* request's instant as the newest entry, so the
    /// freshness check defers past the older obligation's TTL: the unbounded
    /// session this whole mechanism exists to close. Removing the last match
    /// can leave the older instant, so the session closes out from under a
    /// live handler. The transport cannot tell the two apart, so it refuses
    /// the second admission instead.
    ///
    /// Idle reaping is off and the pipe is deliberately never closed, so the
    /// refusal is the only thing that can end this session. A build that
    /// admitted the duplicate would leave `root` uncancelled and this test
    /// would exhaust its bound.
    #[tokio::test]
    async fn stdio_refuses_a_second_outstanding_obligation_under_one_request_id() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use tokio::io::AsyncWriteExt;

        let first_admitted = Arc::new(tokio::sync::Notify::new());
        let probe = OutOfOrderProbeServer {
            first_admitted: first_admitted.clone(),
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, client_io) = tokio::io::duplex(16 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            // No idle timer: only the duplicate-id refusal can close this.
            None,
            None,
            Some(Duration::from_secs(3600)),
        );
        let obligations = transport.in_flight_handle();
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());
        let (_client_read, mut client_write) = tokio::io::split(client_io);

        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"probe\",\"arguments\":{}}}\n";
        client_write
            .write_all(request)
            .await
            .expect("write the first request");
        tokio::time::timeout(Duration::from_secs(2), first_admitted.notified())
            .await
            .expect("rmcp never admitted the first request");

        // Premise: id 1 is genuinely still outstanding. Without this the second
        // request would just be a reuse after retirement, which is legitimate
        // and not what this test is about.
        assert_eq!(
            obligations.lock().expect("obligation queue poisoned").len(),
            1,
            "fixture premise: the first request must still be outstanding when the duplicate \
             arrives, otherwise nothing is ambiguous"
        );
        assert!(
            !root.is_cancelled(),
            "fixture premise: a single outstanding request must not have closed the session"
        );

        client_write
            .write_all(request)
            .await
            .expect("write the duplicate-id request");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.is_cancelled() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            root.is_cancelled(),
            "a second outstanding obligation under an id that already has one must close the \
             session; admitting it leaves retirement to pick an entry arbitrarily, which \
             corrupts the freshness check in one direction or the other"
        );

        drop(client_write);
        let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
    }

    /// Regression: an id whose obligation has gone STALE is still a duplicate.
    ///
    /// Staleness is a statement about the freshness check, not about the
    /// handler: rmcp keeps a spawned handler alive independently of this
    /// receive loop, so an entry past its TTL routinely names a request that
    /// is still running and has simply not answered yet. If the staleness drop
    /// runs before the duplicate scan, that entry is gone by the time the scan
    /// looks, the reused id is admitted as a fresh obligation, and the first of
    /// the two eventual responses retires the NEW entry by id match — leaving
    /// the older live handler untracked and the idle branch free to close out
    /// from under it. Scanning before pruning refuses the reuse instead.
    ///
    /// This is the arm the earlier ordering passed: the plain duplicate test
    /// above uses an hour-long TTL, so its entry is never stale and the two
    /// orderings are indistinguishable there.
    ///
    /// Idle reaping is off and the pipe is never closed, so the refusal is the
    /// only thing that can end this session.
    #[tokio::test]
    async fn stdio_refuses_a_reused_id_whose_obligation_is_already_stale() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use tokio::io::AsyncWriteExt;

        const OBLIGATION_TTL: Duration = Duration::from_millis(100);

        let first_admitted = Arc::new(tokio::sync::Notify::new());
        let probe = OutOfOrderProbeServer {
            first_admitted: first_admitted.clone(),
            // Parks forever on the first call: the handler outlives its TTL.
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, client_io) = tokio::io::duplex(16 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            // No idle timer: only the duplicate-id refusal can close this.
            None,
            None,
            Some(OBLIGATION_TTL),
        );
        let obligations = transport.in_flight_handle();
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());
        let (_client_read, mut client_write) = tokio::io::split(client_io);

        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"probe\",\"arguments\":{}}}\n";
        client_write
            .write_all(request)
            .await
            .expect("write the first request");
        tokio::time::timeout(Duration::from_secs(2), first_admitted.notified())
            .await
            .expect("rmcp never admitted the first request");

        tokio::time::sleep(OBLIGATION_TTL * 3).await;

        // Premise, asserted rather than assumed: the entry is still in the
        // queue AND the transport's own freshness rule already calls it stale.
        // Both halves matter — a test that only slept would pass against a
        // build that pruned the entry, since an empty queue also admits the
        // reuse without closing, which is the very defect under test.
        {
            let queue = obligations.lock().expect("obligation queue poisoned");
            let (id, admitted_at) = queue
                .front()
                .expect(
                    "fixture premise: the unanswered obligation must still be queued when the \
                     reused id arrives; an empty queue means something pruned it and this test \
                     can no longer distinguish the orderings",
                )
                .clone();
            assert_eq!(
                id,
                rmcp::model::NumberOrString::Number(1),
                "fixture premise: the queued entry must be the request under test"
            );
            assert!(
                admitted_at.elapsed() >= OBLIGATION_TTL,
                "fixture premise: the entry must already be STALE by the transport's own rule, \
                 otherwise this is just the plain duplicate case"
            );
        }
        assert!(
            !root.is_cancelled(),
            "fixture premise: a single outstanding request must not have closed the session"
        );

        client_write
            .write_all(request)
            .await
            .expect("write the request reusing the stale id");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.is_cancelled() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            root.is_cancelled(),
            "reusing an id whose obligation is stale but whose handler is still alive must close \
             the session. Admitting it lets the first response retire the wrong entry, which \
             leaves a live handler untracked and the idle close free to fire under it."
        );

        drop(client_write);
        let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
    }

    /// Regression: the obligation queue must not grow without bound.
    ///
    /// Retirement is keyed by request id, so it only ever removes the entry
    /// whose response was actually written. A request that never produces one
    /// — a handler that panics, a request the peer cancels — leaves its entry
    /// behind. Under the earlier oldest-first retirement any completion
    /// removed *some* entry, so the queue tracked admissions minus
    /// completions; keying by id means an unanswered request now holds its
    /// slot for the life of the session unless something drops it.
    ///
    /// Nothing here waits on the idle timer: it is disabled, so the only thing
    /// that can bound this queue is the staleness drop.
    #[tokio::test]
    async fn stdio_obligation_queue_drops_entries_past_their_ttl() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use tokio::io::AsyncWriteExt;

        const OBLIGATION_TTL: Duration = Duration::from_millis(100);
        // Longer than the TTL, so every earlier admission is already stale by
        // the time the next one arrives and the queue can never hold two.
        const ADMISSION_GAP: Duration = Duration::from_millis(150);
        const ADMISSIONS: u32 = 4;

        let probe = SlowProbeServer {
            started: Arc::new(tokio::sync::Notify::new()),
            // Never answers, so nothing is ever retired by id.
            delay: Duration::from_secs(3600),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            // Idle reaping off: this test is about the queue, not the timer.
            None,
            None,
            Some(OBLIGATION_TTL),
        );
        let obligations = transport.in_flight_handle();
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

        for id in 1..=ADMISSIONS {
            client_io
                .write_all(
                    format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\
                         \"params\":{{\"name\":\"probe\",\"arguments\":{{}}}}}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write request");
            tokio::time::sleep(ADMISSION_GAP).await;
        }

        let outstanding = obligations.lock().expect("obligation queue poisoned").len();
        assert!(
            outstanding <= 1,
            "an unanswered request must not hold its obligation slot past the TTL: after \
             {ADMISSIONS} admissions spaced {}ms apart against a {}ms TTL the queue holds \
             {outstanding} entries. Without the staleness drop it would hold {ADMISSIONS}, one \
             per admission, and would keep growing for as long as the session lives.",
            ADMISSION_GAP.as_millis(),
            OBLIGATION_TTL.as_millis(),
        );

        drop(client_io);
        let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
    }

    /// Regression: a peer that keeps sending requests without consuming
    /// responses cannot make the transport's outstanding state grow without
    /// limit. The third request is rejected before rmcp can spawn its handler.
    #[tokio::test]
    async fn stdio_closes_when_outstanding_request_limit_is_reached() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use tokio::io::AsyncWriteExt;

        const MAX_OUTSTANDING: usize = 2;
        let probe = SlowProbeServer {
            started: Arc::new(tokio::sync::Notify::new()),
            delay: Duration::from_secs(3600),
        };
        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let transport =
            crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
                AsyncRwTransport::new_server(server_read, server_write),
                root.clone(),
                None,
                None,
                Some(Duration::from_secs(3600)),
                MAX_OUTSTANDING,
            );
        let obligations = transport.in_flight_handle();
        let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

        for id in 1..=MAX_OUTSTANDING + 1 {
            let request = format!(
                "{}\n",
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/call",
                    "params": {"name": "probe", "arguments": {}}
                })
            );
            client_io
                .write_all(request.as_bytes())
                .await
                .expect("write request");
        }

        tokio::time::timeout(Duration::from_secs(2), root.cancelled())
            .await
            .expect("the transport must close after reaching its admission limit");
        assert_eq!(
            obligations.lock().expect("obligation queue poisoned").len(),
            MAX_OUTSTANDING,
            "the rejected request must not enter the outstanding tracker"
        );

        drop(client_io);
        let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
    }

    /// Parks forever on the first call and answers every later one promptly,
    /// so a test can produce out-of-order response completion: the request
    /// admitted first stays outstanding while a later one completes.
    #[derive(Clone)]
    struct OutOfOrderProbeServer {
        first_admitted: Arc<tokio::sync::Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl rmcp::ServerHandler for OutOfOrderProbeServer {
        fn call_tool(
            &self,
            _request: rmcp::model::CallToolRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_
        {
            let first_admitted = self.first_admitted.clone();
            let calls = self.calls.clone();
            async move {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    first_admitted.notify_one();
                    // Never answers. This is the older obligation, and it is
                    // the one whose TTL must govern when the session closes.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                Ok(rmcp::model::CallToolResult::success(Vec::new()))
            }
        }
    }

    /// Distinguishes handler behavior by tool name: `"slow"` blocks for
    /// `slow_delay` (keeping `in_flight` above zero across several idle
    /// windows), `"quick"` (or anything else) completes immediately — used
    /// to admit a second request while the first is still running.
    #[tokio::test]
    async fn rmcp_cancellation_token_reaches_request_read_scope() {
        let token = tokio_util::sync::CancellationToken::new();
        let token_for_scope = token.clone();
        let observed = tokio::spawn(async move {
            scope_mcp_request_read_cancellation(token_for_scope, async {
                khive_storage::wait_for_request_read_cancellation().await;
                true
            })
            .await
        });

        token.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), observed)
                .await
                .expect("rmcp cancellation never reached the read scope")
                .expect("scope task panicked")
        );
    }

    #[tokio::test]
    async fn already_cancelled_rmcp_token_is_visible_without_yielding() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();

        let observed = scope_mcp_request_read_cancellation(token, async {
            khive_storage::request_read_is_cancelled()
        })
        .await;

        assert!(
            observed,
            "a synchronously-ready request raced past a pre-cancelled rmcp context"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn request_tool_path_honors_an_already_cancelled_rmcp_token() {
        std::env::set_var("KHIVE_NO_DAEMON", "1");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();

        let response = server
            .request(
                Parameters(RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                }),
                cancellation,
            )
            .await;
        std::env::remove_var("KHIVE_NO_DAEMON");

        match response {
            Err(error) => {
                let rendered = format!("{error:?}").to_ascii_lowercase();
                assert!(
                    rendered.contains("timeout") || rendered.contains("cancel"),
                    "cancelled request returned an unrelated RPC error: {rendered}"
                );
            }
            Ok(payload) => {
                let parsed: Value = serde_json::from_str(&payload).expect("JSON response envelope");
                assert!(
                    parsed["summary"]["failed"].as_u64().unwrap_or(0) > 0,
                    "the actual request tool path ignored its cancelled token: {parsed}"
                );
            }
        }
    }

    // ── KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 must not disable the bound ──

    /// The response-delivery deadline used to treat `0` as "disable
    /// the bound", mirroring `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS=0`. Unlike the
    /// idle timeout, an unbounded response-delivery deadline restores the
    /// exact defect this deadline exists to close (a peer that admits a
    /// request and stops reading pins the bridge's response write forever)
    /// — so `0` is now a hard startup error instead of a supported opt-out.
    #[test]
    #[serial_test::serial]
    fn response_deadline_from_env_rejects_zero() {
        std::env::set_var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS", "0");
        let result = stdio_bridge_response_deadline_from_env();
        std::env::remove_var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS");

        let error = result.expect_err(
            "KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 must be rejected, not silently accepted \
             as \"disable the bound\"",
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS"),
            "error must name the offending variable: {rendered}"
        );
        assert!(
            rendered.contains("=0"),
            "error must name the rejected value: {rendered}"
        );
        assert!(
            rendered.contains("1..=u64::MAX"),
            "error must state the accepted range: {rendered}"
        );
    }

    /// The response-delivery deadline had no test that ever let it elapse:
    /// the only coverage rejected `0` at startup, which exercises the parser
    /// and not the bound. A bound whose expiry is never observed is a claim,
    /// so this drives a real write against a peer that has stopped reading.
    #[tokio::test]
    async fn response_write_past_its_deadline_is_abandoned_and_closes_the_session() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;

        // A 16-byte pipe that nobody reads. `_client_io` is held, not dropped,
        // so this is a peer that is present and simply not reading — the case
        // the deadline exists for. Dropping it would produce a broken pipe
        // instead, which is a different failure and already handled.
        let (server_io, _client_io) = tokio::io::duplex(16);
        let (read, write) = tokio::io::split(server_io);
        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(read, write),
            root.clone(),
            None,
            Some(Duration::from_millis(50)),
            None,
        );

        // An error response is a response for this purpose: `send` bounds
        // `Response` and `Error` alike, because either one can leave the write
        // pinned.
        let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            id: Some(rmcp::model::RequestId::Number(1)),
            error: rmcp::model::ErrorData::internal_error("x".repeat(4096), None),
        });

        let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
            .await
            .expect("the deadline must resolve the write; it hung instead")
            .expect_err("a write that outlived its deadline must not report success");

        assert!(
            error.to_string().contains("deadline"),
            "the error must name the deadline that abandoned the write: {error}"
        );
        assert!(
            root.is_cancelled(),
            "a peer that stops reading must close the session, not just fail one write"
        );
    }

    /// Discriminating arm for the test above. Without this, that test would
    /// also pass against a deadline that fired on every response regardless of
    /// whether the peer was reading.
    #[tokio::test]
    async fn response_write_inside_its_deadline_succeeds_and_leaves_the_session_open() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;
        use tokio::io::AsyncReadExt;

        let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
        let (read, write) = tokio::io::split(server_io);
        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(read, write),
            root.clone(),
            None,
            Some(Duration::from_secs(30)),
            None,
        );

        // This peer reads.
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let _ = client_io.read(&mut buf).await;
            buf
        });

        let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            id: Some(rmcp::model::RequestId::Number(1)),
            error: rmcp::model::ErrorData::internal_error("read by the peer", None),
        });

        tokio::time::timeout(Duration::from_secs(2), transport.send(message))
            .await
            .expect("a write to a reading peer must not hit the 2s test bound")
            .expect("a write to a reading peer must succeed");

        assert!(
            !root.is_cancelled(),
            "a response delivered inside its deadline must not close the session"
        );
        let _ = reader.await;
    }

    /// The deadline bounds a write left PENDING. It says nothing about a write
    /// that fails immediately, which is what a half-closed peer produces: one
    /// that closes the side it reads from while keeping the side it writes to
    /// open. rmcp does not close the session for us there — the response-send
    /// task logs the error and returns (`rmcp-1.8.0` `src/service.rs:1105-1112`)
    /// — the receive loop stays pending on the still-open read side, and idle
    /// reaping is off by default, so before this the session outlived the
    /// peer's ability to receive anything from it.
    ///
    /// Two independent pipes, because a single `duplex` cannot be half-closed:
    /// dropping either end closes both directions, and `tokio::io::split`
    /// keeps the stream alive until both halves drop. Separate pipes for the
    /// read source and the write sink are what let the peer close exactly one.
    #[tokio::test]
    async fn response_write_that_fails_fast_closes_the_session() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;

        // peer -> server: held open and silent, so the receive side would stay
        // pending forever. This is what makes the missing cancel a leak rather
        // than a race with EOF.
        let (server_read, _peer_write_side) = tokio::io::duplex(1024);
        // server -> peer: the peer has closed the side it reads from, so every
        // write fails with BrokenPipe.
        let (server_write, peer_read_side) = tokio::io::duplex(1024);
        drop(peer_read_side);

        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            None,
            // Long enough that the deadline cannot be what resolves this
            // write. If the fix were the deadline rather than the error path,
            // the 2s bound below would trip instead of the assertion.
            Some(Duration::from_secs(30)),
            None,
        );

        let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            id: Some(rmcp::model::RequestId::Number(1)),
            error: rmcp::model::ErrorData::internal_error("peer closed its read side", None),
        });

        let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
            .await
            .expect("a write to a closed pipe must fail fast, not wait out its deadline")
            .expect_err("a write to a closed pipe must not report success");

        assert!(
            !error.to_string().contains("deadline"),
            "this must exercise the error path, not the deadline path: {error}"
        );
        assert!(
            root.is_cancelled(),
            "a response that could not be written must close the session; rmcp only logs it"
        );
    }

    /// A failed NOTIFICATION write closes the session too, and this test used
    /// to assert the opposite. The rule was scoped to responses on the ground
    /// that rmcp carried its own accounting for server-initiated messages. It
    /// does carry accounting and the accounting does not close anything: a
    /// failed notification send delivers `ServiceError::TransportSend` to the
    /// notification's own responder (`rmcp-1.8.0` `src/service.rs:1074-1093`)
    /// and a failed request send does the same to the caller's responder
    /// (`:1066-1073`), while the serve loop's only exits are receive EOF, token
    /// cancellation, and a send-task join error (`:1028-1062`). So the same
    /// broken writer strands the session whatever class of message hit it.
    ///
    /// The discriminating arm is now
    /// `notification_write_to_a_reading_peer_leaves_the_session_open`: without
    /// it, this test would pass against a transport that cancelled on every
    /// write, successful ones included.
    #[tokio::test]
    async fn notification_write_that_fails_fast_also_closes_the_session() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;

        let (server_read, _peer_write_side) = tokio::io::duplex(1024);
        let (server_write, peer_read_side) = tokio::io::duplex(1024);
        drop(peer_read_side);

        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            None,
            Some(Duration::from_secs(30)),
            None,
        );

        let message = rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            notification: rmcp::model::ServerNotification::ProgressNotification(
                rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                    progress_token: rmcp::model::ProgressToken(
                        rmcp::model::NumberOrString::Number(1),
                    ),
                    progress: 1.0,
                    total: None,
                    message: None,
                }),
            ),
        });

        let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
            .await
            .expect("a write to a closed pipe must fail fast, not wait out its deadline")
            .expect_err("a write to a closed pipe must not report success");

        assert!(
            !error.to_string().contains("deadline"),
            "this must exercise the error path, not the deadline path: {error}"
        );
        assert!(
            root.is_cancelled(),
            "a notification that could not be written must close the session; rmcp hands the \
             error to the notification's own responder and never breaks the serve loop"
        );
    }

    /// Discriminating arm for the two fail-fast tests above. Without it, both
    /// would pass against a transport that cancelled on every write rather than
    /// on every FAILED write, which is a far worse rule than either.
    #[tokio::test]
    async fn notification_write_to_a_reading_peer_leaves_the_session_open() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;
        use tokio::io::AsyncReadExt;

        let (server_read, _peer_write_side) = tokio::io::duplex(1024);
        let (server_write, mut peer_read_side) = tokio::io::duplex(16 * 1024);

        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            None,
            Some(Duration::from_secs(30)),
            None,
        );

        // This peer reads.
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let _ = peer_read_side.read(&mut buf).await;
            buf
        });

        let message = rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            notification: rmcp::model::ServerNotification::ProgressNotification(
                rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                    progress_token: rmcp::model::ProgressToken(
                        rmcp::model::NumberOrString::Number(1),
                    ),
                    progress: 1.0,
                    total: None,
                    message: None,
                }),
            ),
        });

        tokio::time::timeout(Duration::from_secs(2), transport.send(message))
            .await
            .expect("a write to a reading peer must not hit the 2s test bound")
            .expect("a write to a reading peer must succeed");

        assert!(
            !root.is_cancelled(),
            "a delivered notification must not close the session"
        );
        let _ = reader.await;
    }

    /// A writer whose first flush is interrupted and which works from then on.
    ///
    /// This is the shape Tokio's blocking stdout adapter presents on EINTR: it
    /// restores its idle state and puts the writer back before returning the
    /// flush error (`tokio-1.52.4/src/io/blocking.rs:146-176`), and its
    /// `uninterruptibly!` retry macro (`:183-192`) is not applied to that
    /// branch. Writes are accepted and discarded, because what this arm asserts
    /// is the session's fate, not the bytes.
    struct InterruptOnceWriter {
        interrupted_yet: bool,
    }

    impl tokio::io::AsyncWrite for InterruptOnceWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.interrupted_yet {
                return std::task::Poll::Ready(Ok(()));
            }
            self.interrupted_yet = true;
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "flush interrupted by a signal",
            )))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The exception to "every failed outbound write closes the session", and
    /// it is narrower than the writer's health alone.
    ///
    /// An interrupted flush leaves the writer usable, so closing on it would
    /// trade a lost message for a lost session. The second send is what makes
    /// that claim rather than assuming it: if the transport were dead the
    /// assertion could not distinguish a correct decision from a lucky one.
    ///
    /// This arm uses a NOTIFICATION because the exception is scoped to the
    /// classes whose loss someone can observe. See
    /// `an_interrupted_response_still_closes_the_session` for the other side of
    /// that boundary, which is the arm that would go red if the scope were
    /// dropped.
    #[tokio::test]
    async fn an_interrupted_write_leaves_the_session_open_and_the_writer_usable() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;

        let (server_read, _peer_write_side) = tokio::io::duplex(1024);
        let writer = InterruptOnceWriter {
            interrupted_yet: false,
        };

        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, writer),
            root.clone(),
            None,
            Some(Duration::from_secs(30)),
            None,
        );

        let notification = || {
            rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
                jsonrpc: rmcp::model::JsonRpcVersion2_0,
                notification: rmcp::model::ServerNotification::ProgressNotification(
                    rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                        progress_token: rmcp::model::ProgressToken(
                            rmcp::model::NumberOrString::Number(1),
                        ),
                        progress: 1.0,
                        total: None,
                        message: None,
                    }),
                ),
            })
        };

        let error = tokio::time::timeout(Duration::from_secs(2), transport.send(notification()))
            .await
            .expect("an interrupted flush must resolve, not hang")
            .expect_err("an interrupted flush must be reported as an error");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::Interrupted,
            "this arm must exercise the interrupted class, not some other failure: {error}"
        );
        assert!(
            !root.is_cancelled(),
            "an interrupted write must not close the session; the writer is still usable"
        );

        tokio::time::timeout(Duration::from_secs(2), transport.send(notification()))
            .await
            .expect("the second write must resolve, not hang")
            .expect("the writer is usable after an interrupted flush, so the next write succeeds");
        assert!(
            !root.is_cancelled(),
            "a successful write after an interrupted one must leave the session open"
        );
    }

    /// The boundary of that exception: a usable writer is not enough when the
    /// message was a RESPONSE.
    ///
    /// The interrupted flush leaves the writer able to carry the next message,
    /// exactly as in the notification arm, so this test differs from that one
    /// in the message class and nothing else. The outcome differs because rmcp
    /// treats the classes differently. A failed notification or server-initiated
    /// request send reaches a local responder (`rmcp-1.8.0`
    /// `src/service.rs:1074-1093` and `:1066-1073`), so something in the process
    /// learns the message was lost. A failed response send is only logged
    /// (`:1095-1112`): nothing goes to the peer, no local caller is waiting, and
    /// the serve loop keeps running. The client that asked the question would
    /// wait on an answer that is not coming and could not tell that from a slow
    /// one. Closing is what turns that into an EOF it can act on.
    #[tokio::test]
    async fn an_interrupted_response_still_closes_the_session() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport;

        let (server_read, _peer_write_side) = tokio::io::duplex(1024);
        let writer = InterruptOnceWriter {
            interrupted_yet: false,
        };

        let root = tokio_util::sync::CancellationToken::new();
        let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, writer),
            root.clone(),
            None,
            // Long enough that the deadline cannot be what resolves this write,
            // so a pass here cannot come from the timeout path.
            Some(Duration::from_secs(30)),
            None,
        );

        let response = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            id: Some(rmcp::model::RequestId::Number(1)),
            error: rmcp::model::ErrorData::internal_error("answering a request", None),
        });

        let error = tokio::time::timeout(Duration::from_secs(2), transport.send(response))
            .await
            .expect("an interrupted flush must resolve, not hang")
            .expect_err("an interrupted flush must be reported as an error");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::Interrupted,
            "this arm must exercise the interrupted class, not some other failure: {error}"
        );
        assert!(
            root.is_cancelled(),
            "an interrupted RESPONSE must still close the session: the writer's health does not \
             help a peer that is waiting on an answer rmcp will only log the loss of"
        );
    }
}
