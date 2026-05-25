//! kkernel — khive admin/management library.
//!
//! See [ADR-003](../../docs/adr/ADR-003-system-architecture.md) for the
//! kernel/MCP split rationale. This library exposes the building blocks that
//! the `kkernel` binary composes into subcommands:
//!
//! - [`sync`] — build a queryable SQLite DB from NDJSON sources (issue #174).
//! - [`pack_introspect`] — enumerate registered packs and their handler surface.
//! - [`coordinator`] — SubstrateCoordinator for cross-backend dispatch (ADR-029).
//!
//! Migration and other admin operations will land here as separate modules.

pub mod coordinator;
pub mod pack_introspect;
pub mod sync;

// Force the pack crates into the binary so their `inventory::submit!` blocks
// run at startup. Cargo deps alone are not enough — the linker drops crates
// whose symbols aren't referenced, and `inventory` registration is one such
// dropped symbol. The simplest way to keep them is to re-export a marker
// type that the binary sees. We don't expose these in the public API; the
// `#[allow(unused_imports)]` makes the intent explicit.
#[doc(hidden)]
#[allow(unused_imports)]
mod _pack_links {
    use khive_pack_brain::BrainPack as _;
    use khive_pack_gtd::GtdPack as _;
    use khive_pack_kg::KgPack as _;
    use khive_pack_memory::MemoryPack as _;
}
