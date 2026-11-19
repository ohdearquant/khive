use std::path::Path;

use anyhow::{Context, Result};
use khive_mcp::server::KhiveMcpServer;
use khive_types::RefusalReason;

use super::{
    annotate_and_emit_refusals, parse_ops_file_line, read_bounded_ops_line, OpsFileEntry,
    MAX_OPS_FILE_FAILURE_DETAILS, MAX_OPS_FILE_FAILURE_ERROR_BYTES, OPS_FILE_CHUNK_MAX_BYTES,
    OPS_FILE_CHUNK_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpsFileDispatchMode {
    BoundedParallel,
    Serial,
}

impl OpsFileDispatchMode {
    const fn is_serial(self) -> bool {
        matches!(self, Self::Serial)
    }
}

/// Extract the failed entries of one dispatched chunk as `{op_index, tool,
/// error, reason?}` objects, with `op_index` global across chunks. A failure summary
/// without the per-op reason strings is unactionable: a gate rejection, a
/// schema error, and a transient failure all look identical, and pipelines
/// that trust the counts alone lose records silently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OpsFileReportMode {
    /// Preserve the pre-save-file CLI wire exactly for compatibility.
    LegacyNoSave,
    /// Bound durable manifest diagnostics independently from saved rows.
    BoundedSave,
}

pub(super) fn collect_op_failures(
    parsed: &serde_json::Value,
    applied_before: usize,
    mode: OpsFileReportMode,
) -> Vec<serde_json::Value> {
    let Some(results) = parsed["results"].as_array() else {
        return Vec::new();
    };
    results
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry["ok"].as_bool() == Some(false))
        .map(|(i, entry)| {
            let error = match &entry["error"] {
                serde_json::Value::Null => serde_json::Value::from("unknown error"),
                other if mode == OpsFileReportMode::BoundedSave => bounded_failure_error(other),
                other => other.clone(),
            };
            let mut failure = serde_json::json!({
                "op_index": applied_before + i,
                "tool": entry["tool"].as_str().unwrap_or("?"),
                "error": error,
            });
            if mode == OpsFileReportMode::BoundedSave {
                failure["aborted"] =
                    serde_json::Value::Bool(entry["aborted"].as_bool().unwrap_or(false));
                if let Some(reason) = entry["reason"].as_str().and_then(RefusalReason::from_token) {
                    failure["reason"] = serde_json::json!(reason.as_str());
                }
            }
            failure
        })
        .collect()
}

pub(super) fn retain_failure_detail(
    mode: OpsFileReportMode,
    failure: serde_json::Value,
    failures: &mut Vec<serde_json::Value>,
    omitted: &mut usize,
) -> bool {
    if mode == OpsFileReportMode::BoundedSave && failures.len() >= MAX_OPS_FILE_FAILURE_DETAILS {
        *omitted += 1;
        false
    } else {
        failures.push(failure);
        true
    }
}

