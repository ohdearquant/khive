//! Routing of command verbs to the caller's computer (ADR-220).
//!
//! A caller whose configuration has a `[computer]` table has two homes for its
//! operations: the store, reached as today, and its computer. This module
//! decides the home of each operation, cuts a request that has both homes into
//! one request per home, writes each of those back as request text, and puts
//! the per-home results back together in input order. It performs no I/O: a
//! [`HomeSender`] carries each per-home request.
//!
//! A request whose operations all have one home does not pass through
//! [`execute_split`]: it is sent whole and unchanged.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;

use async_trait::async_trait;
use futures::future::{join, join_all};
use khive_request::{
    parse_request, value_nesting_within_limit, ArgValue, ExecutionMode, ParsedOp, ParsedRequest,
    NESTING_DEPTH_LIMIT,
};
use khive_runtime::{ComputerSectionConfig, ConfigError, DomainDisposition, COMMAND_RUNNING_PACKS};
use serde_json::{json, Value};

use super::{aborted_entry, batch_status, failure_entry, substitution_error_payload};

/// Refusal kind for a command-running pack that the caller's computer does not serve.
pub(super) const RUNS_ON_COMPUTER: &str = "runs_on_computer";
/// Failure kind for a computer response that is not the khive envelope.
pub(super) const COMPUTER_PROTOCOL: &str = "computer_protocol";

/// The one verb that stores an operation for the store to run later.
const SCHEDULE_VERB: &str = "schedule.schedule";
/// How many `schedule.schedule` actions inside one another are read before the
/// operation is refused as unreadable.
const MAX_SCHEDULE_NESTING: usize = 8;

/// Where an operation is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Home {
    Store,
    Computer,
}

/// What the router does with one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Placement {
    /// Send it to this home.
    Home(Home),
    /// Refuse it here with `runs_on_computer`: its pack runs commands, and the
    /// caller's computer does not serve that pack.
    RunsOnComputer,
}

/// The routing rule read from one caller's `[computer]` table.
#[derive(Debug, Clone)]
pub(super) struct ComputerRouting {
    routed: BTreeSet<String>,
    /// `name (host:port)`, used in every message that names the computer.
    label: String,
}

impl ComputerRouting {
    pub(super) fn new(config: &ComputerSectionConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        let endpoint = config.endpoint()?;
        let host = if endpoint.host.contains(':') {
            format!("[{}]", endpoint.host)
        } else {
            endpoint.host
        };
        Ok(Self {
            routed: config.packs.iter().cloned().collect(),
            label: format!("{} ({host}:{})", config.name, endpoint.port),
        })
    }

    pub(super) fn label(&self) -> &str {
        &self.label
    }

    /// The placement of one operation. A `schedule.schedule` whose action would
    /// run, when it fires on the store's machine, a pack that may not run there
    /// for this caller is refused like the pack itself (ADR-220 section 6).
    pub(super) fn place(&self, op: &ParsedOp) -> Placement {
        if self.schedules_off_computer(op, 0) {
            Placement::RunsOnComputer
        } else {
            self.place_tool(&op.tool)
        }
    }

    /// The placement of a tool name, by the pack it carries. A bare verb (the
    /// kg pack's verbs, `verbs`, `whoami`) is the store's.
    fn place_tool(&self, tool: &str) -> Placement {
        let Some((pack, _verb)) = tool.split_once('.') else {
            return Placement::Home(Home::Store);
        };
        if self.routed.contains(pack) {
            Placement::Home(Home::Computer)
        } else if COMMAND_RUNNING_PACKS.contains(&pack) {
            Placement::RunsOnComputer
        } else {
            Placement::Home(Home::Store)
        }
    }

