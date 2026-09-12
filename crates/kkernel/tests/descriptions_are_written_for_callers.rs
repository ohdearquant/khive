//! Every verb and parameter description a caller is handed is a published text.
//!
//! These strings are rendered verbatim into the schema the MCP surface serves and
//! into the connector schema a consumer product publishes, so they reach end users
//! and model callers, not only engineers. They were written for engineers, and a
//! number of them still carry decision-record numbers, issue numbers, crate paths
//! and internal function names. One shipped an entire edge endpoint rule table with
//! a hand-maintained-mirror caveat and a source path.
//!
//! Nothing about that is caught by review, because the text reads correct to the
//! person writing it: the second audience arrived after the words did. This test is
//! the forced consumer. It scans the published surface of every pack the server
//! links by default and fails on an internal reference that is not in the recorded
//! baseline below.
//!
//! The population is the pack list in `khive-mcp`'s `pack` module minus the two
//! feature-gated packs. A pack added there and not here is a hole in this test, so
//! the scan asserts it walked a real surface before believing any absence.
//!
//! The baseline may only SHRINK. An entry that no longer matches fails the test too,
//! so the list cannot quietly rot into a permission slip.

use khive_types::pack::{HandlerDef, Pack, Visibility};

/// Patterns that mean "written for someone with the repository open".
fn internal_reference(text: &str) -> Option<&'static str> {
    if text.contains("ADR-") {
        return Some("decision-record number");
    }
    if text.contains("crates/") || text.contains(".rs`") || text.contains(".rs ") {
        return Some("source path");
    }
    if text.contains("khive#") || text.contains("issue #") {
        return Some("issue reference");
    }
    // A bare `#1234` is an issue number; a `#` followed by fewer digits is not
    // (shard labels, fragment anchors), so the threshold is deliberate.
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' {
            let digits = bytes[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if digits >= 3 {
                return Some("issue number");
            }
        }
    }
    None
}

/// One recorded offender: pack, verb, and the parameter name, or the verb's own
/// description when the name is empty.
type Site = (&'static str, &'static str, &'static str);

/// Known at the time this test landed. Shrink it; never add to it.
///
/// A new description carrying one of these patterns is a defect in the new text,
/// not a gap in this list: write the sentence for the caller and put the
/// engineering detail in a `//` comment beside the definition, where it still
/// reaches the engineer and does not reach the product.
const BASELINE: &[Site] = &[];

fn collect(pack: &'static str, handlers: &'static [HandlerDef], out: &mut Vec<(Site, String)>) {
    for h in handlers {
        // `Subhandler` is operator-only; `Verb` is what the MCP surface publishes.
        if h.visibility != Visibility::Verb {
            continue;
        }
        if let Some(kind) = internal_reference(h.description) {
            out.push(((pack, h.name, ""), kind.to_string()));
        }
        for p in h.params {
            if let Some(kind) = internal_reference(p.description) {
                out.push(((pack, h.name, p.name), kind.to_string()));
            }
        }
    }
}

fn offenders() -> Vec<(Site, String)> {
    let mut out = Vec::new();
    collect("agent", khive_pack_agent::AgentPack::HANDLERS, &mut out);
    collect("blob", khive_pack_blob::BlobPack::HANDLERS, &mut out);
    collect("brain", khive_pack_brain::BrainPack::HANDLERS, &mut out);
    collect("code", khive_pack_code::CodePack::HANDLERS, &mut out);
    collect("comm", khive_pack_comm::CommPack::HANDLERS, &mut out);
    collect("exec", khive_pack_exec::ExecPack::HANDLERS, &mut out);
    collect("git", khive_pack_git::GitPack::HANDLERS, &mut out);
    collect("gtd", khive_pack_gtd::GtdPack::HANDLERS, &mut out);
    collect("kg", khive_pack_kg::KgPack::HANDLERS, &mut out);
    collect(
        "knowledge",
        khive_pack_knowledge::KnowledgePack::HANDLERS,
        &mut out,
    );
    collect("memory", khive_pack_memory::MemoryPack::HANDLERS, &mut out);
    collect(
        "schedule",
        khive_pack_schedule::SchedulePack::HANDLERS,
        &mut out,
    );
    collect(
        "session",
        khive_pack_session::SessionPack::HANDLERS,
        &mut out,
    );
    collect(
        "telemetry",
        khive_pack_telemetry::TelemetryPack::HANDLERS,
        &mut out,
    );
    collect("tool", khive_pack_tool::ToolPack::HANDLERS, &mut out);
    collect("web", khive_pack_web::WebPack::HANDLERS, &mut out);
    collect(
        "workspace",
        khive_pack_workspace::WorkspacePack::HANDLERS,
        &mut out,
    );
    out
}

#[test]
fn no_shipped_description_carries_an_internal_reference() {
    // Assert the instrument reads a non-empty population before believing any
    // absence it reports: a scan that walks nothing passes silently.
    let mut scanned = 0usize;
    for handlers in [
        khive_pack_kg::KgPack::HANDLERS,
        khive_pack_gtd::GtdPack::HANDLERS,
    ] {
        scanned += handlers
            .iter()
            .filter(|h| h.visibility == Visibility::Verb)
            .count();
    }
    assert!(
        scanned > 5,
        "control: the scan must see a real surface, saw {scanned} visible handlers"
    );

    // And that the detector fires on a sentence carrying each pattern.
    assert!(internal_reference("see ADR-169 for the rule").is_some());
    assert!(internal_reference("defined in crates/khive-runtime/src/pack.rs").is_some());
    assert!(internal_reference("regression for #1234").is_some());
    assert!(
        internal_reference("An IANA zone name, e.g. America/New_York.").is_none(),
        "the detector must not fire on ordinary caller-facing prose"
    );

    let found = offenders();
    let unrecorded: Vec<_> = found
        .iter()
        .filter(|(site, _)| !BASELINE.contains(site))
        .collect();
    assert!(
        unrecorded.is_empty(),
        "these shipped descriptions carry internal references:\n{}",
        unrecorded
            .iter()
            .map(|((pack, verb, param), kind)| format!(
                "  {pack}.{verb}{}{param} — {kind}",
                if param.is_empty() { "" } else { "." }
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let stale: Vec<_> = BASELINE
        .iter()
        .filter(|site| !found.iter().any(|(f, _)| f == *site))
        .collect();
    assert!(
        stale.is_empty(),
        "baseline entries no longer match and must be deleted from the list: {stale:?}"
    );
}
