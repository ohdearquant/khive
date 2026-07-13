//! Schedule pack — `schedule.remind`, `schedule.schedule`, `schedule.agenda`, `schedule.cancel`.
//!
//! All verbs operate on `scheduled_event` notes. At fire time, the execution
//! environment delivers reminders to the creating actor's inbox and dispatches
//! scheduled actions.
pub mod handlers;
mod pack;
mod tests;
mod vocab;

pub use pack::SchedulePack;
