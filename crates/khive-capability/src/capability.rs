//! Keyed, process-local grants over one capability kernel.

use std::collections::HashSet;

use lion_core::state::RevocationState;
use lion_core::{CapId, Capability, Kernel, PluginId, Right, Rights, SealedTag, SecurityLevel};

use crate::error::KernelError;
use crate::namespace::NamespaceMap;

// Kernel-minted grants grow upward in the low half. Private anchors grow
// downward in the high half; MAX cannot be a parent in the seal encoding.
const PRIVATE_ID_START: CapId = 1_u128 << 127;

/// Trusted grant management, separate from caller authorization at a wire boundary.
///
/// The manager owns one kernel and its process-local seal key. It does not expose
/// raw insertion, mutable kernel access, or private bootstrap anchors as grants.
pub struct CapabilityManager {
    kernel: Kernel,
    anchors: HashSet<CapId>,
    next_anchor_id: CapId,
}

impl CapabilityManager {
    /// Construct with caller-supplied random key bytes; an all-zero key is refused.
    ///
    /// The caller must obtain these bytes from a cryptographically suitable source.
    /// It does not generate the seal key and cannot establish the supplied key's entropy.
    pub fn with_key(key: [u8; 32]) -> Result<Self, KernelError> {
        Ok(Self {
            kernel: Kernel::with_key(key).map_err(|e| KernelError::Core(e.to_string()))?,
            anchors: HashSet::new(),
            next_anchor_id: CapId::MAX - 1,
        })
    }

    pub fn register_actor(
        &mut self,
        plugin_id: PluginId,
        level: SecurityLevel,
    ) -> Result<(), KernelError> {
        if plugin_id == 0 {
            return Err(KernelError::Core("actor id must be nonzero".into()));
        }
        self.kernel
            .register_plugin(plugin_id, level, 0)
            .map_err(|e| KernelError::Core(e.to_string()))
    }

    /// Mint a sealed grant with exactly the requested nonempty rights.
    ///
    /// A private, unsealed anchor supplies this mint's namespace and rights. Only
    /// the kernel-delegated, sealed child is returned. This is a library operation,
    /// not a boot hook or an authorization decision for an untrusted caller.
    pub fn mint(
        &mut self,
        plugin_id: PluginId,
        namespace: &str,
        rights: Rights,
    ) -> Result<CapId, KernelError> {
        self.require_actor(plugin_id)?;
        if rights.is_empty() {
            return Err(KernelError::DelegationFailed(
                "rights cannot be empty".into(),
            ));
        }
        self.ensure_grant_id_available()?;
        let anchor_id = self.next_anchor_id;
        if !(PRIVATE_ID_START..CapId::MAX).contains(&anchor_id) {
            return Err(KernelError::IdSpaceExhausted);
        }
        let anchor = Capability::new(
            anchor_id,
            plugin_id,
            NamespaceMap::resource_id(namespace),
            rights,
            None,
            self.kernel.state().kernel().key_epoch(),
            SealedTag::from_bytes([0; 32]),
        )
        .map_err(|e| KernelError::Core(e.to_string()))?;
        self.kernel
            .insert_cap_raw(anchor, plugin_id)
            .map_err(|e| KernelError::Core(e.to_string()))?;
        self.anchors.insert(anchor_id);
        self.next_anchor_id -= 1;
        self.kernel
            .delegate_cap(anchor_id, plugin_id, rights)
            .map_err(|e| KernelError::DelegationFailed(e.to_string()))
    }

    /// Compatibility name for [`Self::mint`]; the returned grant is now sealed.
    ///
    /// Its parent is a private anchor, not another publicly usable grant.
    pub fn bootstrap_root_capability(
        &mut self,
        plugin_id: PluginId,
        namespace: &str,
        rights: Rights,
    ) -> Result<CapId, KernelError> {
        self.mint(plugin_id, namespace, rights)
    }

    /// Delegate from a current held grant; the kernel performs rights intersection.
    ///
    /// Management callers are trusted. This does not add a separate Delegate-bit
    /// policy or authorize a wire caller to act on another actor's grant.
    pub fn compose(
        &mut self,
        parent_cap_id: CapId,
        target_plugin: PluginId,
        requested_rights: Rights,
    ) -> Result<CapId, KernelError> {
        self.require_live_grant(parent_cap_id)?;
        self.require_actor(target_plugin)?;
        self.ensure_grant_id_available()?;
        self.kernel
            .delegate_cap(parent_cap_id, target_plugin, requested_rights)
            .map_err(|e| KernelError::DelegationFailed(e.to_string()))
    }