    /// Whether `op` is a `schedule.schedule` whose action, or an action nested
    /// in it, names a pack that runs commands or that this caller's computer
    /// serves. The store runs a scheduled action on its own machine when it
    /// fires, so neither may be stored. An action that is not a literal string,
    /// does not parse, or nests deeper than [`MAX_SCHEDULE_NESTING`] cannot be
    /// read here and counts as one.
    fn schedules_off_computer(&self, op: &ParsedOp, depth: usize) -> bool {
        if op.tool != SCHEDULE_VERB {
            return false;
        }
        let action = match op.args.get("action") {
            // The store refuses the missing argument itself.
            None => return false,
            Some(ArgValue::Value(Value::String(action))) => action,
            Some(_) => return true,
        };
        if depth >= MAX_SCHEDULE_NESTING {
            return true;
        }
        let Ok(action) = parse_request(action) else {
            return true;
        };
        action.ops.iter().any(|inner| {
            self.place_tool(&inner.tool) != Placement::Home(Home::Store)
                || self.schedules_off_computer(inner, depth + 1)
        })
    }

    fn runs_on_computer_entry(&self, tool: &str) -> Value {
        let message = if tool == SCHEDULE_VERB {
            format!(
                "{tool} was not run: this caller's commands run on its computer {}, and a \
                 scheduled action runs on the store's machine; the action names a pack that \
                 runs commands or that the computer serves, or cannot be read before it is \
                 stored",
                self.label
            )
        } else {
            let pack = tool.split_once('.').map_or(tool, |(pack, _)| pack);
            format!(
                "{tool} was not run: this caller's commands run on its computer {}, \
                 and [computer].packs does not route the {pack} pack",
                self.label
            )
        };
        failure_entry(
            tool,
            json!({ "kind": RUNS_ON_COMPUTER, "message": message }),
            DomainDisposition::NotCommitted,
        )
    }
}

/// A run of consecutive operations of one unit with one placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Segment {
    pub(super) placement: Placement,
    pub(super) ops: Range<usize>,
}

/// One unit of a parsed request (the single operation, the chain, or one member
/// of a parallel batch), cut into segments in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnitPlan {
    pub(super) segments: Vec<Segment>,
}

impl UnitPlan {
    fn single_placement(&self) -> Option<Placement> {
        match self.segments.as_slice() {
            [only] => Some(only.placement),
            _ => None,
        }
    }

    fn ops(&self) -> Range<usize> {
        let start = self.segments.first().map_or(0, |segment| segment.ops.start);
        let end = self
            .segments
            .last()
            .map_or(start, |segment| segment.ops.end);
        start..end
    }
}

/// Every operation of a parsed request placed, unit by unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RoutePlan {
    pub(super) mode: ExecutionMode,
    pub(super) units: Vec<UnitPlan>,
}

impl RoutePlan {
    pub(super) fn new(routing: &ComputerRouting, parsed: &ParsedRequest) -> Self {
        let units = parsed
            .ranges
            .iter()
            .map(|range| {
                let mut segments: Vec<Segment> = Vec::new();
                for index in range.clone() {
                    let placement = routing.place(&parsed.ops[index]);
                    match segments.last_mut() {
                        Some(last) if last.placement == placement => last.ops.end = index + 1,
                        _ => segments.push(Segment {
                            placement,
                            ops: index..index + 1,
                        }),
                    }
                }
                UnitPlan { segments }
            })
            .collect();
        Self {
            mode: parsed.mode.clone(),
            units,
        }
    }

    /// The one home that serves every operation, when there is one. Such a
    /// request is sent whole and unchanged; any other goes to [`execute_split`].
    pub(super) fn whole_home(&self) -> Option<Home> {
        let mut placements = self.units.iter().map(UnitPlan::single_placement);
        let Some(Some(Placement::Home(home))) = placements.next() else {
            return None;
        };
        placements
            .all(|placement| placement == Some(Placement::Home(home)))
            .then_some(home)
    }
}

/// Why a slice of a parsed request cannot be written back as request text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RenderError {
    /// A tool name that is not `verb` or `pack.verb` of identifiers.
    Tool(String),
    /// An argument name that is not an identifier.
    ArgName(String),
    /// A string that starts with a backslash before a `$prev` form. The request
    /// language reads that backslash as the escape that keeps `$prev` literal,
    /// so the string itself has no spelling.
    BackslashPrevString,
    /// A value nested deeper than the request language accepts.
    TooDeep,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tool(tool) => write!(f, "tool name {tool:?} cannot be written in a request"),
            Self::ArgName(name) => {
                write!(f, "argument name {name:?} cannot be written in a request")
            }
            Self::BackslashPrevString => f.write_str(
                "a string that starts with a backslash before $prev cannot be written in a request",
            ),
            Self::TooDeep => write!(
                f,
                "an argument nests deeper than {NESTING_DEPTH_LIMIT} levels"
            ),
        }
    }
}

