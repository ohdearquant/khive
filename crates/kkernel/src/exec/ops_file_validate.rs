use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result};
use khive_types::RefusalReason;

use super::{
    refusal_error, should_defer_chunk_entry, MAX_OPS_FILE_BYTES, MAX_OPS_FILE_LINE_BYTES,
    OPS_FILE_CHUNK_SIZE,
};

/// A single parsed op entry from an ops-file line.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct OpsFileEntry {
    pub(crate) tool: String,
    pub(crate) args: serde_json::Value,
}

#[derive(Debug)]
pub(super) struct ValidatedOpsFile {
    pub(super) snapshot: std::fs::File,
    pub(super) total: usize,
    pub(super) per_verb: BTreeMap<String, usize>,
}

pub(super) fn parse_ops_file_line(raw: &str, line_num: usize) -> Result<Option<OpsFileEntry>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let obj: serde_json::Value = serde_json::from_str(trimmed).map_err(|error| {
        refusal_error(
            RefusalReason::ParseError,
            format!("ops-file line {line_num}: invalid JSON: {error}"),
        )
    })?;
    let obj = obj.as_object().ok_or_else(|| {
        refusal_error(
            RefusalReason::ParseError,
            format!(
                "ops-file line {line_num}: expected a JSON object \
                 {{\"tool\":...,\"args\":...}}, got a non-object value"
            ),
        )
    })?;
    let tool = obj
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            refusal_error(
                RefusalReason::ParseError,
                format!("ops-file line {line_num}: missing or non-string \"tool\" field"),
            )
        })?
        .to_owned();
    let args = match obj.get("args") {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(v) if v.is_object() => v.clone(),
        Some(v) => {
            return Err(refusal_error(
                RefusalReason::ParseError,
                format!("ops-file line {line_num}: \"args\" must be a JSON object, got {v}"),
            ))
        }
    };
    Ok(Some(OpsFileEntry { tool, args }))
}

pub(super) fn read_bounded_ops_line<R: std::io::BufRead>(
    reader: &mut R,
    line_num: usize,
) -> Result<Option<String>> {
    read_bounded_ops_line_with_limit(reader, line_num, MAX_OPS_FILE_LINE_BYTES)
}

pub(super) fn read_bounded_ops_line_with_limit<R: std::io::BufRead>(
    reader: &mut R,
    line_num: usize,
    limit: usize,
) -> Result<Option<String>> {
    let mut bytes = Vec::new();
    let read = {
        let mut limited = (&mut *reader).take((limit + 1) as u64);
        limited
            .read_until(b'\n', &mut bytes)
            .with_context(|| format!("read ops-file line {line_num}"))?
    };
    if read == 0 {
        return Ok(None);
    }
    if read > limit {
        anyhow::bail!("ops-file line {line_num} exceeds the {limit}-byte physical-line limit");
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("ops-file line {line_num} is not valid UTF-8: {error}"))
}

/// Validate the complete source before runtime construction or writes, while
/// spooling a stable bounded snapshot instead of retaining all argument Values.
pub(super) fn validate_ops_file(path: &Path) -> Result<ValidatedOpsFile> {
    let file =
        std::fs::File::open(path).with_context(|| format!("open ops-file {}", path.display()))?;
    let metadata_len = file
        .metadata()
        .with_context(|| format!("stat ops-file {}", path.display()))?
        .len();
    if metadata_len > MAX_OPS_FILE_BYTES {
        anyhow::bail!(
            "ops-file {} is {metadata_len} bytes, exceeding the {MAX_OPS_FILE_BYTES}-byte total limit",
            path.display()
        );
    }
    let mut reader = std::io::BufReader::new(file);
    let mut snapshot = tempfile::tempfile().context("create validated ops-file snapshot")?;
    let mut total = 0_usize;
    let mut total_bytes = 0_u64;
    let mut per_verb = BTreeMap::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        total_bytes = total_bytes
            .checked_add(raw.len() as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("ops-file byte count overflow"))?;
        if total_bytes > MAX_OPS_FILE_BYTES {
            anyhow::bail!(
                "ops-file exceeds the {MAX_OPS_FILE_BYTES}-byte total limit while reading line {line_num}"
            );
        }
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            *per_verb.entry(op.tool).or_insert(0) += 1;
            snapshot
                .write_all(raw.trim().as_bytes())
                .context("write validated ops-file snapshot")?;
            snapshot
                .write_all(b"\n")
                .context("write validated ops-file snapshot newline")?;
            total += 1;
        }
        line_num += 1;
    }
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    Ok(ValidatedOpsFile {
        snapshot,
        total,
        per_verb,
    })
}