pub(super) fn ops_file_progress_line(
    mode: OpsFileReportMode,
    applied: usize,
    total: usize,
    succeeded: usize,
    failed: usize,
    aborted: usize,
) -> String {
    match mode {
        OpsFileReportMode::LegacyNoSave => {
            format!("applied {applied}/{total} (ok={succeeded}, failed={failed})")
        }
        OpsFileReportMode::BoundedSave => format!(
            "applied {applied}/{total} (ok={succeeded}, failed={failed}, aborted={aborted})"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn ops_file_summary(
    mode: OpsFileReportMode,
    total: usize,
    succeeded: usize,
    failed: usize,
    aborted: usize,
    failures: Vec<serde_json::Value>,
    failure_details_omitted: usize,
) -> serde_json::Value {
    let mut summary = match mode {
        OpsFileReportMode::LegacyNoSave => serde_json::json!({
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
        }),
        OpsFileReportMode::BoundedSave => serde_json::json!({
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
            "aborted": aborted,
        }),
    };
    if !failures.is_empty() {
        summary["failures"] = serde_json::Value::Array(failures);
    }
    if mode == OpsFileReportMode::BoundedSave && failure_details_omitted > 0 {
        summary["failure_details_omitted"] = serde_json::json!(failure_details_omitted);
    }
    summary
}

fn bounded_failure_error(error: &serde_json::Value) -> serde_json::Value {
    let mut writer = CountingWriter::default();
    if serde_json::to_writer(&mut writer, error).is_ok()
        && writer.bytes <= MAX_OPS_FILE_FAILURE_ERROR_BYTES
    {
        error.clone()
    } else {
        serde_json::Value::String(format!(
            "error detail omitted: exceeds {MAX_OPS_FILE_FAILURE_ERROR_BYTES}-byte ops-file diagnostic limit"
        ))
    }
}

fn required_summary_count(parsed: &serde_json::Value, field: &str) -> Result<usize> {
    let value = parsed
        .pointer(&format!("/summary/{field}"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("dispatch result is missing integer summary.{field}"))?;
    usize::try_from(value).context("dispatch summary count does not fit usize")
}

fn classify_ordered_chunk(
    expected_tools: &[String],
    results: &[serde_json::Value],
) -> Result<(usize, usize, usize)> {
    if results.len() != expected_tools.len() {
        anyhow::bail!(
            "ordered chunk result count {} does not match input count {}",
            results.len(),
            expected_tools.len()
        );
    }
    let mut succeeded = 0_usize;
    let mut failed = 0_usize;
    let mut aborted = 0_usize;
    for (index, (expected_tool, row)) in expected_tools.iter().zip(results).enumerate() {
        let object = row
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} is not a JSON object"))?;
        let returned_tool = object
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} has no string tool"))?;
        if returned_tool != expected_tool {
            anyhow::bail!(
                "dispatch result row {index} tool mismatch: expected {:?}, got {:?}",
                expected_tool,
                returned_tool
            );
        }
        let ok = object
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} has no boolean ok"))?;
        let row_aborted = match object.get("aborted") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| {
                anyhow::anyhow!("dispatch result row {index} has non-boolean aborted")
            })?,
        };
        match (ok, row_aborted) {
            (true, false) => {
                if !object.contains_key("result") {
                    anyhow::bail!(
                        "dispatch result row {index} is successful but has no result field"
                    );
                }
                if object.contains_key("error") {
                    anyhow::bail!(
                        "dispatch result row {index} is successful but also has an error field"
                    );
                }
                succeeded += 1;
            }
            (false, false) => {
                if !object.contains_key("error") {
                    anyhow::bail!("dispatch result row {index} failed but has no error field");
                }
                if object.contains_key("result") {
                    anyhow::bail!("dispatch result row {index} failed but also has a result field");
                }
                failed += 1;
            }
            (false, true) => {
                if !object.contains_key("error") {
                    anyhow::bail!("dispatch result row {index} aborted but has no error field");
                }
                if object.contains_key("result") {
                    anyhow::bail!(
                        "dispatch result row {index} aborted but also has a result field"
                    );
                }
                aborted += 1;
            }
            (true, true) => {
                anyhow::bail!("dispatch result row {index} cannot be both successful and aborted")
            }
        }
    }
    Ok((succeeded, failed, aborted))
}

pub(super) fn validate_ordered_chunk_envelope(
    expected_tools: &[String],
    parsed: &serde_json::Value,
    chunk_number: usize,
) -> Result<(usize, usize, usize)> {
    let results = parsed
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("dispatch chunk {chunk_number} returned no results array")
        })?;
    let chunk_total = required_summary_count(parsed, "total")?;
    let chunk_succeeded = required_summary_count(parsed, "succeeded")?;
    let chunk_failed = required_summary_count(parsed, "failed")?;
    let chunk_aborted = required_summary_count(parsed, "aborted")?;
    let (derived_succeeded, derived_failed, derived_aborted) =
        classify_ordered_chunk(expected_tools, results)?;
    if chunk_total != expected_tools.len()
        || chunk_succeeded != derived_succeeded
        || chunk_failed != derived_failed
        || chunk_aborted != derived_aborted
    {
        anyhow::bail!(
            "dispatch chunk {chunk_number} summary disagrees with ordered rows: expected total {}, summary total {}, derived/summary succeeded {derived_succeeded}/{chunk_succeeded}, failed {derived_failed}/{chunk_failed}, aborted {derived_aborted}/{chunk_aborted}",
            expected_tools.len(),
            chunk_total,
        );
    }
    let status = parsed
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("dispatch chunk {chunk_number} returned no string status")
        })?;
    let expected_status = if derived_failed == 0 && derived_aborted == 0 {
        "success"
    } else {
        "partial"
    };
    if status != expected_status {
        anyhow::bail!(
            "dispatch chunk {chunk_number} status disagrees with ordered rows: expected {expected_status:?}, got {status:?}"
        );
    }
    Ok((chunk_succeeded, chunk_failed, chunk_aborted))
}

