//! KG dialect for khive-mcp (ADR-025 + ADR-027).
//!
//! Bundles the `kg` and `gtd` packs into a single registrar so `khive-mcp`
//! does not need to import either pack crate directly. `khive-mcp` is a
//! generic dispatch shell; this crate owns the knowledge of which packs
//! the KG dialect provides and what names they answer to.
//!
//! ## Adding a new dialect
//!
//! New dialects can be added to `khive-mcp` by:
//! 1. Adding a Cargo dependency from `khive-mcp` to the new dialect crate.
//! 2. Adding a `register(server, runtime)` call in `khive-mcp/src/server.rs`.
//!
//! A future ADR may introduce a generic registrar composition API; that work
//! is not in this PR.

pub use khive_pack_gtd::GtdPack;
pub use khive_pack_kg::KgPack;
pub use khive_pack_memory::MemoryPack;

use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

/// Pack names this dialect knows about, in canonical lowercase form.
///
/// Used by `khive-mcp` to build its `BUILTIN_PACKS` constant and error
/// messages without importing either pack crate directly.
pub const BUILTIN_PACK_NAMES: &[&str] = &["kg", "gtd", "memory"];

/// Register a named pack from this dialect into `builder`.
///
/// Returns `Ok(())` when the name is recognised; returns `Err(name)` when the
/// name is unknown so `khive-mcp` can keep its existing pack-not-found error
/// without needing to know which packs this dialect provides.
pub fn register_pack(
    name: &str,
    runtime: KhiveRuntime,
    builder: &mut VerbRegistryBuilder,
) -> Result<(), String> {
    match name {
        "kg" => {
            builder.register(KgPack::new(runtime));
            Ok(())
        }
        "gtd" => {
            builder.register(GtdPack::new(runtime));
            Ok(())
        }
        "memory" => {
            builder.register(MemoryPack::new(runtime));
            Ok(())
        }
        other => Err(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use khive_runtime::{KhiveRuntime, RuntimeConfig};

    fn make_runtime() -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime")
    }

    #[test]
    fn builtin_pack_names_includes_kg_and_gtd() {
        assert!(BUILTIN_PACK_NAMES.contains(&"kg"));
        assert!(BUILTIN_PACK_NAMES.contains(&"gtd"));
        assert!(BUILTIN_PACK_NAMES.contains(&"memory"));
    }

    #[test]
    fn register_kg_pack_succeeds() {
        let rt = make_runtime();
        let mut builder = VerbRegistryBuilder::new();
        register_pack("kg", rt, &mut builder).expect("kg registration must succeed");
        let registry = builder.build().expect("registry builds");
        let verb_names: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
        // KG pack registers 11 verbs; spot-check a few.
        assert!(verb_names.contains(&"create"));
        assert!(verb_names.contains(&"link"));
        assert!(verb_names.contains(&"traverse"));
    }

    #[test]
    fn register_gtd_pack_succeeds() {
        let rt = make_runtime();
        let mut builder = VerbRegistryBuilder::new();
        register_pack("gtd", rt, &mut builder).expect("gtd registration must succeed");
        let registry = builder.build().expect("registry builds");
        let verb_names: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
        assert!(verb_names.contains(&"assign"));
        assert!(verb_names.contains(&"next"));
        assert!(verb_names.contains(&"complete"));
    }

    #[test]
    fn register_unknown_pack_returns_err() {
        let rt = make_runtime();
        let mut builder = VerbRegistryBuilder::new();
        let err = register_pack("nosuchpack", rt, &mut builder)
            .expect_err("unknown pack must return Err");
        assert_eq!(err, "nosuchpack");
    }

    #[test]
    fn both_packs_register_together() {
        let rt1 = make_runtime();
        let rt2 = make_runtime();
        let rt3 = make_runtime();
        let mut builder = VerbRegistryBuilder::new();
        register_pack("kg", rt1, &mut builder).unwrap();
        register_pack("gtd", rt2, &mut builder).unwrap();
        register_pack("memory", rt3, &mut builder).unwrap();
        let registry = builder.build().expect("registry builds");
        let verb_names: Vec<&str> = registry.all_verbs().iter().map(|v| v.name).collect();
        // KG verbs present
        assert!(verb_names.contains(&"link"));
        // GTD verbs present
        assert!(verb_names.contains(&"transition"));
        // Memory verbs present
        assert!(verb_names.contains(&"remember"));
        assert!(verb_names.contains(&"recall"));
    }
}
