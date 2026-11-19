use super::*;

#[test]
fn bootstrap_and_validate() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();

    let cap_id = mgr
        .bootstrap_root_capability(1, "team:docs", Rights::read_write())
        .unwrap();

    mgr.validate(1, cap_id, "team:docs", Right::Read).unwrap();
    mgr.validate(1, cap_id, "team:docs", Right::Write).unwrap();
}

#[test]
fn wrong_namespace_denied() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();

    let cap_id = mgr
        .bootstrap_root_capability(1, "team:docs", Rights::read_write())
        .unwrap();

    let result = mgr.validate(1, cap_id, "team:other", Right::Read);
    assert!(result.is_err());
}

#[test]
fn insufficient_rights() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();

    let cap_id = mgr
        .bootstrap_root_capability(1, "team:docs", Rights::read_only())
        .unwrap();

    let result = mgr.validate(1, cap_id, "team:docs", Right::Write);
    assert!(result.is_err());
}

#[test]
fn compose_delegates_with_confinement() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    mgr.register_actor(2, SecurityLevel::Public).unwrap();

    let root_cap = mgr
        .bootstrap_root_capability(1, "team:docs", Rights::read_write())
        .unwrap();

    let child_cap = mgr
        .compose(root_cap, 2, Rights::singleton(Right::Read))
        .unwrap();

    mgr.validate(2, child_cap, "team:docs", Right::Read)
        .unwrap();

    let result = mgr.validate(2, child_cap, "team:docs", Right::Write);
    assert!(result.is_err());
}

#[test]
fn revoke_invalidates() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();

    let cap_id = mgr
        .bootstrap_root_capability(1, "team:docs", Rights::read_write())
        .unwrap();

    mgr.revoke(cap_id).unwrap();

    let result = mgr.validate(1, cap_id, "team:docs", Right::Read);
    assert!(result.is_err());
}

#[test]
fn sealed_grant_verifies_and_rejects_changed_target() {
    assert!(CapabilityManager::with_key([0; 32]).is_err());
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    let id = mgr.mint(1, "team:docs", Rights::read_write()).unwrap();
    let cap = mgr.get_cap(id).unwrap();
    assert!(mgr.verify_seal(cap));
    let changed = Capability::new(
        cap.id(),
        cap.holder(),
        cap.target() ^ 1,
        *cap.rights(),
        cap.parent(),
        cap.epoch(),
        *cap.signature(),
    )
    .unwrap();
    assert!(!mgr.verify_seal(&changed));
    assert!(!CapabilityManager::with_key([8; 32])
        .unwrap()
        .verify_seal(cap));

    // A caller-supplied parent must be checked before the core's parent + 1 encoding.
    let invalid_parent = Capability::new(
        cap.id(),
        cap.holder(),
        cap.target(),
        *cap.rights(),
        Some(CapId::MAX),
        cap.epoch(),
        *cap.signature(),
    )
    .unwrap();
    assert!(!mgr.verify_seal(&invalid_parent));
}

#[test]
fn delegation_intersects_rights_and_refuses_empty() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    mgr.register_actor(2, SecurityLevel::Public).unwrap();
    let parent = mgr.mint(1, "team:docs", Rights::read_only()).unwrap();
    let child = mgr.compose(parent, 2, Rights::read_write()).unwrap();
    let cap = mgr.get_cap(child).unwrap();
    assert_eq!(*cap.rights(), Rights::read_only());
    assert_eq!(cap.parent(), Some(parent));
    assert_eq!(cap.holder(), 2);
    assert_eq!(cap.target(), NamespaceMap::resource_id("team:docs"));
    assert!(mgr.verify_seal(cap));
    mgr.validate(2, child, "team:docs", Right::Read).unwrap();
    assert!(mgr.validate(2, child, "team:docs", Right::Write).is_err());

    let count = mgr.kernel.state().kernel().revocation().cap_count();
    let next = mgr.kernel.state().kernel().next_cap_id();
    assert!(matches!(
        mgr.compose(parent, 2, Rights::singleton(Right::Write)),
        Err(KernelError::DelegationFailed(_))
    ));
    assert_eq!(mgr.kernel.state().kernel().revocation().cap_count(), count);
    assert_eq!(mgr.kernel.state().kernel().next_cap_id(), next);
    mgr.validate(1, parent, "team:docs", Right::Read).unwrap();
}