/// Writes one unit (one operation, or a chain of them) as request text that
/// parses back to the same operations.
pub(super) fn render_unit(ops: &[ParsedOp]) -> Result<String, RenderError> {
    let mut out = String::new();
    for (position, op) in ops.iter().enumerate() {
        if position > 0 {
            out.push_str(" | ");
        }
        render_op(op, &mut out)?;
    }
    Ok(out)
}

fn render_op(op: &ParsedOp, out: &mut String) -> Result<(), RenderError> {
    let mut parts = op.tool.split('.');
    let writable = match (parts.next(), parts.next(), parts.next()) {
        (Some(verb), None, None) => is_identifier(verb),
        (Some(pack), Some(verb), None) => is_identifier(pack) && is_identifier(verb),
        _ => false,
    };
    if !writable {
        return Err(RenderError::Tool(op.tool.clone()));
    }
    out.push_str(&op.tool);
    out.push('(');
    for (position, (name, arg)) in op.args.iter().enumerate() {
        if !is_identifier(name) {
            return Err(RenderError::ArgName(name.clone()));
        }
        if position > 0 {
            out.push_str(", ");
        }
        out.push_str(name);
        out.push('=');
        render_arg(arg, out)?;
    }
    out.push(')');
    Ok(())
}

/// The request language's identifier: an ASCII letter or `_`, then ASCII
/// letters, digits and `_`.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn render_arg(arg: &ArgValue, out: &mut String) -> Result<(), RenderError> {
    match arg {
        ArgValue::Value(value) => {
            if !value_nesting_within_limit(value, NESTING_DEPTH_LIMIT) {
                return Err(RenderError::TooDeep);
            }
            render_value(value, out)
        }
        // The quoted form keeps the path byte for byte: the parser promotes a
        // quoted `$prev`, `$prev.<path>` or `$prev[<n>]<path>` string to the
        // reference with that same path.
        ArgValue::PrevRef { path } => {
            let separator = if path.is_empty() || path.starts_with('[') {
                ""
            } else {
                "."
            };
            push_json_string(&format!("$prev{separator}{path}"), out);
            Ok(())
        }
        ArgValue::Array(elements) => {
            out.push('[');
            for (position, element) in elements.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                render_arg(element, out)?;
            }
            out.push(']');
            Ok(())
        }
        ArgValue::Object(pairs) => {
            out.push('{');
            for (position, (key, value)) in pairs.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                push_json_string(key, out);
                out.push(':');
                render_arg(value, out)?;
            }
            out.push('}');
            Ok(())
        }
    }
}

fn render_value(value: &Value, out: &mut String) -> Result<(), RenderError> {
    match value {
        Value::String(text) => render_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                render_value(item, out)?;
            }
            out.push(']');
            Ok(())
        }
        Value::Object(map) => {
            out.push('{');
            for (position, (key, item)) in map.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                push_json_string(key, out);
                out.push(':');
                render_value(item, out)?;
            }
            out.push('}');
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            out.push_str(&value.to_string());
            Ok(())
        }
    }
}

/// A string literal. The parser turns a quoted `$prev` form into a reference,
/// and a quoted backslash before one into the literal without the backslash,
/// so a literal `$prev` form is written with that backslash.
fn render_string(text: &str, out: &mut String) -> Result<(), RenderError> {
    if is_prev_form(text) {
        push_json_string(&format!("\\{text}"), out);
    } else if text.strip_prefix('\\').is_some_and(is_prev_form) {
        return Err(RenderError::BackslashPrevString);
    } else {
        push_json_string(text, out);
    }
    Ok(())
}

