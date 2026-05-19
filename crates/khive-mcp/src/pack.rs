//! Dialect registrar trait for `khive-mcp` (ADR-025 + ADR-027).
//!
//! A `DialectRegistrar` encapsulates which packs an external dialect crate
//! knows about and how to instantiate them by name. `KhiveMcpServer` is the
//! generic dispatch shell; the KG dialect is hardwired via `KgDialect`.
//!
//! ## v0.1.x status
//!
//! `khive-mcp` ships with the KG dialect hardwired: `server.rs` calls
//! `KgDialect::register(...)` directly. The `DialectRegistrar` trait exists
//! as a named seam for a future composable shell; wiring a second dialect
//! still requires adding a Cargo dependency and a `register(...)` call in
//! `server.rs`. A future ADR may introduce generic registrar composition.

use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

/// Register named packs from a dialect into a [`VerbRegistryBuilder`].
///
/// Implementations return `Ok(())` for every name they recognise and
/// `Err(name)` for names they do not handle.  `KhiveMcpServer` accumulates
/// errors and surfaces them as [`PackRegError`](crate::server::PackRegError)
/// if an unknown name is encountered.
pub trait DialectRegistrar {
    /// All pack names this dialect provides (lowercase, canonical).
    fn builtin_pack_names() -> &'static [&'static str];

    /// Try to register `name` into `builder` using `runtime`.
    ///
    /// Returns `Ok(())` on success; returns `Err(name)` if `name` is not
    /// known to this dialect, so callers can compose multiple registrars.
    fn register(
        name: &str,
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), String>;
}

/// The built-in KG dialect registrar.
///
/// Delegates to [`khive_dialect_kg::register_pack`] so `khive-mcp`'s server
/// module does not import `KgPack` or `GtdPack` directly.
pub struct KgDialect;

impl KgDialect {
    /// Pack names provided by the KG dialect, as a `const` slice.
    ///
    /// Exposed as an inherent `const fn` (not via the trait) so it can be used
    /// in `const` contexts (e.g. `pub const BUILTIN_PACKS`).
    pub const fn pack_names() -> &'static [&'static str] {
        khive_dialect_kg::BUILTIN_PACK_NAMES
    }
}

impl DialectRegistrar for KgDialect {
    fn builtin_pack_names() -> &'static [&'static str] {
        khive_dialect_kg::BUILTIN_PACK_NAMES
    }

    fn register(
        name: &str,
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), String> {
        khive_dialect_kg::register_pack(name, runtime, builder)
    }
}
