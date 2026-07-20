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
pub use khive_pack_blob::BlobPack as _BlobPack;
#[doc(hidden)]
pub use khive_pack_brain::BrainPack as _BrainPack;
#[doc(hidden)]
pub use khive_pack_code::CodePack as _CodePack;
#[doc(hidden)]
pub use khive_pack_comm::CommPack as _CommPack;
#[doc(hidden)]
pub use khive_pack_git::GitPack as _GitPack;
#[doc(hidden)]
pub use khive_pack_gtd::GtdPack as _GtdPack;
#[doc(hidden)]
pub use khive_pack_kg::KgPack as _KgPack;
#[doc(hidden)]
pub use khive_pack_memory::MemoryPack as _MemoryPack;
#[doc(hidden)]
pub use khive_pack_schedule::SchedulePack as _SchedulePack;
#[doc(hidden)]
pub use khive_pack_session::SessionPack as _SessionPack;
#[doc(hidden)]
pub use khive_pack_workspace::WorkspacePack as _WorkspacePack;
