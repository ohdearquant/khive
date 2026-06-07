//! khive-pack-schedule — Schedule pack: stores time-triggered reminders and verb dispatches.
//!
//! The pack exposes four verbs: `schedule.remind`, `schedule.schedule`,
//! `schedule.agenda`, and `schedule.cancel`. All operate on `scheduled_event`
//! notes. Trigger evaluation is the execution environment's responsibility —
//! the pack stores intent only.
pub mod handlers;
mod pack;
mod tests;
mod vocab;

pub use pack::SchedulePack;
