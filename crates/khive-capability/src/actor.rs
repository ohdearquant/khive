//! Actor registry — maps session/process identifiers to lion-core PluginIds.
//!
//! Each connected client gets a
//! unique PluginId for the duration of its connection. The registry tracks
//! active actors and their associated metadata.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use lion_core::PluginId;

#[derive(Debug, Clone)]
pub struct ActorInfo {
    pub plugin_id: PluginId,
    pub session_id: String,
    pub namespace: String,
    pub tenant: String,
}

#[derive(Debug)]
pub struct ActorRegistry {
    actors: HashMap<String, ActorInfo>,
    next_id: AtomicU64,
}

impl ActorRegistry {
    pub fn new() -> Self {
        Self {
            actors: HashMap::new(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn register(&mut self, session_id: String, namespace: String, tenant: String) -> ActorInfo {
        let plugin_id = self.next_id.fetch_add(1, Ordering::Relaxed) as u128;
        let info = ActorInfo {
            plugin_id,
            session_id: session_id.clone(),
            namespace,
            tenant,
        };
        self.actors.insert(session_id, info.clone());
        info
    }

    pub fn get(&self, session_id: &str) -> Option<&ActorInfo> {
        self.actors.get(session_id)
    }

    pub fn remove(&mut self, session_id: &str) -> Option<ActorInfo> {
        self.actors.remove(session_id)
    }

    pub fn active_count(&self) -> usize {
        self.actors.len()
    }
}

impl Default for ActorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let mut reg = ActorRegistry::new();
        let info = reg.register("sess-abc".into(), "team:docs".into(), "tenant-a".into());
        assert_eq!(info.plugin_id, 1);
        assert_eq!(reg.get("sess-abc").unwrap().namespace, "team:docs");
    }

    #[test]
    fn sequential_ids() {
        let mut reg = ActorRegistry::new();
        let a = reg.register("s1".into(), "ns1".into(), "t1".into());
        let b = reg.register("s2".into(), "ns2".into(), "t1".into());
        assert_eq!(a.plugin_id, 1);
        assert_eq!(b.plugin_id, 2);
    }

    #[test]
    fn remove_actor() {
        let mut reg = ActorRegistry::new();
        reg.register("sess-abc".into(), "team:docs".into(), "t".into());
        assert_eq!(reg.active_count(), 1);
        reg.remove("sess-abc");
        assert_eq!(reg.active_count(), 0);
        assert!(reg.get("sess-abc").is_none());
    }
}
