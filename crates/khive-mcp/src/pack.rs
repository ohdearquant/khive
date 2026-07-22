//! Pack registration helpers for `khive-mcp`.
//!
//! Force-references one public symbol per pack crate so the linker includes their
//! `inventory::submit!` constructors in the final binary. To add a new pack: add
//! the crate as a dependency and add a `pub use` line referencing any public type.

pub use khive_runtime::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};

// Force-link pack crates so their `inventory::submit!` constructors are
// included by the linker. These are the only direct references to the pack
// crate types inside `khive-mcp`.
#[doc(hidden)]
pub use khive_pack_kg::KgPack as _KgPack;
