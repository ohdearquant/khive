//! kkernel — khive admin/management library.

mod atomic_apply;
pub mod coordinator;
pub mod dbpath;
pub mod engine;
pub mod exec;
pub mod kg;
pub mod pack_introspect;
pub mod pending_events;
pub mod reindex;
pub mod sync;
pub mod vector;

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
    use khive_pack_brain::BrainPack as _;
    use khive_pack_code::CodePack as _;
    use khive_pack_comm::CommPack as _;
    use khive_pack_formal::FormalPack as _;
    use khive_pack_gtd::GtdPack as _;
    use khive_pack_kg::KgPack as _;
    use khive_pack_knowledge::KnowledgePack as _;
    use khive_pack_memory::MemoryPack as _;
    use khive_pack_schedule::SchedulePack as _;
    use khive_pack_session::SessionPack as _;
}
