//! Operation graphs (ADR-222): an operation type is published once, as data, with its slots,
//! its steps and the rules that ask or skip a step. An operation is opened against one pinned
//! type version and grows only by appended records: bindings of identities to slots, facts and
//! acts, each with the time it was observed and the time it was recorded. The frontier is a
//! read: from the records as of a time, it lists each act owed now, each act blocked and what
//! it waits on, and each slot nobody has bound yet, grouped by who supplies it.

mod definition;
mod frontier;
mod graph;
mod pack;
mod resolve;
mod schema;

pub use pack::OperationPack;
