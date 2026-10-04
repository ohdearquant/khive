use std::collections::BTreeMap;

use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::{RuntimeError, VerbRegistry};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::coordinator::{BackendSearchFailureKind, CoordSearchResult};

pub(super) const MAX_BACKEND_ERROR_ENTRIES: usize = 16;
pub(super) const MAX_BACKEND_ERROR_KEY_CHARS: usize = 256;
pub(super) const MAX_BACKEND_ERROR_MESSAGE_CHARS: usize = 1_024;
/// A legal request carries at most `MAX_OPS` operation envelopes. Reserving a
/// quarter of the daemon frame for their mandatory diagnostic metadata leaves
/// another quarter for JSON escaping and half for fixed envelope fields.
pub(super) const MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP: usize =
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
pub(super) const MISSING_BACKEND_ERROR_MESSAGE: &str =
    "backend search failed without diagnostic detail";
/// Server-named retry pace for ADR-130 Amendment 2. One failed backend uses
/// the published 2s floor; a wider all-timeout outage adds 250ms per extra
/// failed leg, capped at 10s. The full failure set (including omitted
/// diagnostics) controls the value.
const SEARCH_RETRY_AFTER_FLOOR_MS: u64 = 2_000;
const SEARCH_RETRY_AFTER_PER_FAILED_BACKEND_MS: u64 = 250;
const SEARCH_RETRY_AFTER_CEILING_MS: u64 = 10_000;

pub(super) fn search_retry_after_ms(failed_backend_count: usize) -> u64 {
    let extra_backends = u64::try_from(failed_backend_count.saturating_sub(1)).unwrap_or(u64::MAX);
    SEARCH_RETRY_AFTER_FLOOR_MS
        .saturating_add(extra_backends.saturating_mul(SEARCH_RETRY_AFTER_PER_FAILED_BACKEND_MS))
        .min(SEARCH_RETRY_AFTER_CEILING_MS)
}

/// Per-operation completeness discriminator for the `search` verb (ADR-130
/// §1). `SearchDegradation::status == None` means "not a search op" — no
/// `status` field is emitted on that operation's envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchStatus {
    Complete,
    Partial,
}

impl SearchStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchArmStatus {
    Ran,
    Skipped,
    Error,
}

impl SearchArmStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::Skipped => "skipped",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SearchArmEvidence {
    pub(super) status: SearchArmStatus,
    pub(super) candidate_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SearchArmParticipation {
    pub(super) text: SearchArmEvidence,
    pub(super) vector: SearchArmEvidence,
    pub(super) text_mode: &'static str,
}

impl SearchArmParticipation {
    fn complete(vector_selected: bool) -> Self {
        Self {
            text: SearchArmEvidence {
                status: SearchArmStatus::Ran,
                candidate_count: 0,
            },
            vector: SearchArmEvidence {
                status: if vector_selected {
                    SearchArmStatus::Ran
                } else {
                    SearchArmStatus::Skipped
                },
                candidate_count: 0,
            },
            text_mode: "all_terms",
        }
    }