fn is_prev_form(text: &str) -> bool {
    text == "$prev" || text.starts_with("$prev.") || text.starts_with("$prev[")
}

fn push_json_string(text: &str, out: &mut String) {
    out.push_str(&Value::String(text.to_owned()).to_string());
}

/// Carries one per-home request.
#[async_trait]
pub(super) trait HomeSender: Sync {
    /// Sends `ops` whole to `home`. `indices` are the input positions of its
    /// operations, in order, so per-operation overrides can be cut to match.
    async fn send(&self, home: Home, ops: String, indices: Vec<usize>) -> HomeOutcome;
}

/// What one per-home request produced.
#[derive(Debug, Clone)]
pub(super) enum HomeOutcome {
    /// The home's khive envelope.
    Envelope(Value),
    /// No envelope: every operation the request carried fails with this error
    /// and disposition.
    Failed {
        error: Value,
        disposition: DomainDisposition,
    },
}

/// Runs a request whose operations have more than one placement and returns
/// the khive envelope: each entry exactly as its home returned it, in input
/// order, with `summary` and `status` computed from the entries.
///
/// Units of a parallel batch that have one home are sent together, one request
/// per home, concurrently. Every other unit runs by the chain rule: segments in
/// order, each sent as a chain to its home, the first operation of a segment
/// taking `$prev` from the last result of the segment before as a literal, and
/// the first failure aborting what follows.
pub(super) async fn execute_split(
    routing: &ComputerRouting,
    plan: &RoutePlan,
    parsed: &ParsedRequest,
    sender: &dyn HomeSender,
) -> Value {
    let mut placed: Vec<(usize, Value)> = Vec::with_capacity(parsed.ops.len());
    if plan.mode == ExecutionMode::Parallel {
        let mut store_units = Vec::new();
        let mut computer_units = Vec::new();
        let mut chain_units = Vec::new();
        for unit in &plan.units {
            match unit.single_placement() {
                Some(Placement::Home(Home::Store)) => store_units.push(unit),
                Some(Placement::Home(Home::Computer)) => computer_units.push(unit),
                _ => chain_units.push(unit),
            }
        }
        let whole = [(Home::Store, store_units), (Home::Computer, computer_units)]
            .into_iter()
            .filter(|(_, units)| !units.is_empty())
            .map(|(home, units)| send_units(routing, home, units, parsed, sender));
        let chains = chain_units
            .into_iter()
            .map(|unit| run_unit(routing, unit, parsed, sender));
        let (whole, chains) = join(join_all(whole), join_all(chains)).await;
        placed.extend(whole.into_iter().chain(chains).flatten());
    } else {
        for unit in &plan.units {
            placed.extend(run_unit(routing, unit, parsed, sender).await);
        }
    }
    assemble(parsed, placed)
}

/// The envelope of a request that was not answered at all: every operation
/// fails with `error` and `disposition`.
pub(super) fn failed_envelope(
    parsed: &ParsedRequest,
    error: Value,
    disposition: DomainDisposition,
) -> Value {
    let placed = parsed
        .ops
        .iter()
        .enumerate()
        .map(|(index, op)| {
            let entry = failure_entry(op.tool.clone(), error.clone(), disposition);
            (index, entry)
        })
        .collect();
    assemble(parsed, placed)
}

/// Sends units that all have one home to it as one bracketed batch.
async fn send_units(
    routing: &ComputerRouting,
    home: Home,
    units: Vec<&UnitPlan>,
    parsed: &ParsedRequest,
    sender: &dyn HomeSender,
) -> Vec<(usize, Value)> {
    let mut placed = Vec::new();
    let mut texts = Vec::with_capacity(units.len());
    let mut indices = Vec::new();
    for unit in units {
        let range = unit.ops();
        match render_unit(&parsed.ops[range.clone()]) {
            Ok(text) => {
                texts.push(text);
                indices.extend(range);
            }
            Err(error) => {
                let entry = refused(parsed, range.start, not_routable(&error));
                placed.extend(fail_range(parsed, range, entry));
            }
        }
    }
    if !texts.is_empty() {
        let ops = format!("[{}]", texts.join(", "));
        let outcome = sender.send(home, ops, indices.clone()).await;
        placed.extend(entries_from_outcome(
            routing, home, outcome, &indices, parsed,
        ));
    }
    placed
}

