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

use khive_runtime::RuntimeConfig;
use khive_types::pack::{HandlerDef, Pack, Visibility};
use std::collections::BTreeSet;

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

/// Packs linked into this binary, paired with the handler table each publishes.
///
/// This list is a claim about the population and the test checks it against the
/// runtime's own declaration rather than trusting it, because a hand-typed
/// population is exactly what goes stale: an earlier revision of this file was
/// one pack short and reported a clean scan of a surface it had not walked.
fn packs() -> Vec<(&'static str, &'static [HandlerDef])> {
    vec![
        ("agent", khive_pack_agent::AgentPack::HANDLERS),
        ("blob", khive_pack_blob::BlobPack::HANDLERS),
        ("brain", khive_pack_brain::BrainPack::HANDLERS),
        ("code", khive_pack_code::CodePack::HANDLERS),
        ("comm", khive_pack_comm::CommPack::HANDLERS),
        ("exec", khive_pack_exec::ExecPack::HANDLERS),
        ("git", khive_pack_git::GitPack::HANDLERS),
        ("gtd", khive_pack_gtd::GtdPack::HANDLERS),
        ("kg", khive_pack_kg::KgPack::HANDLERS),
        ("knowledge", khive_pack_knowledge::KnowledgePack::HANDLERS),
        ("memory", khive_pack_memory::MemoryPack::HANDLERS),
        ("schedule", khive_pack_schedule::SchedulePack::HANDLERS),
        ("session", khive_pack_session::SessionPack::HANDLERS),
        ("telemetry", khive_pack_telemetry::TelemetryPack::HANDLERS),
        ("tool", khive_pack_tool::ToolPack::HANDLERS),
        ("web", khive_pack_web::WebPack::HANDLERS),
        ("workspace", khive_pack_workspace::WorkspacePack::HANDLERS),
    ]
}

/// Linked and publishable, but outside the default set a fresh install loads.
/// A deployment turns these on by configuration, with no rebuild, so their
/// descriptions reach callers and belong in this scan.
const PACKS_OUTSIDE_THE_SHIPPED_SET: [&str; 3] = ["agent", "telemetry", "web"];

fn offenders() -> Vec<(Site, String)> {
    let mut out = Vec::new();
    for (name, handlers) in packs() {
        collect(name, handlers, &mut out);
    }
    out
}

#[test]
fn no_shipped_description_carries_an_internal_reference() {
    // The population this scan claims to cover, checked against the runtime's
    // own declaration. Without this the list above is a hand-typed copy that
    // goes quietly stale, and a pack missing from it reads as a clean surface.
    let scanned_packs: BTreeSet<String> = packs()
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect();
    let shipped: BTreeSet<String> = RuntimeConfig::built_in_packs().into_iter().collect();
    let unscanned: Vec<&String> = shipped.difference(&scanned_packs).collect();
    assert!(
        unscanned.is_empty(),
        "these packs ship by default and this scan does not walk them: {unscanned:?}"
    );
    let beyond_shipped: BTreeSet<String> = scanned_packs.difference(&shipped).cloned().collect();
    let declared_extra: BTreeSet<String> = PACKS_OUTSIDE_THE_SHIPPED_SET
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        beyond_shipped, declared_extra,
        "the packs scanned beyond the shipped set must be the ones declared above"
    );

    // Assert the instrument reads a non-empty population before believing any
    // absence it reports: a scan that walks nothing passes silently.
    let scanned: usize = packs()
        .into_iter()
        .flat_map(|(_, handlers)| handlers.iter())
        .filter(|h| h.visibility == Visibility::Verb)
        .count();
    assert!(
        scanned > 50,
        "control: the scan must see a real surface, saw {scanned} published handlers"
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