pub(super) fn should_defer_chunk_entry(
    current_count: usize,
    current_bytes: usize,
    next_bytes: usize,
) -> bool {
    current_count > 0
        && (current_count >= OPS_FILE_CHUNK_SIZE
            || current_bytes.saturating_add(next_bytes) > OPS_FILE_CHUNK_MAX_BYTES)
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::FileTooLarge, "JSON byte count overflow")
        })?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct AbortedOpsFileError {
    message: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) manifest: serde_json::Value,
}

impl std::fmt::Display for AbortedOpsFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AbortedOpsFileError {}

#[allow(clippy::too_many_arguments)]
fn emit_aborted_ops_file_manifest(
    error: anyhow::Error,
    save_path: &str,
    requested_total: usize,
    confirmed_ops: usize,
    committed_chunks: &[usize],
    dispatched_chunk: Option<usize>,
    summary: serde_json::Value,
) -> anyhow::Error {
    let message = format!("{error:#}");
    let mut manifest = serde_json::json!({
        "status": "aborted",
        "path": save_path,
        "file_published": false,
        "requested_total": requested_total,
        "confirmed_ops": confirmed_ops,
        "unconfirmed_ops": requested_total.saturating_sub(confirmed_ops),
        "committed_chunks": committed_chunks,
        "summary": summary,
        "error": message.clone(),
    });
    if let Some(chunk_number) = dispatched_chunk {
        manifest["dispatched_chunk"] = serde_json::json!(chunk_number);
    }
    println!(
        "{}",
        serde_json::to_string(&manifest).expect("serialize aborted ops-file manifest")
    );
    anyhow::Error::new(AbortedOpsFileError { message, manifest })
}

