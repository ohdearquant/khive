//! Pack registration helpers for `khive-mcp` (ADR-063).
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

pub use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};

// Force-link pack crates so their `inventory::submit!` constructors are
// included by the linker. These are the only direct references to the pack
// crate types inside `khive-mcp`.
#[doc(hidden)]
pub use khive_pack_brain::BrainPack as _BrainPack;
#[doc(hidden)]
pub use khive_pack_gtd::GtdPack as _GtdPack;
#[doc(hidden)]
pub use khive_pack_kg::KgPack as _KgPack;
#[doc(hidden)]
pub use khive_pack_memory::MemoryPack as _MemoryPack;