    fn observe_result(&mut self, result: &Value) {
        let (text, vector) = search_arm_candidate_counts(result);
        self.text.candidate_count = text;
        self.vector.candidate_count = vector;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BackendErrorDiagnostic {
    pub(super) kind: BackendSearchFailureKind,
    pub(super) message: String,
    pub(super) backend_id_masked: bool,
    pub(super) backend_id_truncated: bool,
    pub(super) backend_id_chars: usize,
}

#[derive(Debug, Default)]
pub(super) struct SearchDegradation {
    pub(super) status: Option<SearchStatus>,
    pub(super) retryable: bool,
    pub(super) arm_participation: Option<SearchArmParticipation>,
    pub(super) retry_after_ms: Option<u64>,
    pub(super) missing_backends: Vec<String>,
    pub(super) backend_errors: BTreeMap<String, BackendErrorDiagnostic>,
    pub(super) backend_errors_omitted: usize,
}

impl SearchDegradation {
    /// A search op that ran to completion: single-backend/no-coordinator
    /// registry dispatch, or (in principle) a coordinator fan-out where
    /// every selected backend succeeded — `from_result` is used for the
    /// latter instead, since it also has to compute `missing_backends`.
    pub(super) fn complete(result: &Value, vector_selected: bool) -> Self {
        let (_, vector_candidates) = search_arm_candidate_counts(result);
        let mut arm_participation =
            SearchArmParticipation::complete(vector_selected || vector_candidates > 0);
        arm_participation.observe_result(result);
        Self {
            status: Some(SearchStatus::Complete),
            retryable: false,
            arm_participation: Some(arm_participation),
            retry_after_ms: None,
            missing_backends: Vec::new(),
            backend_errors: BTreeMap::new(),
            backend_errors_omitted: 0,
        }
    }

    pub(super) fn from_result(
        result: &CoordSearchResult,
        final_result: &Value,
        text_mode: &'static str,
    ) -> Self {
        let vector_selected = result
            .per_backend
            .iter()
            .any(|backend| backend.vector_selected || backend.vector_error.is_some())
            || result.entity_hits.iter().any(|hit| {
                matches!(
                    hit.source,
                    khive_runtime::SearchSource::Vector | khive_runtime::SearchSource::Both
                )
            })
            || result.note_hits.iter().any(|hit| {
                matches!(
                    hit.source,
                    khive_runtime::SearchSource::Vector | khive_runtime::SearchSource::Both
                )
            });
        // A whole-backend `error` means that backend's dispatch task failed
        // before either arm could be attributed (auth, timeout, join failure,
        // or a failed text leg — the text leg fails loud inside
        // `hybrid_search_outcome`, so a text-arm failure always surfaces
        // here). `vector_error` is populated only when the text leg
        // completed and the vector leg alone failed, so it never doubles as
        // a text-arm signal.
        let text_failed = result
            .per_backend
            .iter()
            .any(|backend| backend.error.is_some());
        let vector_failed = result.per_backend.iter().any(|backend| {
            backend.vector_error.is_some() || (backend.vector_selected && backend.error.is_some())
        });
        let mut arm_participation = SearchArmParticipation {
            text: SearchArmEvidence {
                status: if text_failed {
                    SearchArmStatus::Error
                } else {
                    SearchArmStatus::Ran
                },
                candidate_count: 0,
            },
            vector: SearchArmEvidence {
                status: if !vector_selected {
                    SearchArmStatus::Skipped
                } else if vector_failed {
                    SearchArmStatus::Error
                } else {
                    SearchArmStatus::Ran
                },
                candidate_count: 0,
            },
            text_mode,
        };
        arm_participation.observe_result(final_result);
        let failed_backend_count = result
            .per_backend
            .iter()
            .filter(|backend| backend.error.is_some())
            .count();
        let retryable = failed_backend_count > 0
            && result
                .per_backend
                .iter()
                .filter_map(|backend| backend.error.as_ref())
                .all(|failure| failure.kind == BackendSearchFailureKind::Timeout);
        let retry_after_ms = retryable.then(|| search_retry_after_ms(failed_backend_count));
        let mut candidates = BTreeMap::new();
        for (backend, failure) in result
            .per_backend
            .iter()
            .filter_map(|backend| backend.error.as_ref().map(|failure| (backend, failure)))
        {
            let (key, backend_id_masked, backend_id_truncated, backend_id_chars) =
                bounded_backend_error_key(backend.backend_id.as_str());
            candidates.insert(
                key,
                BackendErrorDiagnostic {
                    kind: failure.kind,
                    message: bounded_backend_error_message(&failure.message),
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
                retryable,
                arm_participation: Some(arm_participation),
                retry_after_ms,
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
            retryable,
            arm_participation: Some(arm_participation),
            retry_after_ms,
            missing_backends,
            backend_errors,
            backend_errors_omitted,
        }
    }

    pub(super) fn with_text_mode(mut self, text_mode: &'static str) -> Self {
        if let Some(participation) = self.arm_participation.as_mut() {
            participation.text_mode = text_mode;
        }
        self
    }

    pub(super) fn is_partial(&self) -> bool {
        self.status == Some(SearchStatus::Partial)
    }
}

fn search_arm_candidate_counts(result: &Value) -> (usize, usize) {
    result
        .as_array()
        .into_iter()
        .flatten()
        .fold((0, 0), |(text, vector), hit| {
            let source = hit.get("source").and_then(Value::as_str);
            match source {
                Some(s) if s == khive_runtime::SearchSource::Text.as_str() => (text + 1, vector),
                Some(s) if s == khive_runtime::SearchSource::Vector.as_str() => (text, vector + 1),
                Some(s) if s == khive_runtime::SearchSource::Both.as_str() => {
                    (text + 1, vector + 1)
                }
                _ => (text, vector),
            }
        })
}

pub(super) fn search_arm_participation_value(participation: SearchArmParticipation) -> Value {
    let mut value = json!({
        "text": {
            "status": participation.text.status.as_str(),
            "candidate_count": participation.text.candidate_count,
            "mode": participation.text_mode,
        },
        "vector": {
            "status": participation.vector.status.as_str(),
            "candidate_count": participation.vector.candidate_count,
        },
    });
    if participation.text.status == SearchArmStatus::Ran && participation.text.candidate_count == 0
    {
        value["text"]["reason"] = json!(if participation.text_mode == "all_terms" {
            "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms."
        } else {
            "No text candidate survived matching, filtering, fusion, and the result limit."
        });
    }
    value
}

pub(super) fn bounded_backend_error_message(message: &str) -> String {
    // Bound the masker's own input window (see `secret_gate::mask_bounded`)
    // rather than masking the full, unbounded message: cost stays
    // proportional to the shared window regardless of message length, and a
    // token straddling the window is dropped whole rather than echoed
    // unmasked.
    let result = khive_runtime::secret_gate::mask_bounded(
        khive_runtime::secret_gate::RedactionSurface::McpDiagnostic,
        message,
        khive_runtime::secret_gate::MASK_WINDOW_CHARS,
        MAX_BACKEND_ERROR_MESSAGE_CHARS,
    );
    if result.text.trim().is_empty() {
        return MISSING_BACKEND_ERROR_MESSAGE.to_string();
    }
    result.text
}

pub(super) fn bounded_backend_error_key(backend_id: &str) -> (String, bool, bool, usize) {
    let backend_id_chars = backend_id.chars().count();
    // Bound the masker's own input window (see `secret_gate::mask_bounded`)
    // rather than masking the full, unbounded id. The window doubles as the
    // output cap here — this function applies its own further prefix +
    // fingerprint bounding below.
    let result = khive_runtime::secret_gate::mask_bounded(
        khive_runtime::secret_gate::RedactionSurface::McpDiagnostic,
        backend_id,
        khive_runtime::secret_gate::MASK_WINDOW_CHARS,
        khive_runtime::secret_gate::MASK_WINDOW_CHARS,
    );
    let backend_id_masked = result.redacted || result.truncated || result.text.trim().is_empty();
    let sanitized = if result.text.trim().is_empty() {
        "masked-backend"
    } else {
        result.text.as_str()
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

pub(super) fn backend_errors_value(errors: &BTreeMap<String, BackendErrorDiagnostic>) -> Value {
    Value::Object(
        errors
            .iter()
            .map(|(backend, diagnostic)| {
                let mut value = json!({
                    "kind": diagnostic.kind.as_str(),
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

pub(super) fn search_diagnostic_value(degradation: &SearchDegradation) -> Value {
    debug_assert_eq!(
        degradation.retryable,
        degradation.retry_after_ms.is_some(),
        "retryable search failures must always carry a server-named pace"
    );
    let mut value = json!({
        "kind": "search_incomplete",
        "message": "no-match was not established because selected backends failed",
        "retryable": degradation.retryable,
        "missing_backends": degradation.missing_backends,
        "backend_errors": backend_errors_value(&degradation.backend_errors),
    });
    if let Some(participation) = degradation.arm_participation {
        value["arm_participation"] = search_arm_participation_value(participation);
    }
    if let Some(retry_after_ms) = degradation.retry_after_ms {
        value["retry_after_ms"] = json!(retry_after_ms);
    }
    if degradation.backend_errors_omitted > 0 {
        value["backend_errors_truncated"] = Value::Bool(true);
        value["backend_errors_omitted"] = json!(degradation.backend_errors_omitted);
    }
    value
}

pub(super) fn search_diagnostic_wire_len(degradation: &SearchDegradation) -> usize {
    serde_json::to_vec(&search_diagnostic_value(degradation))
        .expect("search diagnostic metadata is always serializable")
        .len()
}

pub(super) struct OpSuccess {
    pub(super) result: Value,
    pub(super) degradation: SearchDegradation,
}

impl OpSuccess {
    pub(super) fn complete(result: Value) -> Self {
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
pub(super) fn op_success_from_registry_result(
    tool: &str,
    is_help: bool,
    result: Value,
    vector_selected: bool,
    text_mode: &'static str,
) -> OpSuccess {
    if tool == "search" && !is_help {
        OpSuccess {
            degradation: SearchDegradation::complete(&result, vector_selected)
                .with_text_mode(text_mode),
            result,
        }
    } else {
        OpSuccess::complete(result)
    }
}

pub(super) fn validated_search_text_mode(
    args: &Value,
    registry: &VerbRegistry,
) -> Result<&'static str, RuntimeError> {
    let mut handler_args = args.clone();
    if let Some(fields) = handler_args.as_object_mut() {
        fields.remove("namespace");
    }
    ValidatedSearchRequest::from_value(handler_args, registry)
        .map(|request| request.text_mode_name())
}

/// Structured error for a search whose selected backends failed such that no
/// hit survived server-side filtering (ADR-130 §1 `search_incomplete`).
///
/// Distinguishes a degraded read from a genuine no-match: a genuine no-match
/// keeps `ok=true` with an empty `result`; this is `ok=false` — a caller
/// doing `if response.ok && response.result.is_empty()` sees the two cases
/// differently, instead of concluding "no match" in both.
pub(super) fn search_incomplete_error(degradation: SearchDegradation) -> Value {
    search_diagnostic_value(&degradation)
}
