//! Runtime-owned declaration of finalizer constructor entry points
//! (ADR-115 Amendment 1 §3; executable contract §2).
//!
//! This is the sole admission criterion: a mutation is admission-capable if
//! and only if the final stored entity or note candidate reaches one of the
//! constructors declared below. The acceptance matrix in [`super::matrix`]
//! is generated from this declaration — it must never be hand-maintained in
//! parallel. Curation, atomic-prepare, proposal materialization, knowledge,
//! git, session, MCP direct writes, edge metadata, proposal-only metadata,
//! merge reasons, and embedding-content overrides are deliberately absent:
//! they remain reservation-only and out of the admission surface.

/// The stored substrate a declared constructor produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) enum Substrate {
    Entity,
    Note,
}

/// One admission-capable finalizer entry point.
///
/// `id` is the stable, unique identifier used by the generated acceptance
/// matrix and by downstream reachability checks; it is not a wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) struct FinalizerEntryPoint {
    pub(crate) id: &'static str,
    pub(crate) constructor: &'static str,
    pub(crate) mutation: &'static str,
    pub(crate) substrate: Substrate,
    pub(crate) origins: &'static [&'static str],
}

/// The complete, closed set of admission-capable finalizer entry points.
///
/// Exactly six rows, per the executable contract §2 table. Direct
/// code-ingest is an origin of an entity or note row, not a seventh family.
#[allow(dead_code)] // consumed by the execution/outcome increment (step 15)
pub(crate) const FINALIZER_ENTRY_POINTS: &[FinalizerEntryPoint] = &[
    FinalizerEntryPoint {
        id: "entity.create",
        constructor: "entity candidate",
        mutation: "create",
        substrate: Substrate::Entity,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "entity.update",
        constructor: "entity candidate",
        mutation: "update",
        substrate: Substrate::Entity,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "entity.bulk",
        constructor: "entity candidate",
        mutation: "bulk",
        substrate: Substrate::Entity,
        origins: &["runtime"],
    },
    FinalizerEntryPoint {
        id: "note.create",
        constructor: "note candidate",
        mutation: "create",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "note.update",
        constructor: "note candidate",
        mutation: "update",
        substrate: Substrate::Note,
        origins: &["runtime", "code.ingest"],
    },
    FinalizerEntryPoint {
        id: "note.atomic_message",
        constructor: "note candidate",
        mutation: "atomic message",
        substrate: Substrate::Note,
        origins: &["runtime"],
    },
];

