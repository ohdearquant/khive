//! Schedule pack — `schedule.remind`, `schedule.schedule`, `schedule.agenda`, `schedule.cancel`.
//!
//! All verbs operate on `scheduled_event` notes; trigger evaluation is the execution environment's responsibility.
pub mod handlers;
mod pack;
mod tests;
mod vocab;

pub use pack::SchedulePack;
