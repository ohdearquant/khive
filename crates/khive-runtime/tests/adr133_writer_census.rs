//! ADR-133 D8: writer-classification census scanner.
//!
//! Verifies the mechanism the full census depends on rather than the full
//! 100-verb population: the manifest is pinned to a source revision and to
//! the exact built-in pack set, and the scanner is fail-closed — absence of
//! evidence for a write classifies `UNKNOWN`, never `NO-WRITER` — proved by
//! re-deriving the classification from live source at test time rather than
//! trusting the manifest's own claim.
//!
//! `comm.read` is the mandatory positive control (ADR-133 acceptance
//! criterion 2): a scanner that fails to classify it `WRITER` voids its own
//! run.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct WriterCensusManifest {
    schema: String,
    base_revision: String,
    #[allow(dead_code)]
    final_revision: Option<String>,
    pack_set: Vec<String>,
    positive_control: String,
    entries: Vec<WriterCensusEntry>,
}

#[derive(Debug, Deserialize)]
struct WriterCensusEntry {
    verb: String,
    classification: Classification,
    source_file: String,
    evidence_marker: String,
    #[allow(dead_code)]
    note: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
enum Classification {
    #[serde(rename = "WRITER")]
    Writer,
    #[allow(dead_code)]
    #[serde(rename = "WRITER-COND")]
    WriterCond,
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

/// The pinned revision this census's `entries` were classified against. Must
/// match `9b53aaff2f2260e68c0a0cbe2eeace77cb087a8e` (the tip this Round-2
/// slice built from) exactly: the committed manifest is only meaningful
/// against the exact source it describes, not "close enough".
const PINNED_BASE_REVISION: &str = "9b53aaff2f2260e68c0a0cbe2eeace77cb087a8e";

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}/../..", env!("CARGO_MANIFEST_DIR")))
}

fn load_manifest() -> WriterCensusManifest {
    let manifest_path = format!(
        "{}/tests/data/adr133-writer-census.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("failed to read {manifest_path}: {e}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{manifest_path} is not a valid writer-census manifest: {e}"))
}

/// Fail-closed re-classification from live source: `WRITER` only when the
/// entry's declared evidence marker is actually present in the entry's
/// declared source file, read fresh at test time. Any other outcome —
/// missing file, missing marker — classifies `UNKNOWN`. This function must
/// never return a classification stronger than the evidence it just read,
/// which is what makes the positive control below a real assertion about
/// source rather than a restatement of the manifest's own claim.
fn reclassify_from_live_source(entry: &WriterCensusEntry) -> Classification {
    let path = workspace_root().join(&entry.source_file);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Classification::Unknown;
    };
    if text.contains(&entry.evidence_marker) {
        Classification::Writer
    } else {
        Classification::Unknown
    }
}

#[test]
fn writer_census_is_revision_and_pack_pinned() {
    let manifest = load_manifest();

    assert_eq!(
        manifest.schema, "adr133-writer-census/v1",
        "manifest must declare its schema version explicitly"
    );
    assert_eq!(
        manifest.base_revision, PINNED_BASE_REVISION,
        "the committed manifest must be pinned to the exact revision it was classified \
         against — a drifted pin makes every entry's evidence stale"
    );

    let mut manifest_pack_set = manifest.pack_set.clone();
    manifest_pack_set.sort();
    let mut live_pack_set = khive_runtime::RuntimeConfig::built_in_packs();
    live_pack_set.sort();
    assert_eq!(
        manifest_pack_set, live_pack_set,
        "the manifest's pinned pack set must equal the live built-in pack set exactly — \
         population is deployment-dependent (ADR-133), so a mismatch here means the manifest \
         describes a different server than the one under test"
    );

    assert_eq!(
        manifest.positive_control, "comm.read",
        "the positive control itself is pinned: this file must not silently swap it for a \
         verb the scanner finds easier to classify"
    );

    // No entry may claim a stronger classification than the manifest's own
    // evidence supports at commit time: WRITER/WRITER-COND require a
    // non-empty marker, and the fixture must never assert NO-WRITER (not a
    // variant this schema even accepts) for anything.
    for entry in &manifest.entries {
        assert!(
            !entry.evidence_marker.trim().is_empty()
                || entry.classification == Classification::Unknown,
            "entry {:?} claims {:?} with no evidence marker; fail-closed requires \
             evidence for every non-UNKNOWN classification",
            entry.verb,
            entry.classification
        );
    }
}

#[test]
fn writer_census_positive_control_comm_read() {
    let manifest = load_manifest();

    let comm_read = manifest
        .entries
        .iter()
        .find(|e| e.verb == manifest.positive_control)
        .unwrap_or_else(|| {
            panic!(
                "manifest must carry an entry for its declared positive control {:?}",
                manifest.positive_control
            )
        });

    assert_eq!(
        comm_read.classification,
        Classification::Writer,
        "comm.read must be classified WRITER in the manifest itself (ADR-133 acceptance \
         criterion 2's positive control)"
    );

    // The real assertion: re-derive the classification from source fresh,
    // rather than trusting the manifest's static claim. A scanner that
    // cannot find the evidence for its own known-positive control voids the
    // whole run.
    let live = reclassify_from_live_source(comm_read);
    assert_eq!(
        live,
        Classification::Writer,
        "the census scanner failed to classify comm.read as a writer against live source \
         ({} :: {:?}); per ADR-133 this voids the run rather than being reported as a pass",
        comm_read.source_file,
        comm_read.evidence_marker
    );
}

#[test]
fn writer_census_scanner_is_fail_closed_on_missing_evidence() {
    let manifest = load_manifest();

    let unknown_entries: Vec<&WriterCensusEntry> = manifest
        .entries
        .iter()
        .filter(|e| e.classification == Classification::Unknown)
        .collect();
    assert!(
        !unknown_entries.is_empty(),
        "fixture must exercise at least one UNKNOWN entry, or this test cannot distinguish \
         a scanner that always answers WRITER from one that actually reads evidence"
    );

    for entry in unknown_entries {
        let live = reclassify_from_live_source(entry);
        assert_eq!(
            live,
            Classification::Unknown,
            "entry {:?} is declared UNKNOWN but the scanner found its marker in live source; \
             the fixture and live source have drifted",
            entry.verb
        );
    }
}

#[test]
fn writer_census_manifest_round_trips_as_json() {
    let manifest_path = format!(
        "{}/tests/data/adr133-writer-census.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&manifest_path).expect("manifest must be readable");
    let value: Value = serde_json::from_str(&text).expect("manifest must be valid JSON");
    assert!(
        value.get("entries").and_then(Value::as_array).is_some(),
        "manifest must carry an entries array"
    );
}
