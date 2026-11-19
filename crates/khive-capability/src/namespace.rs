//! Namespace → ResourceId mapping.
//!
//! Uses BLAKE3 hash to deterministically map namespace strings to u128 ResourceIds.
//! Stateless: any kernel instance produces the same mapping for the same input.

use lion_core::ResourceId;

pub struct NamespaceMap;

impl NamespaceMap {
    pub fn resource_id(namespace: &str) -> ResourceId {
        let hash = blake3::hash(namespace.as_bytes());
        let bytes: [u8; 16] = hash.as_bytes()[..16]
            .try_into()
            .expect("blake3 produces 32 bytes");
        u128::from_le_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_mapping() {
        let a = NamespaceMap::resource_id("team:docs");
        let b = NamespaceMap::resource_id("team:docs");
        assert_eq!(a, b);
    }

    #[test]
    fn different_namespaces_different_ids() {
        let a = NamespaceMap::resource_id("team:docs");
        let b = NamespaceMap::resource_id("team:other");
        assert_ne!(a, b);
    }

    #[test]
    fn nonzero() {
        let id = NamespaceMap::resource_id("team:docs");
        assert_ne!(id, 0);
    }
}