/// Runs one unit by the chain rule (ADR-220 section 7).
async fn run_unit(
    routing: &ComputerRouting,
    unit: &UnitPlan,
    parsed: &ParsedRequest,
    sender: &dyn HomeSender,
) -> Vec<(usize, Value)> {
    let mut placed = Vec::new();
    let mut prev: Option<Value> = None;
    let mut failed_at: Option<usize> = None;
    for segment in &unit.segments {
        if let Some(failed_at) = failed_at {
            placed.extend(
                segment
                    .ops
                    .clone()
                    .map(|index| (index, chain_aborted_entry(parsed, index, failed_at))),
            );
            continue;
        }
        let first = segment.ops.start;
        let home = match segment.placement {
            Placement::Home(home) => home,
            Placement::RunsOnComputer => {
                let entry = routing.runs_on_computer_entry(&parsed.ops[first].tool);
                placed.extend(fail_range(parsed, segment.ops.clone(), entry));
                failed_at = Some(first);
                continue;
            }
        };
        let mut ops = parsed.ops[segment.ops.clone()].to_vec();
        if let (Some(prev), Some(op)) = (&prev, ops.first_mut()) {
            if let Err(error) = resolve_prev_to_literals(op, prev) {
                let entry = refused(parsed, first, error);
                placed.extend(fail_range(parsed, segment.ops.clone(), entry));
                failed_at = Some(first);
                continue;
            }
        }
        let text = match render_unit(&ops) {
            Ok(text) => text,
            Err(error) => {
                let entry = refused(parsed, first, not_routable(&error));
                placed.extend(fail_range(parsed, segment.ops.clone(), entry));
                failed_at = Some(first);
                continue;
            }
        };
        let indices: Vec<usize> = segment.ops.clone().collect();
        let outcome = sender.send(home, text, indices.clone()).await;
        let entries = entries_from_outcome(routing, home, outcome, &indices, parsed);
        failed_at = entries
            .iter()
            .find(|(_, entry)| !entry_succeeded(entry))
            .map(|(index, _)| *index);
        prev = entries
            .last()
            .map(|(_, entry)| entry.get("result").cloned().unwrap_or(Value::Null));
        placed.extend(entries);
    }
    placed
}

/// Replaces every `$prev` reference in `op` with the value it names in `prev`,
/// or returns the dispatcher's own `substitution_error` for the first one that
/// does not resolve.
fn resolve_prev_to_literals(op: &mut ParsedOp, prev: &Value) -> Result<(), Value> {
    for (name, arg) in &mut op.args {
        if matches!(arg, ArgValue::Value(_)) {
            continue;
        }
        match arg.resolve_all(prev) {
            Some(value) => *arg = ArgValue::Value(value),
            None => return Err(substitution_error_payload(name, arg, prev)),
        }
    }
    Ok(())
}

fn entries_from_outcome(
    routing: &ComputerRouting,
    home: Home,
    outcome: HomeOutcome,
    indices: &[usize],
    parsed: &ParsedRequest,
) -> Vec<(usize, Value)> {
    let (error, disposition) = match outcome {
        HomeOutcome::Envelope(envelope) => match envelope_entries(envelope, indices, parsed) {
            Ok(entries) => return indices.iter().copied().zip(entries).collect(),
            Err(problem) => (
                malformed_response(routing, home, &problem),
                DomainDisposition::Unknown,
            ),
        },
        HomeOutcome::Failed { error, disposition } => (error, disposition),
    };
    indices
        .iter()
        .map(|&index| {
            let tool = parsed.ops[index].tool.clone();
            (index, failure_entry(tool, error.clone(), disposition))
        })
        .collect()
}