    /// Read a canonical grant, including a revoked grant; private anchors are hidden.
    pub fn get_cap(&self, cap_id: CapId) -> Option<&Capability> {
        (cap_id < PRIVATE_ID_START)
            .then(|| self.kernel.get_cap(cap_id))
            .flatten()
    }

    /// Verify only the signed payload under this manager's process-local key.
    ///
    /// The kernel seal covers holder, target, rights, parent and epoch. It does not
    /// cover the capability ID or mutable revocation flag. This predicate alone
    /// does not establish table membership, a holder's authority or liveness.
    pub fn verify_seal(&self, cap: &Capability) -> bool {
        // The core's parent encoding uses parent + 1; reject its sentinel before
        // invoking that encoding on a caller-supplied capability.
        cap.parent() != Some(CapId::MAX) && self.kernel.state().kernel().verify_cap_seal(cap)
    }

    /// Check the stored grant and every ancestor, ending at a known private anchor.
    ///
    /// This checks current liveness. Use [`Self::validate`] for holder, seal,
    /// namespace and right checks together. A later protected write still needs
    /// its own synchronization with revocation; this predicate is not a transaction.
    pub fn is_live(&self, cap_id: CapId) -> bool {
        chain_is_live(
            self.kernel.state().kernel().revocation(),
            &self.anchors,
            cap_id,
        )
    }

    pub fn validate(
        &self,
        plugin_id: PluginId,
        cap_id: CapId,
        namespace: &str,
        required_right: Right,
    ) -> Result<(), KernelError> {
        if !self.kernel.plugin_holds(plugin_id, cap_id) {
            return Err(KernelError::InsufficientRights {
                needed: format!("{required_right}"),
                have: "no capability held".into(),
            });
        }
        let cap = self.require_live_grant(cap_id)?;
        if cap.holder() != plugin_id {
            return Err(KernelError::InsufficientRights {
                needed: format!("{required_right}"),
                have: "capability belongs to another actor".into(),
            });
        }
        if cap.target() != NamespaceMap::resource_id(namespace) {
            return Err(KernelError::NamespaceDenied(namespace.to_string()));
        }
        if !cap.has_right(required_right) {
            return Err(KernelError::InsufficientRights {
                needed: format!("{required_right}"),
                have: format!("{}", cap.rights()),
            });
        }
        Ok(())
    }

    pub fn revoke(&mut self, cap_id: CapId) -> Result<(), KernelError> {
        self.get_cap(cap_id)
            .ok_or(KernelError::CapabilityRevoked(cap_id))?;
        self.kernel
            .revoke_cap(cap_id)
            .map_err(|e| KernelError::Core(e.to_string()))
    }

    fn require_actor(&self, plugin_id: PluginId) -> Result<(), KernelError> {
        if plugin_id == 0 || self.kernel.plugin_level(plugin_id).is_none() {
            return Err(KernelError::UnknownActor(plugin_id.to_string()));
        }
        Ok(())
    }

    fn ensure_grant_id_available(&self) -> Result<(), KernelError> {
        if self.kernel.state().kernel().next_cap_id() >= PRIVATE_ID_START {
            return Err(KernelError::IdSpaceExhausted);
        }
        Ok(())
    }

    fn require_live_grant(&self, cap_id: CapId) -> Result<&Capability, KernelError> {
        let cap = self
            .get_cap(cap_id)
            .ok_or(KernelError::CapabilityRevoked(cap_id))?;
        if !self.kernel.plugin_holds(cap.holder(), cap_id) {
            return Err(KernelError::UnknownActor(cap.holder().to_string()));
        }
        if !self.verify_seal(cap) {
            return Err(KernelError::InvalidSeal(cap_id));
        }
        if !self.is_live(cap_id) {
            return Err(KernelError::CapabilityRevoked(cap_id));
        }
        Ok(cap)
    }
}

// Use table size, not numeric IDs, to bound traversal. Anchors have high IDs and
// grants have low IDs, so a parent-less-than-child assumption does not apply.
fn chain_is_live(state: &RevocationState, anchors: &HashSet<CapId>, cap_id: CapId) -> bool {
    if cap_id >= PRIVATE_ID_START {
        return false;
    }
    let mut current = cap_id;
    for _ in 0..state.cap_count() {
        let Some(cap) = state.get(current) else {
            return false;
        };
        if !cap.is_valid() {
            return false;
        }
        if anchors.contains(&current) {
            return current >= PRIVATE_ID_START && current != CapId::MAX && cap.parent().is_none();
        }
        if current >= PRIVATE_ID_START {
            return false;
        }
        match cap.parent() {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
