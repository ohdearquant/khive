//! Runtime-owned history of confirmed node public keys.
//!
//! Call these methods on the runtime bound to comm's assigned backend, exactly
//! as for ordinary comm note operations. The runtime never supplies SQL. Pins
//! are retained indefinitely: removing a configuration entry does not revoke a
//! retained public key, and no pin is overwritten or deleted.
use crate::error::RuntimeResult;
use crate::KhiveRuntime;
use khive_db::stores::note::node_pins::NodePinStore;
pub use khive_db::stores::note::node_pins::{NodePinKey, NodePinRecord};

impl KhiveRuntime {
    fn node_pin_store(&self) -> NodePinStore {
        NodePinStore::new(self.backend().pool_arc())
    }

    /// Retain confirmed pins in one transaction. An exact existing payload is a
    /// no-op; different bytes at an existing identity refuse the whole call.
    pub async fn retain_confirmed_node_pins(&self, pins: Vec<NodePinRecord>) -> RuntimeResult<()> {
        self.node_pin_store().retain(pins).await?;
        Ok(())
    }

    /// Read the confirmed pin for one exact identity, including old key epochs.
    pub async fn confirmed_node_pin(
        &self,
        key: NodePinKey,
    ) -> RuntimeResult<Option<NodePinRecord>> {
        Ok(self.node_pin_store().get(key).await?)
    }
}

#[cfg(test)]
#[path = "node_pins_tests.rs"]
mod tests;
