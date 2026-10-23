//! Charter workflows: a phrase-registry language (ADR-208) compiled into definitions, and a
//! recording engine that tracks runs, phases and observations (ADR-193 M1). The engine
//! records what happened and what each phase waits on; it admits and performs no action.

pub mod engine;
pub mod lang;
mod pack;
mod schema;
pub mod transition;

pub use pack::CharterPack;
