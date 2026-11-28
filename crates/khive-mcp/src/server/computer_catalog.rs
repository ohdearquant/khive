//! The catalog a caller with a computer sees (ADR-220 section 8).
//!
//! A routed pack's verbs are the computer's alone. The bridge reads the
//! computer's catalog with `verbs()`, drops the store's rows for every routed
//! pack and adds the computer's. The `request` tool's description is built
//! from the read made when the bridge starts; `verbs()` reads the computer
//! again each time it is called. `<verb>(help=true)` for a routed verb needs
//! nothing here: its pack sends it to the computer, which answers it.
//!
//! Every read of the computer's catalog has its own deadline, shorter than
//! the call timeout, so a computer that accepts a connection and never
//! answers costs the caller the routed packs' catalog and nothing else.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::Duration;

use khive_request::{ArgValue, ParsedOp, ParsedRequest};
use khive_runtime::{
    present_with_policy, render_format, DomainDisposition, OutputFormat, PresentationMode,
};
use rmcp::ErrorData as McpError;
use serde_json::{json, Map, Value};

use super::bridge::ForwardFnPtr;
use super::computer_bridge::ComputerBridge;
use super::computer_client::{ForwardedOptions, COMPUTER_UNREACHABLE};
use super::computer_routing::{ComputerRouting, COMPUTER_PROTOCOL};
use super::{
    batch_status, failure_entry, invalid_request_error, parse_output_format,
    parse_presentation_mode, serialize_response_value, KhiveMcpServer,
};
use crate::tools::request::RequestParams;

/// The introspection verb whose answer this module composes.
pub(super) const VERBS_VERB: &str = "verbs";

/// The longest a read of the computer's catalog may take, session open
/// included, when the call timeout is longer.
pub(super) const CATALOG_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// The computer's catalog rows for the packs it serves this caller.
#[derive(Debug, Clone)]
pub(super) struct RoutedCatalog {
    /// Each row as the computer gave it, with `served_by` and `computer` added.
    rows: Vec<Value>,
    /// How many verbs the computer lists for each routed pack.
    counts: BTreeMap<String, usize>,
}

impl ComputerBridge {
    /// Reads the computer's catalog with one `verbs()` call, within
    /// `catalog_timeout`. A read that does not finish in time fails with
    /// `computer_unreachable` and commits nothing.
    pub(super) async fn read_catalog(&self) -> Result<RoutedCatalog, (Value, DomainDisposition)> {
        let options = ForwardedOptions {
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..ForwardedOptions::default()
        };
        let read = self.client.send(format!("{VERBS_VERB}()"), &options);
        let text = match tokio::time::timeout(self.catalog_timeout, read).await {
            Ok(answer) => answer.map_err(|failure| (failure.error, failure.disposition))?,
            Err(_) => {
                let error = json!({
                    "kind": COMPUTER_UNREACHABLE,
                    "message": format!(
                        "the computer {} did not answer verbs() within {} s; a catalog read \
                         commits nothing",
                        self.label(),
                        self.catalog_timeout.as_secs()
                    ),
                });
                return Err((error, DomainDisposition::NotCommitted));
            }
        };
        let envelope: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let entry = &envelope["results"][0];
        let protocol = || {
            json!({
                "kind": COMPUTER_PROTOCOL,
                "message": format!(
                    "the computer {} did not answer verbs() with its catalog",
                    self.label()
                ),
            })
        };
        if entry["ok"] != Value::Bool(true) {
            let error = if entry["error"].is_object() {
                entry["error"].clone()
            } else {
                protocol()
            };
            return Err((error, DomainDisposition::NotCommitted));
        }
        let Some(rows) = entry["result"]["verbs"].as_array() else {
            return Err((protocol(), DomainDisposition::NotCommitted));
        };
        let mut catalog = RoutedCatalog {
            rows: Vec::new(),
            counts: self
                .routing
                .routed_packs()
                .map(|pack| (pack.to_owned(), 0))
                .collect(),
        };
        for row in rows {
            let Some(count) = row["pack"]
                .as_str()
                .and_then(|pack| catalog.counts.get_mut(pack))
            else {
                continue;
            };
            let mut row = row.clone();
            let Some(fields) = row.as_object_mut() else {
                continue;
            };
            fields.insert("served_by".into(), json!("computer"));
            fields.insert("computer".into(), json!(self.routing.name()));
            *count += 1;
            catalog.rows.push(row);
        }
        Ok(catalog)
    }

    /// The catalog read when this bridge started, or `None` when the computer
    /// could not be read then. It is read once; a call made while that read is
    /// in flight waits for it.
    pub(super) async fn catalog_at_start(&self) -> Option<&RoutedCatalog> {
        self.catalog_at_start
            .get_or_init(|| async {
                match self.read_catalog().await {
                    Ok(catalog) => Some(catalog),
                    Err((error, _)) => {
                        tracing::warn!(
                            target: "khive_mcp::server",
                            computer = %self.label(),
                            kind = %error["kind"].as_str().unwrap_or("unknown"),
                            "the computer's catalog could not be read when the bridge started"
                        );
                        None
                    }
                }
            })
            .await
            .as_ref()
    }
}

