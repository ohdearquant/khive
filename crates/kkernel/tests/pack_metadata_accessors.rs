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
//! `brain_consumer_kinds` is outside the compared list. Brain is checked through
//! its factory-installed runtime, including entity types, embedding policies and
//! column additions; the other packs retain the seven accessor comparisons.

use async_trait::async_trait;
#[cfg(feature = "pack-agent")]
use khive_pack_agent::AgentPack;
use khive_pack_blob::BlobPack;
use khive_pack_brain::BrainPack;
use khive_pack_charter::CharterPack;
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
#[cfg(feature = "pack-telemetry")]
use khive_pack_telemetry::TelemetryPack;
use khive_pack_tool::ToolPack;
#[cfg(feature = "pack-web")]
use khive_pack_web::WebPack;
use khive_pack_workspace::WorkspacePack;
use khive_runtime::pack::{PackRegistry, PackRuntime};
use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackFactory, PackInstall, RuntimeError, SchemaPlan, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{
    EdgeEndpointRule, EntityKind, EntityTypeDef, HandlerDef, NoteEmbeddingPolicy,
    NoteEmbeddingPolicySpec, NoteKindSpec, Pack, PackColumnAddition, PackColumnAffinity,
    PackSchemaPlan,
};
use std::collections::HashMap;
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

// Both Brain and the nonempty controls depend on KG. Observe the installed
// registry, preserving dependency order for metadata with aggregate accessors.
fn installed_drift<P: Pack>(registry: &VerbRegistry) -> Vec<&'static str> {
    let mut disagree = Vec::new();
    let names = registry.pack_names();
    let expected_names = [<KgPack as Pack>::NAME, P::NAME];
    if names != expected_names {
        disagree.push("name");
    }
    if registry.pack_note_kinds(P::NAME) != Some(P::NOTE_KINDS) {
        disagree.push("note_kinds");
    }
    if registry.pack_entity_kinds(P::NAME) != Some(P::ENTITY_KINDS) {
        disagree.push("entity_kinds");
    }
    if registry.pack_verbs(P::NAME) != Some(P::HANDLERS) {
        disagree.push("handlers");
    }
    if registry.all_edge_rules() != [<KgPack as Pack>::EDGE_RULES, P::EDGE_RULES].concat() {
        disagree.push("edge_rules");
    }
    if registry.pack_requires(P::NAME) != Some(P::REQUIRES) {
        disagree.push("requires");
    }
    let expected_specs: Vec<_> = <KgPack as Pack>::NOTE_KIND_SPECS
        .iter()
        .chain(P::NOTE_KIND_SPECS)
        .collect();
    if registry.all_note_kind_specs() != expected_specs {
        disagree.push("note_kind_specs");
    }
    let entity_types = registry.all_entity_types();
    let actual_types: Vec<_> = entity_types
        .iter()
        .map(|definition| (definition.kind, definition.type_name, definition.aliases))
        .collect();
    let expected_types: Vec<_> = <KgPack as Pack>::ENTITY_TYPES
        .iter()
        .chain(P::ENTITY_TYPES)
        .map(|definition| (definition.kind, definition.type_name, definition.aliases))
        .collect();
    if actual_types != expected_types {
        disagree.push("entity_types");
    }
    if registry.all_note_embedding_policies()
        != [
            <KgPack as Pack>::NOTE_EMBEDDING_POLICIES,
            P::NOTE_EMBEDDING_POLICIES,
        ]
        .concat()
    {
        disagree.push("note_embedding_policies");
    }
    // Empty schema plans have pack="". Pair each plan with its installed pack
    // name rather than treating that empty string as an ownership key.
    let schemas = registry.all_schema_plans_with_columns();
    let actual_columns: Vec<_> = names
        .iter()
        .zip(&schemas)
        .map(|(name, (plan, columns))| (*name, plan.pack, *columns))
        .collect();
    let expected_columns = [
        (
            <KgPack as Pack>::NAME,
            <KgPack as Pack>::SCHEMA_PLAN.map_or("", |plan| plan.pack),
            <KgPack as Pack>::SCHEMA_COLUMN_ADDITIONS,
        ),
        (
            P::NAME,
            P::SCHEMA_PLAN.map_or("", |plan| plan.pack),
            P::SCHEMA_COLUMN_ADDITIONS,
        ),
    ];
    if schemas.len() != names.len() || actual_columns != expected_columns {
        disagree.push("schema_column_additions");
    }
    disagree
}

fn checked_brain_install(runtime: &KhiveRuntime) -> Checked {
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &[
            <KgPack as Pack>::NAME.to_owned(),
            <BrainPack as Pack>::NAME.to_owned(),
        ],
        runtime.clone(),
        &mut builder,
    )
    .expect("install the linked brain factory and its KG dependency");
    let registry = builder.build().expect("valid brain metadata");
    Checked {
        name: <BrainPack as Pack>::NAME,
        drift: installed_drift::<BrainPack>(&registry),
    }
}

