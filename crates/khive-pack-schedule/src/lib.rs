//! Schedule pack — `schedule.remind`, `schedule.schedule`, `schedule.agenda`, `schedule.cancel`.
//!
//! All verbs operate on `scheduled_event` notes. At fire time, the execution
//! environment delivers reminders to the creating actor's inbox and dispatches
//! scheduled actions under creator identity derived from an immutable,
//! pack-written provenance event. The mirrored `created_by_actor` note
//! property is display metadata only and is never an authorization source.
pub mod handlers;
mod pack;
mod tests;
mod vocab;

pub use pack::SchedulePack;

/// Internal event verb used to bind a scheduled-event note to the actor from
/// the dispatch-minted [`khive_runtime::NamespaceToken`]. The event log is
/// append-only and cannot be written through the public KG CRUD verbs, unlike
/// a note's caller-editable `properties` object.
pub const CREATOR_PROVENANCE_VERB: &str = "schedule.creator_provenance";

/// Payload marker for the current creator-provenance event schema.
pub const CREATOR_PROVENANCE_MARKER_V1: &str = "schedule_creator_v1";