/// A source-level properties write. This inventory describes reservation
/// coverage today; it grants no route stamp authority (ADR-115 Amendment 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct RouteInventoryEntry {
    pub(crate) id: &'static str,
    /// `crate-name/src/file.rs::enclosing::function`.
    pub(crate) site: &'static str,
    pub(crate) target: Substrate,
    pub(crate) write_class: WriteClass,
    pub(crate) kind_policy: KindPolicy,
    pub(crate) reservation: Reservation,
    pub(crate) transaction: TransactionOwner,
    pub(crate) stamp: StampCapability,
    pub(crate) family: Option<&'static str>,
    pub(crate) acceptance: Acceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum WriteClass {
    WholeObject,
    SingleKey { key_path: &'static str },
    PrivilegedEscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum KindPolicy {
    KindHookCreate,
    SpecializedWriter,
    UpdateAgainstSnapshot,
    ProposalRevalidate,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Reservation {
    NamedCheck {
        function: &'static str,
        file: &'static str,
    },
    ByConstruction,
    PrivilegedEscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum TransactionOwner {
    WriterTx,
    WriterTask,
    RunAtomicUnit,
    WithWriter,
    SingleStatement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum StampCapability {
    ReservationOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Acceptance {
    Test { path: &'static str },
    Missing,
}

/// Source routes known to the signed Amendment 5 census. The source census
/// test below is deliberately closed-world: any still-unlisted production site
/// fails, rather than being inferred into this table.
#[allow(dead_code)]
pub(crate) const ROUTE_INVENTORY: &[RouteInventoryEntry] = &[
    RouteInventoryEntry {
        id: "runtime.atomic.entity.create",
        site: "khive-runtime/src/atomic_prepare.rs::prepare_add_entity",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::ProposalRevalidate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.create"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.atomic.entity.update",
        site: "khive-runtime/src/atomic_prepare.rs::prepare_update_entity_plan_with_version_and_type",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "prepare_update_entity", file: "khive-runtime/src/operations.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.update"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.bulk.entity",
        site: "khive-runtime/src/operations.rs::bulk_entity_plan",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTx,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.bulk"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.atomic.note.create",
        site: "khive-runtime/src/atomic_prepare.rs::prepare_add_note",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::ProposalRevalidate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.create"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.atomic.note.update",
        site: "khive-runtime/src/note_write.rs::KhiveRuntime::prepare_versioned_note_update",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "prepare_update_note_from_snapshot", file: "khive-runtime/src/curation.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.update"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.atomic.message",
        site: "khive-runtime/src/atomic_message.rs::prepare_atomic_note_requests",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.atomic_message"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "gtd.transition.statement",
        site: "khive-pack-gtd/src/handlers.rs::gtd_transition_statement",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTx,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-gtd/tests/lifecycle.rs::lifecycle_writes_refuse_a_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "schedule.activate",
        site: "khive-pack-schedule/src/handlers.rs::activate_with_creator_provenance",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-schedule/src/handlers.rs::reservation_tests::public_schedule_verbs_refuse_reserved_key_on_staged_row" },
    },
    RouteInventoryEntry {
        id: "comm.heartbeat",
        site: "khive-pack-comm/src/handlers.rs::handle_heartbeat",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-comm/tests/integration.rs::heartbeat_refuses_carried_reserved_property_without_changing_row" },
    },
    RouteInventoryEntry {
        id: "pending.claim",
        site: "khive-mcp/src/pending_events.rs::claim_pending_event",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "check_fixed_path_whole_object_snapshot", file: "khive-mcp/src/pending_events.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.corrupt",
        site: "khive-mcp/src/pending_events.rs::finalize_corrupt_receipt",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.finalize",
        site: "khive-mcp/src/pending_events.rs::finalize_firing_event",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.invoking",
        site: "khive-mcp/src/pending_events.rs::mark_dispatch_invoking",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "check_fixed_path_whole_object_snapshot", file: "khive-mcp/src/pending_events.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.outcome",
        site: "khive-mcp/src/pending_events.rs::persist_dispatch_outcome",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "check_fixed_path_whole_object_snapshot", file: "khive-mcp/src/pending_events.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.requeue",
        site: "khive-mcp/src/pending_events.rs::requeue_legacy_claim",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "check_fixed_path_whole_object_snapshot", file: "khive-mcp/src/pending_events.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::whole_object_dispatch_writes_refuse_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "pending.lease",
        site: "khive-mcp/src/pending_events.rs::renew_dispatch_lease",
        target: Substrate::Note,
        write_class: WriteClass::SingleKey { key_path: "$.lease_expires_at" },
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::ByConstruction,
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-mcp/src/pending_events.rs::tests::renewable_lease_prevents_live_overrun_reclaim_and_double_dispatch" },
    },
    RouteInventoryEntry {
        id: "code.source.mutate",
        site: "khive-pack-code/src/source_ingest.rs::mutate_entity",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-code/src/source_ingest.rs::code_entity_mutation_refuses_reserved_candidate_and_carried_properties" },
    },
    RouteInventoryEntry {
        id: "comm.ingest.legacy_slug",
        site: "khive-pack-comm/src/handlers.rs::handle_ingest",
        target: Substrate::Note,
        write_class: WriteClass::SingleKey { key_path: "$.channel_slug" },
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::ByConstruction,
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "comm.reply.read",
        site: "khive-pack-comm/src/handlers.rs::handle_reply",
        target: Substrate::Note,
        write_class: WriteClass::SingleKey { key_path: "$.read" },
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::ByConstruction,
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-comm/tests/integration.rs::i113_addressee_reply_succeeds" },
    },
    RouteInventoryEntry {
        id: "comm.read.one",
        site: "khive-pack-comm/src/handlers.rs::mark_read_target",
        target: Substrate::Note,
        write_class: WriteClass::SingleKey { key_path: "$.read" },
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::ByConstruction,
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-comm/tests/integration.rs::i1797_read_returns_message_and_body_false_keeps_ack_shape" },
    },
    RouteInventoryEntry {
        id: "comm.read.atomic",
        site: "khive-pack-comm/src/handlers.rs::mark_read_targets_atomic",
        target: Substrate::Note,
        write_class: WriteClass::SingleKey { key_path: "$.read" },
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::ByConstruction,
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-comm/tests/integration.rs::i1387_atomic_mark_read_rolls_back_an_earlier_live_patch" },
    },
    RouteInventoryEntry {
        id: "gtd.repair",
        site: "khive-pack-gtd/src/repair.rs::checked_update_sql",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-gtd/tests/repair.rs::repair_apply_refuses_a_carried_reserved_property" },
    },
    RouteInventoryEntry {
        id: "schedule.cancel",
        site: "khive-pack-schedule/src/handlers.rs::cancel_pending_event",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::SingleStatement,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-schedule/src/handlers.rs::reservation_tests::public_cancel_refuses_reserved_key_on_pending_row" },
    },
    RouteInventoryEntry {
        id: "web.get_or_create",
        site: "khive-pack-web/src/entities.rs::get_or_create",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-pack-web/src/entities.rs::tests::get_or_create_rejects_reserved_properties_before_placeholder_insert" },
    },
    RouteInventoryEntry {
        id: "curation.entity.update",
        site: "khive-runtime/src/curation.rs::KhiveRuntime::persist_prepared_entity_update",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.update"),
        acceptance: Acceptance::Test { path: "khive-runtime/src/curation.rs::tests::persist_prepared_entity_update_rejects_reserved_final_object" },
    },
    RouteInventoryEntry {
        id: "curation.outbound.delivery",
        site: "khive-runtime/src/curation.rs::KhiveRuntime::replace_outbound_message_properties",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/curation.rs::tests::outbound_property_replacements_refuse_carried_reserved_key" },
    },
    RouteInventoryEntry {
        id: "curation.outbound.owner",
        site: "khive-runtime/src/curation.rs::KhiveRuntime::replace_outbound_message_properties_as_owner",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/curation.rs::tests::outbound_property_replacements_refuse_carried_reserved_key" },
    },
    RouteInventoryEntry {
        id: "curation.merge.entity",
        site: "khive-runtime/src/curation.rs::merge_entity_sql",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::UpdateAgainstSnapshot,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WithWriter,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/curation/merge_reservation_tests.rs::merge_reservation_refuses_forged_source_and_target_before_domain_mutation" },
    },
    RouteInventoryEntry {
        id: "runtime.keyed_message",
        site: "khive-runtime/src/keyed_message.rs::create_keyed_message_pair",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.atomic_message"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.note_create",
        site: "khive-runtime/src/note_create.rs::prepare_note_create",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::RunAtomicUnit,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.create"),
        acceptance: Acceptance::Test { path: "khive-runtime/src/operations.rs::tests::create_note_rejects_reserved_secret_gate_key" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.insert_note_if_absent",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::insert_note_if_absent",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_reserved_note_properties", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.replace_note_if_unchanged",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::replace_note_if_unchanged",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_reserved_note_properties", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.update_note_properties",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::update_note_properties",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_reserved_note_properties", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.upsert_note",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::upsert_note",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_reserved_note_properties", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.upsert_notes",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::upsert_notes",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_reserved_note_properties", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.set_note_property",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::set_note_property",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_existing_secret_gate_property", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.try_patch_note_property",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::try_patch_note_property",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_existing_secret_gate_property", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.note_store.patch_note_property_atomic",
        site: "khive-runtime/src/note_store_guard.rs::PolicyEnforcingNoteStore::patch_note_property_atomic",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::None,
        reservation: Reservation::NamedCheck { function: "reject_existing_secret_gate_property", file: "khive-runtime/src/note_store_guard.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/note_store_guard/tests.rs::public_note_store_refuses_reserved_property_on_every_whole_object_route" },
    },
    RouteInventoryEntry {
        id: "runtime.claim_entity",
        site: "khive-runtime/src/operations.rs::KhiveRuntime::claim_entity_if_absent",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.create"),
        acceptance: Acceptance::Missing,
    },
    RouteInventoryEntry {
        id: "runtime.create_entity_inner",
        site: "khive-runtime/src/operations.rs::KhiveRuntime::create_entity_with_embedding_report_inner",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.create"),
        acceptance: Acceptance::Test { path: "khive-runtime/src/operations.rs::tests::create_entity_rejects_reserved_secret_gate_key" },
    },
    RouteInventoryEntry {
        id: "runtime.create_note_inner",
        site: "khive-runtime/src/operations.rs::KhiveRuntime::create_note_inner",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.create"),
        acceptance: Acceptance::Test { path: "khive-runtime/src/operations.rs::tests::create_note_rejects_reserved_secret_gate_key" },
    },
    RouteInventoryEntry {
        id: "runtime.import.entity",
        site: "khive-runtime/src/portability.rs::KhiveRuntime::import_kg",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::KindHookCreate,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-runtime/src/portability.rs::tests::import_entity_with_reserved_secret_gate_property_is_rejected" },
    },
    RouteInventoryEntry {
        id: "vcs.sync.entities",
        site: "khive-vcs/src/sync.rs::upsert_entities",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: None,
        acceptance: Acceptance::Test { path: "khive-vcs/src/sync.rs::direct_entity_upsert_refuses_reserved_properties_before_storage" },
    },
    RouteInventoryEntry {
        id: "kkernel.code_ingest.entity",
        site: "kkernel/src/code_ingest.rs::persist_ingest_entity",
        target: Substrate::Entity,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: Some("entity.create"),
        acceptance: Acceptance::Test { path: "kkernel/src/code_ingest.rs::tests::direct_ingest_writers_reject_reserved_properties_before_storage" },
    },
    RouteInventoryEntry {
        id: "kkernel.code_ingest.note",
        site: "kkernel/src/code_ingest.rs::persist_ingest_note",
        target: Substrate::Note,
        write_class: WriteClass::WholeObject,
        kind_policy: KindPolicy::SpecializedWriter,
        reservation: Reservation::NamedCheck { function: "reject_reserved_secret_gate_property", file: "khive-runtime/src/secret_gate.rs" },
        transaction: TransactionOwner::WriterTask,
        stamp: StampCapability::ReservationOnly,
        family: Some("note.create"),
        acceptance: Acceptance::Test { path: "kkernel/src/code_ingest.rs::tests::direct_ingest_writers_reject_reserved_properties_before_storage" },
    },
];

/// Existing routes without a real-path acceptance test are explicit debt.
#[allow(dead_code)]
pub(crate) const PINNED_MISSING_ACCEPTANCE: usize = 9;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn declares_exactly_six_entry_points() {
        assert_eq!(FINALIZER_ENTRY_POINTS.len(), 6);
    }

    #[test]
    fn entry_point_ids_are_unique() {
        let ids: BTreeSet<&str> = FINALIZER_ENTRY_POINTS.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), FINALIZER_ENTRY_POINTS.len());
    }

    #[test]
    fn matches_contract_id_list_exactly() {
        let ids: Vec<&str> = FINALIZER_ENTRY_POINTS.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![
                "entity.create",
                "entity.update",
                "entity.bulk",
                "note.create",
                "note.update",
                "note.atomic_message",
            ]
        );
    }

    #[test]
    fn every_finalizer_family_has_a_route() {
        for entry in FINALIZER_ENTRY_POINTS {
            assert!(
                ROUTE_INVENTORY
                    .iter()
                    .any(|route| route.family == Some(entry.id)),
                "{} has no source route",
                entry.id
            );
        }
    }

    #[test]
    fn missing_acceptance_count_is_pinned() {
        assert_eq!(
            ROUTE_INVENTORY
                .iter()
                .filter(|route| route.acceptance == Acceptance::Missing)
                .count(),
            PINNED_MISSING_ACCEPTANCE
        );
    }
}
