#[test]
fn brain_profile_params_has_required_profile_id() {
    let h = find_handler("brain.profile");
    assert!(!h.params.is_empty(), "brain.profile must have params");
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "profile_id" && p.required),
        "brain.profile must have required profile_id param"
    );
}

#[test]
fn brain_profiles_params_has_lifecycle_filter() {
    let h = find_handler("brain.profiles");
    assert!(!h.params.is_empty(), "brain.profiles must have params");
    assert!(
        h.params.iter().any(|p| p.name == "lifecycle"),
        "brain.profiles must document lifecycle filter param"
    );
}

#[test]
fn brain_resolve_params_has_consumer_kind_required() {
    let h = find_handler("brain.resolve");
    assert!(!h.params.is_empty(), "brain.resolve must have params");
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "consumer_kind" && p.required),
        "brain.resolve must have required consumer_kind"
    );
    assert!(
        h.params.iter().any(|p| p.name == "actor"),
        "brain.resolve must document optional actor"
    );
    assert!(
        h.params.iter().any(|p| p.name == "namespace"),
        "brain.resolve must document optional namespace"
    );
}

#[test]
fn brain_bind_params_has_required_profile_id_and_optionals() {
    let h = find_handler("brain.bind");
    assert!(!h.params.is_empty(), "brain.bind must have params");
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "profile_id" && p.required),
        "brain.bind must have required profile_id"
    );
    assert!(
        h.params.iter().any(|p| p.name == "actor"),
        "brain.bind must document actor"
    );
    assert!(
        h.params.iter().any(|p| p.name == "namespace"),
        "brain.bind must document namespace"
    );
    assert!(
        h.params.iter().any(|p| p.name == "consumer_kind"),
        "brain.bind must document consumer_kind"
    );
    assert!(
        h.params.iter().any(|p| p.name == "priority"),
        "brain.bind must document priority"
    );
}

#[test]
fn brain_unbind_params_non_empty_all_optional() {
    let h = find_handler("brain.unbind");
    assert!(!h.params.is_empty(), "brain.unbind must have params");
    assert!(
        h.params.iter().all(|p| !p.required),
        "brain.unbind params must all be optional (filter semantics)"
    );
    assert!(
        h.params.iter().any(|p| p.name == "profile_id"),
        "brain.unbind must document profile_id filter"
    );
    assert!(
        h.params.iter().any(|p| p.name == "actor"),
        "brain.unbind must document actor filter"
    );
}

#[test]
fn brain_activate_deactivate_archive_each_have_profile_id() {
    for verb in ["brain.activate", "brain.deactivate", "brain.archive"] {
        let h = find_handler(verb);
        assert!(!h.params.is_empty(), "{verb} must have params");
        assert!(
            h.params
                .iter()
                .any(|p| p.name == "profile_id" && p.required),
            "{verb} must have required profile_id param"
        );
    }
}

#[test]
fn brain_reset_params_has_optional_profile_id() {
    let h = find_handler("brain.reset");
    assert!(!h.params.is_empty(), "brain.reset must have params");
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "profile_id" && !p.required),
        "brain.reset profile_id must be optional (defaults to balanced-recall-v1)"
    );
}
