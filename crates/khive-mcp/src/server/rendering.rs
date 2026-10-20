use khive_request::DslError;
use khive_runtime::daemon::DAEMON_LEXICAL_TIMEOUT_MARKER;
use khive_runtime::presentation::{
    prepare_format_value_with_note_content, render_format_with_note_content, NoteContentScope,
};
use khive_runtime::{
    render_format, DomainDisposition, OutputFormat, PresentationMode, VerbPresentationPolicy,
    VerbRegistry,
};
use khive_types::RefusalReason;
use rmcp::ErrorData as McpError;
#[cfg(doc)]
use rmcp::ServerHandler;
use serde_json::{json, Value};

use crate::tools::request::RequestParams;

use super::batch_status;
use super::batch_types::{aborted_entry, error_disposition, error_with_disposition, failure_entry};

/// Attach a registry-level audit advisory to successful operation entries
/// without changing their canonical `result` values.
///
/// Help introspection is excluded because it short-circuits before the
/// gate/audit lifecycle and therefore would not append an audit row.
pub(super) fn attach_audit_persistence_advisories(response: &mut Value, registry: &VerbRegistry) {
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

pub(super) fn invalid_request_error(message: String) -> McpError {
    request_error(
        message,
        rmcp::model::ErrorCode::INVALID_PARAMS,
        "invalid_input",
        DomainDisposition::NotCommitted,
    )
}

#[cfg(unix)]
pub(super) fn cancelled_forward_error(request_id: Option<u64>) -> McpError {
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

pub(super) fn request_internal_error(message: String) -> McpError {
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
pub(super) fn save_to_write_error(message: String, result: &Value) -> McpError {
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

pub(super) fn dsl_err_to_mcp(e: DslError) -> McpError {
    let mut data = json!({ "reason": RefusalReason::ParseError.as_str(),
        "kind":"parse_error", "message":e.to_string() });
    if let DslError::WriteKeyConflict { conflict_ops, .. } = &e {
        data["conflict_ops"] = json!(conflict_ops);
    }
    McpError::invalid_params(
        e.to_string(),
        Some(error_with_disposition(
            data,
            DomainDisposition::NotCommitted,
        )),
    )
}

/// Parse an optional presentation mode string from the request envelope.
///
/// `None` → default (`Agent`). Known values: `"agent"`, `"verbose"`, `"human"`.
pub(super) fn parse_presentation_mode(s: Option<&str>) -> Result<PresentationMode, String> {
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
pub(super) fn parse_output_format(s: Option<&str>) -> Result<Option<OutputFormat>, String> {
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
pub(super) fn validate_request_overrides(
    p: &RequestParams,
    op_count: usize,
) -> Result<(), McpError> {
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
pub(super) struct RenderContext<'a> {
    pub(super) registry: &'a VerbRegistry,
    pub(super) content_scopes: &'a [NoteContentScope],
}

/// Preserve a daemon-only signal before auto/table rendering turns a result
/// into display text. The daemon moves it into response-frame metadata.
pub(super) fn mark_daemon_lexical_timeout(response: &mut Value) {
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
pub(super) fn render_result(
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
pub(super) fn fit_rendered_batch_envelope(
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
pub(super) fn envelope_metadata(
    map: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    map.iter()
        .filter(|(key, _)| key.as_str() != "results")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The escaped byte length of `{..metadata, "results": []}`. Since
/// `metadata` excludes `results`, this costs nothing proportional to batch
/// size — it changes only when `refresh_frame_budget_outcome` rewrites
/// `summary`/`status`, both tiny fixed-shape objects.
pub(super) fn envelope_metadata_escaped_len(metadata: &serde_json::Map<String, Value>) -> usize {
    let mut probe = metadata.clone();
    probe.insert("results".to_string(), Value::Array(Vec::new()));
    json_escaped_len(&serde_json::to_vec(&Value::Object(probe)).expect("value is serializable"))
}

/// The escaped byte length `entry` contributes once embedded in the
/// envelope's `results` array (i.e. as it will be re-escaped a second time
/// when the whole envelope is wrapped in a daemon response frame).
pub(super) fn entry_escaped_len(entry: &Value) -> usize {
    json_escaped_len(&serde_json::to_vec(entry).expect("value is serializable"))
}

/// `envelope_metadata_escaped_len(metadata)` plus the array literal built
/// from `entry_escaped_lens` — the incremental form of
/// `envelope_metadata_escaped_len` with `results` populated instead of
/// empty. `metadata_escaped_len` already counts the two bracket bytes of
/// `results`'s empty `[]`, and those brackets stay in place either way, so
/// only the entries and the commas between them are additional.
pub(super) fn envelope_escaped_len(
    entry_escaped_lens: &[usize],
    metadata_escaped_len: usize,
) -> usize {
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
pub(super) fn empty_rendered_daemon_frame_len(served_config_id: &str) -> usize {
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

pub(super) fn frame_budget_omission(entry: &Value, registry: &VerbRegistry) -> Value {
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

pub(super) fn serialized_response_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("serde_json::Value is always serializable")
        .len()
}

pub(super) fn serialize_response_value(value: &Value) -> String {
    serde_json::to_string(value).expect("serde_json::Value is always serializable")
}

pub(super) fn rendered_response_fits_daemon_frame(rendered: &str, served_config_id: &str) -> bool {
    rendered_response_daemon_frame_len(rendered, served_config_id)
        <= khive_runtime::daemon::MAX_FRAME_BYTES
}

pub(super) fn rendered_response_daemon_frame_len(rendered: &str, served_config_id: &str) -> usize {
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

/// Build the `initialize` instructions string from the packs this server
/// actually loaded and the packs that are linked into the binary but were not
/// selected. Extracted from [`ServerHandler::get_info`] so the docs-pointer
/// section (#594) is unit-testable without standing up a full server.
///
/// The verb catalog is not repeated here. ADR-016 places it in the `request`
/// tool description, which every MCP client receives through `tools/list`;
/// carrying a second copy in the instructions doubled the schema a client
/// loads per session without adding a line it could not already read.
///
/// The two pack lists answer different questions and the instructions must not
/// merge them (#2913). `loaded` is what the caller can call right now: the
/// selection resolved at startup, plus any configured mounts. `unloaded` is
/// what `KHIVE_PACKS` or `--pack` could additionally select from this binary.
/// Naming the linked set alone told callers that packs contributing nothing to
/// the verb catalog were available, and every verb of theirs is refused.
pub(super) fn build_instructions(loaded: &str, unloaded: &str) -> String {
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
         unless the verb's help says it wraps them in an envelope. The `request` tool's \
         description lists every verb registered on this server, one line each; \
         `verb(help=true)` returns that verb's full description and arguments. For \
         detailed usage of each verb, see the corresponding plugin's SKILL.md files.\n\
         Docs: https://ohdearquant.github.io/khive/ (hosted) or docs/*.md in the repo \
         checkout. Treat the live verb catalog and help=true as authoritative over \
         cached/training knowledge. Config/backend issues: docs/configuration.md. Usage \
         patterns: docs/guide/tips-and-tricks.md."
    )
}

/// Preserve the request envelope verbatim while making its all-failed state
/// visible to MCP clients that inspect `isError` instead of parsing text.
pub(super) fn mark_all_failed_request_result(result: &mut rmcp::model::CallToolResult) {
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