/// The entries of a home's envelope, checked to answer `indices` one for one.
fn envelope_entries(
    envelope: Value,
    indices: &[usize],
    parsed: &ParsedRequest,
) -> Result<Vec<Value>, String> {
    let Value::Object(mut envelope) = envelope else {
        return Err("the response is not a JSON object".into());
    };
    let Some(Value::Array(entries)) = envelope.remove("results") else {
        return Err("the response has no results array".into());
    };
    if entries.len() != indices.len() {
        return Err(format!(
            "the response has {} results for {} operations",
            entries.len(),
            indices.len()
        ));
    }
    for (position, (entry, &index)) in entries.iter().zip(indices).enumerate() {
        let expected = parsed.ops[index].tool.as_str();
        let answers = entry.get("tool").and_then(Value::as_str) == Some(expected)
            && entry.get("ok").and_then(Value::as_bool).is_some();
        if !answers {
            return Err(format!("result {position} does not answer {expected}"));
        }
    }
    Ok(entries)
}

fn malformed_response(routing: &ComputerRouting, home: Home, problem: &str) -> Value {
    match home {
        Home::Computer => json!({
            "kind": COMPUTER_PROTOCOL,
            "message": format!(
                "computer {} answered with a response that is not the khive envelope: {problem}",
                routing.label
            ),
        }),
        Home::Store => json!({
            "kind": "internal_error",
            "message": format!(
                "the store answered a routed request with a response that is not the khive \
                 envelope: {problem}"
            ),
        }),
    }
}

fn entry_succeeded(entry: &Value) -> bool {
    entry.get("ok").and_then(Value::as_bool) == Some(true)
}

fn not_routable(error: &RenderError) -> Value {
    json!({
        "kind": "invalid_params",
        "message": format!("the operation was not run: {error}"),
    })
}

fn refused(parsed: &ParsedRequest, index: usize, error: Value) -> Value {
    let tool = parsed.ops[index].tool.clone();
    failure_entry(tool, error, DomainDisposition::NotCommitted)
}

/// The first operation of `range` fails with `entry`; the rest are aborted.
fn fail_range(parsed: &ParsedRequest, range: Range<usize>, entry: Value) -> Vec<(usize, Value)> {
    let failed_at = range.start;
    let mut placed = vec![(failed_at, entry)];
    placed.extend(
        range
            .skip(1)
            .map(|index| (index, chain_aborted_entry(parsed, index, failed_at))),
    );
    placed
}

fn chain_aborted_entry(parsed: &ParsedRequest, index: usize, failed_at: usize) -> Value {
    let failed_tool = &parsed.ops[failed_at].tool;
    aborted_entry(
        parsed.ops[index].tool.clone(),
        Some(format!(
            "not executed: op #{failed_at} ({failed_tool:?}) failed earlier in this chain, \
             so the chain aborted before reaching this op. Fix op #{failed_at}; this op's \
             own arguments, including any $prev reference, were never evaluated."
        )),
    )
}

fn assemble(parsed: &ParsedRequest, placed: Vec<(usize, Value)>) -> Value {
    let mut slots: Vec<Option<Value>> = vec![None; parsed.ops.len()];
    for (index, entry) in placed {
        if let Some(slot) = slots.get_mut(index) {
            *slot = Some(entry);
        }
    }
    let results: Vec<Value> = slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.unwrap_or_else(|| {
                let tool = parsed.ops[index].tool.clone();
                let error = json!({
                    "kind": "internal_error",
                    "message": "no result was assembled for this operation",
                });
                failure_entry(tool, error, DomainDisposition::Unknown)
            })
        })
        .collect();
    let (mut succeeded, mut failed, mut aborted) = (0usize, 0usize, 0usize);
    for entry in &results {
        if entry_succeeded(entry) {
            succeeded += 1;
        } else if entry.get("aborted").and_then(Value::as_bool) == Some(true) {
            aborted += 1;
        } else {
            failed += 1;
        }
    }
    json!({
        "results": results,
        "summary": {
            "total": parsed.ops.len(),
            "succeeded": succeeded,
            "failed": failed,
            "aborted": aborted,
        },
        "status": batch_status(failed, aborted),
    })
}

#[cfg(test)]
#[path = "computer_routing_tests.rs"]
mod tests;
