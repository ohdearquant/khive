#[test]
fn brain_config_params_has_parameter() {
    let h = find_handler("brain.config");
    assert!(
        !h.params.is_empty(),
        "brain.config must document the parameter arg"
    );
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "parameter" && !p.required),
        "brain.config parameter must be optional"
    );
}

#[test]
fn brain_events_params_has_limit() {
    let h = find_handler("brain.events");
    assert!(
        !h.params.is_empty(),
        "brain.events must document the limit arg"
    );
    assert!(
        h.params.iter().any(|p| p.name == "limit" && !p.required),
        "brain.events limit must be optional"
    );
}

#[test]
fn brain_emit_params_non_empty_with_target_and_signal() {
    let h = find_handler("brain.emit");
    assert!(
        !h.params.is_empty(),
        "brain.emit must have params (mirrors brain.feedback)"
    );
    assert!(matches!(h.visibility, khive_types::Visibility::Subhandler));
    assert_eq!(h.params.iter().filter(|p| p.name == "id").count(), 1);
    assert!(!h.params.iter().any(|p| p.name == "target_id"));
    let id = h.params.iter().find(|p| p.name == "id").unwrap();
    assert!(id.required, "brain.emit must have required id");
    assert_eq!(id.param_type, "uuid");
    assert!(matches!(
        id.resolution_mode,
        khive_types::IdResolutionMode::UnscopedById
    ));
    assert!(id.description.contains("target_id"));
    assert!(id.description.contains("both"));
    assert!(id.description.contains("is rejected"));
    assert!(
        h.params.iter().any(|p| p.name == "signal" && p.required),
        "brain.emit must have required signal"
    );
}

#[test]
fn brain_bindings_params_all_optional() {
    let h = find_handler("brain.bindings");
    assert!(
        h.params.iter().all(|p| !p.required),
        "brain.bindings: all params must be optional filter args"
    );
    assert!(
        h.params.iter().any(|p| p.name == "profile_id"),
        "brain.bindings must document profile_id filter"
    );
    assert!(
        h.params.iter().any(|p| p.name == "consumer_kind"),
        "brain.bindings must document consumer_kind filter"
    );
}

#[test]
fn brain_create_profile_params_has_required_name() {
    let h = find_handler("brain.create_profile");
    assert!(
        !h.params.is_empty(),
        "brain.create_profile must have params"
    );
    assert!(
        h.params.iter().any(|p| p.name == "name" && p.required),
        "brain.create_profile must have required name param"
    );
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "consumer_kind" && !p.required),
        "brain.create_profile consumer_kind must be optional"
    );
}
