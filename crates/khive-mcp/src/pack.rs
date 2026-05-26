//! Pack registration helpers for `khive-mcp` (ADR-027).
//!
//! Pack discovery is handled by `inventory`-based self-registration: each pack
//! crate submits a `PackRegistration` at link time (via `inventory::submit!`),
//! and [`PackRegistry::register_packs`] walks the collected slice to instantiate
//! the requested packs.
//!
//! This module pulls in the pack crate types by name so the linker includes
//! their object files — and therefore their `inventory::submit!` constructors —
//! in the final binary. Without at least one symbol reference per crate the
//! linker may dead-strip the crate entirely and the inventory constructors will
//! not run.
//!
//! To add a new first-party pack: (1) add its crate as a `[dependency]` in
//! `khive-mcp/Cargo.toml`, (2) add a `pub use` line below referencing any
//! public type from the crate — this is the force-link anchor that keeps the
//! linker from stripping the `inventory::submit!` constructor.

pub use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};

// Force-link pack crates so their `inventory::submit!` constructors are
// included by the linker. These are the only direct references to the pack
// crate types inside `khive-mcp`.
#[doc(hidden)]
pub use khive_pack_brain::BrainPack as _BrainPack;
#[doc(hidden)]
pub use khive_pack_comm::CommPack as _CommPack;
#[doc(hidden)]
pub use khive_pack_gtd::GtdPack as _GtdPack;
#[doc(hidden)]
pub use khive_pack_kg::KgPack as _KgPack;
#[doc(hidden)]
pub use khive_pack_knowledge::KnowledgePack as _KnowledgePack;
#[doc(hidden)]
pub use khive_pack_memory::MemoryPack as _MemoryPack;
#[doc(hidden)]
pub use khive_pack_schedule::SchedulePack as _SchedulePack;