/// The `pack` and `category` filters of one `verbs` operation.
struct VerbsFilter {
    pack: Option<String>,
    category: Option<String>,
}

impl VerbsFilter {
    /// The filters of `op`, or `None` when one of them is not a literal
    /// string (a `$prev` reference), which this bridge cannot read.
    fn of(op: &ParsedOp) -> Option<Self> {
        let read = |name: &str| match op.args.get(name) {
            None => Some(None),
            Some(ArgValue::Value(Value::String(value))) => Some(Some(value.clone())),
            Some(_) => None,
        };
        Some(Self {
            pack: read("pack")?,
            category: read("category")?,
        })
    }

    /// The same test the `verbs` verb applies to its own rows.
    fn admits(&self, row: &Value) -> bool {
        let matches = |filter: &Option<String>, field: &str| {
            filter.as_deref().is_none_or(|filter| {
                row[field]
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case(filter))
            })
        };
        matches(&self.pack, "pack") && matches(&self.category, "category")
    }
}

/// The store's answer to one `verbs` operation with every routed pack's rows
/// replaced by the computer's. The store's rows carry `served_by: store`.
/// With filters this bridge cannot read, the routed packs' rows are dropped
/// and none of the computer's are added.
fn compose(
    store: &Value,
    filter: Option<&VerbsFilter>,
    catalog: &RoutedCatalog,
    routing: &ComputerRouting,
) -> Value {
    let routed = |row: &Value| {
        row["pack"]
            .as_str()
            .is_some_and(|pack| routing.routes_pack(pack))
    };
    let mut rows: Vec<Value> = store["verbs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| !routed(row))
        .map(|row| {
            let mut row = row.clone();
            if let Some(fields) = row.as_object_mut() {
                fields.insert("served_by".into(), json!("store"));
            }
            row
        })
        .collect();
    if let Some(filter) = filter {
        rows.extend(
            catalog
                .rows
                .iter()
                .filter(|row| filter.admits(row))
                .cloned(),
        );
    }
    let mut counts: Map<String, Value> = store["pack_counts"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    counts.retain(|pack, _| !routing.routes_pack(pack));
    for (pack, count) in &catalog.counts {
        counts.insert(pack.clone(), json!(count));
    }
    json!({ "total": rows.len(), "verbs": rows, "pack_counts": counts })
}

/// The caller's per-operation overrides with `raw` at each of `indices`.
fn per_op(
    caller: &Option<Vec<Option<String>>>,
    count: usize,
    indices: &[usize],
    raw: &str,
) -> Vec<Option<String>> {
    let mut modes = caller.clone().unwrap_or_default();
    modes.resize(count, None);
    for &index in indices {
        modes[index] = Some(raw.to_owned());
    }
    modes
}

/// Moves `newly_failed` answers from succeeded to failed in the summary.
fn restate_summary(envelope: &mut Value, newly_failed: u64) {
    let summary = &envelope["summary"];
    let succeeded = summary["succeeded"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(newly_failed);
    let failed = summary["failed"].as_u64().unwrap_or(0) + newly_failed;
    let aborted = summary["aborted"].as_u64().unwrap_or(0);
    let status = batch_status(
        usize::try_from(failed).unwrap_or(usize::MAX),
        usize::try_from(aborted).unwrap_or(usize::MAX),
    );
    envelope["summary"]["succeeded"] = json!(succeeded);
    envelope["summary"]["failed"] = json!(failed);
    envelope["status"] = json!(status);
}

impl KhiveMcpServer {
    /// The verb catalog in the `request` tool's description. For a caller with
    /// a computer it is built from the store's packs that are not routed and
    /// the computer's catalog as read when this bridge started; when that
    /// read failed, each routed pack is listed as the computer's with its
    /// catalog unavailable.
    pub(super) async fn request_tool_catalog(&self) -> String {
        let Some(computer) = &self.computer else {
            return self.verb_catalog();
        };
        let routing = &computer.routing;
        let mut verbs: Vec<(String, String, String)> = self
            .catalog_entries()
            .into_iter()
            .filter(|(pack, _, _)| !routing.routes_pack(pack))
            .collect();
        match computer.catalog_at_start().await {
            Some(catalog) => verbs.extend(catalog.rows.iter().map(|row| {
                let field = |name: &str| row[name].as_str().unwrap_or_default().to_owned();
                (field("pack"), field("verb"), field("description"))
            })),
            None => verbs.extend(routing.routed_packs().map(|pack| {
                (
                    pack.to_owned(),
                    format!("{pack}.*"),
                    format!(
                        "served by the computer {}; its catalog could not be read when this \
                         bridge started, and verbs(pack=\"{pack}\") reads it again",
                        computer.label()
                    ),
                )
            })),
        }
        self.render_verb_catalog(verbs)
    }

    /// Starts the one read of the computer's catalog that the tool description
    /// is built from, so that it is under way as the bridge starts serving.
    pub(crate) fn read_computer_catalog_in_background(&self) {
        if let Some(computer) = self.computer.clone() {
            tokio::spawn(async move {
                computer.catalog_at_start().await;
            });
        }
    }

    /// Runs a request with a `verbs` operation for a caller with a computer.
    /// Each `verbs` answer is taken from the store unpresented, composed with
    /// the computer's catalog read for this request, and then presented and
    /// formatted as the caller asked. When the computer cannot be read, each
    /// `verbs` operation fails with the computer's failure. A `$prev` in a
    /// later operation of a chain reads the store's answer before composition.
    pub(super) async fn request_with_computer_catalog(
        &self,
        computer: &ComputerBridge,
        p: RequestParams,
        parsed: &ParsedRequest,
        forward_fn: ForwardFnPtr,
    ) -> Result<String, McpError> {
        if p.save_to.is_some() {
            return Err(invalid_request_error(format!(
                "save_to is not available for a request with verbs while the computer {} \
                 serves part of the catalog; nothing was run",
                computer.label()
            )));
        }
        let count = parsed.ops.len();
        let indices: Vec<usize> = parsed
            .ops
            .iter()
            .enumerate()
            .filter(|(_, op)| op.tool == VERBS_VERB)
            .map(|(index, _)| index)
            .collect();
        let shapes = indices
            .iter()
            .map(|&index| self.answer_shape(&p, index))
            .collect::<Result<Vec<_>, _>>()?;
        let unpresented = RequestParams {
            plan: None,
            ops: p.ops.clone(),
            presentation: p.presentation.clone(),
            presentation_per_op: Some(per_op(&p.presentation_per_op, count, &indices, "verbose")),
            save_to: None,
            format: p.format.clone(),
            format_per_op: Some(per_op(&p.format_per_op, count, &indices, "json")),
            request_id: p.request_id,
        };
        let text = match self.computer_route(parsed) {
            Some((routed, plan)) => {
                self.request_routed(&routed, &plan, parsed, unpresented, forward_fn)
                    .await?
            }
            None => {
                self.request_on_store(unpresented, parsed, forward_fn)
                    .await?
            }
        };
        let Ok(mut envelope) = serde_json::from_str::<Value>(&text) else {
            return Ok(text);
        };
        let mut catalog = None;
        let mut newly_failed = 0;
        for (&index, &(presentation, format)) in indices.iter().zip(&shapes) {
            let Some(entry) = envelope["results"].get_mut(index) else {
                continue;
            };
            if entry["ok"] != Value::Bool(true) || entry["tool"] != VERBS_VERB {
                continue;
            }
            if catalog.is_none() {
                catalog = Some(computer.read_catalog().await);
            }
            match catalog.as_ref() {
                Some(Ok(catalog)) => {
                    let filter = VerbsFilter::of(&parsed.ops[index]);
                    let composed = compose(
                        &entry["result"],
                        filter.as_ref(),
                        catalog,
                        &computer.routing,
                    );
                    entry["result"] = self.present_answer(composed, presentation, format);
                }
                Some(Err((error, disposition))) => {
                    *entry = failure_entry(VERBS_VERB, error.clone(), *disposition);
                    newly_failed += 1;
                }
                None => {}
            }
        }
        if newly_failed > 0 {
            restate_summary(&mut envelope, newly_failed);
        }
        Ok(serialize_response_value(&envelope))
    }

    /// The presentation and format the caller asked for the answer at `index`.
    fn answer_shape(
        &self,
        p: &RequestParams,
        index: usize,
    ) -> Result<(PresentationMode, OutputFormat), McpError> {
        let at = |per_op: &Option<Vec<Option<String>>>| {
            per_op
                .as_ref()
                .and_then(|values| values.get(index))
                .cloned()
                .flatten()
        };
        let presentation = match at(&p.presentation_per_op) {
            Some(mode) => parse_presentation_mode(Some(&mode)),
            None => parse_presentation_mode(p.presentation.as_deref()),
        }
        .map_err(invalid_request_error)?;
        let format = match at(&p.format_per_op) {
            Some(format) => parse_output_format(Some(&format)),
            None => parse_output_format(p.format.as_deref()),
        }
        .map_err(invalid_request_error)?
        .unwrap_or(self.default_output_format);
        Ok((presentation, format))
    }

    /// `result` presented and formatted as the store presents a `verbs` answer.
    fn present_answer(
        &self,
        result: Value,
        presentation: PresentationMode,
        format: OutputFormat,
    ) -> Value {
        let policy = self.registry.presentation_policy_for(VERBS_VERB);
        let result =
            present_with_policy(result, presentation, chrono::Utc::now().timestamp(), policy);
        if format == OutputFormat::Json {
            khive_runtime::presentation::prepare_format_value(result, format, presentation)
        } else {
            Value::String(render_format(result, format, presentation))
        }
    }
}