fn parse_validated_snapshot<R>(snapshot: &mut R) -> Result<Vec<OpsFileEntry>>
where
    R: std::io::Read + std::io::Seek,
{
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    let mut reader = std::io::BufReader::new(snapshot);
    let mut ops = Vec::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            ops.push(op);
        }
        line_num += 1;
    }
    Ok(ops)
}

/// Validate every decoded JSON op in the stable snapshot before the first
/// handler can mutate state.
///
/// This is deliberately a bounded second pass: at most one ordinary logical
/// chunk is materialized, and one over-target physical line may stand alone.
/// It pins the typed parser's nesting, `$prev`, and reserved-envelope guards
/// across the whole file rather than discovering a later invalid op after an
/// earlier serial dispatch has committed.
pub(super) fn preflight_typed_validated_snapshot<R>(
    snapshot: &mut R,
    expected_total: usize,
) -> Result<()>
where
    R: std::io::Read + std::io::Seek,
{
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot for typed preflight")?;
    let result: Result<()> = (|| {
        let mut reader = std::io::BufReader::new(&mut *snapshot);
        let mut processed = 0_usize;
        let mut line_number = 1_usize;
        let mut chunk_number = 0_usize;
        let mut pending: Option<(OpsFileEntry, usize)> = None;
        let mut eof = false;

        while !eof {
            let mut chunk = Vec::with_capacity(OPS_FILE_CHUNK_SIZE);
            let mut chunk_bytes = 0_usize;
            if let Some((op, bytes)) = pending.take() {
                chunk_bytes = bytes;
                chunk.push(op);
            }
            while chunk.len() < OPS_FILE_CHUNK_SIZE {
                let Some(raw) = read_bounded_ops_line(&mut reader, line_number)? else {
                    eof = true;
                    break;
                };
                let physical_bytes = raw.len().saturating_add(1);
                if let Some(op) = parse_ops_file_line(&raw, line_number)? {
                    if should_defer_chunk_entry(chunk.len(), chunk_bytes, physical_bytes) {
                        pending = Some((op, physical_bytes));
                        line_number += 1;
                        break;
                    }
                    chunk_bytes = chunk_bytes.saturating_add(physical_bytes);
                    chunk.push(op);
                }
                line_number += 1;
            }
            if chunk.is_empty() {
                break;
            }

            chunk_number += 1;
            let chunk_len = chunk.len();
            let typed_ops = chunk
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
            khive_request::parse_typed_json_batch(typed_ops).map_err(|error| {
                refusal_error(
                    RefusalReason::ParseError,
                    format!("ops-file typed preflight chunk {chunk_number}: {error}"),
                )
            })?;
            processed += chunk_len;
        }

        if processed != expected_total {
            anyhow::bail!(
                "validated ops-file snapshot changed during typed preflight: expected \
                 {expected_total} ops, read {processed}"
            );
        }
        Ok(())
    })();
    snapshot
        .rewind()
        .context("rewind typed-preflighted ops-file snapshot")?;
    result
}

pub(super) fn validated_tool_names<R>(snapshot: &mut R) -> Result<Vec<String>>
where
    R: std::io::Read + std::io::Seek,
{
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    let mut reader = std::io::BufReader::new(snapshot);
    let mut tools = Vec::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            tools.push(op.tool);
        }
        line_num += 1;
    }
    Ok(tools)
}

/// Enforce the atomic operation ceiling before the validated snapshot is
/// parsed into owned JSON values. The second guard in `atomic_apply` remains
/// defense in depth for callers that bypass this CLI transport seam.
pub(super) fn parse_atomic_validated_snapshot<R>(
    snapshot: &mut R,
    total: usize,
    max_ops: usize,
) -> Result<Vec<OpsFileEntry>>
where
    R: std::io::Read + std::io::Seek,
{
    if total > max_ops {
        anyhow::bail!(
            "--atomic op count {total} exceeds the configured maximum {max_ops}; \
             split the file or raise --atomic-max-ops"
        );
    }
    parse_validated_snapshot(snapshot)
}

/// Parse a JSONL ops-file.
///
/// Returns the ordered list of ops, or an error naming the first malformed
/// line.  Blank lines are skipped.
///
/// Each line must be a JSON object `{"tool":"verb","args":{...}}`.  `"args"`
/// is optional and defaults to an empty object.  Any other top-level keys are
/// silently ignored so the format is forward-compatible.
#[cfg(test)]
pub(crate) fn parse_ops_file(path: &Path) -> Result<Vec<OpsFileEntry>> {
    let mut validated = validate_ops_file(path)?;
    parse_validated_snapshot(&mut validated.snapshot)
}