/// Apply a parsed ops-file against the given server, printing progress to
/// stderr and either the final summary or a success/aborted save manifest.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn apply_ops_file_reader<R: std::io::BufRead>(
    server: &KhiveMcpServer,
    reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
) -> Result<serde_json::Value> {
    apply_ops_file_reader_with_dispatch_mode(
        server,
        reader,
        total,
        presentation,
        _output_format,
        save_file,
        strict,
        OpsFileDispatchMode::BoundedParallel,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_ops_file_reader_with_dispatch_mode<R: std::io::BufRead>(
    server: &KhiveMcpServer,
    reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    dispatch_mode: OpsFileDispatchMode,
) -> Result<serde_json::Value> {
    apply_ops_file_reader_with_response_transform_and_dispatch_mode(
        server,
        reader,
        total,
        presentation,
        _output_format,
        save_file,
        strict,
        dispatch_mode,
        |_, raw| raw,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn apply_ops_file_reader_with_response_transform<R, F>(
    server: &KhiveMcpServer,
    reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    response_transform: F,
) -> Result<serde_json::Value>
where
    R: std::io::BufRead,
    F: FnMut(usize, String) -> String,
{
    apply_ops_file_reader_with_response_transform_and_dispatch_mode(
        server,
        reader,
        total,
        presentation,
        _output_format,
        save_file,
        strict,
        OpsFileDispatchMode::BoundedParallel,
        response_transform,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn apply_ops_file_reader_with_response_transform_and_dispatch_mode<R, F>(
    server: &KhiveMcpServer,
    mut reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    dispatch_mode: OpsFileDispatchMode,
    mut response_transform: F,
) -> Result<serde_json::Value>
where
    R: std::io::BufRead,
    F: FnMut(usize, String) -> String,
{
    let report_mode = if save_file.is_some() {
        OpsFileReportMode::BoundedSave
    } else {
        OpsFileReportMode::LegacyNoSave
    };
    let mut total_succeeded: usize = 0;
    let mut total_failed: usize = 0;
    let mut total_aborted: usize = 0;
    let mut failures: Vec<serde_json::Value> = Vec::new();
    let mut failure_details_omitted = 0_usize;
    // Preflight the destination before the first chunk can commit. Rows then
    // stream to its sibling temp file. Success publishes it atomically; after
    // dispatch begins, failure drops it and emits a reconciliation manifest.
    let save_path = save_file.clone();
    let mut save_sink = save_file
        .as_deref()
        .map(|path| khive_mcp::save_sink::JsonlSaveSink::new(Path::new(path), false))
        .transpose()?;
    let mut processed = 0_usize;
    let mut snapshot_line = 1_usize;
    let mut chunk_idx = 0_usize;
    let mut eof = false;
    let mut pending: Option<(OpsFileEntry, usize)> = None;
    let mut confirmed_ops = 0_usize;
    let mut committed_chunks = Vec::new();
    let mut dispatched_chunk = None;

    let execution_result: Result<()> = async {
        while !eof {
            // `--serial` changes handler concurrency, not the established
            // logical chunk/progress/reconciliation boundary.
            let mut chunk = Vec::with_capacity(OPS_FILE_CHUNK_SIZE);
            let mut chunk_bytes = 0_usize;
            if let Some((op, bytes)) = pending.take() {
                chunk_bytes = bytes;
                chunk.push(op);
            }
            while chunk.len() < OPS_FILE_CHUNK_SIZE {
                let Some(raw) = read_bounded_ops_line(&mut reader, snapshot_line)? else {
                    eof = true;
                    break;
                };
                let physical_bytes = raw.len().saturating_add(1);
                if let Some(op) = parse_ops_file_line(&raw, snapshot_line)? {
                    if should_defer_chunk_entry(chunk.len(), chunk_bytes, physical_bytes) {
                        pending = Some((op, physical_bytes));
                        snapshot_line += 1;
                        break;
                    }
                    chunk_bytes = chunk_bytes.saturating_add(physical_bytes);
                    chunk.push(op);
                }
                snapshot_line += 1;
            }
            if chunk.is_empty() {
                break;
            }
            let applied_before = processed;

            let chunk_len = chunk.len();
            let expected_tools: Vec<String> = chunk.iter().map(|op| op.tool.clone()).collect();
            let typed_ops: Vec<khive_request::TypedJsonOp> = chunk
                .into_iter()
                .map(|op| {
                    let serde_json::Value::Object(args) = op.args else {
                        unreachable!("validated ops-file args are always JSON objects")
                    };
                    khive_request::TypedJsonOp {
                        tool: op.tool,
                        args,
                    }
                })
                .collect();

            let chunk_number = chunk_idx + 1;
            dispatched_chunk = Some(chunk_number);
            let output_format = save_sink.is_some().then(|| "json".to_string());
            let raw = if dispatch_mode.is_serial() {
                server
                    .dispatch_typed_json_batch_serial_local_for_exec(
                        typed_ops,
                        presentation.clone(),
                        output_format,
                        strict,
                    )
                    .await
            } else {
                server
                    .dispatch_typed_json_batch_local_for_exec(
                        typed_ops,
                        presentation.clone(),
                        // Inline --save-file writes raw results before format
                        // rendering. Reproduce that lossless shape for combined
                        // bulk save. No-save preserves its legacy behavior.
                        output_format,
                        strict,
                    )
                    .await
            }
            .map_err(|error| anyhow::anyhow!("dispatch chunk {chunk_number}: {error}"))?;
            let raw = response_transform(chunk_number, raw);

            let mut parsed: serde_json::Value =
                serde_json::from_str(&raw).context("parse dispatch result")?;
            annotate_and_emit_refusals(&mut parsed, strict);
            let (chunk_succeeded, chunk_failed, chunk_aborted) =
                validate_ordered_chunk_envelope(&expected_tools, &parsed, chunk_number)?;
            let chunk_results = parsed["results"]
                .as_array()
                .expect("validated ordered results array");

            total_succeeded += chunk_succeeded;
            total_failed += chunk_failed;
            total_aborted += chunk_aborted;
            confirmed_ops += chunk_len;
            committed_chunks.push(chunk_number);
            dispatched_chunk = None;

            for failure in collect_op_failures(&parsed, applied_before, report_mode) {
                let reason = match &failure["error"] {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let op_index = failure["op_index"].clone();
                let tool = failure["tool"].as_str().unwrap_or("?").to_string();
                if retain_failure_detail(
                    report_mode,
                    failure,
                    &mut failures,
                    &mut failure_details_omitted,
                ) {
                    eprintln!("op {} ({}) failed: {reason}", op_index, tool);
                }
            }

            if let Some(save_sink) = save_sink.as_mut() {
                for row in chunk_results {
                    save_sink.write_row(row)?;
                }
            }

            processed += chunk_len;
            let applied_now = processed;
            eprintln!(
                "{}",
                ops_file_progress_line(
                    report_mode,
                    applied_now,
                    total,
                    total_succeeded,
                    total_failed,
                    total_aborted,
                )
            );
            chunk_idx += 1;
        }
        if processed != total {
            anyhow::bail!(
                "validated ops-file snapshot changed: expected {total} ops, read {}",
                processed
            );
        }
        Ok(())
    }
    .await;

    if let Err(error) = execution_result {
        if let Some(path) = save_path
            .as_deref()
            .filter(|_| !committed_chunks.is_empty() || dispatched_chunk.is_some())
        {
            drop(save_sink.take());
            let summary = ops_file_summary(
                report_mode,
                confirmed_ops,
                total_succeeded,
                total_failed,
                total_aborted,
                failures,
                failure_details_omitted,
            );
            return Err(emit_aborted_ops_file_manifest(
                error,
                path,
                total,
                confirmed_ops,
                &committed_chunks,
                dispatched_chunk,
                summary,
            ));
        }
        return Err(error);
    }

    let summary = ops_file_summary(
        report_mode,
        total,
        total_succeeded,
        total_failed,
        total_aborted,
        failures,
        failure_details_omitted,
    );
    let output = if let Some(save_sink) = save_sink {
        let manifest = match save_sink.finish(summary.clone()) {
            Ok(manifest) => manifest,
            Err(error) => {
                let path = save_path
                    .as_deref()
                    .expect("save sink exists only when save path exists");
                return Err(emit_aborted_ops_file_manifest(
                    error,
                    path,
                    total,
                    confirmed_ops,
                    &committed_chunks,
                    None,
                    summary,
                ));
            }
        };
        println!(
            "{}",
            serde_json::to_string(&manifest).expect("serialize save manifest")
        );
        manifest
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).expect("serialize summary")
        );
        summary
    };
    if total > 0 && total_succeeded == 0 {
        match report_mode {
            OpsFileReportMode::LegacyNoSave => anyhow::bail!(
                "every op failed: {total_failed} op(s) failed out of {total}, 0 succeeded (see printed summary above)"
            ),
            OpsFileReportMode::BoundedSave => anyhow::bail!(
                "every op failed: {total_failed} failed, {total_aborted} aborted out of {total}, 0 succeeded (see printed output above)"
            ),
        }
    }
    if strict {
        match report_mode {
            OpsFileReportMode::LegacyNoSave if total_failed > 0 => anyhow::bail!(
                "--strict: {total_failed} op(s) failed out of {total} (see printed summary above)"
            ),
            OpsFileReportMode::BoundedSave if total_failed > 0 || total_aborted > 0 => {
                anyhow::bail!(
                    "--strict: {total_failed} op(s) failed, {total_aborted} op(s) aborted out of {total} (see printed output above)"
                )
            }
            _ => {}
        }
    }
    Ok(output)
}

#[cfg(test)]
pub(super) async fn apply_ops_file(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
) -> Result<serde_json::Value> {
    let total = ops.len();
    let mut encoded = Vec::new();
    for op in ops {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"tool": op.tool, "args": op.args}),
        )
        .context("serialize test ops-file entry")?;
        encoded.push(b'\n');
    }
    apply_ops_file_reader(
        server,
        std::io::Cursor::new(encoded),
        total,
        presentation,
        output_format,
        save_file,
        strict,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_ops_file_with_dispatch_mode(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    dispatch_mode: OpsFileDispatchMode,
) -> Result<serde_json::Value> {
    let total = ops.len();
    let mut encoded = Vec::new();
    for op in ops {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"tool": op.tool, "args": op.args}),
        )
        .context("serialize test ops-file entry")?;
        encoded.push(b'\n');
    }
    apply_ops_file_reader_with_dispatch_mode(
        server,
        std::io::Cursor::new(encoded),
        total,
        presentation,
        output_format,
        save_file,
        strict,
        dispatch_mode,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_ops_file_with_response_transform<F>(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    response_transform: F,
) -> Result<serde_json::Value>
where
    F: FnMut(usize, String) -> String,
{
    let total = ops.len();
    let mut encoded = Vec::new();
    for op in ops {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"tool": op.tool, "args": op.args}),
        )
        .context("serialize test ops-file entry")?;
        encoded.push(b'\n');
    }
    apply_ops_file_reader_with_response_transform(
        server,
        std::io::Cursor::new(encoded),
        total,
        presentation,
        output_format,
        save_file,
        strict,
        response_transform,
    )
    .await
}
