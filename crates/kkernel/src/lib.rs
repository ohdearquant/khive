//! kkernel — khive admin/management library.

mod atomic_apply;
pub mod code_audit;
pub mod coordinator;
pub mod dbpath;
pub mod engine;
pub mod exec;
pub mod kg;
pub mod pack_introspect;
pub mod reindex;
pub mod sync;
pub mod vector;

// `pending_events` (the scheduled-event drain) now lives in `khive-mcp`
// (`khive_mcp::pending_events`), not here — the daemon-resident tick (ADR-106)
// needs to call it from `khive-mcp::serve`, which cannot depend back on
// `kkernel`. `kkernel exec --pending-events` (`exec.rs`) calls the moved
// module directly.

// Force the pack crates into the binary so their `inventory::submit!` blocks
// run at startup. Cargo deps alone are not enough — the linker drops
// crates whose symbols aren't referenced, and `inventory` registration is one
// such dropped symbol. The simplest way to keep them is to reference a marker
// type that the binary sees. We don't expose these in the public API; the
// `#[allow(unused_imports)]` makes the intent explicit.
//
// To add a new first-party pack: (1) add its crate as a `[dependency]` in
// `kkernel/Cargo.toml`, (2) add a `use` line below referencing any public type
// — this is the force-link anchor that prevents linker dead-stripping.
#[doc(hidden)]
#[allow(unused_imports)]
mod _pack_links {
    use khive_pack_comm::CommPack as _;
    use khive_pack_gtd::GtdPack as _;
    use khive_pack_kg::KgPack as _;
    use khive_pack_memory::MemoryPack as _;
    use khive_pack_schedule::SchedulePack as _;
    use khive_pack_session::SessionPack as _;
}