/// One row per pack this binary links. A pack added later needs a row here:
/// `every_linked_pack_has_a_row_in_this_check` fails until it has one.
fn linked_packs() -> Vec<Checked> {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    #[allow(unused_mut)] // only the optional-pack pushes below need `mut`
    let mut packs = vec![
        checked(&BlobPack::new(rt.clone())),
        checked_brain_install(&rt),
        checked(&CharterPack::new(rt.clone())),
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
        checked(&ToolPack::new(rt.clone())),
        checked(&WorkspacePack::new(rt.clone())),
    ];
    #[cfg(feature = "pack-agent")]
    packs.push(checked(&AgentPack::from_runtime(rt.clone())));
    #[cfg(feature = "pack-telemetry")]
    packs.push(checked(&TelemetryPack::new(rt.clone())));
    #[cfg(feature = "pack-web")]
    packs.push(checked(&WebPack::new(rt.clone())));
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

#[derive(Clone, Copy, Debug)]
enum InstallFault {
    None,
    MissingEntityTypes,
    MissingEmbeddingPolicies,
    MissingColumnAdditions,
    MissingAll,
    WrongAlias,
    WrongEmbeddingPolicy,
    WrongColumnAffinity,
}

struct InstalledProbe(InstallFault);

impl Pack for InstalledProbe {
    const NAME: &'static str = "metadata_probe_4206";
    const NOTE_KINDS: &'static [&'static str] = &["metadata_probe_note_4206"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "metadata_probe_type_4206",
        aliases: &["metadata_probe_alias_4206"],
    }];
    const NOTE_EMBEDDING_POLICIES: &'static [NoteEmbeddingPolicySpec] =
        &[NoteEmbeddingPolicySpec {
            kind: "metadata_probe_note_4206",
            policy: NoteEmbeddingPolicy::DefaultModel,
        }];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: Self::NAME,
        statements: &["CREATE TABLE IF NOT EXISTS metadata_probe_state_4206 (value TEXT)"],
    });
    const SCHEMA_COLUMN_ADDITIONS: &'static [PackColumnAddition] = &[PackColumnAddition {
        table: "metadata_probe_state_4206",
        column: "value",
        affinity: PackColumnAffinity::Text,
    }];
}

#[async_trait]
impl PackRuntime for InstalledProbe {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        Self::EDGE_RULES
    }
    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }
    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        Self::NOTE_KIND_SPECS
    }

    fn entity_types(&self) -> &'static [EntityTypeDef] {
        match self.0 {
            InstallFault::MissingEntityTypes | InstallFault::MissingAll => &[],
            InstallFault::WrongAlias => &[EntityTypeDef {
                kind: EntityKind::Concept,
                type_name: "metadata_probe_type_4206",
                aliases: &["wrong_metadata_probe_alias_4206"],
            }],
            _ => Self::ENTITY_TYPES,
        }
    }

    fn note_embedding_policies(&self) -> &'static [NoteEmbeddingPolicySpec] {
        match self.0 {
            InstallFault::MissingEmbeddingPolicies | InstallFault::MissingAll => &[],
            InstallFault::WrongEmbeddingPolicy => &[NoteEmbeddingPolicySpec {
                kind: "metadata_probe_note_4206",
                policy: NoteEmbeddingPolicy::AllModels,
            }],
            _ => Self::NOTE_EMBEDDING_POLICIES,
        }
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: Self::NAME,
            statements: Self::SCHEMA_PLAN.expect("probe schema").statements,
        }
    }

    fn schema_column_additions(&self) -> &'static [PackColumnAddition] {
        match self.0 {
            InstallFault::MissingColumnAdditions | InstallFault::MissingAll => &[],
            InstallFault::WrongColumnAffinity => &[PackColumnAddition {
                table: "metadata_probe_state_4206",
                column: "value",
                affinity: PackColumnAffinity::Integer,
            }],
            _ => Self::SCHEMA_COLUMN_ADDITIONS,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "metadata probe has no handler for {verb:?}"
        )))
    }
}

struct InstallProbeFactory(InstallFault);

impl PackFactory for InstallProbeFactory {
    fn name(&self) -> &'static str {
        InstalledProbe::NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        InstalledProbe::REQUIRES
    }
    fn intentionally_verbless(&self) -> bool {
        true
    }

    fn create(&self, _runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(InstalledProbe(InstallFault::None))
    }

    fn create_install(&self, _runtime: KhiveRuntime) -> PackInstall {
        PackInstall {
            runtime: Box::new(InstalledProbe(self.0)),
            resolver: None,
            dispatch_hook: None,
        }
    }
}

#[test]
fn installed_metadata_check_observes_factory_install_and_nonempty_values() {
    static FACTORIES: [InstallProbeFactory; 8] = [
        InstallProbeFactory(InstallFault::None),
        InstallProbeFactory(InstallFault::MissingEntityTypes),
        InstallProbeFactory(InstallFault::MissingEmbeddingPolicies),
        InstallProbeFactory(InstallFault::MissingColumnAdditions),
        InstallProbeFactory(InstallFault::MissingAll),
        InstallProbeFactory(InstallFault::WrongAlias),
        InstallProbeFactory(InstallFault::WrongEmbeddingPolicy),
        InstallProbeFactory(InstallFault::WrongColumnAffinity),
    ];
    let expected: &[&[&str]] = &[
        &[],
        &["entity_types"],
        &["note_embedding_policies"],
        &["schema_column_additions"],
        &[
            "entity_types",
            "note_embedding_policies",
            "schema_column_additions",
        ],
        &["entity_types"],
        &["note_embedding_policies"],
        &["schema_column_additions"],
    ];
    assert_eq!(FACTORIES.len(), expected.len());
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let names = [
        <KgPack as Pack>::NAME.to_owned(),
        InstalledProbe::NAME.to_owned(),
    ];
    for (factory, expected) in FACTORIES.iter().zip(expected) {
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs_with_runtimes_with_extra_factories(
            &[factory],
            &names,
            &HashMap::new(),
            &runtime,
            &mut builder,
        )
        .expect("install the probe factory");
        let registry = builder.build().expect("probe metadata remains valid");
        assert_eq!(
            installed_drift::<InstalledProbe>(&registry),
            *expected,
            "factory install fault {:?}; create returns correct metadata in every case",
            factory.0,
        );
    }
}