#[test]
fn liveness_requires_every_ancestor() {
    for revoke_parent in [true, false] {
        let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
        for actor in 1..=3 {
            mgr.register_actor(actor, SecurityLevel::Public).unwrap();
        }
        let root = mgr.mint(1, "team:docs", Rights::read_write()).unwrap();
        let parent = mgr.compose(root, 2, Rights::read_write()).unwrap();
        let leaf = mgr.compose(parent, 3, Rights::read_only()).unwrap();
        let unrelated = mgr.mint(1, "team:docs", Rights::read_only()).unwrap();
        for id in [root, parent, leaf, unrelated] {
            assert!(mgr.is_live(id));
        }
        let ancestor = if revoke_parent { parent } else { root };
        let state = mgr.kernel.state().kernel().revocation();
        // Unlike public cascading revoke, this snapshot changes only the ancestor.
        // The same production walker must reject a still-valid leaf beneath it.
        let ancestor_only = state.revoke(ancestor);
        assert!(ancestor_only.is_valid(leaf));
        assert!(!ancestor_only.is_valid(ancestor));
        assert!(chain_is_live(state, &mgr.anchors, leaf));
        assert!(!chain_is_live(&ancestor_only, &mgr.anchors, leaf));
        assert!(chain_is_live(&ancestor_only, &mgr.anchors, unrelated));

        let anchor = mgr.get_cap(root).unwrap().parent().unwrap();
        let anchor_only = state.revoke(anchor);
        assert!(anchor_only.is_valid(leaf));
        assert!(!anchor_only.is_valid(anchor));
        assert!(!chain_is_live(&anchor_only, &mgr.anchors, leaf));
        assert!(chain_is_live(&anchor_only, &mgr.anchors, unrelated));

        mgr.revoke(ancestor).unwrap();
        assert!(!mgr.is_live(leaf));
        assert!(mgr.is_live(unrelated));
        assert!(mgr.validate(3, leaf, "team:docs", Right::Read).is_err());
        assert!(mgr.compose(leaf, 1, Rights::read_only()).is_err());
    }
}

#[test]
fn mint_and_delegate_keep_private_anchors_disjoint_and_unusable() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    mgr.register_actor(2, SecurityLevel::Public).unwrap();
    let root = mgr.mint(1, "team:docs", Rights::read_only()).unwrap();
    let child = mgr.compose(root, 2, Rights::read_only()).unwrap();
    let second = mgr
        .mint(1, "team:docs", Rights::singleton(Right::Write))
        .unwrap();
    let second_child = mgr
        .compose(second, 2, Rights::singleton(Right::Write))
        .unwrap();
    assert_eq!([root, child, second, second_child], [0, 1, 2, 3]);
    assert_eq!(mgr.anchors.len(), 2);
    assert!(mgr.anchors.contains(&(CapId::MAX - 1)));
    assert!(mgr.anchors.contains(&(CapId::MAX - 2)));
    let anchor = mgr.get_cap(root).unwrap().parent().unwrap();
    assert!(mgr.get_cap(anchor).is_none());
    assert!(!mgr.is_live(anchor));
    assert!(mgr.validate(1, anchor, "team:docs", Right::Read).is_err());
    assert!(mgr.compose(anchor, 2, Rights::read_only()).is_err());
    assert!(mgr.revoke(anchor).is_err());
    mgr.revoke(root).unwrap();
    assert!(!mgr.is_live(child));
    assert!(mgr.is_live(second_child));
    mgr.validate(2, second_child, "team:docs", Right::Write)
        .unwrap();
}

