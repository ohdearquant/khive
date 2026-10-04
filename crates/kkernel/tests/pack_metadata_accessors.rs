//! Every pack states its metadata twice: as associated consts in `impl Pack`, and
//! as accessors in `impl PackRuntime` that must return the same values (`Pack`
//! has associated consts, so it cannot be a trait object, and the runtime holds
//! packs as `dyn PackRuntime`). This test pins the two statements together for
//! every pack linked into this binary.
//!
//! Compared: name, note kinds, entity kinds, handlers, edge rules, requires and
//! note kind specs. Not compared: `schema_plan` (a `SchemaPlan`, against an
//! `Option<PackSchemaPlan>` const) and `validation_rules` (rule values, against
//! a const of rule ids) have no const of the same type, and
//! `brain_consumer_kinds`, `entity_types`, `note_embedding_policies` and
//! `schema_column_additions` are outside the compared list.

use async_trait::async_trait;
use khive_pack_agent::AgentPack;
use khive_pack_blob::BlobPack;
use khive_pack_brain::BrainPack;
use khive_pack_code::CodePack;
use khive_pack_comm::CommPack;
use khive_pack_exec::ExecPack;
#[cfg(feature = "pack-formal")]
use khive_pack_formal::FormalPack;
use khive_pack_git::GitPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_pack_memory::MemoryPack;
#[cfg(feature = "pack-moodboard")]
use khive_pack_moodboard::MoodboardPack;
use khive_pack_schedule::SchedulePack;
use khive_pack_session::SessionPack;
use khive_pack_telemetry::TelemetryPack;
use khive_pack_tool::ToolPack;
use khive_pack_web::WebPack;
use khive_pack_workspace::WorkspacePack;
use khive_runtime::pack::{PackRegistry, PackRuntime};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{EdgeEndpointRule, HandlerDef, NoteKindSpec, Pack};
// Keep kkernel's production force-link anchors, including feature-gated packs.
use kkernel as _;
use serde_json::Value;

/// One pack's `Pack::NAME` and the name of every accessor that disagrees with
/// the const it restates.
struct Checked {
    name: &'static str,
    drift: Vec<&'static str>,
}

/// Compares each accessor reported through `PackRuntime` with the `Pack` const
/// it restates, and returns the name of every accessor that disagrees.
fn drift<P: Pack + PackRuntime>(pack: &P) -> Vec<&'static str> {
    let mut disagree = Vec::new();
    if pack.name() != P::NAME {
        disagree.push("name");
    }
    if pack.note_kinds() != P::NOTE_KINDS {
        disagree.push("note_kinds");
    }
    if pack.entity_kinds() != P::ENTITY_KINDS {
        disagree.push("entity_kinds");
    }
    if pack.handlers() != P::HANDLERS {
        disagree.push("handlers");
    }
    if pack.edge_rules() != P::EDGE_RULES {
        disagree.push("edge_rules");
    }
    if pack.requires() != P::REQUIRES {
        disagree.push("requires");
    }
    if pack.note_kind_specs() != P::NOTE_KIND_SPECS {
        disagree.push("note_kind_specs");
    }
    disagree
}

fn checked<P: Pack + PackRuntime>(pack: &P) -> Checked {
    Checked {
        name: P::NAME,
        drift: drift(pack),
    }
}

/// One row per pack this binary links. A pack added later needs a row here:
/// `every_linked_pack_has_a_row_in_this_check` fails until it has one.
fn linked_packs() -> Vec<Checked> {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    #[allow(unused_mut)] // only the optional-pack pushes below need `mut`
    let mut packs = vec![
        checked(&AgentPack::from_runtime(rt.clone())),
        checked(&BlobPack::new(rt.clone())),
        // The factory installs `BrainPackRuntime`, which delegates every
        // accessor to the `BrainPack` compared here.
        checked(&BrainPack::new(rt.clone())),
        checked(&CodePack::new(rt.clone())),
        checked(&CommPack::new(rt.clone())),
        checked(&ExecPack::new(rt.clone())),
        checked(&GitPack::new(rt.clone())),
        checked(&GtdPack::new(rt.clone())),
        checked(&KgPack::new(rt.clone())),
        // `false` is the role the serving factory gives a short-lived client: it
        // serves the persisted index and builds nothing from the corpus.
        checked(&KnowledgePack::new_with_index_role(rt.clone(), false)),
        checked(&MemoryPack::new_with_index_role(rt.clone(), false)),
        checked(&SchedulePack::new(rt.clone())),
        checked(&SessionPack::new(rt.clone())),
        checked(&TelemetryPack::new(rt.clone())),
        checked(&ToolPack::new(rt.clone())),
        checked(&WebPack::new(rt.clone())),
        checked(&WorkspacePack::new(rt.clone())),
    ];
    #[cfg(feature = "pack-formal")]
    packs.push(checked(&FormalPack::new(rt.clone())));
    #[cfg(feature = "pack-moodboard")]
    packs.push(checked(&MoodboardPack::new(rt.clone())));
    packs
}

/// Declares one thing in `Pack` and another in `PackRuntime` for every compared
/// item, so `drift` can be seen to fail on each of them.
struct DriftedPack;

impl Pack for DriftedPack {
    const NAME: &'static str = "declared";
    const NOTE_KINDS: &'static [&'static str] = &["declared_note"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for DriftedPack {
    fn name(&self) -> &str {
        "restated"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &["restated_note"]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &["restated_entity"]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        <GtdPack as Pack>::HANDLERS
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        <GtdPack as Pack>::EDGE_RULES
    }
    fn requires(&self) -> &'static [&'static str] {
        &["restated_dep"]
    }
    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        <GtdPack as Pack>::NOTE_KIND_SPECS
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "drifted control pack has no handler for verb {verb:?}"
        )))
    }
}

#[test]
fn every_linked_pack_runtime_accessor_equals_its_pack_const() {
    let drifted: Vec<String> = linked_packs()
        .iter()
        .filter(|row| !row.drift.is_empty())
        .map(|row| format!("{}: {}", row.name, row.drift.join(", ")))
        .collect();
    assert!(
        drifted.is_empty(),
        "PackRuntime accessors disagree with the Pack consts they restate: {drifted:?}"
    );
}

#[test]
fn every_linked_pack_has_a_row_in_this_check() {
    let mut linked = PackRegistry::discovered_names();
    linked.sort_unstable();
    let rows = linked_packs();
    let mut covered: Vec<&str> = rows.iter().map(|row| row.name).collect();
    covered.sort_unstable();
    assert_eq!(
        linked, covered,
        "a linked pack without a row in linked_packs() is not compared with its consts"
    );
}

#[test]
fn drift_check_flags_every_accessor_that_disagrees_with_its_const() {
    let reported = drift(&DriftedPack).join(",");
    let expected = "name,note_kinds,entity_kinds,handlers,edge_rules,requires,note_kind_specs";
    assert_eq!(
        reported, expected,
        "every compared accessor must be able to report a disagreement with its const"
    );
}
