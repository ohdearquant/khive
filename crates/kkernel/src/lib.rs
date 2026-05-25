//! kkernel — khive admin/management library.
//!
//! See [ADR-076](../../docs/adr/ADR-076-kkernel-and-mcp-split.md) for the
//! kernel/MCP split rationale. This library exposes the building blocks that
//! the `kkernel` binary composes into subcommands:
//!
//! - [`sync`] — build a queryable SQLite DB from NDJSON sources (issue #174).
//! - [`pack_introspect`] — enumerate registered packs and their handler surface.
//! - [`kg`] — KG validation, init, and hook management (ADR-034, ADR-035).
//! - [`engine`] — embedding model lifecycle management (ADR-043).
//! - [`vector`] — vector store introspection and orphan sweep (ADR-044).

pub mod engine;
pub mod kg;
pub mod pack_introspect;
pub mod sync;
pub mod vector;

// Force the pack crates into the binary so their `inventory::submit!` blocks
// run at startup (ADR-027). Cargo deps alone are not enough — the linker drops
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
    use khive_pack_comm::CommPack as _;
    use khive_pack_gtd::GtdPack as _;
    use khive_pack_kg::KgPack as _;
    use khive_pack_memory::MemoryPack as _;
    use khive_pack_schedule::SchedulePack as _;
}