#[test]
fn rejected_mints_and_unknown_recipients_do_not_allocate() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    assert!(mgr.register_actor(0, SecurityLevel::Public).is_err());
    assert!(mgr.kernel.plugin_level(0).is_none());
    assert!(mgr.mint(0, "team:docs", Rights::read_only()).is_err());
    assert!(mgr.mint(99, "team:docs", Rights::read_only()).is_err());
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    assert!(mgr.mint(1, "team:docs", Rights::empty()).is_err());
    assert_eq!(mgr.kernel.state().kernel().revocation().cap_count(), 0);
    assert_eq!(mgr.kernel.state().kernel().next_cap_id(), 0);
    assert!(mgr.anchors.is_empty());

    let root = mgr.mint(1, "team:docs", Rights::read_only()).unwrap();
    let count = mgr.kernel.state().kernel().revocation().cap_count();
    let next = mgr.kernel.state().kernel().next_cap_id();
    assert!(matches!(
        mgr.compose(root, 99, Rights::read_only()),
        Err(KernelError::UnknownActor(_))
    ));
    assert_eq!(mgr.kernel.state().kernel().revocation().cap_count(), count);
    assert_eq!(mgr.kernel.state().kernel().next_cap_id(), next);

    for exhausted in [PRIVATE_ID_START - 1, CapId::MAX] {
        mgr.next_anchor_id = exhausted;
        assert!(matches!(
            mgr.mint(1, "team:docs", Rights::read_only()),
            Err(KernelError::IdSpaceExhausted)
        ));
        assert_eq!(mgr.kernel.state().kernel().revocation().cap_count(), count);
        assert_eq!(mgr.kernel.state().kernel().next_cap_id(), next);
    }
}

#[test]
fn seal_verification_does_not_replace_stored_authority() {
    let mut mgr = CapabilityManager::with_key([7; 32]).unwrap();
    mgr.register_actor(1, SecurityLevel::Public).unwrap();
    mgr.register_actor(2, SecurityLevel::Public).unwrap();
    let id = mgr.mint(1, "team:docs", Rights::read_only()).unwrap();
    let cap = mgr.get_cap(id).unwrap();
    // IDs are not signed; a valid payload cannot make an unknown ID authoritative.
    let unknown = Capability::new(
        50,
        cap.holder(),
        cap.target(),
        *cap.rights(),
        cap.parent(),
        cap.epoch(),
        *cap.signature(),
    )
    .unwrap();
    assert!(mgr.verify_seal(&unknown));
    assert!(mgr.get_cap(unknown.id()).is_none());
    assert!(!mgr.is_live(unknown.id()));
    assert!(mgr
        .validate(1, unknown.id(), "team:docs", Right::Read)
        .is_err());
    assert!(mgr.validate(2, id, "team:docs", Right::Read).is_err());

    // Even a canonical held record cannot bypass the seal check. Raw insertion is
    // test-only here; the public manager exposes no insertion or mutable kernel.
    let unsigned = Capability::new(
        51,
        1,
        cap.target(),
        *cap.rights(),
        cap.parent(),
        cap.epoch(),
        SealedTag::from_bytes([0; 32]),
    )
    .unwrap();
    mgr.kernel.insert_cap_raw(unsigned, 1).unwrap();
    assert!(matches!(
        mgr.validate(1, 51, "team:docs", Right::Read),
        Err(KernelError::InvalidSeal(51))
    ));
    assert!(matches!(
        mgr.compose(51, 2, Rights::read_only()),
        Err(KernelError::InvalidSeal(51))
    ));
    mgr.revoke(id).unwrap();
    assert!(mgr.verify_seal(mgr.get_cap(id).unwrap()));
    assert!(!mgr.is_live(id));
    assert!(mgr.validate(1, id, "team:docs", Right::Read).is_err());
}

#[test]
fn liveness_refuses_missing_untrusted_and_cyclic_roots() {
    let anchors = HashSet::new();
    let raw = |id, parent| {
        Capability::new(
            id,
            1,
            1,
            Rights::read_only(),
            parent,
            0,
            SealedTag::from_bytes([0; 32]),
        )
        .unwrap()
    };
    let mut missing = RevocationState::empty();
    missing.insert_mut(raw(1, Some(99))).unwrap();
    assert!(!chain_is_live(&missing, &anchors, 1));
    assert!(!chain_is_live(&missing, &anchors, 99));
    let mut untrusted = RevocationState::empty();
    untrusted.insert_mut(raw(1, None)).unwrap();
    assert!(!chain_is_live(&untrusted, &anchors, 1));
    let mut cycle = RevocationState::empty();
    cycle.insert_mut(raw(1, Some(2))).unwrap();
    cycle.insert_mut(raw(2, Some(1))).unwrap();
    assert!(cycle.is_valid(1));
    assert!(cycle.is_valid(2));
    assert!(!chain_is_live(&cycle, &anchors, 1));
}
